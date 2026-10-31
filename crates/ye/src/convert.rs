//! convert 泵：固定链序（解码 → 像素转换 → scale → 编码 → 封装）。
//!
//! 施工图④(a)（M2）：通用图调度（`FilterGraph::topological_order`）已随
//! M3 交付真实现，本泵内置**固定链序**——视频帧出解码器后依次过
//! 「像素转换槽 → scale 槽」，音频帧在帧模式下解码后直通（crop/fps/
//! volume 等变换随处理图编排接入）；泵的图编排接入归后续批次。
//!
//! ## 两条输出路径
//!
//! - [`convert_frames`]：**帧消费模式**（M2 主路径）——过链后的帧交给
//!   调用方回调（剪的抽帧/波形、单测断言均走这里），不落盘；
//! - [`convert`]：**编码模式**（锻接入契约的库层形态，M3 集成阶段
//!   落地）——视频链 `解码 → 处理链（缩放/像素格式）→ H.264 编码
//!   → MP4 封装`，音频三态（[`AudioEncodeSettings`]）：直通（对标
//!   `-c:a copy`）/ AAC-LC 重编码（对标 `-c:a aac -b:a`）/ 剥轨
//!   （对标 `-an`）；视频亦可剥轨（`video = None`，对标 `-vn` 的
//!   纯音频输出）。编码器/封装器均经注入的注册表取得，泵本体不
//!   依赖具体实现 crate。
//!
//! ## 编码模式的轨道策略
//!
//! - **视频轨**（至多一路——多视频流输入显式 `InvalidArguments` 拒收，
//!   多视频轨归后续批次）：源帧率取流标称值，缺则按 `样例数 ÷ 时长`
//!   精确有理数推导，再缺省 25fps；输出时基 = `1/帧率分子`（帧时长 =
//!   帧率分母 tick，整数无舍入——NTSC 等分数帧率下打戳与 muxer 衔接
//!   校验均精确）；输出包时戳由编码器按帧号重排（输入帧 pts 被忽略，
//!   见「H.264 编码器输出契约」），首包恒 0；
//! - **音频轨**（任意路）按 [`AudioEncodeSettings`] 三态：
//!   - **直通**：包数据/关键帧/时戳原样，每轨整体平移到首包 dts = 0
//!     （progressive MP4 的 stts 从 0 起算，与
//!     [`remux`](crate::container::remux) 同法）；
//!   - **AAC-LC 重编码**：解码（packed f32）→ 采样累积凑 1024 →
//!     AAC 编码器 → 原始访问单元写 MP4（esds 模板）；末帧补零到
//!     1024；输出时戳由编码器按帧号重排（时间线从 0 起算，无平移）；
//!     采样率/声道沿用源流（无重采样器，改采样率归后续批次）；
//!   - **剥轨**：不进输出容器；
//! - **字幕等轨道**：丢弃（不进输出容器）。
//!
//! ## avcC 的取得（`Encoder` trait 无配置回读面）
//!
//! 视频轨模板的 extradata（avcC 记录）由**首个 IDR 包的带内 SPS/PPS**
//! 构造（消费「H.264 编码器输出契约」：IDR 包带内含 SPS+PPS+IDR 三个
//! NAL、AVCC 4 字节长度前缀、lengthSizeMinusOne=3）——与
//! `ye-codec-h264` 的 `build_avcc` 同构。为此**封装头延迟到首个视频包
//! 产出时**才落：此前到达的音频包先缓冲、落头后原序冲出（常态仅几包）；
//! 无视频帧的输入（纯音频）在 EOF 落头。
//!
//! ## 像素核槽（注入消费）
//!
//! 像素转换/scale 的核是**注入的纯函数**（[`PixelOps`]）：核本体在
//! `ye-filter/src/video/`（纯函数 crate）。泵只认函数签名、不依赖核的
//! 模块路径——本模块末尾提供 ye-filter 核的桥接
//! （[`filter_convert`]/[`filter_scale`]/[`filter_ops`]）作缺省接线。
//!
//! ## 进度与取消（契约：docs/contracts.md「锻的最小 API 面」）
//!
//! - 进度 `(processed_us, total_us)` **微秒**，与锻的
//!   `transcode-progress` 事件负载 `{jobId, timeUs, totalUs}` 同源；
//!   帧模式逐帧上报、编码模式逐输出包上报（视频帧过链编码落盘后、
//!   音频包直通后），`processed_us` 单调不减（跨流取各流已处理时长的
//!   最大值），`total_us` 取流时长（缺时长 → 0）；
//! - 取消 [`AtomicBool`]：**每包检查点**（读出一包后立即检查），置位即
//!   返回 [`ConvertError::Cancelled`]。半成品清理归调用方（泵持有的是
//!   `WriteSeek` 抽象、无路径概念；CLI 层失败即删输出文件）。
//!
//! ## 全库约定
//!
//! 零第三方依赖（仅 std + workspace crate）、不含 unsafe；数据驱动
//! 失败一律 `Result`，无 panic 路径；`ConvertError` 手写
//! `Display`/`Error`（禁 thiserror）。

use std::sync::atomic::{AtomicBool, Ordering};

use ye_codec::{
    AudioEncoderConfig, AudioFrame, AudioParams, CodecId, CodecParameters, CodecRegistry, Decoded,
    Decoder, Encoded, Encoder, EncoderConfig, Frame, MediaType, Packet, QualityMode,
    VideoEncoderConfig, VideoFrame, VideoParams,
};
use ye_container::{
    ContainerError, ContainerRegistry, Metadata, Muxer, ReadSeek, Stream, StreamTemplate, WriteSeek,
};
use ye_filter::{FilterError as FilterKernelError, Yuv420pBuffer, Yuv420pView, YuvRange};
use ye_pixfmt::{ChannelLayout, ColorRange, PixelFormat, SampleFormat};
use ye_time::{Rational, TimeError, Timestamp};

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
/// [`VideoFrame::new`] 的默认跨距对齐。
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

/// 编码模式（[`convert`]）的视频设置：处理链 + H.264 质量面。
///
/// 处理链后的帧最终强制 yuv420p（H.264 编码器的输入面）——非 yuv420p
/// 的处理后帧会再过一次转换槽（[`PixelOps::convert`]）。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`（`VideoChain`/`QualityMode`
/// 均纯值载荷）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoEncodeSettings {
    /// 视频处理链（缩放/像素格式目标）。
    pub chain: VideoChain,
    /// GOP（IDR 间隔，帧数）：1 = 全 I 帧，0 非法（编码器工厂拒收）。
    pub gop: u32,
    /// 质量模式（恒定 QP / 简单 CRF，值域 1–51）。
    pub quality: QualityMode,
    /// 目标码率（bit/s；`None` = 纯质量模式）。`Some` 时编码器启用真码控
    /// （M3 补齐）：`Qp` 基准 → CBR 风味（QP 受虚拟缓冲反馈调制，平均
    /// 码率趋近目标）；`Crf` 基准 → A-VBR（CRF 起步、目标码率作封顶）。
    pub bit_rate: Option<u64>,
}

impl Default for VideoEncodeSettings {
    /// 缺省：no-op 链 + GOP 250（对齐 x264 缺省）+ 简单 CRF 23（无码控）。
    fn default() -> Self {
        VideoEncodeSettings {
            chain: VideoChain {
                format: None,
                size: None,
            },
            gop: 250,
            quality: QualityMode::Crf(23),
            bit_rate: None,
        }
    }
}

/// 编码模式（[`convert`]）的音频设置：直通 / AAC-LC 重编码 / 剥轨。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq`（纯值载荷）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioEncodeSettings {
    /// 直通（重封装不重编码；缺省行为，对标 `-c:a copy`）。
    #[default]
    Copy,
    /// AAC-LC 重编码（对标 `-c:a aac -b:a`）：目标码率 bit/s（>0，
    /// 编码器工厂校验）；采样率/声道沿用源流。
    Aac {
        /// 目标码率（bit/s）。
        bitrate_bps: u32,
    },
    /// 剥音轨（输出容器不含音频，对标 `-an`）。
    Drop,
}

