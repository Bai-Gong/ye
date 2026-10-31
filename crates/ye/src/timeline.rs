//! Static, streaming timeline renderer. Windows are half-open and share time zero.
//! Each occurrence owns an input cursor; repeated material can play concurrently.
use crate::compose::ComposeSettings;
use crate::convert::{ConvertError, ConvertProgress, ConvertStats, FrameSink};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use ye_codec::{AudioFrame, CodecRegistry, Decoded, Decoder, Frame, MediaType, VideoFrame};
use ye_container::{ContainerRegistry, Demuxer, ReadSeek, Stream, WriteSeek};
use ye_pixfmt::{
    ChannelLayout, ColorDesc, ColorPrimaries, ColorRange, ColorSpace, ColorTransfer, PixelFormat,
    SampleFormat,
};
use ye_time::{Rational, Timestamp};

const MICRO: Rational = Rational::new(1, 1_000_000);
const RATE: u32 = 48000;

#[derive(Debug, Clone, Copy)]
pub struct Transform {
    /// Offset of the source center from the canvas center, in canvas pixels.
    pub x: f64,
    pub y: f64,
    pub scale: f64,
    pub rotation: f64,
    pub opacity: f64,
    pub z: i32,
}
impl Default for Transform {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            scale: 1.0,
            rotation: 0.0,
            opacity: 1.0,
            z: 0,
        }
    }
}
#[derive(Debug, Clone, Copy)]
pub struct AudioMix {
    pub gain: f32,
    pub pan: f32,
}
impl Default for AudioMix {
    fn default() -> Self {
        Self {
            gain: 1.0,
            pan: 0.0,
        }
    }
}
#[derive(Debug, Clone, Copy)]
pub enum Layer {
    Video(Transform),
    Audio(AudioMix),
}

/// Video and audio occurrences are separate; mute/solo are resolved by the editor.
pub struct TimelineClip {
    pub input: Box<dyn ReadSeek>,
    pub source_in_us: i64,
    pub source_out_us: i64,
    pub timeline_start_us: i64,
    pub timeline_end_us: i64,
    pub layer: Layer,
}

fn invalid(message: impl Into<String>) -> ConvertError {
    ConvertError::InvalidArguments(message.into())
}

/// Check DTO shape without starting codecs. Every independent blocker is returned.
pub fn check_timeline(
    clips: &[TimelineClip],
    settings: &ComposeSettings,
    duration_us: i64,
) -> Vec<String> {
    let mut issues = Vec::new();
    if duration_us <= 0 {
        issues.push("时间线时长须为正".into());
    }
    if settings.width == 0
        || settings.height == 0
        || !settings.width.is_multiple_of(2)
        || !settings.height.is_multiple_of(2)
    {
        issues.push("输出画幅须为正偶数".into());
    }
    if settings.frame_rate.num() <= 0 {
        issues.push("输出帧率须为正".into());
    }
    if matches!(settings.audio, crate::convert::AudioEncodeSettings::Copy) {
        issues.push("时间线音频要求重编码，不能直通".into());
    }
    for (i, c) in clips.iter().enumerate() {
        let window = c.source_out_us.checked_sub(c.source_in_us);
        if c.source_in_us < 0
            || c.timeline_start_us < 0
            || window.is_none_or(|n| n <= 0)
            || window != c.timeline_end_us.checked_sub(c.timeline_start_us)
            || c.timeline_end_us > duration_us
        {
            issues.push(format!(
                "第 {} 层源窗口与时间线窗口须有效、等长且位于输出范围内",
                i + 1
            ));
        }
        match c.layer {
            Layer::Video(t) => {
                if [t.x, t.y, t.scale, t.rotation, t.opacity]
                    .iter()
                    .any(|v| !v.is_finite())
                    || t.scale <= 0.0
                    || !(0.0..=1.0).contains(&t.opacity)
                {
                    issues.push(format!("第 {} 层变换非法", i + 1));
                }
            }
            Layer::Audio(m) => {
                if !m.gain.is_finite()
                    || !(0.0..=2.0).contains(&m.gain)
                    || !m.pan.is_finite()
                    || !(-1.0..=1.0).contains(&m.pan)
                {
                    issues.push(format!("第 {} 层音频增益/声像非法", i + 1));
                }
            }
        }
    }
    issues
}

