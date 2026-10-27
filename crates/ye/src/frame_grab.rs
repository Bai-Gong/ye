//! 抽帧门面：`convert_frames` 泵 + yuv420p→rgba8 链 → 单帧 RGBA / PNG
//! （剪的缩略图管线出口，m2-jian-plan §2.2）。
//!
//! ## 两个入口
//!
//! - [`decode_frame_rgba`]：打开输入 → [`convert_frames`] 逐帧解码（链
//!   yuv420p→rgba8、size 缺省保持原尺寸）→ 取**首个 pts ≥ time_us** 的
//!   视频帧（紧致 rgba8 字节）；
//! - [`grab_frame_png`]：[`decode_frame_rgba`] → [`crate::png::encode_rgba`]
//!   一步出口（png.rs 定位的「最后一环」）。
//!
//! ## 取帧时序（取到即停，勿把自然 EOF 误译成功）
//!
//! 取到目标帧的 sink 回调内**立即置位内部 cancel 旗标**：泵在下一包
//! 检查点返回 [`ConvertError::Cancelled`]，门面翻译为**成功**；目标帧
//! 恰在末包时泵走自然收尾（排空段不检 cancel）返回 `Ok`，同为成功。
//! 两条路径统一按「帧已在手」判定——cancel 生效前同包残余帧与排空段
//! 帧被忽略，cancel 点之后泵的失败不追溯（目标帧已完整在手；对照
//! `ffmpeg -ss T -frames:v 1` 取到即止的语义）。相对地，**未取到帧时
//! 泵的 `Ok` 是自然 EOF → 明确 `Err`**（见下），绝不误译成功。
//!
//! ## 时点语义
//!
//! - `time_us` 负值钳 0（与 jian ffmpeg 轨 ffmpeg.rs:127 同语义）；
//! - 判定域：帧 pts 重标度到 µs 后与 `time_us` 比较（`pts ≥ time_us`）；
//! - **pts 缺失的帧**：跳过时点判定但计数——若整个素材无任何带 pts
//!   的视频帧 → 明确 `Err`（不猜帧序）；
//! - EOF 仍未取到 → 明确 `Err`（区分三形：无视频帧送达 / 全帧无 pts /
//!   时点超出最晚 pts，文案各述其因）。
//!
//! ## 前置流布局校验（与 [`crate::waveform`] 同动机）
//!
//! [`convert_frames`] 给每条音/视频流都建解码器、[`Frame`] 不携带流
//! 标识——多视频流素材若不前置校验，「首个 pts ≥ 时点」可能落在任意
//! 一路的帧上，是静默错帧。故喂泵前经 [`ContainerRegistry::probe`] 读
//! [`MediaHeader`]，要求**恰一条视频流**（0 条 → Err，≥2 条 → 明确
//! Err）。probe 按值消费输入，底层输入经 [`SharedInput`]（`Arc<Mutex>`
//! 共享）同时供 probe 与泵持有；probe 后游标显式回卷 0 再喂泵（与
//! waveform 同法）。
//!
//! ## 音轨行为（写明；行为锁定归 T3 向量）
//!
//! 泵给**每条**音频流也建解码器（convert.rs 主循环逐流 `create_decoder`）：
//! 本门面的解码注册表只装 H.264（`aac` feature 开启时顺带 AAC，解码后
//! 帧在 sink 丢弃）——**含不可解音轨的 mp4（AAC 能力面外如多声道，或
//! `aac` 未开启时的任何 AAC 音轨）会因音频解码器 Err 使整个抽帧调用
//! 打回**，即使视频轨本身完全可解。行为可兜（回退归 jian 命令层），
//! 由 T3 的「视频+不可解音轨」向量（vec-video-multi）锁定；泵级选流
//! 绕行归 M4（m2-jian-plan §6）。
//!
//! ## 能力面外
//!
//! High/CABAC 等解码能力面外的素材：解码 Err 原样透传（全程 `?` 无
//! 兜底分支），回退归 jian 命令层。
//!
//! ## 全库约定
//!
//! 零第三方依赖（仅 std + workspace crate）、不含 unsafe；数据驱动失败
//! 一律 `Result`，无 panic 路径；错误复用 [`ConvertError`]（手写
//! `Display`/`Error` 见 convert 模块），doc 注释中文并写明 derive 清单。

use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use ye_codec::{CodecId, CodecRegistry, Frame, MediaType, VideoFrame};
use ye_container::{ContainerError, ContainerRegistry, MediaHeader, ReadSeek};
use ye_pixfmt::PixelFormat;
use ye_time::Rational;

use crate::convert::{ConvertError, VideoChain, convert_frames, filter_ops};

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

/// 共享输入句柄：同一底层输入（`Arc<Mutex>`）供 probe 与解码泵各持
/// 一份克隆——probe 按值消费 `Box<dyn ReadSeek>`，不经共享则喂泵前
/// 无法回卷（模块文档「前置流布局校验」）。与 [`crate::waveform`]
/// 的同构私有件（顺序使用，锁无竞争）。
struct SharedInput {
    /// 底层输入。
    inner: Arc<Mutex<Box<dyn ReadSeek>>>,
}

impl SharedInput {
    /// 包一份底层输入的克隆视图（喂给 probe / 泵的 `Box<dyn ReadSeek>`）。
    fn share(inner: &Arc<Mutex<Box<dyn ReadSeek>>>) -> Box<dyn ReadSeek> {
        Box::new(SharedInput {
            inner: inner.clone(),
        })
    }
}

