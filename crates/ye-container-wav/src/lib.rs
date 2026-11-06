//! RIFF/WAVE PCM import and export. Mono/stereo U8, S16/S24/S32 LE and F32 LE.
//! Reads bounded 1024-sample packets, skips unknown chunks without allocating them,
//! supports standard and WAVE_FORMAT_EXTENSIBLE fmt headers. RF64/RIFX and
//! compressed codecs are explicitly unsupported. RIFF output is limited to 4 GiB.
//! RIFF alignment: <https://learn.microsoft.com/en-us/windows/win32/xaudio2/resource-interchange-file-format--riff->.
//! Extensible format: <https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/ksmedia/ns-ksmedia-waveformatextensible>.
//! A muxer rejects nonempty output before writing; it never truncates existing data.
#![forbid(unsafe_code)]
use std::io::SeekFrom;
use ye_codec::{MediaType, Packet, PacketFlags};
use ye_codec_pcm::{PcmFormat, parameters};
use ye_container::{
    ContainerError, ContainerRegistry, Demuxer, DemuxerFactory, MediaHeader, Metadata, Muxer,
    MuxerFactory, ProbeScore, ReadSeek, Stream, StreamTemplate, WriteSeek,
};
use ye_pixfmt::ChannelLayout;
use ye_time::{Rational, Rounding, Timestamp};

fn bad(s: impl Into<String>) -> ContainerError {
    ContainerError::InvalidData(s.into())
}
fn le16(b: &[u8]) -> u16 {
    u16::from_le_bytes(b[..2].try_into().unwrap())
}
fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}
fn codec_error(e: ye_codec::CodecError) -> ContainerError {
    bad(e.to_string())
}

pub struct WavDemuxer {
    input: Box<dyn ReadSeek>,
    header: MediaHeader,
    data_start: u64,
    frames: u64,
    cursor: u64,
    align: u16,
    time_base: Rational,
}
impl WavDemuxer {
    pub fn new(mut input: Box<dyn ReadSeek>) -> Result<Self, ContainerError> {
        let file_len = input.seek(SeekFrom::End(0))?;
        input.seek(SeekFrom::Start(0))?;
        let mut head = [0; 12];
        input.read_exact(&mut head)?;
        if &head[..4] != b"RIFF" || &head[8..] != b"WAVE" {
            return Err(bad("only little-endian RIFF/WAVE is supported"));
        }
        let riff_end = u64::from(le32(&head[4..])) + 8;
        if riff_end < 12 || riff_end > file_len {
            return Err(bad("truncated or invalid RIFF size"));
        }
        let mut at = 12;
        let mut fmt = None;
        let mut data = None;
        while at < riff_end {
            if riff_end - at < 8 {
                return Err(bad("truncated WAVE chunk header"));
            }
            input.seek(SeekFrom::Start(at))?;
            let mut chunk = [0; 8];
            input.read_exact(&mut chunk)?;
            let size = u64::from(le32(&chunk[4..]));
            let end = at + 8 + size;
            if end + (size & 1) > riff_end {
                return Err(bad("WAVE chunk extends beyond RIFF"));
            }
            match &chunk[..4] {
                b"fmt " => {
                    if fmt.is_some() || !(16..=64).contains(&size) {
                        return Err(bad("duplicate or unsupported WAVE fmt chunk"));
                    }
                    let mut bytes = vec![0; size as usize];
                    input.read_exact(&mut bytes)?;
                    fmt = Some(bytes);
                }
                b"data" => {
                    if data.is_some() {
                        return Err(bad("multiple WAVE data chunks unsupported"));
                    }
                    data = Some((at + 8, size));
                }
                _ => {}
            }
            at = end + (size & 1);
        }
        let f = fmt.ok_or_else(|| bad("missing WAVE fmt"))?;
        let (data_start, data_bytes) = data.ok_or_else(|| bad("missing WAVE data"))?;
        let mut tag = le16(&f);
        let channels = le16(&f[2..]);
        let rate = le32(&f[4..]);
        let byte_rate = le32(&f[8..]);
        let align = le16(&f[12..]);
        let bits = le16(&f[14..]);
        let layout = match channels {
            1 => ChannelLayout::MONO,
            2 => ChannelLayout::STEREO,
            _ => return Err(bad("WAVE requires mono/stereo")),
        };
        if f.len() > 16 && (f.len() < 18 || usize::from(le16(&f[16..])) + 18 > f.len()) {
            return Err(bad("invalid WAVE fmt extension length"));
        }
        if tag == 0xfffe {
            if f.len() < 40 || le16(&f[16..]) < 22 || le16(&f[18..]) != bits {
                return Err(bad("unsupported WAVE extensible valid bits"));
            }
            let mask = le32(&f[20..]);
            if mask != 0 && mask != if channels == 1 { 4 } else { 3 } {
                return Err(bad("unsupported WAVE channel mask"));
            }
            if f[26..40] != [0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113] {
                return Err(bad("unsupported WAVE subtype GUID"));
            }
            tag = le16(&f[24..]);
        }
        let format = PcmFormat::from_tag_bits(tag, bits).map_err(codec_error)?;
        if rate == 0
            || rate > i32::MAX as u32
            || align != channels * (bits / 8)
            || u64::from(byte_rate) != u64::from(rate) * u64::from(align)
            || !data_bytes.is_multiple_of(u64::from(align))
        {
            return Err(bad("invalid WAVE sample rate, alignment or data length"));
        }
        let frames = data_bytes / u64::from(align);
        let time_base = Rational::new(1, rate as i32);
        let duration = Timestamp::new(frames as i64, time_base);
        let header = MediaHeader {
            format_name: "wav".into(),
            streams: vec![Stream {
                index: 0,
                media_type: MediaType::Audio,
                codec: parameters(format, rate, layout).map_err(codec_error)?,
                time_base,
                start_time: Some(Timestamp::new(0, time_base)),
                duration: Some(duration),
                nb_frames: Some(frames),
                metadata: Metadata::new(),
            }],
            duration: Some(duration.rescale(ye_time::CANONICAL_TIME_BASE)?),
            start_time: Some(Timestamp::new(0, ye_time::CANONICAL_TIME_BASE)),
            bit_rate: Some(u64::from(byte_rate) * 8),
            metadata: Metadata::new(),
        };
        input.seek(SeekFrom::Start(data_start))?;
        Ok(Self {
            input,
            header,
            data_start,
            frames,
            cursor: 0,
            align,
            time_base,
        })
    }
}
impl Demuxer for WavDemuxer {
    fn format_name(&self) -> &str {
        "wav"
    }
    fn read_header(&mut self) -> Result<MediaHeader, ContainerError> {
        Ok(self.header.clone())
    }
    fn read_packet(&mut self) -> Result<Option<Packet>, ContainerError> {
        let n = (self.frames - self.cursor).min(1024);
        if n == 0 {
            return Ok(None);
        }
        let mut bytes = vec![0; n as usize * usize::from(self.align)];
        self.input.read_exact(&mut bytes)?;
        let pts = Timestamp::new(self.cursor as i64, self.time_base);
        self.cursor += n;
        Ok(Some(Packet {
            data: bytes.into(),
            stream_index: 0,
            pts: Some(pts),
            dts: Some(pts),
            duration: Some(Timestamp::new(n as i64, self.time_base)),
            flags: PacketFlags::KEYFRAME,
        }))
    }
    fn seek(&mut self, index: u32, target: Timestamp) -> Result<Timestamp, ContainerError> {
        if index != 0 {
            return Err(ContainerError::StreamNotFound(index));
        }
        let frame = target
            .rescale_with(self.time_base, Rounding::Down)?
            .ticks()
            .max(0) as u64;
        self.cursor = frame.min(self.frames);
        self.input.seek(SeekFrom::Start(
            self.data_start + self.cursor * u64::from(self.align),
        ))?;
        Ok(Timestamp::new(self.cursor as i64, self.time_base))
    }
}

