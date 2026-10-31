//! Unified frame grab: display-time keyframe seek, video-only decode, RGBA/PNG.
//! Select the first frame whose exact PTS reaches time_us; negative targets clamp
//! to zero, targets beyond EOF fail. Unrelated audio is not required to decode.
//! The same selected-stream cursor is used by timeline rendering.

use std::sync::atomic::{AtomicBool, Ordering};

use ye_codec::{CodecId, CodecRegistry, Frame, MediaType, VideoFrame};
#[cfg(test)]
use ye_container::MediaHeader;
use ye_container::{ContainerRegistry, ReadSeek};
use ye_pixfmt::PixelFormat;
use ye_time::Rational;

use crate::convert::{ConvertError, filter_convert};

/// 微秒时基（`time_us` 参数与帧 pts 比较的公共域）。
const MICRO: Rational = Rational::new(1, 1_000_000);

/// 抽帧结果（rgba8 紧致字节）。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`（纯值载荷：宽高与紧致
/// 字节，无浮点无句柄）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrabbedFrame {
    /// 帧宽（像素）。
    pub width: u32,
    /// 帧高（像素）。
    pub height: u32,
    /// rgba8 紧致字节（`width × height × 4`，行主序，无跨距 padding）。
    pub rgba: Vec<u8>,
}

/// 抽帧只注册视频解码器，不要求无关音轨具备解码能力。
fn decode_registries() -> (ContainerRegistry, CodecRegistry) {
    let mut containers = ContainerRegistry::new();
    crate::container::mp4::register_mp4(&mut containers);
    let mut codecs = CodecRegistry::new();
    codecs.register_decoder(CodecId::H264, crate::codec_h264::decoder_factory);
    (containers, codecs)
}

/// 校验单视频流布局；音轨不参与抽帧。
#[cfg(test)]
fn validate_single_video_stream(header: &MediaHeader) -> Result<(), ConvertError> {
    let video = header
        .streams
        .iter()
        .filter(|stream| stream.media_type == MediaType::Video)
        .count();
    match video {
        0 => Err(ConvertError::InvalidArguments(format!(
            "抽帧要求恰一条视频流，{} 容器无视频流",
            header.format_name
        ))),
        1 => Ok(()),
        n => Err(ConvertError::InvalidArguments(format!(
            "抽帧要求恰一条视频流，检测到 {n} 条（Frame 不带流标识，多路会静默错帧——拒绝）"
        ))),
    }
}

/// 抽帧泵内游标：时点判定 + 已取帧 + pts 计数（EOF 判 Err 的依据）。
struct GrabState {
    /// 目标时点（µs，入口已钳 0）。
    time_us: i64,
    /// 已取帧（`Some` 后忽略一切后续帧，见 [`GrabState::feed`]）。
    grabbed: Option<GrabbedFrame>,
    /// 送达 sink 的视频帧总数。
    frames_seen: u64,
    /// 是否见过带 pts 的视频帧。
    saw_pts: bool,
    /// 见过的最晚视频帧 pts（µs；越界 Err 文案用）。
    latest_pts_us: Option<i64>,
}

impl GrabState {
    /// 喂一帧（泵的 sink 回调体）。
    ///
    /// 音频帧丢弃；视频帧做时点判定——首个 pts ≥ `time_us` 即取帧并
    /// 置 cancel，调用方随即返回。
    fn feed(&mut self, frame: Frame, cancel: &AtomicBool) -> Result<(), ConvertError> {
        let Frame::Video(video) = frame else {
            // 防御性忽略非视频帧。
            return Ok(());
        };
        self.frames_seen += 1;
        if self.grabbed.is_some() {
            // 已取得的帧不被后续调用覆盖。
            return Ok(());
        }
        if video.format() != PixelFormat::Rgba8 {
            return Err(ConvertError::InvalidData(format!(
                "抽帧链要求 rgba8 帧，收到 {}（链配置与解码器输出不符）",
                video.format().name()
            )));
        }
        let Some(pts) = video.pts() else {
            // 无 pts：跳过时点判定（计数已记 frames_seen）——不猜帧序。
            return Ok(());
        };
        let pts_us = pts.rescale(MICRO)?.ticks();
        self.saw_pts = true;
        self.latest_pts_us = Some(self.latest_pts_us.map_or(pts_us, |old| old.max(pts_us)));
        if ye_time::Timestamp::compare(pts, ye_time::Timestamp::new(self.time_us, MICRO)).is_ge() {
            self.grabbed = Some(tight_rgba(&video));
            // 通知调用方停止读取后续包。
            cancel.store(true, Ordering::Relaxed);
        }
        Ok(())
    }
}

