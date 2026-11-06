//! Interleaved little-endian PCM. Decoder output and encoder input are packed F32.
//! Codec extradata is exactly `[WAVE format tag (1 or 3), bits per sample]`.
//! Floating point PCM preserves finite samples outside [-1,1]; integer encoding
//! clips to its representable range and rounds to nearest, ties away from zero.
#![forbid(unsafe_code)]
use std::collections::VecDeque;
use ye_codec::{
    AudioFrame, AudioParams, CodecError, CodecId, CodecParameters, CodecRegistry, Decoded, Decoder,
    Encoded, Encoder, EncoderConfig, Frame, MediaType, Packet, PacketFlags,
};
use ye_pixfmt::{ChannelLayout, SampleFormat};
use ye_time::{Rational, Timestamp};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PcmFormat {
    U8,
    S16,
    S24,
    S32,
    F32,
}
impl PcmFormat {
    pub fn bits(self) -> u8 {
        match self {
            Self::U8 => 8,
            Self::S16 => 16,
            Self::S24 => 24,
            Self::S32 | Self::F32 => 32,
        }
    }
    pub fn tag(self) -> u8 {
        if self == Self::F32 { 3 } else { 1 }
    }
    pub fn from_tag_bits(tag: u16, bits: u16) -> Result<Self, CodecError> {
        match (tag, bits) {
            (1, 8) => Ok(Self::U8),
            (1, 16) => Ok(Self::S16),
            (1, 24) => Ok(Self::S24),
            (1, 32) => Ok(Self::S32),
            (3, 32) => Ok(Self::F32),
            _ => Err(CodecError::InvalidArguments("unsupported PCM format")),
        }
    }
    pub fn from_parameters(p: &CodecParameters) -> Result<Self, CodecError> {
        if p.codec != CodecId::Pcm || p.media_type != MediaType::Audio {
            return Err(CodecError::UnsupportedCodec(p.codec));
        }
        let Some([tag, bits]) = p.extradata.as_deref() else {
            return Err(CodecError::InvalidArguments(
                "PCM extradata must contain tag and bits",
            ));
        };
        Self::from_tag_bits(u16::from(*tag), u16::from(*bits))
    }
}
fn validate_audio(rate: u32, layout: ChannelLayout) -> Result<(), CodecError> {
    if rate == 0
        || rate > i32::MAX as u32
        || ![ChannelLayout::MONO, ChannelLayout::STEREO].contains(&layout)
    {
        return Err(CodecError::InvalidArguments(
            "PCM requires positive sample rate and mono/stereo",
        ));
    }
    Ok(())
}
pub fn parameters(
    format: PcmFormat,
    rate: u32,
    layout: ChannelLayout,
) -> Result<CodecParameters, CodecError> {
    validate_audio(rate, layout)?;
    let mut p = CodecParameters::new(CodecId::Pcm, MediaType::Audio);
    p.extradata = Some(vec![format.tag(), format.bits()]);
    p.audio = Some(AudioParams {
        sample_rate: rate,
        channel_layout: Some(layout),
        sample_format: Some(SampleFormat::F32),
    });
    p.bit_rate = Some(u64::from(rate) * u64::from(layout.channels()) * u64::from(format.bits()));
    Ok(p)
}
pub struct PcmDecoder {
    format: PcmFormat,
    rate: u32,
    layout: ChannelLayout,
    queue: VecDeque<AudioFrame>,
    finished: bool,
}
impl PcmDecoder {
    pub fn new(p: &CodecParameters) -> Result<Self, CodecError> {
        let format = PcmFormat::from_parameters(p)?;
        let a = p
            .audio
            .as_ref()
            .ok_or(CodecError::InvalidArguments("missing PCM audio parameters"))?;
        let layout = a
            .channel_layout
            .ok_or(CodecError::InvalidArguments("missing PCM channel layout"))?;
        validate_audio(a.sample_rate, layout)?;
        Ok(Self {
            format,
            rate: a.sample_rate,
            layout,
            queue: VecDeque::new(),
            finished: false,
        })
    }
}
impl Decoder for PcmDecoder {
    fn codec_id(&self) -> CodecId {
        CodecId::Pcm
    }
    fn feed(&mut self, p: &Packet) -> Result<(), CodecError> {
        if self.finished {
            return Err(CodecError::InvalidState("PCM decoder already flushed"));
        }
        if !self.queue.is_empty() {
            return Err(CodecError::InvalidState(
                "pull PCM frame before feeding another packet",
            ));
        }
        let bytes = usize::from(self.format.bits() / 8);
        let align = bytes * self.layout.channels() as usize;
        if p.data.is_empty() || !p.data.len().is_multiple_of(align) {
            return Err(CodecError::InvalidData(
                "partial or empty PCM sample frame".into(),
            ));
        }
        let mut frame = AudioFrame::new(
            SampleFormat::F32,
            self.layout,
            self.rate,
            p.data.len() / align,
            p.pts,
        )?;
        for (raw, out) in p
            .data
            .chunks_exact(bytes)
            .zip(frame.data_mut().chunks_exact_mut(4))
        {
            let value = match self.format {
                PcmFormat::U8 => (f32::from(raw[0]) - 128.0) / 128.0,
                PcmFormat::S16 => f32::from(i16::from_le_bytes([raw[0], raw[1]])) / 32768.0,
                PcmFormat::S24 => {
                    ((i32::from_le_bytes([raw[0], raw[1], raw[2], 0]) << 8) >> 8) as f32 / 8388608.0
                }
                PcmFormat::S32 => i32::from_le_bytes(raw.try_into().unwrap()) as f32 / 2147483648.0,
                PcmFormat::F32 => f32::from_le_bytes(raw.try_into().unwrap()),
            };
            if !value.is_finite() {
                return Err(CodecError::InvalidData(
                    "nonfinite floating point PCM".into(),
                ));
            }
            out.copy_from_slice(&value.to_le_bytes());
        }
        self.queue.push_back(frame);
        Ok(())
    }
    fn flush(&mut self) {
        self.finished = true;
    }
    fn pull(&mut self) -> Result<Decoded, CodecError> {
        Ok(match self.queue.pop_front() {
            Some(f) => Decoded::Frame(Frame::Audio(f)),
            None if self.finished => Decoded::Finished,
            None => Decoded::NeedInput,
        })
    }
}

