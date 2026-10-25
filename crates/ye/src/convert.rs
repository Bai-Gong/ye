//! convert 泵：固定链序（解码 → 像素转换 → scale → 编码槽）的转码骨架。
//!
//! 施工图④(a)（M2）：通用图调度（`FilterGraph::topological_order`）归
//! 施工图⑦（M3），本泵内置**固定链序**——视频帧出解码器后依次过
//! 「像素转换槽 → scale 槽」，音频帧解码后直通（crop/fps/volume 等
//! 变换随像素核/处理图后续里程碑接入）。
//!
//! ## 两条输出路径
//!
//! - [`convert_frames`]：**帧消费模式**（M2 主路径）——过链后的帧交给
//!   调用方回调（剪的抽帧/波形、单测断言均走这里），不落盘；
//! - [`convert`]：**编码模式**（锻 M3 接入契约的库层形态）——链尾接
//!   编码槽，**M2 为明确 Unsupported 桩**（编码器随施工图⑤⑥交付，
//!   见 [`ENCODE_SLOT_STUB`]），调用即失败、不消费输入、不产出输出。
//!
//! ## 像素核槽（注入消费）
//!
//! 像素转换/scale 的核是**注入的纯函数**（[`PixelOps`]）：核本体在
//! `ye-filter/src/video/`（施工图④(b)，纯函数 crate）。泵只认函数
//! 签名、不依赖核的模块路径——核落地前后泵均编译，接线点在门面
//! 侧一行桥接（签名以核的模块文档为准，预期与本槽同形：
//! `fn(&VideoFrame, …) -> Result<VideoFrame, _>`）。
//!
//! ## 进度与取消（契约：docs/contracts.md「锻的最小 API 面」）
//!
//! - 进度 `(processed_us, total_us)` **微秒**，与锻的
//!   `transcode-progress` 事件负载 `{jobId, timeUs, totalUs}` 同源；
//!   逐帧上报（帧过完链后），`processed_us` 单调不减（跨流取各流
//!   已处理时长的最大值），`total_us` 取流时长（缺时长 → 0）；
//! - 取消 [`AtomicBool`]：**每包检查点**（读出一包后立即检查），
//!   置位即返回 [`ConvertError::Cancelled`]——M2 帧模式无半成品
//!   落盘，直接退出；编码模式（M3）落地后补「清理半成品」。
//!
//! ## 全库约定
//!
//! 零第三方依赖（仅 std + workspace crate）、不含 unsafe；数据驱动
//! 失败一律 `Result`，无 panic 路径；`ConvertError` 手写
//! `Display`/`Error`（禁 thiserror）。

use std::sync::atomic::{AtomicBool, Ordering};

use ye_codec::{CodecRegistry, Decoded, Decoder, Frame, MediaType, VideoFrame};
use ye_container::{ContainerError, ContainerRegistry, ReadSeek, WriteSeek};
use ye_pixfmt::PixelFormat;
use ye_time::{Rational, TimeError};

/// 微秒时基（进度的报告域）。
const MICRO: Rational = Rational::new(1, 1_000_000);

/// 进度回调：`(processed_us, total_us)`——已处理/总时长，微秒。
///
/// 与锻现有 `transcode-progress` 事件负载同源（`timeUs`/`totalUs`
/// 直传回调值）；≥100ms 节流与 jobId 路由归锻侧事件层，ye 只提供
/// 进程内每任务回调。
pub type ConvertProgress<'a> = &'a dyn Fn(i64, i64);

/// 像素转换槽签名：把帧转到 `target` 格式（纯函数，核归 ye-filter）。
///
/// 契约：输出帧宽高与输入一致、pts/duration 透传；输出布局按
/// [`ye_codec::VideoFrame::new`] 的默认跨距对齐。
pub type PixelConvertFn = fn(&VideoFrame, PixelFormat) -> Result<VideoFrame, ConvertError>;

/// scale 槽签名：把帧缩放到 `(width, height)`（纯函数，核归 ye-filter）。
///
/// 契约同 [`PixelConvertFn`]（格式不变、pts/duration 透传）。
pub type ScaleFn = fn(&VideoFrame, u32, u32) -> Result<VideoFrame, ConvertError>;

/// 像素核槽：转换与缩放两个纯函数（注入消费，见模块文档）。
///
/// derive 清单：`Debug, Clone, Copy`（`fn` 指针字段；不作 `PartialEq`
/// 要求——按指针值比较意义有限）。
#[derive(Debug, Clone, Copy)]
pub struct PixelOps {
    /// 像素格式转换槽（链序第 2 环）。
    pub convert: PixelConvertFn,
    /// 缩放槽（链序第 3 环）。
    pub scale: ScaleFn,
}