/// 帧 → 紧致 rgba（去行尾对齐 padding；与 convert 模块 `packed_tight`
/// 同式）。
fn tight_rgba(frame: &VideoFrame) -> GrabbedFrame {
    let stride = frame.layout().plane(0).stride;
    let row = frame.width() as usize * 4;
    let mut rgba = Vec::with_capacity(row * frame.height() as usize);
    for y in 0..frame.height() as usize {
        rgba.extend_from_slice(&frame.plane(0)[y * stride..y * stride + row]);
    }
    GrabbedFrame {
        width: frame.width(),
        height: frame.height(),
        rgba,
    }
}

/// EOF 未取到的明确 Err（模块文档「时点语义」——勿把自然 EOF 误译
/// 成功；三形各述其因）。
fn eof_error(state: &GrabState) -> ConvertError {
    if state.frames_seen == 0 {
        ConvertError::InvalidData(format!(
            "时点 {} µs 未取到帧：素材无视频帧送达",
            state.time_us
        ))
    } else if !state.saw_pts {
        ConvertError::InvalidData(format!(
            "时点 {} µs 未取到帧：全部 {} 个视频帧均无 pts，无法定位时点（不猜帧序）",
            state.time_us, state.frames_seen
        ))
    } else {
        ConvertError::InvalidData(format!(
            "时点 {} µs 超出素材：最晚视频帧 pts {} µs",
            state.time_us,
            state.latest_pts_us.unwrap_or_default()
        ))
    }
}

/// 抽帧门面·RGBA 出口：解码取**首个 pts ≥ `time_us`** 的视频帧
/// （链 yuv420p→rgba8，宽高保持原尺寸）。
///
/// 从显示时点前的关键帧解码，保留重排后的精确 PTS；忽略无关音轨。
///
/// # Errors
///
/// - 流布局不符（无视频流 / 多视频流）→
///   [`ConvertError::InvalidArguments`]；
/// - EOF 未取到（无视频帧 / 全帧无 pts / 时点超出最晚 pts）→
///   [`ConvertError::InvalidData`]；
/// - 所选视频流的解封装或解码失败原样透传。
pub fn decode_frame_rgba(
    input: Box<dyn ReadSeek>,
    time_us: i64,
) -> Result<GrabbedFrame, ConvertError> {
    let time_us = time_us.max(0);
    let (containers, codecs) = decode_registries();
    let mut source = crate::timeline::Source::open(
        &containers,
        &codecs,
        input,
        MediaType::Video,
        time_us,
        None,
    )?;
    let cancel = AtomicBool::new(false);
    let mut state = GrabState {
        time_us,
        grabbed: None,
        frames_seen: 0,
        saw_pts: false,
        latest_pts_us: None,
    };
    while let Some(frame) = source.next(&cancel)? {
        let Frame::Video(video) = frame else {
            return Err(ConvertError::InvalidData(
                "video decoder returned audio".into(),
            ));
        };
        state.feed(
            Frame::Video(filter_convert(&video, PixelFormat::Rgba8)?),
            &cancel,
        )?;
        if let Some(frame) = state.grabbed.take() {
            return Ok(frame);
        }
    }
    Err(eof_error(&state))
}

/// 抽帧门面·PNG 出口：[`decode_frame_rgba`] → [`crate::png::encode_rgba`]
/// 一步组合（剪缩略图管线的最后一环）。
///
/// # Errors
///
/// [`decode_frame_rgba`] 的全部错误面原样透传；PNG 编码失败（缓冲
/// 长度与宽高不符等，理论不可达——字节由本门面按宽高紧致产出）→
/// [`ConvertError::InvalidData`]。
pub fn grab_frame_png(input: Box<dyn ReadSeek>, time_us: i64) -> Result<Vec<u8>, ConvertError> {
    let frame = decode_frame_rgba(input, time_us)?;
    crate::png::encode_rgba(frame.width, frame.height, &frame.rgba)
        .map_err(|err| ConvertError::InvalidData(format!("PNG 编码失败：{err}")))
}