/// PCM encoder factory defaults to S16; use `new` for other output formats.
pub struct PcmEncoder {
    format: PcmFormat,
    rate: u32,
    layout: ChannelLayout,
    time_base: Rational,
    frames: i64,
    queue: VecDeque<Packet>,
    finished: bool,
}
impl PcmEncoder {
    pub fn new(config: &EncoderConfig, format: PcmFormat) -> Result<Self, CodecError> {
        if config.codec != CodecId::Pcm {
            return Err(CodecError::UnsupportedCodec(config.codec));
        }
        let a = config
            .audio
            .as_ref()
            .ok_or(CodecError::InvalidArguments("missing PCM encoder audio"))?;
        validate_audio(a.sample_rate, a.channel_layout)?;
        if a.format != SampleFormat::F32 || config.time_base.num() <= 0 {
            return Err(CodecError::InvalidArguments(
                "PCM encoder requires F32 and positive time base",
            ));
        }
        Ok(Self {
            format,
            rate: a.sample_rate,
            layout: a.channel_layout,
            time_base: config.time_base,
            frames: 0,
            queue: VecDeque::new(),
            finished: false,
        })
    }
    pub fn parameters(&self) -> CodecParameters {
        parameters(self.format, self.rate, self.layout).expect("validated PCM configuration")
    }
}
impl Encoder for PcmEncoder {
    fn codec_id(&self) -> CodecId {
        CodecId::Pcm
    }
    fn feed(&mut self, f: &Frame) -> Result<(), CodecError> {
        if self.finished {
            return Err(CodecError::InvalidState("PCM encoder already flushed"));
        }
        if !self.queue.is_empty() {
            return Err(CodecError::InvalidState(
                "pull PCM packet before feeding another frame",
            ));
        }
        let Frame::Audio(a) = f else {
            return Err(CodecError::InvalidArguments("PCM encoder requires audio"));
        };
        if a.format() != SampleFormat::F32
            || a.sample_rate() != self.rate
            || *a.channel_layout() != self.layout
            || a.frames() == 0
        {
            return Err(CodecError::InvalidArguments(
                "PCM frame format differs from encoder",
            ));
        }
        let mut data = Vec::with_capacity(
            a.frames() * self.layout.channels() as usize * usize::from(self.format.bits() / 8),
        );
        for raw in a.data().chunks_exact(4) {
            let value = f32::from_le_bytes(raw.try_into().unwrap());
            if !value.is_finite() {
                return Err(CodecError::InvalidData("nonfinite PCM input".into()));
            }
            let value = f64::from(value);
            match self.format {
                PcmFormat::F32 => data.extend_from_slice(raw),
                PcmFormat::U8 => data.push((value * 128.0 + 128.0).round().clamp(0.0, 255.0) as u8),
                PcmFormat::S16 => data.extend_from_slice(
                    &((value * 32768.0).round().clamp(-32768.0, 32767.0) as i16).to_le_bytes(),
                ),
                PcmFormat::S24 => data.extend_from_slice(
                    &((value * 8388608.0).round().clamp(-8388608.0, 8388607.0) as i32)
                        .to_le_bytes()[..3],
                ),
                PcmFormat::S32 => data.extend_from_slice(
                    &((value * 2147483648.0)
                        .round()
                        .clamp(-2147483648.0, 2147483647.0) as i32)
                        .to_le_bytes(),
                ),
            }
        }
        let n = i64::try_from(a.frames())
            .map_err(|_| CodecError::InvalidArguments("PCM frame too long"))?;
        let pts = a
            .pts()
            .unwrap_or(Timestamp::new(
                self.frames,
                Rational::new(1, self.rate as i32),
            ))
            .rescale(self.time_base)?;
        let duration =
            Timestamp::new(n, Rational::new(1, self.rate as i32)).rescale(self.time_base)?;
        self.frames = self
            .frames
            .checked_add(n)
            .ok_or(CodecError::InvalidArguments("PCM timestamp overflow"))?;
        self.queue.push_back(Packet {
            data: data.into(),
            stream_index: 0,
            pts: Some(pts),
            dts: Some(pts),
            duration: Some(duration),
            flags: PacketFlags::KEYFRAME,
        });
        Ok(())
    }
    fn flush(&mut self) {
        self.finished = true;
    }
    fn pull(&mut self) -> Result<Encoded, CodecError> {
        Ok(match self.queue.pop_front() {
            Some(p) => Encoded::Packet(p),
            None if self.finished => Encoded::Finished,
            None => Encoded::NeedInput,
        })
    }
}
pub fn decoder_factory(p: &CodecParameters) -> Result<Box<dyn Decoder>, CodecError> {
    Ok(Box::new(PcmDecoder::new(p)?))
}
pub fn encoder_factory(c: &EncoderConfig) -> Result<Box<dyn Encoder>, CodecError> {
    Ok(Box::new(PcmEncoder::new(c, PcmFormat::S16)?))
}
pub fn register_pcm(registry: &mut CodecRegistry) {
    registry.register_decoder(CodecId::Pcm, decoder_factory);
    registry.register_encoder(CodecId::Pcm, encoder_factory);
}