/// 帧消费回调：过链后的帧（视频已转换/缩放，音频直通）逐帧送达。
///
/// 返回 `Err` 即终止泵（消费方失败原样透传，见 [`ConvertError::Sink`]）。
pub type FrameSink<'a> = &'a mut dyn FnMut(Frame) -> Result<(), ConvertError>;

/// 转码统计（两条路径共用的成功载荷）。
///
/// 帧模式：`streams` = 参与解码的流数、`video_frames`/`audio_frames` =
/// 出链送达消费方的帧数；编码模式：`streams` = 输出容器的流数、
/// `video_frames` = 重编码帧数、`audio_frames` = 直通包数或重编码
/// 输出包数。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConvertStats {
    /// 流数（帧模式 = 解码流数；编码模式 = 输出流数）。
    pub streams: usize,
    /// 视频帧数（帧模式 = 送达帧数；编码模式 = 重编码帧数）。
    pub video_frames: u64,
    /// 音频计数（帧模式 = 解码帧数；编码模式 = 直通包数）。
    pub audio_frames: u64,
    /// 进度终值：已处理时长（µs；无帧时戳时可能为 0）。
    pub processed_us: i64,
    /// 总时长（µs；流缺时长 → 0）。
    pub total_us: i64,
}

/// 帧消费泵：`解码 → 像素转换 → scale → 帧回调`（固定链序，M2 主路径）。
///
/// 行为契约：
///
/// - **打开输入**：`containers` auto-probe 打开 `input`，读头取流表；
/// - **接解码器**：视频/音频流经 `codecs` 逐流创建解码器（按流的
///   [`CodecParameters`]，未注册 →
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

/// 编码转码：视频链 `解码 → 处理链 → H.264 编码 → MP4 封装`，音频按
/// [`AudioEncodeSettings`] 三态（直通/AAC-LC 重编码/剥轨）——锻接入
/// 契约的库层形态。
///
/// 行为契约（轨道策略与 avcC 取得见模块文档）：
///
/// - **输入**：`containers` auto-probe 打开 `input`；`video = Some(..)` 时
///   视频流至多一路（多路 → [`ConvertError::InvalidArguments`]），视频
///   解码器与 H.264 编码器经 `codecs` 取得（未注册 → `NotRegistered`
///   透传）；`video = None` 剥视频（纯音频输出，对标 `-vn`）；
/// - **视频链**：解码帧 → [`VideoChain`]（转换槽 → scale 槽，no-op
///   跳过）→ 强制 yuv420p（非 yuv420p 再过转换槽）→ H.264 编码；
///   输出帧率/时基见模块文档，输出包时戳由编码器按帧号重排；
/// - **音频**：`Copy` 任意路直通（每轨平移到首包 dts = 0，包数据/
///   关键帧/pts−dts 偏移原样）；音频包缺 dts →
///   [`ConvertError::InvalidData`]（无法平移时间线）；`Aac` 重编码
///   （采样率/声道沿用源流，采样累积凑 1024、末帧补零）；`Drop`
///   剥轨；
/// - **封装**：`output_format` 点名 muxer（如 `"mp4"`）；封装头延迟到
///   首个视频包（avcC 就绪）或 EOF（纯音频）；字幕轨丢弃；
/// - **进度/取消**：逐输出包上报、每包检查点（见模块文档）；
/// - **半成品清理**：失败即返回错误（封装器随所有权丢弃释放输出），
///   输出文件的删除归调用方（见模块文档）。
///
/// # Errors
///
/// 见 [`ConvertError`] 各变体；下层解封装/解码/编码/封装失败原样包装
/// 透出。
// 10 个参数（超 clippy 默认上限）：与 `convert_frames` 同理——注册表 ×2 /
// 输入 / 输出格式名 / 输出 / 视频设置 / 音频设置 / 像素槽 / 进度 / 取消
// 各占一席即锻契约的参数形态；输出格式名单列（非硬编码 "mp4"）保持泵
// 对容器注册表中立，编码槽当前固定 H.264 视频 + AAC-LC 音频（参数面随
// 真编码配置演进）。
#[allow(clippy::too_many_arguments)]
pub fn convert(
    containers: &ContainerRegistry,
    codecs: &CodecRegistry,
    input: Box<dyn ReadSeek>,
    output_format: &str,
    output: Box<dyn WriteSeek>,
    video: Option<&VideoEncodeSettings>,
    audio: &AudioEncodeSettings,
    pixels: PixelOps,
    progress: ConvertProgress<'_>,
    cancel: &AtomicBool,
) -> Result<ConvertStats, ConvertError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(ConvertError::Cancelled);
    }
    let (_in_format, mut demuxer) = containers.open_demuxer(input)?;
    let header = demuxer.read_header()?;

    // 编码模式单视频轨（视频剥轨时无此约束——全部剥掉无从冲突）：
    // 多视频流的模板/extradata 编排归后续批次。
    if video.is_some() {
        let video_count = header
            .streams
            .iter()
            .filter(|s| s.media_type == MediaType::Video)
            .count();
        if video_count > 1 {
            return Err(ConvertError::InvalidArguments(format!(
                "编码模式暂只支持单视频流输入（检测到 {video_count} 路），多视频流归后续批次"
            )));
        }
    }

    // total_us = 各流时长（µs）最大值；缺时长贡献 0。
    let mut total_us = 0i64;
    for stream in &header.streams {
        if let Some(duration) = stream.duration {
            let us = duration.rescale(MICRO)?.ticks();
            total_us = total_us.max(us);
        }
    }

    // 逐流建轨（轨序 = 输入流序；字幕等轨道标记丢弃）。
    let mut lanes = Vec::with_capacity(header.streams.len());
    for stream in &header.streams {
        let lane = match stream.media_type {
            // video = None：剥视频（纯音频输出，对标 -vn）。
            MediaType::Video => match video {
                Some(settings) => Lane::Video(build_video_lane(codecs, stream, settings)?),
                None => Lane::Dropped,
            },
            MediaType::Audio => match *audio {
                AudioEncodeSettings::Copy => Lane::Audio(AudioLane::Copy {
                    template: StreamTemplate {
                        media_type: MediaType::Audio,
                        codec: stream.codec.clone(),
                        time_base: stream.time_base,
                        frame_rate: None,
                        metadata: stream.metadata.clone(),
                    },
                    shift: None,
                    out_index: 0,
                }),
                AudioEncodeSettings::Aac { bitrate_bps } => Lane::Audio(AudioLane::Encode(
                    build_audio_encode_lane(codecs, stream, bitrate_bps)?,
                )),
                AudioEncodeSettings::Drop => Lane::Dropped,
            },
            MediaType::Subtitle => Lane::Dropped,
        };
        lanes.push(lane);
    }

    // 剥轨守卫：视频剥除且音频全剥（或无轨）→ 拒绝产出零轨容器
    // （对标 ffmpeg「输出不含任何流」的失败语义，不静默产空文件）。
    if !lanes
        .iter()
        .any(|lane| matches!(lane, Lane::Video(_) | Lane::Audio(_)))
    {
        return Err(ConvertError::InvalidArguments(
            "剥轨后无输出轨（视频剥除且音频全剥或无音轨），拒绝产出零轨容器".into(),
        ));
    }

    let muxer = containers.create_muxer(output_format, output)?;
    let chain = video.map_or(
        VideoChain {
            format: None,
            size: None,
        },
        |settings| settings.chain,
    );
    let mut run = EncodeRun {
        lanes,
        muxer,
        header: HeaderState::Pending(Vec::new()),
        chain,
        pixels,
        processed_us: 0,
        total_us,
        progress,
        video_frames: 0,
        audio_frames: 0,
        streams_out: 0,
    };

    while let Some(packet) = demuxer.read_packet()? {
        if cancel.load(Ordering::Relaxed) {
            return Err(ConvertError::Cancelled);
        }
        run.handle_packet(packet)?;
    }
    run.finish()
}