/// 视频链参数（固定链序「转换 → scale」的槽位配置）。
///
/// `None` = 该环跳过（不调核、帧直通下一环）；目标与帧现状相同也
/// 跳过（泵负责判断，核不收 no-op 请求）。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoChain {
    /// 目标像素格式（`None` = 保持解码输出格式）。
    pub format: Option<PixelFormat>,
    /// 目标尺寸 `(宽, 高)`（`None` = 保持原尺寸）。
    pub size: Option<(u32, u32)>,
}

/// 帧消费回调：过链后的帧（视频已转换/缩放，音频直通）逐帧送达。
///
/// 返回 `Err` 即终止泵（消费方失败原样透传，见 [`ConvertError::Sink`]）。
pub type FrameSink<'a> = &'a mut dyn FnMut(Frame) -> Result<(), ConvertError>;

/// 转码统计（两条路径共用的成功载荷）。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConvertStats {
    /// 参与解码的流数（视频 + 音频；字幕等未接流的轨道不计）。
    pub streams: usize,
    /// 出链送达消费方的视频帧数。
    pub video_frames: u64,
    /// 出链送达消费方的音频帧数。
    pub audio_frames: u64,
    /// 进度终值：已处理时长（µs；无帧时戳时可能为 0）。
    pub processed_us: i64,
    /// 总时长（µs；流缺时长 → 0）。
    pub total_us: i64,
}

/// 编码槽桩文案：编码器随施工图⑤⑥交付（roadmap M3 排期）。
pub const ENCODE_SLOT_STUB: &str = "convert 编码槽未交付：编码器随施工图⑤⑥交付\
（M2 为固定链骨架，帧消费管线见 ye::convert::convert_frames）";