/// One decoder, one selected stream. Decode preroll after a keyframe seek.
struct Source {
    demux: Box<dyn Demuxer>,
    decoder: Box<dyn Decoder>,
    stream: Stream,
    eof: bool,
    saw_frame: bool,
}
impl Source {
    fn open(
        containers: &ContainerRegistry,
        codecs: &CodecRegistry,
        input: Box<dyn ReadSeek>,
        kind: MediaType,
        trim_us: i64,
        trim_out_us: i64,
    ) -> Result<Self, ConvertError> {
        let (_, mut demux) = containers.open_demuxer(input)?;
        let header = demux.read_header()?;
        let selected: Vec<_> = header
            .streams
            .iter()
            .filter(|s| s.media_type == kind)
            .collect();
        if selected.len() != 1 {
            return Err(invalid(format!(
                "任务要求恰一条 {kind:?} 流，实际 {} 条",
                selected.len()
            )));
        }
        let stream = selected[0].clone();
        if let Some(duration) = stream.duration {
            let end = duration.checked_add(
                stream
                    .start_time
                    .unwrap_or(Timestamp::new(0, stream.time_base)),
            )?;
            if Timestamp::compare(Timestamp::new(trim_out_us, MICRO), end).is_gt() {
                return Err(invalid("源窗口超出素材时长"));
            }
        }
        let decoder = codecs.create_decoder(&stream.codec)?;
        // Extra audio preroll restores AAC overlap state before the requested window.
        let preroll = if kind == MediaType::Audio { 100_000 } else { 0 };
        demux.seek(
            stream.index,
            Timestamp::new(trim_us.saturating_sub(preroll), MICRO),
        )?;
        Ok(Self {
            demux,
            decoder,
            stream,
            eof: false,
            saw_frame: false,
        })
    }
    fn next(&mut self, cancel: &AtomicBool) -> Result<Option<Frame>, ConvertError> {
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(ConvertError::Cancelled);
            }
            match self.decoder.pull()? {
                Decoded::Frame(f) => {
                    self.saw_frame = true;
                    return Ok(Some(f));
                }
                Decoded::Finished => return Ok(None),
                Decoded::NeedInput if self.eof => return Ok(None),
                Decoded::NeedInput => {}
            }
            loop {
                if cancel.load(Ordering::Relaxed) {
                    return Err(ConvertError::Cancelled);
                }
                match self.demux.read_packet()? {
                    Some(p) if p.stream_index == self.stream.index => {
                        self.decoder.feed(&p)?;
                        break;
                    }
                    Some(_) => {}
                    None => {
                        self.decoder.flush();
                        self.eof = true;
                        break;
                    }
                }
            }
        }
    }
}

struct VideoCursor {
    source: Source,
    held: Option<VideoFrame>,
    next: Option<VideoFrame>,
    ended: bool,
}
impl VideoCursor {
    fn at(
        &mut self,
        at: Timestamp,
        cancel: &AtomicBool,
    ) -> Result<Option<&VideoFrame>, ConvertError> {
        loop {
            if self.next.is_none() && !self.ended {
                match self.source.next(cancel)? {
                    Some(Frame::Video(v)) => {
                        v.pts().ok_or_else(|| {
                            ConvertError::InvalidData("video frame has no PTS".into())
                        })?;
                        self.next = Some(v);
                    }
                    Some(_) => {
                        return Err(ConvertError::InvalidData(
                            "video decoder returned audio".into(),
                        ));
                    }
                    None => {
                        self.ended = true;
                        if !self.source.saw_frame {
                            return Err(ConvertError::InvalidData(
                                "窗口内没有可解码的视频帧".into(),
                            ));
                        }
                    }
                }
            }
            if self
                .next
                .as_ref()
                .is_some_and(|v| v.pts().is_some_and(|p| Timestamp::compare(p, at).is_le()))
            {
                self.held = self.next.take();
            } else {
                break;
            }
        }
        Ok(self.held.as_ref())
    }
}