/// 建视频轨（解码器 + H.264 编码器 + 帧率/时基推导）。
fn build_video_lane(
    codecs: &CodecRegistry,
    stream: &Stream,
    video: &VideoEncodeSettings,
) -> Result<VideoLane, ConvertError> {
    let source = stream.codec.video.as_ref().ok_or_else(|| {
        ConvertError::InvalidArguments(format!(
            "视频流 #{} 缺宽高参数（容器未携带），无法配置编码器",
            stream.index
        ))
    })?;
    let (width, height) = video.chain.size.unwrap_or((source.width, source.height));
    let frame_rate = stream_frame_rate(stream).unwrap_or(Rational::RATE_25);
    if frame_rate.num() <= 0 {
        return Err(ConvertError::InvalidArguments(format!(
            "视频流 #{} 帧率非正（{}/{}），无法配置编码器",
            stream.index,
            frame_rate.num(),
            frame_rate.den()
        )));
    }
    // 输出时基 = 1/帧率分子：帧时长 = 帧率分母 tick，整数无舍入
    //（NTSC 等分数帧率下编码器打戳与 muxer 的 dts 衔接校验均精确）。
    let time_base = Rational::new(1, frame_rate.num());
    let config = EncoderConfig {
        codec: CodecId::H264,
        time_base,
        bit_rate: video.bit_rate,
        video: Some(VideoEncoderConfig {
            width,
            height,
            format: PixelFormat::Yuv420p,
            frame_rate,
            gop: video.gop,
            quality: video.quality,
        }),
        audio: None,
    };
    Ok(VideoLane {
        decoder: codecs.create_decoder(&stream.codec)?,
        encoder: codecs.create_encoder(&config)?,
        frame_rate,
        time_base,
        size: (width, height),
        extradata: None,
        out_index: 0,
    })
}

/// 视频源帧率：标称值 > `样例数 ÷ 时长` 精确推导 > `None`（调用方
/// 缺省 25fps）。推导全程整数运算（i64 + gcd 归约），不用 f64。
fn stream_frame_rate(stream: &Stream) -> Option<Rational> {
    if let Some(rate) = stream.codec.video.as_ref().and_then(|v| v.frame_rate)
        && rate.num() > 0
    {
        return Some(rate);
    }
    let (nb, duration) = (stream.nb_frames?, stream.duration?);
    if nb == 0 || duration.ticks() <= 0 {
        return None;
    }
    // fps = nb / duration秒 = nb·tb.den / (ticks·tb.num)
    let tb = duration.time_base();
    let num = i64::try_from(nb).ok()?.checked_mul(i64::from(tb.den()))?;
    let den = duration.ticks().checked_mul(i64::from(tb.num()))?;
    if num <= 0 || den <= 0 {
        return None;
    }
    let g = gcd_u64(num as u64, den as u64);
    let (num, den) = (num / g as i64, den / g as i64);
    Rational::try_new(i32::try_from(num).ok()?, i32::try_from(den).ok()?).ok()
}

