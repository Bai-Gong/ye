//! Streaming PCM WAV export with presentation-time alignment and transactional files.
//! Preserve source sample rate/layout, remove codec priming/padding and fill timeline
//! gaps with silence. This is format conversion, not pitch-preserving time stretching.

use crate::{
    codec_pcm::{PcmEncoder, PcmFormat},
    convert::ConvertError,
};
use std::{
    fs::File,
    io::BufReader,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};
use ye_codec::{
    AudioEncoderConfig, AudioFrame, CodecId, CodecRegistry, Encoded, Encoder, EncoderConfig, Frame,
    MediaType,
};
use ye_container::{ContainerRegistry, Metadata, Muxer, ReadSeek, StreamTemplate, WriteSeek};
use ye_pixfmt::{ChannelLayout, SampleFormat};
use ye_time::{Rational, Rounding, Timestamp};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WavExportOptions {
    /// Actual container index. None requires exactly one audio stream.
    pub stream_index: Option<u32>,
    pub format: PcmFormat,
}
impl Default for WavExportOptions {
    fn default() -> Self {
        Self {
            stream_index: None,
            format: PcmFormat::S16,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WavExportStats {
    pub sample_rate: u32,
    pub channels: u32,
    /// Sample frames per channel, including any leading timeline silence.
    pub sample_frames: u64,
}

fn invalid(message: &str) -> ConvertError {
    ConvertError::InvalidData(message.into())
}
fn check_cancel(cancel: &AtomicBool) -> Result<(), ConvertError> {
    if cancel.load(Ordering::Relaxed) {
        Err(ConvertError::Cancelled)
    } else {
        Ok(())
    }
}
fn io_error(error: std::io::Error) -> ConvertError {
    ye_container::ContainerError::Io(error).into()
}

struct Writer<'a> {
    encoder: PcmEncoder,
    mux: Box<dyn Muxer>,
    rate: u32,
    layout: ChannelLayout,
    pending: Vec<u8>,
    written: i64,
    total: i64,
    cancel: &'a AtomicBool,
    progress: &'a dyn Fn(i64, i64),
}
impl Writer<'_> {
    fn align(&self) -> usize {
        self.layout.channels() as usize * 4
    }
    fn cursor(&self) -> i64 {
        self.written + (self.pending.len() / self.align()) as i64
    }
    fn drain(&mut self) -> Result<(), ConvertError> {
        loop {
            check_cancel(self.cancel)?;
            match self.encoder.pull()? {
                Encoded::Packet(packet) => self.mux.write_packet(&packet)?,
                Encoded::NeedInput | Encoded::Finished => return Ok(()),
            }
        }
    }
    fn flush(&mut self) -> Result<(), ConvertError> {
        check_cancel(self.cancel)?;
        if self.pending.is_empty() {
            return Ok(());
        }
        let frames = self.pending.len() / self.align();
        let mut frame = AudioFrame::new(
            SampleFormat::F32,
            self.layout,
            self.rate,
            frames,
            Some(Timestamp::new(
                self.written,
                Rational::new(1, self.rate as i32),
            )),
        )?;
        frame.data_mut().copy_from_slice(&self.pending);
        self.encoder.feed(&Frame::Audio(frame))?;
        self.drain()?;
        self.written += frames as i64;
        self.pending.clear();
        let micro = |n: i64| ((i128::from(n) * 1_000_000) / i128::from(self.rate)) as i64;
        (self.progress)(micro(self.written), micro(self.total));
        check_cancel(self.cancel)
    }
    fn push(&mut self, mut data: &[u8]) -> Result<(), ConvertError> {
        while !data.is_empty() {
            check_cancel(self.cancel)?;
            let take = data.len().min(1024 * self.align() - self.pending.len());
            self.pending.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.pending.len() == 1024 * self.align() {
                self.flush()?;
            }
        }
        Ok(())
    }
    fn silence_to(&mut self, end: i64) -> Result<(), ConvertError> {
        let zero = [0u8; 8192];
        while self.cursor() < end {
            let count = (end - self.cursor()).min(1024) as usize;
            self.push(&zero[..count * self.align()])?;
        }
        Ok(())
    }
}

/// Decode a selected audio stream into WAV, keeping its sample rate and channel layout.
/// Input formats are those enabled on `ye` (e.g. wav, or mp4+aac). Requires a known
/// presentation duration. Output owns the writer; errors may leave partial bytes.
/// For files use [`export_wav`] to preserve existing targets and remove partial output.
/// Progress reports processed/total presentation microseconds and is monotonic.
pub fn write_wav(
    input: Box<dyn ReadSeek>,
    output: Box<dyn WriteSeek>,
    options: &WavExportOptions,
    cancel: &AtomicBool,
    progress: &dyn Fn(i64, i64),
) -> Result<WavExportStats, ConvertError> {
    check_cancel(cancel)?;
    let mut containers = ContainerRegistry::new();
    crate::container::register_enabled(&mut containers);
    let mut codecs = CodecRegistry::new();
    crate::register_decoders(&mut codecs);
    let mut source = crate::timeline::Source::open_selected(
        &containers,
        &codecs,
        input,
        MediaType::Audio,
        0,
        None,
        options.stream_index,
    )?;
    let params = source
        .stream
        .codec
        .audio
        .as_ref()
        .ok_or_else(|| invalid("missing audio parameters"))?;
    let rate = params.sample_rate;
    let layout = params
        .channel_layout
        .ok_or_else(|| invalid("missing channel layout"))?;
    if rate == 0
        || rate > i32::MAX as u32
        || ![ChannelLayout::MONO, ChannelLayout::STEREO].contains(&layout)
    {
        return Err(ConvertError::InvalidArguments(
            "WAV export requires mono/stereo and a positive sample rate".into(),
        ));
    }
    let tb = Rational::new(1, rate as i32);
    let start = source.stream.start_time.unwrap_or(Timestamp::new(0, tb));
    let duration = source
        .stream
        .duration
        .ok_or_else(|| invalid("WAV export requires a known presentation duration"))?;
    let total = start
        .checked_add(duration)?
        .rescale_with(tb, Rounding::Up)?
        .ticks();
    let valid_start = start.rescale_with(tb, Rounding::Up)?.ticks().max(0);
    if total <= 0 || total <= valid_start {
        return Err(invalid("empty audio presentation window"));
    }
    // RIFF has a 32-bit length; reject impossible exports before writing payload.
    let data_bytes = (total as u64)
        .checked_mul(u64::from(layout.channels()) * u64::from(options.format.bits() / 8));
    if data_bytes.is_none_or(|bytes| bytes > u64::from(u32::MAX) - 128) {
        return Err(ConvertError::InvalidArguments(
            "WAV exceeds RIFF size limit; RF64 is not supported".into(),
        ));
    }
    let encoder = PcmEncoder::new(
        &EncoderConfig {
            codec: CodecId::Pcm,
            time_base: tb,
            bit_rate: None,
            video: None,
            audio: Some(AudioEncoderConfig {
                sample_rate: rate,
                channel_layout: layout,
                format: SampleFormat::F32,
            }),
        },
        options.format,
    )?;
    let mut mux = containers.create_muxer("wav", output)?;
    mux.write_header(&[StreamTemplate {
        media_type: MediaType::Audio,
        codec: encoder.parameters(),
        time_base: tb,
        frame_rate: None,
        metadata: Metadata::new(),
    }])?;
    let mut writer = Writer {
        encoder,
        mux,
        rate,
        layout,
        pending: Vec::with_capacity(8192),
        written: 0,
        total,
        cancel,
        progress,
    };
    let mut saw = false;
    while let Some(frame) = source.next(cancel)? {
        let Frame::Audio(frame) = frame else {
            return Err(invalid("audio decoder returned video"));
        };
        if frame.format() != SampleFormat::F32
            || frame.sample_rate() != rate
            || *frame.channel_layout() != layout
            || !frame.data().len().is_multiple_of(writer.align())
        {
            return Err(invalid("audio format changed or PCM is malformed"));
        }
        let at = frame
            .pts()
            .ok_or_else(|| invalid("audio frame has no PTS"))?
            .rescale(tb)?
            .ticks();
        let end = at
            .checked_add(
                i64::try_from(frame.frames()).map_err(|_| invalid("audio frame too long"))?,
            )
            .ok_or_else(|| invalid("audio timestamp overflow"))?;
        let from = at.max(valid_start).max(writer.cursor());
        let to = end.min(total);
        if from < to {
            writer.silence_to(from)?;
            let begin = usize::try_from(from - at).map_err(|_| invalid("audio offset overflow"))?
                * writer.align();
            let stop = usize::try_from(to - at).map_err(|_| invalid("audio offset overflow"))?
                * writer.align();
            writer.push(&frame.data()[begin..stop])?;
            saw = true;
        }
        if writer.cursor() >= total {
            break;
        }
    }
    if !saw {
        return Err(invalid("no audio samples in presentation window"));
    }
    writer.silence_to(total)?;
    writer.flush()?;
    writer.encoder.flush();
    writer.drain()?;
    check_cancel(cancel)?;
    writer.mux.write_trailer()?;
    check_cancel(cancel)?;
    Ok(WavExportStats {
        sample_rate: rate,
        channels: layout.channels(),
        sample_frames: total as u64,
    })
}

/// Export without replacing an existing file. Cancel/failure removes only our temporary file.
pub fn export_wav(
    input: &Path,
    output: &Path,
    options: &WavExportOptions,
    cancel: &AtomicBool,
    progress: &dyn Fn(i64, i64),
) -> Result<WavExportStats, ConvertError> {
    check_cancel(cancel)?;
    let input = File::open(input).map_err(io_error)?;
    let transaction = crate::output::OutputTransaction::new(output).map_err(io_error)?;
    let stats = write_wav(
        Box::new(BufReader::new(input)),
        Box::new(transaction.writer().map_err(io_error)?),
        options,
        cancel,
        progress,
    )?;
    check_cancel(cancel)?;
    transaction.commit().map_err(io_error)?;
    Ok(stats)
}