/// 帧消费泵：`解码 → 像素转换 → scale → 帧回调`（固定链序，M2 主路径）。
///
/// 行为契约：
///
/// - **打开输入**：`containers` auto-probe 打开 `input`，读头取流表；
/// - **接解码器**：视频/音频流经 `codecs` 逐流创建解码器（按流的
///   [`ye_codec::CodecParameters`]，未注册 →
///   [`ConvertError::Codec`](`ye_codec::CodecError::NotRegistered`) 透传）；
///   字幕等其他轨道跳过不进泵；
/// - **固定链序（视频）**：解码帧 →（目标格式与现格式不同 →
///   [`PixelOps::convert`]）→（目标尺寸与现尺寸不同 → [`PixelOps::scale`]）
///   → 回调；**转换在前 scale 在后**（施工图④链序；与 ffmpeg 对拍时
///   参考口径 `-vf scale,format` 的顺序相反，对拍容差须覆盖序差异，
///   见 contracts.md「锻的最小 API 面」）；音频帧解码后直通回调；
/// - **时戳**：帧时戳由解码器透传（包时戳语义归各解码器文档），
///   泵不改写；
/// - **进度**：逐帧上报 `(processed_us, total_us)`（见模块文档）；
///   帧缺 pts 时跳过该帧的上报（processed 不推进）；
/// - **取消**：每包检查点，置位 → [`ConvertError::Cancelled`]
///   （见模块文档）；
/// - **收尾**：输入读尽后逐解码器 `flush` 排空重排缓冲，排空帧同样
///   过链送达（`flush` 后 `pull` 返回 `NeedInput` 视为实现违约，泵
///   容忍并停止该流排空——契约见 [`ye_codec::Decoder`]）。
///
/// # Errors
///
/// 见 [`ConvertError`] 各变体；下层解封装/解码失败原样包装透出。
// 8 个参数（超 clippy 默认上限 1 个）：注册表 ×2 / 输入 / 链配置 /
// 像素槽 / 帧回调 / 进度 / 取消——各占一席即锻契约（docs/contracts.md
// 「锻的最小 API 面」）的参数形态（与 `remux`/`ye_cli::convert` 的
// 显式参数风格一致）；收拢进 options 结构会把「链配置」「消费回调」
// 「控制旗标」三类职责搅进一个载荷对象，调用点反而更啰嗦。
#[allow(clippy::too_many_arguments)]
pub fn convert_frames(
    containers: &ContainerRegistry,
    codecs: &CodecRegistry,
    input: Box<dyn ReadSeek>,
    chain: VideoChain,
    pixels: PixelOps,
    sink: FrameSink<'_>,
    progress: ConvertProgress<'_>,
    cancel: &AtomicBool,
) -> Result<ConvertStats, ConvertError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(ConvertError::Cancelled);
    }
    let (_format, mut demuxer) = containers.open_demuxer(input)?;
    let header = demuxer.read_header()?;

    // 逐流接解码器（视频/音频；其余轨道跳过）。
    let mut decoders: Vec<Option<Box<dyn Decoder>>> = Vec::with_capacity(header.streams.len());
    let mut streams = 0usize;
    for stream in &header.streams {
        let decoder = match stream.media_type {
            MediaType::Video | MediaType::Audio => {
                streams += 1;
                Some(codecs.create_decoder(&stream.codec)?)
            }
            MediaType::Subtitle => None,
        };
        decoders.push(decoder);
    }

    // total_us = 各流时长（µs）最大值；缺时长贡献 0。
    let mut total_us = 0i64;
    for stream in &header.streams {
        if let Some(duration) = stream.duration {
            let us = duration.rescale(MICRO)?.ticks();
            total_us = total_us.max(us);
        }
    }

    let mut run = ChainRun {
        chain,
        pixels,
        processed_us: 0,
        total_us,
        progress,
    };
    let mut video_frames = 0u64;
    let mut audio_frames = 0u64;

    // 主循环：每包检查点（取消）→ 分流解码 → 帧过链 → 回调。
    while let Some(packet) = demuxer.read_packet()? {
        if cancel.load(Ordering::Relaxed) {
            return Err(ConvertError::Cancelled);
        }
        let index = usize::try_from(packet.stream_index).unwrap_or(usize::MAX);
        let Some(Some(decoder)) = decoders.get_mut(index) else {
            continue; // 未接流的轨道（字幕等）：包直通丢弃。
        };
        decoder.feed(&packet)?;
        loop {
            match decoder.pull()? {
                Decoded::Frame(frame) => {
                    let frame = run.process(frame)?;
                    match frame {
                        Frame::Video(_) => video_frames += 1,
                        Frame::Audio(_) => audio_frames += 1,
                    }
                    sink(frame)?;
                }
                Decoded::NeedInput => break,
                Decoded::Finished => break,
            }
        }
    }

    // 排空：逐解码器 flush，重排缓冲里的尾帧同样过链送达。
    for decoder in decoders.iter_mut().flatten() {
        decoder.flush();
        loop {
            match decoder.pull()? {
                Decoded::Frame(frame) => {
                    let frame = run.process(frame)?;
                    match frame {
                        Frame::Video(_) => video_frames += 1,
                        Frame::Audio(_) => audio_frames += 1,
                    }
                    sink(frame)?;
                }
                Decoded::Finished => break,
                Decoded::NeedInput => break,
            }
        }
    }

    Ok(ConvertStats {
        streams,
        video_frames,
        audio_frames,
        processed_us: run.processed_us,
        total_us,
    })
}

/// 编码转码（锻 M3 接入契约的库层形态；M2 编码槽 Unsupported 桩）。
///
/// M2 行为：**调用即 [`ConvertError::UnsupportedStub`]**
/// （[`ENCODE_SLOT_STUB`]），不消费输入、不写出输出。编码器随施工图
/// ⑤⑥交付后本函数接编码槽（`convert_frames` 的链尾 + 编码 + 封装），
/// 参数形态届时按真编码配置定形（可能扩展）。
///
/// # Errors
///
/// M2 恒为 [`ConvertError::UnsupportedStub`]。
pub fn convert(
    input: Box<dyn ReadSeek>,
    output: Box<dyn WriteSeek>,
    progress: ConvertProgress<'_>,
    cancel: &AtomicBool,
) -> Result<ConvertStats, ConvertError> {
    // 编码槽未交付：参数全部不碰（输入不读、输出不写、回调不触发、
    // 旗标不查），让「调用即失败」可被调用方与测试识别。
    let _ = (input, output, progress, cancel);
    Err(ConvertError::UnsupportedStub(ENCODE_SLOT_STUB))
}

/// 单帧过链 + 进度上报的泵内游标（帧模式的折叠状态）。
struct ChainRun<'a> {
    chain: VideoChain,
    pixels: PixelOps,
    processed_us: i64,
    total_us: i64,
    progress: ConvertProgress<'a>,
}