/// 欧几里得 gcd（帧率推导的归约分母；输入非零由调用方保证）。
fn gcd_u64(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

/// 由 IDR 包的带内 SPS/PPS 构造 avcC 记录（模块文档「avcC 的取得」）。
/// `pub(crate)`：compose 合成泵（M4）复用同一构造。
pub(crate) fn avcc_from_idr(packet: &Packet) -> Result<Vec<u8>, ConvertError> {
    let data = &packet.data[..];
    let mut sps: Option<Vec<u8>> = None;
    let mut pps: Option<Vec<u8>> = None;
    let mut pos = 0usize;
    while pos + 4 <= data.len() {
        let len =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;
        if pos.checked_add(len).is_none_or(|end| end > data.len()) {
            return Err(ConvertError::InvalidData(
                "视频输出包的 AVCC 长度前缀越界，无法构造 avcC".into(),
            ));
        }
        let nal = &data[pos..pos + len];
        pos += len;
        match nal.first().map(|h| h & 0x1F) {
            Some(7) if sps.is_none() => sps = Some(nal.to_vec()),
            Some(8) if pps.is_none() => pps = Some(nal.to_vec()),
            _ => {}
        }
    }
    let sps =
        sps.ok_or_else(|| ConvertError::InvalidData("IDR 包缺带内 SPS，无法构造 avcC".into()))?;
    let pps =
        pps.ok_or_else(|| ConvertError::InvalidData("IDR 包缺带内 PPS，无法构造 avcC".into()))?;
    if sps.len() < 4 {
        return Err(ConvertError::InvalidData(
            "带内 SPS 过短（不足 profile/compat/level 三字节），无法构造 avcC".into(),
        ));
    }
    let nal_len = |nal: &[u8]| {
        u16::try_from(nal.len())
            .map_err(|_| ConvertError::InvalidData("参数集 NAL 超过 u16 长度域".into()))
    };
    // avcC：与 ye-codec-h264 的 build_avcc 同构——configurationVersion=1、
    // profile/compat/level 取 SPS 头三字节、lengthSizeMinusOne=3（4 字节
    // 前缀）、SPS/PPS 各一条。
    let mut avcc = Vec::with_capacity(sps.len() + pps.len() + 11);
    avcc.extend_from_slice(&[1, sps[1], sps[2], sps[3], 0xFF, 0xE1]);
    avcc.extend_from_slice(&nal_len(&sps)?.to_be_bytes());
    avcc.extend_from_slice(&sps);
    avcc.push(1);
    avcc.extend_from_slice(&nal_len(&pps)?.to_be_bytes());
    avcc.extend_from_slice(&pps);
    Ok(avcc)
}

// ---------------------------------------------------------------------------
// 编码模式的泵内状态（轨 / 头部延迟 / 主循环游标）
// ---------------------------------------------------------------------------

/// 一条输入流的编码模式轨道。
enum Lane {
    /// 视频轨：解码 → 处理链 → H.264 重编码。
    Video(VideoLane),
    /// 音频轨：包直通（重封装，不重编码）。
    Audio(AudioLane),
    /// 字幕等未接轨道：丢弃。
    Dropped,
}

/// 视频轨状态（解码器 + 编码器 + 输出参数）。
struct VideoLane {
    decoder: Box<dyn Decoder>,
    encoder: Box<dyn Encoder>,
    /// 输出帧率（每秒帧数；编码器打戳依据）。
    frame_rate: Rational,
    /// 输出包时基（= 1/帧率分子，见 `build_video_lane`）。
    time_base: Rational,
    /// 输出尺寸（模板的 video 参数来源）。
    size: (u32, u32),
    /// avcC（首包带内 SPS/PPS 构造；`None` = 尚无输出包）。
    extradata: Option<Vec<u8>>,
    /// 输出流号（`write_header` 时分配）。
    out_index: u32,
}

/// AAC-LC 每帧样本数（长窗；编码轨的成帧粒度）。
const AAC_FRAME_SAMPLES: usize = 1024;

/// 建音频重编码轨（解码器 + AAC 编码器 + esds 模板）。
///
/// 采样率/声道沿用源流——无重采样器/混音器（改采样率/声道归后续
/// 批次）；源流缺采样率/声道参数或超出 AAC 能力面（标准 13 档采样率
/// × 单/双声道）→ [`ConvertError::InvalidArguments`]。
fn build_audio_encode_lane(
    codecs: &CodecRegistry,
    stream: &Stream,
    bitrate_bps: u32,
) -> Result<EncodeAudioLane, ConvertError> {
    let audio = stream.codec.audio.as_ref().ok_or_else(|| {
        ConvertError::InvalidArguments(format!(
            "音频流 #{} 缺采样率/声道参数（容器未携带），无法配置 AAC 编码器",
            stream.index
        ))
    })?;
    let sample_rate = audio.sample_rate;
    if sample_rate == 0 {
        return Err(ConvertError::InvalidArguments(format!(
            "音频流 #{} 采样率为 0，无法配置 AAC 编码器",
            stream.index
        )));
    }
    let Some(layout) = audio.channel_layout else {
        return Err(ConvertError::InvalidArguments(format!(
            "音频流 #{} 缺声道布局（容器未携带），无法配置 AAC 编码器",
            stream.index
        )));
    };
    // ChannelLayout 的声道数（u32 → usize 在本库目标平台恒无损；0 声道
    // 由类型不变式排除）。
    let channels = layout.channels() as usize;
    // AAC 访问单元无带内参数集（与视频轨的 avcC 从首 IDR 回收不同），
    // esds 只能按流参数构造（正本归 ye-codec-aac）。
    let channels_u8 = u8::try_from(layout.channels())
        .map_err(|_| ConvertError::InvalidArguments("AAC 声道数超 u8".into()))?;
    let extradata = aac_esds(sample_rate, channels_u8)?;
    // 时基 = 1/采样率：1024 样本帧的时戳整数无舍入。
    let rate = i32::try_from(sample_rate).map_err(|_| {
        ConvertError::InvalidArguments(format!("音频流 #{} 采样率超 i32 时基域", stream.index))
    })?;
    let time_base = Rational::new(1, rate);
    let config = EncoderConfig {
        codec: CodecId::Aac,
        time_base,
        bit_rate: Some(u64::from(bitrate_bps)),
        video: None,
        audio: Some(AudioEncoderConfig {
            sample_rate,
            channel_layout: layout,
            format: SampleFormat::F32,
        }),
    };
    let mut params = CodecParameters::new(CodecId::Aac, MediaType::Audio);
    params.extradata = Some(extradata);
    params.audio = Some(AudioParams {
        sample_rate,
        channel_layout: Some(layout),
        sample_format: None,
    });
    Ok(EncodeAudioLane {
        decoder: codecs.create_decoder(&stream.codec)?,
        encoder: codecs.create_encoder(&config)?,
        template: StreamTemplate {
            media_type: MediaType::Audio,
            codec: params,
            time_base,
            frame_rate: None,
            metadata: Metadata::new(),
        },
        samples: Vec::new(),
        sample_rate,
        layout,
        channels,
        out_index: 0,
    })
}

/// AAC-LC 的 esds 载荷（MP4 `mp4a` stsd 直填形态，含 4 字节
/// version/flags）。构造正本在 `ye-codec-aac`（AudioSpecificConfig 的
/// 编解码器侧知识）；采样率/声道超出能力面 → `InvalidArguments`。
/// `pub(crate)`：compose 合成泵（M4）复用同一构造。
#[cfg(feature = "aac")]
pub(crate) fn aac_esds(sample_rate: u32, channels: u8) -> Result<Vec<u8>, ConvertError> {
    ye_codec_aac::esds_extradata(sample_rate, channels).ok_or_else(|| {
        ConvertError::InvalidArguments(format!(
            "AAC-LC 能力面不含 采样率 {sample_rate}Hz × {channels} 声道（标准 13 档采样率 × 单/双声道）"
        ))
    })
}

/// 无 `aac` feature：AAC 重编码显式不可用（不静默降级成直通）。
/// `pub(crate)`：compose 合成泵（M4）复用。
#[cfg(not(feature = "aac"))]
pub(crate) fn aac_esds(sample_rate: u32, channels: u8) -> Result<Vec<u8>, ConvertError> {
    let _ = (sample_rate, channels);
    Err(ConvertError::InvalidArguments(
        "AAC-LC 重编码需启用 ye 的 aac feature".into(),
    ))
}

/// 音频轨状态：直通（模板 + 平移）或 AAC-LC 重编码（解码 → 编码链）。
enum AudioLane {
    /// 直通：源流编解码参数原样、时基沿用源流（同容器直通时 muxer
    /// 重标度恒等）。
    Copy {
        /// 直通模板。
        template: StreamTemplate,
        /// 该轨整体平移量（首包 dts；`None` = 未遇首包）。
        shift: Option<Timestamp>,
        /// 输出流号（`write_header` 时分配）。
        out_index: u32,
    },
    /// AAC-LC 重编码（解码 packed f32 → 采样累积凑 1024 → 编码）。
    Encode(EncodeAudioLane),
}

/// 音频轨的输出流号（直通/编码两态共用的读取面）。
fn audio_out_index(lane: &AudioLane) -> u32 {
    match lane {
        AudioLane::Copy { out_index, .. } => *out_index,
        AudioLane::Encode(lane) => lane.out_index,
    }
}

/// AAC-LC 重编码轨状态。
struct EncodeAudioLane {
    /// 源流解码器（输入压缩包 → packed f32 音频帧）。
    decoder: Box<dyn Decoder>,
    /// AAC 编码器（1024 样本帧 → 原始访问单元）。
    encoder: Box<dyn Encoder>,
    /// 输出模板（esds extradata 构造即可用；时间线从 0 起算无平移）。
    template: StreamTemplate,
    /// 采样累积缓冲（packed f32 交错；凑满 `AAC_FRAME_SAMPLES` 帧出一帧）。
    samples: Vec<f32>,
    /// 采样率（成帧 `AudioFrame` 的参数面）。
    sample_rate: u32,
    /// 声道布局（成帧 `AudioFrame` 的参数面）。
    layout: ChannelLayout,
    /// 声道数（一帧 = `AAC_FRAME_SAMPLES × channels` 个采样）。
    channels: usize,
    /// 输出流号（`write_header` 时分配）。
    out_index: u32,
}

/// 封装头状态：视频首包未到 → 缓冲音频包；落头后 → 直写。
enum HeaderState {
    /// 头未写：缓冲的音频包（落头后原序冲出）。
    Pending(Vec<Packet>),
    /// 头已写：后续包直写 muxer。
    Written,
}

/// 编码模式的泵内游标（轨表 + muxer + 头部延迟 + 进度）。
struct EncodeRun<'a> {
    /// 轨表（下标 = 输入流索引）。
    lanes: Vec<Lane>,
    muxer: Box<dyn Muxer>,
    header: HeaderState,
    chain: VideoChain,
    pixels: PixelOps,
    processed_us: i64,
    total_us: i64,
    progress: ConvertProgress<'a>,
    video_frames: u64,
    audio_frames: u64,
    /// 输出流数（落头时定）。
    streams_out: usize,
}