pub struct WavDemuxerFactory;
impl DemuxerFactory for WavDemuxerFactory {
    fn name(&self) -> &'static str {
        "wav"
    }
    fn probe(&self, head: &[u8]) -> ProbeScore {
        if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WAVE" {
            ProbeScore::MAXIMUM
        } else {
            ProbeScore::NONE
        }
    }
    fn open(&self, input: Box<dyn ReadSeek>) -> Result<Box<dyn Demuxer>, ContainerError> {
        Ok(Box::new(WavDemuxer::new(input)?))
    }
}

pub struct WavMuxer {
    output: Box<dyn WriteSeek>,
    time_base: Option<Rational>,
    align: u16,
    frames: u64,
    data_bytes: u64,
    data_start: u64,
    fact_at: Option<u64>,
    finished: bool,
}
impl WavMuxer {
    pub fn new(output: Box<dyn WriteSeek>) -> Self {
        Self {
            output,
            time_base: None,
            align: 0,
            frames: 0,
            data_bytes: 0,
            data_start: 0,
            fact_at: None,
            finished: false,
        }
    }
}
impl Muxer for WavMuxer {
    fn write_header(&mut self, streams: &[StreamTemplate]) -> Result<(), ContainerError> {
        if self.time_base.is_some() {
            return Err(ContainerError::InvalidState("WAVE header already written"));
        }
        if self.output.seek(SeekFrom::End(0))? != 0 {
            return Err(bad("WAVE output must be empty; existing data is preserved"));
        }
        let [s] = streams else {
            return Err(bad("WAVE requires exactly one audio stream"));
        };
        let f = PcmFormat::from_parameters(&s.codec).map_err(codec_error)?;
        let a = s
            .codec
            .audio
            .as_ref()
            .ok_or_else(|| bad("missing PCM audio parameters"))?;
        let layout = a
            .channel_layout
            .ok_or_else(|| bad("missing PCM channel layout"))?;
        parameters(f, a.sample_rate, layout).map_err(codec_error)?;
        if s.media_type != MediaType::Audio || s.time_base.num() <= 0 {
            return Err(bad("invalid WAVE stream type or time base"));
        }
        let align = layout.channels() as u16 * u16::from(f.bits() / 8);
        let rate = a
            .sample_rate
            .checked_mul(u32::from(align))
            .ok_or_else(|| bad("WAVE byte rate overflow"))?;
        let mut header = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        header.extend_from_slice(&(if f == PcmFormat::F32 { 18u32 } else { 16u32 }).to_le_bytes());
        header.extend_from_slice(&u16::from(f.tag()).to_le_bytes());
        header.extend_from_slice(&(layout.channels() as u16).to_le_bytes());
        header.extend_from_slice(&a.sample_rate.to_le_bytes());
        header.extend_from_slice(&rate.to_le_bytes());
        header.extend_from_slice(&align.to_le_bytes());
        header.extend_from_slice(&u16::from(f.bits()).to_le_bytes());
        if f == PcmFormat::F32 {
            header.extend_from_slice(&[0, 0]);
            header.extend_from_slice(b"fact\x04\0\0\0");
            self.fact_at = Some(header.len() as u64);
            header.extend_from_slice(&[0; 4]);
        }
        header.extend_from_slice(b"data\0\0\0\0");
        self.output.seek(SeekFrom::Start(0))?;
        self.output.write_all(&header)?;
        self.time_base = Some(Rational::new(1, a.sample_rate as i32));
        self.align = align;
        self.data_start = header.len() as u64;
        Ok(())
    }
    fn write_packet(&mut self, p: &Packet) -> Result<(), ContainerError> {
        let tb = self
            .time_base
            .ok_or(ContainerError::InvalidState("write WAVE header first"))?;
        if self.finished {
            return Err(ContainerError::InvalidState("WAVE already finalized"));
        }
        if p.stream_index != 0 {
            return Err(ContainerError::StreamNotFound(p.stream_index));
        }
        if p.data.is_empty() || !p.data.len().is_multiple_of(usize::from(self.align)) {
            return Err(bad("partial or empty PCM packet"));
        }
        let n = p.data.len() as u64 / u64::from(self.align);
        let expected = Timestamp::new(self.frames as i64, tb);
        let duration = Timestamp::new(n as i64, tb);
        if p.pts
            .is_none_or(|t| Timestamp::compare(t, expected).is_ne())
            || p.dts
                .is_none_or(|t| Timestamp::compare(t, expected).is_ne())
            || p.duration
                .is_none_or(|t| Timestamp::compare(t, duration).is_ne())
        {
            return Err(bad(
                "WAVE packets must have contiguous sample-exact timestamps",
            ));
        }
        let size = self
            .data_bytes
            .checked_add(p.data.len() as u64)
            .ok_or_else(|| bad("WAVE size overflow"))?;
        if self.data_start + size + (size & 1) - 8 > u64::from(u32::MAX) {
            return Err(bad("RIFF WAVE exceeds 4 GiB; RF64 unsupported"));
        }
        self.output.write_all(&p.data)?;
        self.data_bytes = size;
        self.frames += n;
        Ok(())
    }
    fn write_trailer(&mut self) -> Result<(), ContainerError> {
        if self.time_base.is_none() || self.finished {
            return Err(ContainerError::InvalidState(
                "WAVE not open for finalization",
            ));
        }
        if self.data_bytes & 1 != 0 {
            self.output.write_all(&[0])?;
        }
        let end = self.data_start + self.data_bytes + (self.data_bytes & 1);
        self.output.seek(SeekFrom::Start(4))?;
        self.output.write_all(&((end - 8) as u32).to_le_bytes())?;
        self.output.seek(SeekFrom::Start(self.data_start - 4))?;
        self.output
            .write_all(&(self.data_bytes as u32).to_le_bytes())?;
        if let Some(at) = self.fact_at {
            self.output.seek(SeekFrom::Start(at))?;
            self.output.write_all(&(self.frames as u32).to_le_bytes())?;
        }
        self.output.seek(SeekFrom::Start(end))?;
        self.output.flush()?;
        self.finished = true;
        Ok(())
    }
}
pub struct WavMuxerFactory;
impl MuxerFactory for WavMuxerFactory {
    fn name(&self) -> &'static str {
        "wav"
    }
    fn open(&self, output: Box<dyn WriteSeek>) -> Result<Box<dyn Muxer>, ContainerError> {
        Ok(Box::new(WavMuxer::new(output)))
    }
}
pub fn register_wav(registry: &mut ContainerRegistry) {
    registry.register_demuxer(Box::new(WavDemuxerFactory));
    registry.register_muxer(Box::new(WavMuxerFactory));
}