struct PcmBlock {
    start: i64,
    pcm: Vec<[f32; 2]>,
}
struct AudioCursor {
    source: Source,
    blocks: VecDeque<PcmBlock>,
    ended: bool,
    rate: u32,
    channels: usize,
}
impl AudioCursor {
    fn ensure(&mut self, center: i64, cancel: &AtomicBool) -> Result<(), ConvertError> {
        while !self.ended
            && self
                .blocks
                .back()
                .is_none_or(|b| b.start + b.pcm.len() as i64 <= center + 17)
        {
            match self.source.next(cancel)? {
                Some(Frame::Audio(a)) => {
                    if a.format() != SampleFormat::F32
                        || a.sample_rate() != self.rate
                        || a.channel_layout().channels() as usize != self.channels
                    {
                        return Err(ConvertError::InvalidData(
                            "audio format changed within stream".into(),
                        ));
                    }
                    let start = a
                        .pts()
                        .ok_or_else(|| ConvertError::InvalidData("audio frame has no PTS".into()))?
                        .rescale(Rational::new(1, self.rate as i32))?
                        .ticks();
                    let mut pcm = Vec::new();
                    if !a.data().len().is_multiple_of(4 * self.channels) {
                        return Err(ConvertError::InvalidData("invalid packed PCM".into()));
                    }
                    for s in a.data().chunks_exact(4 * self.channels) {
                        let l = f32::from_le_bytes(s[..4].try_into().map_err(|_| invalid("PCM"))?);
                        let r = f32::from_le_bytes(
                            s[s.len() - 4..].try_into().map_err(|_| invalid("PCM"))?,
                        );
                        if !l.is_finite() || !r.is_finite() {
                            return Err(ConvertError::InvalidData("nonfinite PCM".into()));
                        }
                        pcm.push([l, r]);
                    }
                    self.blocks.push_back(PcmBlock { start, pcm });
                }
                Some(_) => {
                    return Err(ConvertError::InvalidData(
                        "audio decoder returned video".into(),
                    ));
                }
                None => {
                    self.ended = true;
                    if !self.source.saw_frame {
                        return Err(ConvertError::InvalidData("没有可解码的音频帧".into()));
                    }
                }
            }
            // Discard while seeking forward as well as during ordinary playback.
            while self
                .blocks
                .front()
                .is_some_and(|b| b.start + (b.pcm.len() as i64) < center - 17)
            {
                self.blocks.pop_front();
            }
        }
        Ok(())
    }
    fn sample(
        &mut self,
        phase: i128,
        denominator: i128,
        lo: i64,
        hi: i64,
        cancel: &AtomicBool,
    ) -> Result<[f32; 2], ConvertError> {
        let center = i64::try_from(phase.div_euclid(denominator))
            .map_err(|_| invalid("audio clock overflow"))?;
        self.ensure(center, cancel)?;
        let frac = phase.rem_euclid(denominator) as f64 / denominator as f64;
        let cutoff = (f64::from(RATE) / f64::from(self.rate)).min(1.0);
        let mut value = [0.0f64; 2];
        let mut norm = 0.0;
        for k in -15..=16 {
            let distance = f64::from(k) - frac;
            let x = distance * cutoff;
            let sinc = if x.abs() < 1e-12 {
                1.0
            } else {
                (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
            };
            let weight =
                sinc * (0.5 + 0.5 * (std::f64::consts::PI * distance / 16.0).cos()) * cutoff;
            norm += weight;
            let n = center + i64::from(k);
            if i128::from(n) * 1_000_000 < i128::from(lo) * i128::from(self.rate)
                || i128::from(n) * 1_000_000 >= i128::from(hi) * i128::from(self.rate)
            {
                continue;
            }
            for b in &self.blocks {
                if n >= b.start
                    && let Some(s) = b.pcm.get((n - b.start) as usize)
                {
                    for c in 0..2 {
                        value[c] += f64::from(s[c]) * weight;
                    }
                    break;
                }
            }
        }
        while self
            .blocks
            .front()
            .is_some_and(|b| b.start + (b.pcm.len() as i64) < center - 17)
        {
            self.blocks.pop_front();
        }
        Ok(value.map(|v| (v / norm) as f32))
    }
}

struct ActiveClip {
    source_in: i64,
    source_out: i64,
    start: i64,
    end: i64,
    cursor: Cursor,
}
enum Cursor {
    Video(Box<VideoCursor>, Transform),
    Audio(Box<AudioCursor>, AudioMix),
}

fn color() -> ColorDesc {
    ColorDesc {
        primaries: ColorPrimaries::Bt709,
        transfer: ColorTransfer::Bt709,
        space: ColorSpace::Bt709,
        range: ColorRange::Mpeg,
        ..ColorDesc::UNSPECIFIED
    }
}

/// Inverse-mapped bilinear source-over, matching Canvas center/clockwise semantics.
pub fn blend(
    canvas: &mut VideoFrame,
    source: &VideoFrame,
    t: Transform,
) -> Result<(), ConvertError> {
    if [t.x, t.y, t.scale, t.rotation, t.opacity]
        .iter()
        .any(|v| !v.is_finite())
        || t.scale <= 0.0
        || !(0.0..=1.0).contains(&t.opacity)
    {
        return Err(invalid("invalid transform"));
    }
    if canvas.format() != PixelFormat::Rgba8 || source.format() != PixelFormat::Rgba8 {
        return Err(invalid("blend requires RGBA"));
    }
    let (w, h) = (canvas.width() as usize, canvas.height() as usize);
    let (sw, sh) = (source.width() as usize, source.height() as usize);
    let cs = canvas.layout().plane(0).stride;
    let ss = source.layout().plane(0).stride;
    let (sin, cos) = t.rotation.to_radians().sin_cos();
    for y in 0..h {
        for x in 0..w {
            let dx = x as f64 + 0.5 - w as f64 / 2.0 - t.x;
            let dy = y as f64 + 0.5 - h as f64 / 2.0 - t.y;
            let sx = (cos * dx + sin * dy) / t.scale + sw as f64 / 2.0 - 0.5;
            let sy = (-sin * dx + cos * dy) / t.scale + sh as f64 / 2.0 - 0.5;
            if sx < -0.5 || sy < -0.5 || sx >= sw as f64 - 0.5 || sy >= sh as f64 - 0.5 {
                continue;
            }
            let mut rgba = [0.0; 4];
            let (sx, sy) = (
                sx.clamp(0.0, (sw - 1) as f64),
                sy.clamp(0.0, (sh - 1) as f64),
            );
            let (ix, iy) = (sx.floor() as usize, sy.floor() as usize);
            let (fx, fy) = (sx - ix as f64, sy - iy as f64);
            for (xx, yy, weight) in [
                (ix, iy, (1.0 - fx) * (1.0 - fy)),
                ((ix + 1).min(sw - 1), iy, fx * (1.0 - fy)),
                (ix, (iy + 1).min(sh - 1), (1.0 - fx) * fy),
                ((ix + 1).min(sw - 1), (iy + 1).min(sh - 1), fx * fy),
            ] {
                for (c, value) in rgba.iter_mut().enumerate() {
                    *value += f64::from(source.plane(0)[yy * ss + xx * 4 + c]) * weight;
                }
            }
            let alpha = t.opacity * rgba[3] / 255.0;
            let out = &mut canvas.plane_mut(0)[y * cs + x * 4..y * cs + x * 4 + 4];
            for c in 0..3 {
                out[c] = (rgba[c] * alpha + f64::from(out[c]) * (1.0 - alpha))
                    .round()
                    .clamp(0.0, 255.0) as u8;
            }
            out[3] = 255;
        }
    }
    Ok(())
}

/// Render unencoded RGBA and packed 48kHz stereo PCM for acceptance and encoding.
#[allow(clippy::too_many_arguments)]
pub fn render_frames(
    containers: &ContainerRegistry,
    codecs: &CodecRegistry,
    clips: Vec<TimelineClip>,
    settings: &ComposeSettings,
    duration_us: i64,
    sink: FrameSink<'_>,
    progress: ConvertProgress<'_>,
    cancel: &AtomicBool,
) -> Result<ConvertStats, ConvertError> {
    let issues = check_timeline(&clips, settings, duration_us);
    if !issues.is_empty() {
        return Err(invalid(issues.join("；")));
    }
    let mut active = Vec::new();
    for c in clips {
        if cancel.load(Ordering::Relaxed) {
            return Err(ConvertError::Cancelled);
        }
        let cursor = match c.layer {
            Layer::Video(t) => Cursor::Video(
                Box::new(VideoCursor {
                    source: Source::open(
                        containers,
                        codecs,
                        c.input,
                        MediaType::Video,
                        c.source_in_us,
                        c.source_out_us,
                    )?,
                    held: None,
                    next: None,
                    ended: false,
                }),
                t,
            ),
            Layer::Audio(m) => {
                let source = Source::open(
                    containers,
                    codecs,
                    c.input,
                    MediaType::Audio,
                    c.source_in_us,
                    c.source_out_us,
                )?;
                let audio = source
                    .stream
                    .codec
                    .audio
                    .as_ref()
                    .ok_or_else(|| invalid("缺音频参数"))?;
                let rate = audio.sample_rate;
                let channels = audio.channel_layout.map_or(0, |l| l.channels() as usize);
                if ![44100, 48000].contains(&rate) || !(1..=2).contains(&channels) {
                    return Err(invalid("音频支持 44.1/48kHz 单/双声道"));
                }
                Cursor::Audio(
                    Box::new(AudioCursor {
                        source,
                        blocks: VecDeque::new(),
                        ended: false,
                        rate,
                        channels,
                    }),
                    m,
                )
            }
        };
        active.push(ActiveClip {
            source_in: c.source_in_us,
            source_out: c.source_out_us,
            start: c.timeline_start_us,
            end: c.timeline_end_us,
            cursor,
        });
    }
    active.sort_by_key(|c| match c.cursor {
        Cursor::Video(_, t) => t.z,
        Cursor::Audio(..) => 0,
    });
    let fps = settings.frame_rate;
    let numerator = duration_us as u128 * fps.num() as u128;
    let nvideo = u64::try_from(numerator.div_ceil(fps.den() as u128 * 1_000_000))
        .map_err(|_| invalid("video clock overflow"))?;
    let has_audio = !matches!(settings.audio, crate::convert::AudioEncodeSettings::Drop);
    let naudio = if has_audio {
        u64::try_from((duration_us as u128 * u128::from(RATE)).div_ceil(1_000_000))
            .map_err(|_| invalid("audio clock overflow"))?
    } else {
        0
    };
    if nvideo > i64::MAX as u64 || naudio > i64::MAX as u64 {
        return Err(invalid("timeline clock overflow"));
    }
    let (mut v, mut a) = (0u64, 0u64);
    let mut audio_blocks = 0;
    while v < nvideo || a < naudio {
        if cancel.load(Ordering::Relaxed) {
            return Err(ConvertError::Cancelled);
        }
        let video_tick = v < nvideo
            && (a >= naudio
                || u128::from(v) * fps.den() as u128 * u128::from(RATE)
                    <= u128::from(a) * fps.num() as u128);
        let time = if video_tick {
            Timestamp::from_frame_index(v as i64, fps)?
        } else {
            Timestamp::new(a as i64, Rational::new(1, RATE as i32))
        };
        if video_tick {
            let mut frame = VideoFrame::new(
                PixelFormat::Rgba8,
                settings.width,
                settings.height,
                color(),
                Some(time),
                Some(Timestamp::from_frame_index(1, fps)?),
            )?;
            for px in frame.plane_mut(0).chunks_exact_mut(4) {
                px[3] = 255;
            }
            for c in &mut active {
                if Timestamp::compare(time, Timestamp::new(c.start, MICRO)).is_lt()
                    || !Timestamp::compare(time, Timestamp::new(c.end, MICRO)).is_lt()
                {
                    continue;
                }
                if let Cursor::Video(cursor, t) = &mut c.cursor {
                    let source_time =
                        time.checked_add(Timestamp::new(c.source_in - c.start, MICRO))?;
                    if let Some(source) = cursor.at(source_time, cancel)? {
                        if matches!(
                            source.color().transfer,
                            ColorTransfer::Smpte2084 | ColorTransfer::AribStdB67
                        ) {
                            return Err(invalid("暂不支持 HDR 色彩转换"));
                        }
                        let rgba = crate::convert::filter_convert(source, PixelFormat::Rgba8)?;
                        blend(&mut frame, &rgba, *t)?;
                    }
                }
            }
            sink(Frame::Video(frame))?;
            v += 1;
        } else {
            let count = (naudio - a).min(1024) as usize;
            let mut mixed = vec![[0.0f32; 2]; count];
            for c in &mut active {
                if let Cursor::Audio(cursor, mix) = &mut c.cursor {
                    for (i, out) in mixed.iter_mut().enumerate() {
                        let index = a + i as u64;
                        if i128::from(index) * 1_000_000 < i128::from(c.start) * i128::from(RATE)
                            || i128::from(index) * 1_000_000 >= i128::from(c.end) * i128::from(RATE)
                        {
                            continue;
                        }
                        let phase = (i128::from(c.source_in - c.start) * i128::from(RATE)
                            + i128::from(index) * 1_000_000)
                            * i128::from(cursor.rate);
                        let pcm = cursor.sample(
                            phase,
                            i128::from(RATE) * 1_000_000,
                            c.source_in,
                            c.source_out,
                            cancel,
                        )?;
                        // Balance pan: center preserves both channels, extremes silence the opposite side.
                        out[0] += pcm[0] * mix.gain * (1.0 - mix.pan.max(0.0));
                        out[1] += pcm[1] * mix.gain * (1.0 + mix.pan.min(0.0));
                    }
                }
            }
            let mut frame = AudioFrame::new(
                SampleFormat::F32,
                ChannelLayout::STEREO,
                RATE,
                count,
                Some(time),
            )?;
            for (dst, s) in frame
                .data_mut()
                .chunks_exact_mut(4)
                .zip(mixed.into_iter().flatten())
            {
                dst.copy_from_slice(&s.clamp(-1.0, 1.0).to_le_bytes());
            }
            sink(Frame::Audio(frame))?;
            a += count as u64;
            audio_blocks += 1;
        }
        progress(time.rescale(MICRO)?.ticks().min(duration_us), duration_us);
    }
    progress(duration_us, duration_us);
    Ok(ConvertStats {
        streams: if has_audio { 2 } else { 1 },
        video_frames: v,
        audio_frames: audio_blocks,
        processed_us: duration_us,
        total_us: duration_us,
    })
}

/// Encoded timeline output, sharing the established encoder/mux/progress contract.
#[allow(clippy::too_many_arguments)]
pub fn render(
    containers: &ContainerRegistry,
    codecs: &CodecRegistry,
    clips: Vec<TimelineClip>,
    output_format: &str,
    output: Box<dyn WriteSeek>,
    settings: &ComposeSettings,
    duration_us: i64,
    progress: ConvertProgress<'_>,
    cancel: &AtomicBool,
) -> Result<ConvertStats, ConvertError> {
    let issues = check_timeline(&clips, settings, duration_us);
    if !issues.is_empty() {
        return Err(invalid(issues.join("；")));
    }
    let mut encoder = crate::compose::RenderedOutput::new(
        containers,
        codecs,
        output_format,
        output,
        settings,
        duration_us,
    )?;
    render_frames(
        containers,
        codecs,
        clips,
        settings,
        duration_us,
        &mut |frame| encoder.write(frame),
        progress,
        cancel,
    )?;
    encoder.finish()
}