impl EncodeRun<'_> {
    /// 主循环分流：视频轨喂解码器并排空，音频轨直通或重编码，其余丢弃。
    fn handle_packet(&mut self, packet: Packet) -> Result<(), ConvertError> {
        let index = usize::try_from(packet.stream_index).unwrap_or(usize::MAX);
        if matches!(self.lanes.get(index), Some(Lane::Video(_))) {
            if let Some(Lane::Video(lane)) = self.lanes.get_mut(index) {
                lane.decoder.feed(&packet)?;
            }
            self.drain_video_decoder(index)?;
        } else if matches!(
            self.lanes.get(index),
            Some(Lane::Audio(AudioLane::Encode(_)))
        ) {
            self.push_audio_encode(index, packet)?;
        } else if matches!(self.lanes.get(index), Some(Lane::Audio(_))) {
            self.push_audio_copy(index, packet)?;
        } // 字幕等未接轨道：丢弃。
        Ok(())
    }

    /// 排空视频解码器：拉出的帧过链编码（NeedInput/Finished 即止）。
    fn drain_video_decoder(&mut self, index: usize) -> Result<(), ConvertError> {
        loop {
            let pulled = match self.lanes.get_mut(index) {
                Some(Lane::Video(lane)) => lane.decoder.pull()?,
                _ => return Ok(()),
            };
            match pulled {
                Decoded::Frame(Frame::Video(frame)) => self.encode_video_frame(index, frame)?,
                // 视频解码器出音频帧是实现违约：容忍忽略（帧模式同款态度）。
                Decoded::Frame(Frame::Audio(_)) => {}
                Decoded::NeedInput | Decoded::Finished => return Ok(()),
            }
        }
    }

    /// 单帧视频：处理链（转换 → scale）→ 强制 yuv420p → 编码 → 排空。
    fn encode_video_frame(&mut self, index: usize, frame: VideoFrame) -> Result<(), ConvertError> {
        let frame = apply_video_chain(self.chain, self.pixels, frame)?;
        let frame = if frame.format() == PixelFormat::Yuv420p {
            frame
        } else {
            // 编码槽只吃 yuv420p：处理后帧仍非 yuv420p 时再过一次转换槽。
            (self.pixels.convert)(&frame, PixelFormat::Yuv420p)?
        };
        if let Some(Lane::Video(lane)) = self.lanes.get_mut(index) {
            lane.encoder.feed(&Frame::Video(frame))?;
        }
        self.video_frames += 1;
        self.drain_encoder(index)
    }

    /// 排空编码器输出队列（编码逐帧同步，常态一帧一包）。
    fn drain_encoder(&mut self, index: usize) -> Result<(), ConvertError> {
        loop {
            let packet = match self.lanes.get_mut(index) {
                Some(Lane::Video(lane)) => match lane.encoder.pull()? {
                    Encoded::Packet(p) => Some(p),
                    Encoded::NeedInput | Encoded::Finished => None,
                },
                _ => None,
            };
            let Some(packet) = packet else { return Ok(()) };
            self.emit_video_packet(index, packet)?;
        }
    }

    /// 落一个视频输出包：首包构造 avcC 并落封装头 → 改流号 → 写包 →
    /// 进度上报。
    fn emit_video_packet(&mut self, index: usize, mut packet: Packet) -> Result<(), ConvertError> {
        if matches!(self.lanes.get(index), Some(Lane::Video(l)) if l.extradata.is_none()) {
            let avcc = avcc_from_idr(&packet)?;
            if let Some(Lane::Video(lane)) = self.lanes.get_mut(index) {
                lane.extradata = Some(avcc);
            }
        }
        if matches!(self.header, HeaderState::Pending(_)) {
            self.open_output_tracks()?;
        }
        let out_index = match self.lanes.get(index) {
            Some(Lane::Video(lane)) => lane.out_index,
            _ => return Ok(()),
        };
        packet.stream_index = out_index;
        self.muxer.write_packet(&packet)?;
        self.report(packet.pts)?;
        Ok(())
    }

    /// 音频包直通：平移到首包 dts = 0 → 落包（计数/进度/缓冲或直写）。
    fn push_audio_copy(&mut self, index: usize, mut packet: Packet) -> Result<(), ConvertError> {
        let Some(dts) = packet.dts else {
            return Err(ConvertError::InvalidData(format!(
                "音频流原索引 {index} 的包缺 dts，直通封装需要它定时间线"
            )));
        };
        let shift = match self.lanes.get_mut(index) {
            Some(Lane::Audio(AudioLane::Copy { shift, .. })) => *shift.get_or_insert(dts),
            _ => return Ok(()),
        };
        packet.dts = Some(dts.checked_sub(shift)?);
        if let Some(pts) = packet.pts {
            packet.pts = Some(pts.checked_sub(shift)?);
        }
        let report_at = packet.dts;
        self.emit_audio_packet(index, packet, report_at)
    }

    /// 音频重编码：包喂解码器 → 排空解码出的音频帧入累积缓冲。
    fn push_audio_encode(&mut self, index: usize, packet: Packet) -> Result<(), ConvertError> {
        if let Some(Lane::Audio(AudioLane::Encode(lane))) = self.lanes.get_mut(index) {
            lane.decoder.feed(&packet)?;
        }
        self.drain_audio_decoder(index)
    }

    /// 排空音频解码器：拉出的帧采样并入累积缓冲（凑满即成帧编码）。
    fn drain_audio_decoder(&mut self, index: usize) -> Result<(), ConvertError> {
        loop {
            let pulled = match self.lanes.get_mut(index) {
                Some(Lane::Audio(AudioLane::Encode(lane))) => lane.decoder.pull()?,
                _ => return Ok(()),
            };
            match pulled {
                Decoded::Frame(Frame::Audio(frame)) => self.accumulate_audio(index, frame)?,
                // 音频解码器出视频帧是实现违约：容忍忽略（视频轨同款态度）。
                Decoded::Frame(Frame::Video(_)) => {}
                Decoded::NeedInput | Decoded::Finished => return Ok(()),
            }
        }
    }

    /// 解码帧的采样并入累积缓冲（packed f32 交错 → f32 流）。
    fn accumulate_audio(&mut self, index: usize, frame: AudioFrame) -> Result<(), ConvertError> {
        if frame.format() != SampleFormat::F32 {
            return Err(ConvertError::InvalidData(format!(
                "音频解码帧须为 packed f32（AAC 重编码链的输入面），收到 {:?}",
                frame.format()
            )));
        }
        let bytes = frame.data();
        if !bytes.len().is_multiple_of(4) {
            return Err(ConvertError::InvalidData(
                "音频解码帧字节数非 4 的倍数（packed f32 破损）".into(),
            ));
        }
        if let Some(Lane::Audio(AudioLane::Encode(lane))) = self.lanes.get_mut(index) {
            lane.samples.extend(
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| f32::from_le_bytes(*c)),
            );
        }
        self.flush_audio_chunks(index)
    }

    /// 累积缓冲凑满 1024 样本即成帧喂编码器并排空输出（可能多帧）。
    fn flush_audio_chunks(&mut self, index: usize) -> Result<(), ConvertError> {
        loop {
            let frame = {
                let Some(Lane::Audio(AudioLane::Encode(lane))) = self.lanes.get_mut(index) else {
                    return Ok(());
                };
                let need = AAC_FRAME_SAMPLES * lane.channels;
                if lane.samples.len() < need {
                    return Ok(());
                }
                let mut frame = AudioFrame::new(
                    SampleFormat::F32,
                    lane.layout,
                    lane.sample_rate,
                    AAC_FRAME_SAMPLES,
                    None,
                )?;
                let data = frame.data_mut();
                for (i, sample) in lane.samples.drain(..need).enumerate() {
                    data[i * 4..i * 4 + 4].copy_from_slice(&sample.to_le_bytes());
                }
                Frame::Audio(frame)
            };
            if let Some(Lane::Audio(AudioLane::Encode(lane))) = self.lanes.get_mut(index) {
                lane.encoder.feed(&frame)?;
            }
            self.drain_audio_encoder(index)?;
        }
    }

    /// 排空音频编码器输出队列（编码逐帧同步，常态一帧一包）。
    fn drain_audio_encoder(&mut self, index: usize) -> Result<(), ConvertError> {
        loop {
            let packet = match self.lanes.get_mut(index) {
                Some(Lane::Audio(AudioLane::Encode(lane))) => match lane.encoder.pull()? {
                    Encoded::Packet(p) => Some(p),
                    Encoded::NeedInput | Encoded::Finished => None,
                },
                _ => None,
            };
            let Some(packet) = packet else { return Ok(()) };
            let pts = packet.pts;
            self.emit_audio_packet(index, packet, pts)?;
        }
    }

    /// 落一个音频输出包：计数 → 进度 → 缓冲（头未落；按**输入流索引**
    /// 入缓冲，输出流号在落头冲出时才分配改写）或直写。
    fn emit_audio_packet(
        &mut self,
        index: usize,
        mut packet: Packet,
        report_at: Option<Timestamp>,
    ) -> Result<(), ConvertError> {
        self.audio_frames += 1;
        self.report(report_at)?;
        if let HeaderState::Pending(buffer) = &mut self.header {
            // 头未落（视频首包未到）：缓冲，落头后原序冲出。缓冲包统一带
            // **输入流索引**——直通包天然如此；编码包自带 0（Encoder 契约
            // stream_index 恒 0），须改写回输入轨位，否则剥视频（音轨输入
            // 索引 ≠ 0）时冲出循环按 0 回查会命中被剥轨、静默丢包
            // （2026-10-26 M3 补齐 e2e 实测回归）。
            if let Ok(input_index) = u32::try_from(index) {
                packet.stream_index = input_index;
            }
            buffer.push(packet);
            return Ok(());
        }
        let out_index = match self.lanes.get(index) {
            Some(Lane::Audio(lane)) => audio_out_index(lane),
            _ => return Ok(()),
        };
        packet.stream_index = out_index;
        self.muxer.write_packet(&packet)?;
        Ok(())
    }

    /// 落封装头：按输入流序组装模板（视频轨须已有 avcC，无输出包的
    /// 视频轨跳过；音频轨恒在——0 包轨合法），分配输出流号，冲出
    /// 缓冲的音频包。
    fn open_output_tracks(&mut self) -> Result<(), ConvertError> {
        let HeaderState::Pending(buffer) =
            std::mem::replace(&mut self.header, HeaderState::Written)
        else {
            return Ok(()); // 已落头：幂等。
        };
        let mut templates = Vec::new();
        let mut next_index = 0u32;
        for lane in &mut self.lanes {
            match lane {
                Lane::Video(lane) if lane.extradata.is_some() => {
                    lane.out_index = next_index;
                    next_index += 1;
                    let mut params = CodecParameters::new(CodecId::H264, MediaType::Video);
                    params.extradata = lane.extradata.clone();
                    params.video = Some(VideoParams {
                        width: lane.size.0,
                        height: lane.size.1,
                        pixel_format: Some(PixelFormat::Yuv420p),
                        frame_rate: Some(lane.frame_rate),
                    });
                    templates.push(StreamTemplate {
                        media_type: MediaType::Video,
                        codec: params,
                        time_base: lane.time_base,
                        frame_rate: Some(lane.frame_rate),
                        metadata: Metadata::new(),
                    });
                }
                Lane::Audio(lane) => {
                    let template = match lane {
                        AudioLane::Copy {
                            template,
                            out_index,
                            ..
                        } => {
                            *out_index = next_index;
                            template.clone()
                        }
                        AudioLane::Encode(encode) => {
                            encode.out_index = next_index;
                            encode.template.clone()
                        }
                    };
                    next_index += 1;
                    templates.push(template);
                }
                Lane::Video(_) | Lane::Dropped => {}
            }
        }
        self.streams_out = templates.len();
        self.muxer.write_header(&templates)?;
        for mut packet in buffer {
            // 缓冲包的 stream_index 还是输入流索引：改写到该轨的输出流号。
            let index = usize::try_from(packet.stream_index).unwrap_or(usize::MAX);
            if let Some(Lane::Audio(lane)) = self.lanes.get(index) {
                packet.stream_index = audio_out_index(lane);
                self.muxer.write_packet(&packet)?;
            }
        }
        Ok(())
    }

    /// 收尾：逐轨排空（视频/音频解码器 flush → 帧过链编码 → 编码器
    /// flush；音频重编码轨先补零成帧）、头仍未落则按音频-only 落头、
    /// 写容器尾、汇总统计。
    fn finish(&mut self) -> Result<ConvertStats, ConvertError> {
        for index in 0..self.lanes.len() {
            match self.lanes.get_mut(index) {
                Some(Lane::Video(lane)) => lane.decoder.flush(),
                Some(Lane::Audio(AudioLane::Encode(lane))) => lane.decoder.flush(),
                _ => {}
            }
            self.drain_video_decoder(index)?;
            self.drain_audio_decoder(index)?;
            self.flush_audio_padding(index)?;
            if let Some(Lane::Video(lane)) = self.lanes.get_mut(index) {
                lane.encoder.flush();
            }
            if let Some(Lane::Audio(AudioLane::Encode(lane))) = self.lanes.get_mut(index) {
                lane.encoder.flush();
            }
            self.drain_encoder(index)?;
            self.drain_audio_encoder(index)?;
        }
        // 无视频输出包（纯音频输入或零帧视频轨）：音频-only 模板落头。
        if matches!(self.header, HeaderState::Pending(_)) {
            self.open_output_tracks()?;
        }
        self.muxer.write_trailer()?;
        Ok(ConvertStats {
            streams: self.streams_out,
            video_frames: self.video_frames,
            audio_frames: self.audio_frames,
            processed_us: self.processed_us,
            total_us: self.total_us,
        })
    }

    /// 末尾补帧：重编码轨累积缓冲有余量采样（< 1024×channels）→ 补零
    /// 到一帧出包（时长对齐源流的最后一拍；空缓冲不动）。
    fn flush_audio_padding(&mut self, index: usize) -> Result<(), ConvertError> {
        let padded = match self.lanes.get_mut(index) {
            Some(Lane::Audio(AudioLane::Encode(lane))) => {
                let need = AAC_FRAME_SAMPLES * lane.channels;
                if lane.samples.is_empty() {
                    false
                } else {
                    lane.samples.resize(need, 0.0);
                    true
                }
            }
            _ => false,
        };
        if padded {
            self.flush_audio_chunks(index)?;
        }
        Ok(())
    }

    /// 进度上报：时戳（自带时基）换 µs 取单调最大值后回调。
    fn report(&mut self, pts: Option<Timestamp>) -> Result<(), ConvertError> {
        if let Some(pts) = pts {
            let us = pts.rescale(MICRO)?.ticks();
            self.processed_us = self.processed_us.max(us);
        }
        (self.progress)(self.processed_us, self.total_us);
        Ok(())
    }
}