impl ChainRun<'_> {
    /// 固定链序：转换槽 → scale 槽（视频；音频直通），随后上报进度。
    fn process(&mut self, frame: Frame) -> Result<Frame, ConvertError> {
        let frame = match frame {
            Frame::Video(video) => Frame::Video(self.video_chain(video)?),
            Frame::Audio(audio) => Frame::Audio(audio),
        };
        if let Some(pts) = frame.pts() {
            let us = pts.rescale(MICRO)?.ticks();
            // 单调不减：跨流交错时取已处理时长的最大值。
            self.processed_us = self.processed_us.max(us);
        }
        (self.progress)(self.processed_us, self.total_us);
        Ok(frame)
    }

    /// 视频链：格式不同 → 转换槽；尺寸不同 → scale 槽（no-op 跳过）。
    fn video_chain(&self, video: VideoFrame) -> Result<VideoFrame, ConvertError> {
        let mut video = video;
        if let (Some(target), format) = (self.chain.format, video.format())
            && target != format
        {
            video = (self.pixels.convert)(&video, target)?;
        }
        if let Some((width, height)) = self.chain.size
            && (width, height) != (video.width(), video.height())
        {
            video = (self.pixels.scale)(&video, width, height)?;
        }
        Ok(video)
    }
}

/// convert 泵（本模块）全部失败模式的枚举。
///
/// ## 变体语义
///
/// - [`Cancelled`](Self::Cancelled)：取消旗标置位（调用方请求取消，
///   非失败——调用方据此前进/清理）；
/// - [`UnsupportedStub`](Self::UnsupportedStub)：编码槽未交付（M2 桩，
///   文案含交付里程碑标注）；
/// - [`Sink`](Self::Sink)：帧消费回调失败（消费方错误原样回传终止泵）；
/// - [`InvalidArguments`](Self::InvalidArguments)：槽参数非法（如目标
///   格式与槽能力不符——由核侧透传填充）；
/// - [`Container`](Self::Container)/[`Codec`](Self::Codec)/[`Time`](Self::Time)：
///   下层错误原样包装（配套 `From`，`?` 直接透传，不吞下层类型）。
///
/// ## derive 清单
///
/// 仅 `Debug`（含 [`ContainerError`] 变体，其 `Io` 载荷非 `Clone`/
/// `PartialEq`——**测试断言一律 `matches!`，不得 `assert_eq`**）；
/// `Display` 与 `std::error::Error` 手工实现，Display 文案英文短句；
/// doc 注释中文。
#[derive(Debug)]
pub enum ConvertError {
    /// 取消旗标置位：调用方请求取消（每包检查点检出）。
    Cancelled,
    /// 能力未交付（桩），携带含里程碑标注的文案。
    UnsupportedStub(&'static str),
    /// 帧消费回调返回错误（携带其文案）。
    Sink(String),
    /// 槽/链参数非法（像素核透传），携带描述。
    InvalidArguments(String),
    /// 下层 ye-container 错误（打开/读头/读包）。
    Container(ContainerError),
    /// 下层 ye-codec 错误（解码器创建/feed/pull）。
    Codec(ye_codec::CodecError),
    /// 下层 ye-time 错误（进度 µs 换算等）。
    Time(TimeError),
}

impl core::fmt::Display for ConvertError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ConvertError::Cancelled => f.write_str("cancelled"),
            ConvertError::UnsupportedStub(msg) => write!(f, "unsupported stub: {msg}"),
            ConvertError::Sink(msg) => write!(f, "frame sink error: {msg}"),
            ConvertError::InvalidArguments(msg) => write!(f, "invalid arguments: {msg}"),
            ConvertError::Container(err) => write!(f, "container error: {err}"),
            ConvertError::Codec(err) => write!(f, "codec error: {err}"),
            ConvertError::Time(err) => write!(f, "time error: {err}"),
        }
    }
}

impl std::error::Error for ConvertError {
    /// 包装变体透出下层错误源。
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConvertError::Container(err) => Some(err),
            ConvertError::Codec(err) => Some(err),
            ConvertError::Time(err) => Some(err),
            _ => None,
        }
    }
}

impl From<ContainerError> for ConvertError {
    fn from(err: ContainerError) -> ConvertError {
        ConvertError::Container(err)
    }
}

impl From<ye_codec::CodecError> for ConvertError {
    fn from(err: ye_codec::CodecError) -> ConvertError {
        ConvertError::Codec(err)
    }
}

impl From<TimeError> for ConvertError {
    fn from(err: TimeError) -> ConvertError {
        ConvertError::Time(err)
    }
}