// ---------------------------------------------------------------------------
// 单元测试（模块随 mp4+h264+png feature 组合编译，测试同门）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use ye_codec::AudioFrame;
    use ye_codec::VideoParams;
    use ye_container::{Metadata, Stream};
    use ye_pixfmt::{ChannelLayout, ColorDesc, SampleFormat};
    use ye_time::Timestamp;

    /// 造一个给定流类型的 MediaHeader（流布局校验单测用，与 waveform
    /// 同构）。
    fn header_of(types: &[MediaType]) -> MediaHeader {
        use ye_codec::CodecParameters;
        let streams = types
            .iter()
            .enumerate()
            .map(|(index, media_type)| Stream {
                index: index as u32,
                media_type: *media_type,
                codec: CodecParameters::new(CodecId::H264, *media_type),
                time_base: Rational::new(1, 1000),
                start_time: None,
                duration: None,
                nb_frames: None,
                metadata: Metadata::new(),
            })
            .collect();
        MediaHeader {
            format_name: "mp4".into(),
            streams,
            duration: None,
            start_time: None,
            bit_rate: None,
            metadata: Metadata::new(),
        }
    }

    /// 流布局校验：纯音频/双视频头拒绝，恰一视频（含带音频轨）放行。
    #[test]
    fn 流布局校验恰一条视频流() {
        assert!(matches!(
            validate_single_video_stream(&header_of(&[MediaType::Audio])),
            Err(ConvertError::InvalidArguments(_))
        ));
        assert!(matches!(
            validate_single_video_stream(&header_of(&[MediaType::Video, MediaType::Video])),
            Err(ConvertError::InvalidArguments(_))
        ));
        assert!(validate_single_video_stream(&header_of(&[MediaType::Video])).is_ok());
        assert!(
            validate_single_video_stream(&header_of(&[MediaType::Video, MediaType::Audio])).is_ok()
        );
    }

    /// 造一个 rgba8 帧（单色填充，pts 可缺）。
    fn rgba_frame(width: u32, height: u32, pts_us: Option<i64>, fill: u8) -> VideoFrame {
        let mut frame = VideoFrame::new(
            PixelFormat::Rgba8,
            width,
            height,
            ColorDesc::UNSPECIFIED,
            pts_us.map(|us| Timestamp::new(us, MICRO)),
            None,
        )
        .expect("构造 rgba8 帧");
        for byte in frame.plane_mut(0).iter_mut() {
            *byte = fill;
        }
        frame
    }

    /// 造一个 yuv420p 帧（格式不符路径用）。
    fn yuv_frame() -> VideoFrame {
        VideoFrame::new(
            PixelFormat::Yuv420p,
            16,
            16,
            ColorDesc::UNSPECIFIED,
            Some(Timestamp::new(0, MICRO)),
            None,
        )
        .expect("构造 yuv420p 帧")
    }

    /// 空游标（时点 100µs）。
    fn state_at(time_us: i64) -> GrabState {
        GrabState {
            time_us,
            grabbed: None,
            frames_seen: 0,
            saw_pts: false,
            latest_pts_us: None,
        }
    }

    /// 无 pts 帧跳过时点判定但计数；全素材无 pts 的 EOF 判「不猜」Err。
    #[test]
    fn 无pts帧跳过判定但计数() {
        let cancel = AtomicBool::new(false);
        let mut state = state_at(100);
        state
            .feed(Frame::Video(rgba_frame(8, 8, None, 10)), &cancel)
            .expect("无 pts 帧应跳过而非失败");
        state
            .feed(Frame::Video(rgba_frame(8, 8, None, 20)), &cancel)
            .expect("无 pts 帧应跳过而非失败");
        assert_eq!(state.frames_seen, 2, "计数不缺");
        assert!(!state.saw_pts, "未见任何 pts");
        assert!(state.grabbed.is_none(), "不猜帧序——不取帧");
        assert!(!cancel.load(Ordering::Relaxed), "未取帧不置 cancel");
        assert!(
            matches!(eof_error(&state), ConvertError::InvalidData(ref msg) if msg.contains("均无 pts")),
            "EOF 判「全素材无 pts」明确 Err：{:?}",
            eof_error(&state)
        );
    }

    /// 时点判定取首个 pts ≥ time_us 的帧并置 cancel；后续帧忽略。
    #[test]
    fn 首个达点帧被取且后续帧忽略() {
        let cancel = AtomicBool::new(false);
        let mut state = state_at(100);
        // pts 99 < 100：不取。
        state
            .feed(Frame::Video(rgba_frame(8, 8, Some(99), 1)), &cancel)
            .expect("未达点帧应正常通过");
        assert!(state.grabbed.is_none());
        // pts 100 ≥ 100（边界含等号）：取帧、置 cancel。
        state
            .feed(Frame::Video(rgba_frame(8, 8, Some(100), 2)), &cancel)
            .expect("达点帧应取");
        let grabbed = state.grabbed.as_ref().expect("已取帧");
        assert_eq!((grabbed.width, grabbed.height), (8, 8));
        assert!(grabbed.rgba.iter().all(|&b| b == 2), "取的是 pts 100 帧");
        assert!(cancel.load(Ordering::Relaxed), "取帧即置 cancel");
        // cancel 生效前的后续帧：忽略（不覆盖已取帧）。
        state
            .feed(Frame::Video(rgba_frame(8, 8, Some(140), 3)), &cancel)
            .expect("已取帧后续帧忽略而非失败");
        assert!(
            state
                .grabbed
                .as_ref()
                .expect("帧不被覆盖")
                .rgba
                .iter()
                .all(|&b| b == 2)
        );
        // 最晚 pts 记录不推进：已取帧后的帧被完全忽略（含 pts 追踪）。
        assert_eq!(state.latest_pts_us, Some(100));
    }

    /// 音频帧丢弃不计视频帧数；非 rgba8 视频帧明确 Err。
    #[test]
    fn 音频帧丢弃且非rgba帧明确报错() {
        let cancel = AtomicBool::new(false);
        let mut state = state_at(0);
        let audio = AudioFrame::new(SampleFormat::F32, ChannelLayout::MONO, 48_000, 1, None)
            .expect("构造音频帧");
        state
            .feed(Frame::Audio(audio), &cancel)
            .expect("音频帧丢弃");
        assert_eq!(state.frames_seen, 0, "音频不进视频计数");
        assert!(
            matches!(
                state.feed(Frame::Video(yuv_frame()), &cancel),
                Err(ConvertError::InvalidData(_))
            ),
            "链输出非 rgba8 是数据驱动失败"
        );
    }

    /// EOF 三形 Err 各述其因：无视频帧 / 全帧无 pts / 时点超出最晚 pts。
    #[test]
    fn eof三形错误各述其因() {
        let empty = state_at(50);
        assert!(
            matches!(eof_error(&empty), ConvertError::InvalidData(ref m) if m.contains("无视频帧"))
        );
        let mut no_pts = state_at(50);
        no_pts.frames_seen = 3;
        assert!(
            matches!(eof_error(&no_pts), ConvertError::InvalidData(ref m) if m.contains("均无 pts"))
        );
        let mut beyond = state_at(500);
        beyond.frames_seen = 3;
        beyond.saw_pts = true;
        beyond.latest_pts_us = Some(440);
        let err = eof_error(&beyond);
        assert!(
            matches!(&err, ConvertError::InvalidData(m) if m.contains("超出素材")
                && m.contains("440")),
            "越界文案含最晚 pts：{err}"
        );
    }

    /// 紧致抽取去行尾 padding：跨距外的字节不进 rgba。
    #[test]
    fn 紧致抽取去行尾对齐padding() {
        // 宽 5 → 每行 20 字节，跨距按 64 对齐 → 行尾必有 padding。
        let width = 5u32;
        let height = 2u32;
        let mut frame = rgba_frame(width, height, None, 0);
        let stride = frame.layout().plane(0).stride;
        assert!(
            stride > width as usize * 4,
            "测试前提：行尾确有 padding（stride {stride}）"
        );
        for y in 0..height as usize {
            let row = y * stride;
            for x in 0..width as usize * 4 {
                frame.plane_mut(0)[row + x] = (y as u8 + 1) * 16 + (x % 16) as u8;
            }
            for pad in frame.plane_mut(0)[row + width as usize * 4..row + stride].iter_mut() {
                *pad = 0xEE; // padding 毒化：若被收进 tight 即露馅。
            }
        }
        let grabbed = tight_rgba(&frame);
        assert_eq!((grabbed.width, grabbed.height), (width, height));
        assert_eq!(grabbed.rgba.len(), width as usize * height as usize * 4);
        assert!(!grabbed.rgba.contains(&0xEE), "padding 字节不得进紧致输出");
        let expect: Vec<u8> = (0..height as usize)
            .flat_map(|y| {
                (0..width as usize * 4)
                    .map(|x| (y as u8 + 1) * 16 + (x % 16) as u8)
                    .collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(grabbed.rgba, expect);
    }

    /// 模板参数完整性冒烟：`VideoParams` 可组装（防 doc 示例与类型面
    /// 脱节的廉价锚点）。
    #[test]
    fn 视频参数类型面可达() {
        let params = VideoParams {
            width: 96,
            height: 64,
            pixel_format: Some(PixelFormat::Yuv420p),
            frame_rate: Some(Rational::RATE_25),
        };
        assert_eq!(params.width, 96);
    }
}