/// 固定链序：转换槽 → scale 槽（no-op 跳过）。帧模式与编码模式共用。
fn apply_video_chain(
    chain: VideoChain,
    pixels: PixelOps,
    video: VideoFrame,
) -> Result<VideoFrame, ConvertError> {
    let mut video = video;
    if let (Some(target), format) = (chain.format, video.format())
        && target != format
    {
        video = (pixels.convert)(&video, target)?;
    }
    if let Some((width, height)) = chain.size
        && (width, height) != (video.width(), video.height())
    {
        video = (pixels.scale)(&video, width, height)?;
    }
    Ok(video)
}

/// 单帧过链 + 进度上报的帧模式游标。
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
            Frame::Video(video) => Frame::Video(apply_video_chain(self.chain, self.pixels, video)?),
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
}

// ---------------------------------------------------------------------------
// ye-filter 核桥接（像素槽的缺省核，见模块文档「像素核槽」）
// ---------------------------------------------------------------------------

/// 取帧的 yuv420p 三平面视图（跨距直连解码帧平面）。
fn yuv_view(frame: &VideoFrame) -> Result<Yuv420pView<'_>, ConvertError> {
    if frame.format() != PixelFormat::Yuv420p {
        return Err(ConvertError::InvalidArguments(format!(
            "yuv420p 视图要求 yuv420p 帧，收到 {}",
            frame.format().name()
        )));
    }
    let layout = frame.layout();
    Ok(Yuv420pView {
        y: frame.plane(0),
        y_stride: layout.plane(0).stride,
        u: frame.plane(1),
        u_stride: layout.plane(1).stride,
        v: frame.plane(2),
        v_stride: layout.plane(2).stride,
    })
}

/// 帧色彩范围 → 核的量化范围（缺省 Limited，对齐核文档约定）。
fn yuv_range_of(frame: &VideoFrame) -> YuvRange {
    match frame.color().range {
        ColorRange::Jpeg => YuvRange::Full,
        ColorRange::Mpeg | ColorRange::Unspecified => YuvRange::Limited,
    }
}

/// packed 帧（单平面）的紧致字节（逐行取有效宽，去行尾对齐 padding）。
fn packed_tight(frame: &VideoFrame) -> Vec<u8> {
    let bpp = frame.format().component_count() as usize;
    let (w, h) = (frame.width() as usize, frame.height() as usize);
    let stride = frame.layout().plane(0).stride;
    let mut out = Vec::with_capacity(w * h * bpp);
    for y in 0..h {
        out.extend_from_slice(&frame.plane(0)[y * stride..y * stride + w * bpp]);
    }
    out
}

/// 紧致 packed 字节 → 帧（pts/duration/color 透传自 `like`）。
fn packed_frame_from_tight(
    format: PixelFormat,
    width: u32,
    height: u32,
    tight: &[u8],
    like: &VideoFrame,
) -> Result<VideoFrame, ConvertError> {
    let bpp = format.component_count() as usize;
    let mut out = VideoFrame::new(
        format,
        width,
        height,
        like.color(),
        like.pts(),
        like.duration(),
    )?;
    let stride = out.layout().plane(0).stride;
    let row = width as usize * bpp;
    for y in 0..height as usize {
        out.plane_mut(0)[y * stride..y * stride + row]
            .copy_from_slice(&tight[y * row..y * row + row]);
    }
    Ok(out)
}