impl Read for SharedInput {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        lock_input(&self.inner).read(buf)
    }
}

impl Seek for SharedInput {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        lock_input(&self.inner).seek(pos)
    }
}

/// 锁底层输入（中毒容忍：read/seek 无 panic 路径，锁中毒仅因其它线程
/// panic 中断 IO——输入是纯数据游标，无不变式可破坏，取内部句柄续用）。
fn lock_input(shared: &Arc<Mutex<Box<dyn ReadSeek>>>) -> MutexGuard<'_, Box<dyn ReadSeek>> {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 抽帧解码注册表：容器 MP4 + 解码 H.264（`aac` 开启时顺带 AAC——可解
/// 音轨解码后帧在 sink 丢弃；未开/不可解音轨整体打回，见模块文档
/// 「音轨行为」）。
fn decode_registries() -> (ContainerRegistry, CodecRegistry) {
    let mut containers = ContainerRegistry::new();
    crate::container::mp4::register_mp4(&mut containers);
    let mut codecs = CodecRegistry::new();
    codecs.register_decoder(CodecId::H264, crate::codec_h264::decoder_factory);
    #[cfg(feature = "aac")]
    codecs.register_decoder(CodecId::Aac, crate::codec_aac::decoder_factory);
    (containers, codecs)
}

/// 前置流布局校验：**恰一条视频流**（模块文档「前置流布局校验」；
/// 音频流不在此拒——音轨能否解码由泵的解码器创建裁决，见「音轨行为」）。
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

/// 公共前奏：probe 校验流布局 → 共享游标回卷 0（喂泵前）。
///
/// 返回共享句柄（供 [`convert_frames`] 经 [`SharedInput::share`] 持有）。
fn probe_and_rewind(
    containers: &ContainerRegistry,
    input: Box<dyn ReadSeek>,
) -> Result<Arc<Mutex<Box<dyn ReadSeek>>>, ConvertError> {
    let shared = Arc::new(Mutex::new(input));
    let (_format, header) = containers.probe(SharedInput::share(&shared))?;
    validate_single_video_stream(&header)?;
    // probe 读头后游标停在头解析处：显式回卷 0 再喂泵（泵内 probe 窗口
    // 从当前位置读起，不回卷会拿 mdat 中段当探查窗口）。
    lock_input(&shared)
        .seek(SeekFrom::Start(0))
        .map_err(|err| ConvertError::Container(ContainerError::Io(err)))?;
    Ok(shared)
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
    /// 置 cancel（模块文档「取帧时序」）。
    fn feed(&mut self, frame: Frame, cancel: &AtomicBool) -> Result<(), ConvertError> {
        let Frame::Video(video) = frame else {
            // 音频帧解码后直通 sink：抽帧不消费，丢弃（音轨本身能否解码
            // 泵已先行裁决，见模块文档「音轨行为」）。
            return Ok(());
        };
        self.frames_seen += 1;
        if self.grabbed.is_some() {
            // 已取帧：cancel 生效前的同包残余/排空段帧，忽略（模块文档
            // 「取帧时序」）。
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
        if pts_us >= self.time_us {
            self.grabbed = Some(tight_rgba(&video));
            // 取帧即置 cancel：泵在下一包检查点返回 Cancelled（门面翻译
            // 为成功）；末包命中时泵自然收尾，同为成功。
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
/// 行为契约见模块文档（取帧时序 / 时点语义 / 前置流布局校验 / 音轨
/// 行为 / 能力面外）。
///
/// # Errors
///
/// - 流布局不符（无视频流 / 多视频流）→
///   [`ConvertError::InvalidArguments`]；
/// - EOF 未取到（无视频帧 / 全帧无 pts / 时点超出最晚 pts）→
///   [`ConvertError::InvalidData`]；
/// - 解封装/解码失败（High/CABAC 能力面外、不可解音轨打回等）原样
///   透传，不兜底。
pub fn decode_frame_rgba(
    input: Box<dyn ReadSeek>,
    time_us: i64,
) -> Result<GrabbedFrame, ConvertError> {
    // 负值钳 0（模块文档「时点语义」）。
    let time_us = time_us.max(0);
    let (containers, codecs) = decode_registries();
    let shared = probe_and_rewind(&containers, input)?;
    let cancel = AtomicBool::new(false);
    let mut state = GrabState {
        time_us,
        grabbed: None,
        frames_seen: 0,
        saw_pts: false,
        latest_pts_us: None,
    };
    let mut sink = |frame: Frame| state.feed(frame, &cancel);
    let result = convert_frames(
        &containers,
        &codecs,
        SharedInput::share(&shared),
        VideoChain {
            format: Some(PixelFormat::Rgba8),
            size: None,
        },
        filter_ops(),
        &mut sink,
        &|_processed, _total| {},
        &cancel,
    );
    // 帧已在手即成功（泵 Cancelled 的翻译 + 末包自然收尾 + cancel 点
    // 之后不追溯，见模块文档「取帧时序」）；未取到时泵结果原样定夺：
    // Ok 是自然 EOF → 明确 Err；Err 透传（Cancelled 不可能在此出现
    // ——旗标本轮私有且仅取帧后置位）。
    if let Some(frame) = state.grabbed {
        return Ok(frame);
    }
    match result {
        Ok(_) => Err(eof_error(&state)),
        Err(err) => Err(err),
    }
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