/// 紧凑 yuv420p 缓冲 → 帧（pts/duration/color 透传自 `like`）。
fn yuv_frame_from_buffer(
    buf: &Yuv420pBuffer,
    like: &VideoFrame,
) -> Result<VideoFrame, ConvertError> {
    let mut out = VideoFrame::new(
        PixelFormat::Yuv420p,
        u32::try_from(buf.width())
            .map_err(|_| ConvertError::InvalidArguments("帧宽超 u32".into()))?,
        u32::try_from(buf.height())
            .map_err(|_| ConvertError::InvalidArguments("帧高超 u32".into()))?,
        like.color(),
        like.pts(),
        like.duration(),
    )?;
    let (w, h) = (buf.width(), buf.height());
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let strides = (
        out.layout().plane(0).stride,
        out.layout().plane(1).stride,
        out.layout().plane(2).stride,
    );
    for y in 0..h {
        out.plane_mut(0)[y * strides.0..y * strides.0 + w]
            .copy_from_slice(&buf.y()[y * w..y * w + w]);
    }
    for y in 0..ch {
        out.plane_mut(1)[y * strides.1..y * strides.1 + cw]
            .copy_from_slice(&buf.u()[y * cw..y * cw + cw]);
        out.plane_mut(2)[y * strides.2..y * strides.2 + cw]
            .copy_from_slice(&buf.v()[y * cw..y * cw + cw]);
    }
    Ok(out)
}

/// 核错误 → 槽错误（文案透传）。
fn filter_err(err: FilterKernelError) -> ConvertError {
    ConvertError::InvalidArguments(err.to_string())
}

/// 像素转换槽的 ye-filter 核桥接：yuv420p ↔ rgba8/rgb24
/// （BT.601 表模型，对齐 swscale 逐像素路径，见核文档）。
///
/// # Errors
/// 源/目标格式不在核能力面（yuv420p ↔ rgba8/rgb24）→
/// [`ConvertError::InvalidArguments`]；平面切片过短等核侧错误同上透传。
pub fn filter_convert(frame: &VideoFrame, target: PixelFormat) -> Result<VideoFrame, ConvertError> {
    let (w, h) = (frame.width() as usize, frame.height() as usize);
    let range = yuv_range_of(frame);
    match (frame.format(), target) {
        (PixelFormat::Yuv420p, PixelFormat::Rgba8) => {
            let view = yuv_view(frame)?;
            let matrix = match frame.color().space {
                ye_pixfmt::ColorSpace::Bt709 => ye_filter::YuvMatrix::Bt709,
                ye_pixfmt::ColorSpace::Unspecified
                | ye_pixfmt::ColorSpace::Unknown(5)
                | ye_pixfmt::ColorSpace::Smpte170m => ye_filter::YuvMatrix::Bt601,
                other => {
                    return Err(ConvertError::InvalidArguments(format!(
                        "unsupported color matrix {other:?}"
                    )));
                }
            };
            let tight = ye_filter::yuv420p_to_rgba_matrix(&view, w, h, range, matrix)
                .map_err(filter_err)?;
            packed_frame_from_tight(target, frame.width(), frame.height(), &tight, frame)
        }
        (PixelFormat::Yuv420p, PixelFormat::Rgb24) => {
            let view = yuv_view(frame)?;
            let tight = ye_filter::yuv420p_to_rgb24(&view, w, h, range).map_err(filter_err)?;
            packed_frame_from_tight(target, frame.width(), frame.height(), &tight, frame)
        }
        (PixelFormat::Rgba8, PixelFormat::Yuv420p) => {
            let tight = packed_tight(frame);
            let buf = ye_filter::rgba_to_yuv420p(&tight, w, h, range).map_err(filter_err)?;
            yuv_frame_from_buffer(&buf, frame)
        }
        (PixelFormat::Rgb24, PixelFormat::Yuv420p) => {
            let tight = packed_tight(frame);
            let buf = ye_filter::rgb24_to_yuv420p(&tight, w, h, range).map_err(filter_err)?;
            yuv_frame_from_buffer(&buf, frame)
        }
        (source, target) => Err(ConvertError::InvalidArguments(format!(
            "filter_convert 仅支持 yuv420p ↔ rgba8/rgb24，收到 {} → {}",
            source.name(),
            target.name()
        ))),
    }
}

/// scale 槽的 ye-filter 核桥接：yuv420p（swscale 双线性语义）与 rgba8
/// （教科书双线性）两个域。
///
/// # Errors
/// 帧格式不在核能力面（yuv420p/rgba8）或源/目标维非偶（yuv420p 平面
/// 几何）→ [`ConvertError::InvalidArguments`]。
pub fn filter_scale(
    frame: &VideoFrame,
    width: u32,
    height: u32,
) -> Result<VideoFrame, ConvertError> {
    match frame.format() {
        PixelFormat::Yuv420p => {
            let view = yuv_view(frame)?;
            let buf = ye_filter::scale_yuv420p(
                &view,
                frame.width() as usize,
                frame.height() as usize,
                width as usize,
                height as usize,
            )
            .map_err(filter_err)?;
            yuv_frame_from_buffer(&buf, frame)
        }
        PixelFormat::Rgba8 => {
            let tight = packed_tight(frame);
            let out = ye_filter::scale_rgba(
                &tight,
                frame.width() as usize,
                frame.height() as usize,
                width as usize,
                height as usize,
            )
            .map_err(filter_err)?;
            packed_frame_from_tight(PixelFormat::Rgba8, width, height, &out, frame)
        }
        format => Err(ConvertError::InvalidArguments(format!(
            "filter_scale 仅支持 yuv420p/rgba8 域，收到 {}",
            format.name()
        ))),
    }
}

/// 缺省像素核槽：ye-filter 核的两桥接组装（CLI/锻接入的便利入口）。
#[must_use]
pub fn filter_ops() -> PixelOps {
    PixelOps {
        convert: filter_convert,
        scale: filter_scale,
    }
}

/// convert 泵（本模块）全部失败模式的枚举。
///
/// ## 变体语义
///
/// - [`Cancelled`](Self::Cancelled)：取消旗标置位（调用方请求取消，
///   非失败——调用方据此前进/清理）；
/// - [`Sink`](Self::Sink)：帧消费回调失败（消费方错误原样回传终止泵）；
/// - [`InvalidArguments`](Self::InvalidArguments)：槽/链参数非法（如目标
///   格式与槽能力不符——由核侧透传填充）；
/// - [`InvalidData`](Self::InvalidData)：数据驱动失败（如音频直通包缺
///   dts、输出包形态与编码器契约不符）；
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
    /// 帧消费回调返回错误（携带其文案）。
    Sink(String),
    /// 槽/链参数非法（像素核透传），携带描述。
    InvalidArguments(String),
    /// 数据驱动失败（包/帧形态与契约不符），携带描述。
    InvalidData(String),
    /// 下层 ye-container 错误（打开/读头/读包/封装）。
    Container(ContainerError),
    /// 下层 ye-codec 错误（解码/编码器创建/feed/pull）。
    Codec(ye_codec::CodecError),
    /// 下层 ye-time 错误（进度 µs 换算等）。
    Time(TimeError),
}

impl core::fmt::Display for ConvertError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ConvertError::Cancelled => f.write_str("cancelled"),
            ConvertError::Sink(msg) => write!(f, "frame sink error: {msg}"),
            ConvertError::InvalidArguments(msg) => write!(f, "invalid arguments: {msg}"),
            ConvertError::InvalidData(msg) => write!(f, "invalid data: {msg}"),
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

// ---------------------------------------------------------------------------
// 单元测试（纯函数件：设置缺省 / 帧率推导 / avcC 构造）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use ye_codec::PacketFlags;
    use ye_pixfmt::ColorDesc;

    /// 缺省设置：no-op 链 + GOP 250 + 简单 CRF 23（无码控）。
    #[test]
    fn encode_settings_default() {
        let s = VideoEncodeSettings::default();
        assert_eq!(
            s.chain,
            VideoChain {
                format: None,
                size: None
            }
        );
        assert_eq!(s.gop, 250);
        assert_eq!(s.quality, QualityMode::Crf(23));
        assert_eq!(s.bit_rate, None, "缺省无码控（纯质量模式）");
    }

    /// 音频设置缺省：直通（重封装不重编码）。
    #[test]
    fn audio_settings_default_is_copy() {
        assert_eq!(AudioEncodeSettings::default(), AudioEncodeSettings::Copy);
    }

    /// 帧率推导：标称值优先；缺则 样例数÷时长 精确归约；劣化输入 None。
    #[test]
    fn frame_rate_derivation() {
        // ① 标称值优先（即使与样例数/时长口径不同）。
        let mut stream = nominal_stream(Some(Rational::new(30, 1)), 45, 46_080, 15_360);
        assert_eq!(stream_frame_rate(&stream), Some(Rational::new(30, 1)));

        // ② 45 样例 / 46080 tick @ 1/15360 → 691200/46080 → 15/1。
        stream = nominal_stream(None, 45, 46_080, 15_360);
        assert_eq!(stream_frame_rate(&stream), Some(Rational::new(15, 1)));

        // ③ NTSC 分数率：24000 样例 / 1001 s（1001000 tick @ 1/1000）→
        //    24000·1000/1001000 → 归约 → 24000/1001。
        stream = nominal_stream(None, 24_000, 1_001_000, 1000);
        assert_eq!(
            stream_frame_rate(&stream),
            Some(Rational::new(24_000, 1001))
        );

        // ④ 劣化输入：缺样例数 / 零时长 → None；零标称帧率回退到推导
        //    （45 样例 / 100 tick @ 1/90 → 4050/100 → 81/2）。
        assert_eq!(stream_frame_rate(&nominal_stream(None, 0, 100, 90)), None);
        assert_eq!(stream_frame_rate(&nominal_stream(None, 45, 0, 90)), None);
        assert_eq!(
            stream_frame_rate(&nominal_stream(Some(Rational::new(0, 1)), 45, 100, 90)),
            Some(Rational::new(81, 2))
        );
    }

    /// 造一路视频流参数（标称帧率 / 样例数 / 时长 tick / 时基分母）。
    fn nominal_stream(
        rate: Option<Rational>,
        nb: u64,
        duration_ticks: i64,
        timescale: i32,
    ) -> Stream {
        let mut codec = CodecParameters::new(CodecId::H264, MediaType::Video);
        codec.video = Some(VideoParams {
            width: 176,
            height: 144,
            pixel_format: Some(PixelFormat::Yuv420p),
            frame_rate: rate,
        });
        Stream {
            index: 0,
            media_type: MediaType::Video,
            codec,
            time_base: Rational::new(1, timescale),
            start_time: None,
            duration: Some(Timestamp::new(duration_ticks, Rational::new(1, timescale))),
            nb_frames: Some(nb),
            metadata: Metadata::new(),
        }
    }

    /// AVCC 追加辅助（4 字节大端长度前缀 + NAL）。
    fn avcc_nal(out: &mut Vec<u8>, nal: &[u8]) {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }

    /// IDR 包 → avcC：SPS 头三字节进记录、lengthSizeMinusOne=3、
    /// SPS/PPS 各一条。
    #[test]
    fn avcc_from_idr_packet() {
        let sps = [0x67u8, 0x42, 0xC0, 0x1E, 0xAB, 0xCD];
        let pps = [0x68u8, 0xCE, 0x01];
        let idr = [0x65u8, 0x88, 0x84];
        let mut data = Vec::new();
        avcc_nal(&mut data, &sps);
        avcc_nal(&mut data, &pps);
        avcc_nal(&mut data, &idr);
        let mut packet = Packet::empty(0);
        packet.data = Arc::from(data.into_boxed_slice());
        packet.flags.insert(PacketFlags::KEYFRAME);

        let avcc = avcc_from_idr(&packet).expect("avcC 构造");
        let mut want = vec![1, 0x42, 0xC0, 0x1E, 0xFF, 0xE1];
        want.extend_from_slice(&u16::try_from(sps.len()).unwrap().to_be_bytes());
        want.extend_from_slice(&sps);
        want.push(1);
        want.extend_from_slice(&u16::try_from(pps.len()).unwrap().to_be_bytes());
        want.extend_from_slice(&pps);
        assert_eq!(avcc, want);
    }

    /// avcC 失败面：缺 SPS / 缺 PPS / 前缀越界 / SPS 过短。
    #[test]
    fn avcc_from_idr_rejects_malformed() {
        let mut packet = Packet::empty(0);
        // 仅 PPS + IDR：缺 SPS。
        let mut data = Vec::new();
        avcc_nal(&mut data, &[0x68, 0xCE]);
        avcc_nal(&mut data, &[0x65, 0x88]);
        packet.data = Arc::from(data.into_boxed_slice());
        assert!(matches!(
            avcc_from_idr(&packet),
            Err(ConvertError::InvalidData(_))
        ));

        // 仅 SPS + IDR：缺 PPS。
        let mut data = Vec::new();
        avcc_nal(&mut data, &[0x67, 0x42, 0xC0, 0x1E]);
        avcc_nal(&mut data, &[0x65, 0x88]);
        packet.data = Arc::from(data.into_boxed_slice());
        assert!(matches!(
            avcc_from_idr(&packet),
            Err(ConvertError::InvalidData(_))
        ));

        // 长度前缀越界（声明长度超出包体）。
        packet.data = Arc::from(vec![0, 0, 0, 9, 0x67, 0x42].into_boxed_slice());
        assert!(matches!(
            avcc_from_idr(&packet),
            Err(ConvertError::InvalidData(_))
        ));

        // SPS 过短（不足 profile/compat/level 三字节）。
        let mut data = Vec::new();
        avcc_nal(&mut data, &[0x67, 0x42]);
        avcc_nal(&mut data, &[0x68, 0xCE]);
        packet.data = Arc::from(data.into_boxed_slice());
        assert!(matches!(
            avcc_from_idr(&packet),
            Err(ConvertError::InvalidData(_))
        ));
    }

    /// filter_scale 同尺寸恒等：yuv420p 域逐字节复制语义 + 元数据透传。
    #[test]
    fn filter_scale_identity_yuv420p() {
        let (w, h) = (64u32, 48u32);
        let mut frame = VideoFrame::new(
            PixelFormat::Yuv420p,
            w,
            h,
            ColorDesc::UNSPECIFIED,
            Some(Timestamp::new(7, Rational::new(1, 25))),
            Some(Timestamp::new(1, Rational::new(1, 25))),
        )
        .unwrap();
        for plane in 0..3 {
            for byte in frame.plane_mut(plane).iter_mut() {
                *byte = (plane * 51 + 17) as u8;
            }
        }
        let scaled = filter_scale(&frame, w, h).expect("同尺寸缩放");
        assert_eq!((scaled.width(), scaled.height()), (w, h));
        assert_eq!(scaled.pts(), frame.pts());
        assert_eq!(scaled.duration(), frame.duration());
        // 同尺寸：三平面**有效行内容**逐字节一致（plane 含行尾对齐
        // padding，只比有效宽：luma w、chroma w/2）。
        let plane_valid = |f: &VideoFrame, plane: usize| {
            let cols = if plane == 0 {
                w as usize
            } else {
                w as usize / 2
            };
            let rows = if plane == 0 {
                h as usize
            } else {
                h as usize / 2
            };
            let stride = f.layout().plane(plane).stride;
            (0..rows)
                .flat_map(|y| f.plane(plane)[y * stride..y * stride + cols].to_vec())
                .collect::<Vec<_>>()
        };
        for plane in 0..3 {
            assert_eq!(
                plane_valid(&scaled, plane),
                plane_valid(&frame, plane),
                "plane {plane}"
            );
        }
    }

    /// filter_scale 尺寸缩放：跨距重排后行内容一致（176x144 → 320x240）。
    #[test]
    fn filter_scale_upscale_dimensions() {
        let mut frame = VideoFrame::new(
            PixelFormat::Yuv420p,
            176,
            144,
            ColorDesc::UNSPECIFIED,
            None,
            None,
        )
        .unwrap();
        for (i, byte) in frame.plane_mut(0).iter_mut().enumerate() {
            *byte = (i % 251) as u8;
        }
        let scaled = filter_scale(&frame, 320, 240).expect("放大");
        assert_eq!((scaled.width(), scaled.height()), (320, 240));
        assert_eq!(scaled.format(), PixelFormat::Yuv420p);
        // 有内容（非全零）。
        assert!(scaled.plane(0).iter().any(|&b| b != 0));
    }
}
