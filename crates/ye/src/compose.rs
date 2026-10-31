//! compose 合成泵（M4 首轮：**顺序拼接**）：有序片段列表（每段一个 MP4
//! 输入 + 源内裁剪窗口）→ 单个 MP4——视频 H.264 重编码、音频 AAC-LC
//! 重编码（或无音轨）。段间硬接（无转场无叠化），时间线连续从 0。
//!
//! ## 设计裁决（M4 首轮定形）
//!
//! - **逐段校验**：每段须恰一路视频流（缺 → [`ConvertError::InvalidArguments`]
//!   指明段号；多路同拒——多视频流编排归后续批次）；`audio = Aac` 时须恰
//!   一路音频流（缺/多路同拒——段级静音填充归后续批次），且采样率/声道
//!   与首段一致、在 AAC 能力面内（标准采样率 × 单/双声道），违者
//!   `InvalidArguments` 指明段号与参数——**不做重采样/混音**（归后续
//!   批次）；`audio = Copy` 显式拒绝（跨段参数一致性不保，重编码统一，
//!   直通拼接归后续批次）；`audio = Drop` 剥音频。段号一律 1 起报。
//! - **视频 CFR 桶采样**（[`CfrSampler`]）：解码帧先过链（scale 到输出
//!   画幅 → 强制 yuv420p，与现状相同的环跳过），再按输出帧率把段窗口
//!   映射到连续的输出 tick：tick 数 = ⌈窗口时长 × 帧率⌉（窗口含头不含
//!   尾），每 tick 取「该 tick 时刻前（pts ≤ tick 时刻）最近的已解码帧」
//!   （hold-last——空 tick 补前一帧），段首 tick 前无帧则取段内首帧；
//!   随后喂**全局唯一**的 H.264 编码器——输出 pts 按帧号全局连续从 0
//!   起（跨段不重置，编码器打戳契约同 convert）。窗口内一帧都没有 →
//!   `InvalidArguments`（显式拒绝，不产黑帧）。帧假定按 pts 升序到达
//!   （H.264 解码面无 B 帧重排）；窗口比较与时戳换算一律在 µs 域
//!   （`Timestamp::rescale` NearInf 舍入，±1µs——与显式 µs 窗口参数自洽）。
//! - **音频连续累积**：各段解码 f32 采样按窗口**逐采样**过滤（窗口含头
//!   不含尾，µs × 采样率 域整数比较）后跨段连续累积，凑 1024 出帧喂
//!   **全局唯一**的 AAC 编码器（esds 经 [`crate::convert::aac_esds`]
//!   构造）；末段收尾余量补零成一帧（时长对齐最后一拍）。输出时戳按
//!   帧号从 0 递推。
//! - **avcC 封装头延迟**：复用 convert 的「封装头延迟」套路——MP4 stsd
//!   模板要 avcC/esds，avcC 经 [`crate::convert::avcc_from_idr`] 从首个
//!   IDR 包的带内 SPS/PPS 回收，封装头延迟到首个视频包产出时才落；此前
//!   的音频包先缓冲、落头后原序冲出（`HeaderState` 与 convert.rs 同构；
//!   输出流号恒定：视频 0、音频 1）。
//! - **进度/取消**：与 convert 同契约——逐输出包上报
//!   `(processed_us, total_us)`（µs；processed 单调不减、跨流取最大；
//!   total = Σ 各段窗口时长），[`AtomicBool`] 每输入包检查点（入口与
//!   逐段亦查），置位 → [`ConvertError::Cancelled`]。半成品清理归调用方
//!   （泵持有 `WriteSeek` 抽象、无路径概念）。
//! - **统计**：[`ConvertStats`] 对齐 convert 口径——`video_frames` = 总编码
//!   帧数（= Σ 各段 tick 数）、`audio_frames` = 总输出音频包数、`streams`
//!   = 输出流数（1–2）、`total_us` = Σ 窗口时长。
//! - **注册表注入**：与 convert 同款——解码/编码/封装器均经注入的注册表
//!   取得，泵本体不依赖具体实现 crate（无 feature 门；`aac` feature 缺席
//!   时 `Aac` 设置经 [`crate::convert::aac_esds`] 显式报错，不静默降级）。
//! - **逐段顺序解封装**：不做 seek 快进——窗口外帧解码后丢弃（seek 优化
//!   归后续批次）；字幕等其余轨道丢弃。
//!
//! ## 全库约定
//!
//! 同 convert：零第三方依赖（仅 std + workspace crate）、不含 unsafe、
//! 数据驱动失败一律 `Result`、无 panic 路径。

use std::sync::atomic::{AtomicBool, Ordering};

use ye_codec::{
    AudioEncoderConfig, AudioFrame, CodecId, CodecParameters, CodecRegistry, Decoded, Encoded,
    Encoder, EncoderConfig, Frame, MediaType, Packet, QualityMode, VideoFrame, VideoParams,
};
use ye_container::{
    ContainerRegistry, Metadata, Muxer, ReadSeek, Stream, StreamTemplate, WriteSeek,
};
use ye_pixfmt::{ChannelLayout, PixelFormat, SampleFormat};
use ye_time::{Rational, Timestamp};

use crate::convert::{AudioEncodeSettings, ConvertError, ConvertProgress, ConvertStats, PixelOps};

/// 微秒时基（窗口参数与进度报告的公共域）。
const MICRO: Rational = Rational::new(1, 1_000_000);

/// AAC-LC 每帧样本数（长窗；音频累积的成帧粒度）。
const AAC_FRAME_SAMPLES: usize = 1024;

/// i128 整数上取整除法（`num ≥ 0`、`den > 0` 由调用方保证）。
fn ceil_div(num: i128, den: i128) -> i128 {
    (num + den - 1) / den
}

/// 合成片段：一个 MP4 输入 + 源内裁剪窗口（µs，含头不含尾）。
///
/// derive 清单：不 derive（`Box<dyn ReadSeek>` 非 `Debug`/`Clone`）。
pub struct ComposeSegment {
    /// 片段输入（按值消费，auto-probe 打开）。
    pub input: Box<dyn ReadSeek>,
    /// 窗口起点（源时间，µs，≥ 0）。
    pub trim_in_us: i64,
    /// 窗口终点（源时间，µs，> `trim_in_us`）。
    pub trim_out_us: i64,
}

/// 合成设置：输出画幅/帧率 + 视频质量面 + 音频三态（M4 首轮收窄）。
///
/// `audio` 仅接受 [`AudioEncodeSettings::Aac`]（重编码统一）与
/// [`AudioEncodeSettings::Drop`]（剥轨）——`Copy` 显式拒绝（跨段参数一致
/// 性不保）。输出画幅须非零偶数（H.264 4:2:0 编码器要求），帧率须为正。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`（`QualityMode`/
/// `AudioEncodeSettings` 均纯值载荷）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeSettings {
    /// 输出画幅宽（非零偶数）。
    pub width: u32,
    /// 输出画幅高（非零偶数）。
    pub height: u32,
    /// 输出恒定帧率（CFR，桶采样的 tick 间隔依据）。
    pub frame_rate: Rational,
    /// GOP（IDR 间隔，帧数）：1 = 全 I 帧，0 非法（编码器工厂拒收）。
    pub gop: u32,
    /// 质量模式（恒定 QP / 简单 CRF，值域 1–51）。
    pub quality: QualityMode,
    /// 目标码率（bit/s；`None` = 纯质量模式，语义同 convert）。
    pub bit_rate: Option<u64>,
    /// 音频设置：`Aac { bitrate_bps }` 重编码 / `Drop` 剥轨（`Copy` 拒绝）。
    pub audio: AudioEncodeSettings,
}

/// 顺序拼接合成：多段 MP4 → 单个 MP4（行为契约见模块文档）。
///
/// # Errors
///
/// 见 [`ConvertError`] 各变体：参数/逐段校验失败 → `InvalidArguments`
/// （指明段号）；数据驱动失败（帧缺 pts、包形态违约）→ `InvalidData`；
/// 下层解封装/解码/编码/封装错误原样包装透出；取消 → `Cancelled`。
// 9 个参数（超 clippy 默认上限）：与 `convert` 同理——注册表 ×2 / 片段
// 列表 / 输出格式名 / 输出 / 设置 / 像素槽 / 进度 / 取消各占一席即消费方
// （剪 M4 导出轨）的参数形态；输出格式名单列保持泵对容器注册表中立。
#[allow(clippy::too_many_arguments)]
pub fn compose(
    containers: &ContainerRegistry,
    codecs: &CodecRegistry,
    segments: Vec<ComposeSegment>,
    output_format: &str,
    output: Box<dyn WriteSeek>,
    settings: &ComposeSettings,
    pixels: PixelOps,
    progress: ConvertProgress<'_>,
    cancel: &AtomicBool,
) -> Result<ConvertStats, ConvertError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(ConvertError::Cancelled);
    }
    validate_settings(settings)?;
    if segments.is_empty() {
        return Err(ConvertError::InvalidArguments(
            "片段列表为空，无从合成".into(),
        ));
    }
    for (index, segment) in segments.iter().enumerate() {
        check_window(index, segment.trim_in_us, segment.trim_out_us)?;
    }

    // 全局唯一视频编码器：输出时基 = 1/帧率分子（帧时长 = 帧率分母 tick，
    // 整数无舍入——与 convert 的 build_video_lane 同款裁决）。
    let frame_rate = settings.frame_rate;
    let time_base = Rational::new(1, frame_rate.num());
    let config = EncoderConfig {
        codec: CodecId::H264,
        time_base,
        bit_rate: settings.bit_rate,
        video: Some(ye_codec::VideoEncoderConfig {
            width: settings.width,
            height: settings.height,
            format: PixelFormat::Yuv420p,
            frame_rate,
            gop: settings.gop,
            quality: settings.quality,
        }),
        audio: None,
    };
    let video_encoder = codecs.create_encoder(&config)?;

    let total_us: i64 = segments
        .iter()
        .map(|s| s.trim_out_us - s.trim_in_us)
        .try_fold(0i64, |acc, w| acc.checked_add(w))
        .ok_or_else(|| ConvertError::InvalidArguments("Σ窗口时长溢出 i64".into()))?;

    let muxer = containers.create_muxer(output_format, output)?;
    let mut run = ComposeRun {
        muxer,
        video_encoder,
        video_extradata: None,
        video_out_index: 0,
        video_time_base: time_base,
        frame_rate,
        width: settings.width,
        height: settings.height,
        pixels,
        audio: None,
        header: HeaderState::Pending(Vec::new()),
        processed_us: 0,
        total_us,
        presentation_us: None,
        progress,
        video_frames: 0,
        audio_frames: 0,
        streams_out: 0,
    };

    for (index, segment) in segments.into_iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(ConvertError::Cancelled);
        }
        run_segment(
            &mut run, containers, codecs, segment, index, settings, cancel,
        )?;
    }

    // 收尾：视频编码器冲尾 → 音频补零成帧 + 冲尾 → 头仍未落则落（防御
    // 路径——每段强制有视频帧，常态头早已落）→ 写容器尾。
    run.video_encoder.flush();
    run.drain_video_encoder()?;
    if let Some(lane) = run.audio.as_mut() {
        let need = AAC_FRAME_SAMPLES * lane.channels;
        if !lane.samples.is_empty() {
            lane.samples.resize(need, 0.0);
        }
    }
    run.flush_audio_chunks()?;
    if let Some(lane) = run.audio.as_mut() {
        lane.encoder.flush();
    }
    run.drain_audio_encoder()?;
    if matches!(run.header, HeaderState::Pending(_)) {
        run.open_output_tracks()?;
    }
    run.muxer.write_trailer()?;
    Ok(ConvertStats {
        streams: run.streams_out,
        video_frames: run.video_frames,
        audio_frames: run.audio_frames,
        processed_us: run.processed_us,
        total_us: run.total_us,
    })
}

/// 设置校验（纯函数，供单测）：画幅非零偶数、帧率为正、`Copy` 拒绝。
fn validate_settings(settings: &ComposeSettings) -> Result<(), ConvertError> {
    if settings.width == 0 || settings.height == 0 {
        return Err(ConvertError::InvalidArguments(format!(
            "输出画幅须非零，收到 {}x{}",
            settings.width, settings.height
        )));
    }
    if !settings.width.is_multiple_of(2) || !settings.height.is_multiple_of(2) {
        return Err(ConvertError::InvalidArguments(format!(
            "输出画幅须为偶数（H.264 4:2:0 要求），收到 {}x{}",
            settings.width, settings.height
        )));
    }
    if settings.frame_rate.num() <= 0 || settings.frame_rate.den() <= 0 {
        return Err(ConvertError::InvalidArguments(format!(
            "输出帧率须为正，收到 {}/{}",
            settings.frame_rate.num(),
            settings.frame_rate.den()
        )));
    }
    if matches!(settings.audio, AudioEncodeSettings::Copy) {
        return Err(ConvertError::InvalidArguments(
            "compose 不接受音频 Copy（跨段参数一致性不保），请用 Aac 重编码或 Drop 剥轨".into(),
        ));
    }
    Ok(())
}

/// 窗口校验（纯函数，供单测）：起点 ≥ 0、终点 > 起点。
fn check_window(seg_index: usize, trim_in_us: i64, trim_out_us: i64) -> Result<(), ConvertError> {
    if trim_in_us < 0 {
        return Err(ConvertError::InvalidArguments(format!(
            "第 {} 段窗口起点为负（{trim_in_us}µs）",
            seg_index + 1
        )));
    }
    if trim_out_us <= trim_in_us {
        return Err(ConvertError::InvalidArguments(format!(
            "第 {} 段窗口为空（[{trim_in_us}, {trim_out_us})µs，须终点 > 起点）",
            seg_index + 1
        )));
    }
    Ok(())
}

/// 窗口过滤（纯函数，供单测）：pts ∈ [trim_in, trim_out)（含头不含尾）。
fn in_window(pts_us: i64, trim_in_us: i64, trim_out_us: i64) -> bool {
    pts_us >= trim_in_us && pts_us < trim_out_us
}

/// 帧时戳 → µs（缺 pts 即 `InvalidData`，指明段号）。
fn frame_pts_us(seg_index: usize, pts: Option<Timestamp>) -> Result<i64, ConvertError> {
    let Some(pts) = pts else {
        return Err(ConvertError::InvalidData(format!(
            "第 {} 段的帧缺 pts，窗口裁剪无从进行",
            seg_index + 1
        )));
    };
    Ok(pts.rescale(MICRO)?.ticks())
}

/// 单段处理：打开 → 校验流表 → 建解码器（音频轨懒建全局编码器）→
/// 逐包分流解码（视频过链入桶采样器、音频逐采样过滤入全局累积）→
/// 冲尾解码器 → 桶采样收尾（空窗拒绝）。
fn run_segment(
    run: &mut ComposeRun<'_>,
    containers: &ContainerRegistry,
    codecs: &CodecRegistry,
    segment: ComposeSegment,
    seg_index: usize,
    settings: &ComposeSettings,
    cancel: &AtomicBool,
) -> Result<(), ConvertError> {
    let (_format, mut demuxer) = containers.open_demuxer(segment.input)?;
    let header = demuxer.read_header()?;

    let video_indices: Vec<usize> = header
        .streams
        .iter()
        .enumerate()
        .filter(|(_, s)| s.media_type == MediaType::Video)
        .map(|(i, _)| i)
        .collect();
    let Some(video_index) = video_indices.first().copied() else {
        return Err(ConvertError::InvalidArguments(format!(
            "第 {} 段缺视频流，无从合成",
            seg_index + 1
        )));
    };
    if video_indices.len() > 1 {
        return Err(ConvertError::InvalidArguments(format!(
            "第 {} 段含 {} 路视频流（合成只接单路/段），多视频流归后续批次",
            seg_index + 1,
            video_indices.len()
        )));
    }

    // 音频轨：Aac 时须恰一路（缺/多路同拒），参数与首段一致且在能力面内。
    let audio_index: Option<usize> = match settings.audio {
        AudioEncodeSettings::Drop | AudioEncodeSettings::Copy => None, // Copy 已在入口拒绝
        AudioEncodeSettings::Aac { .. } => {
            let indices: Vec<usize> = header
                .streams
                .iter()
                .enumerate()
                .filter(|(_, s)| s.media_type == MediaType::Audio)
                .map(|(i, _)| i)
                .collect();
            if indices.is_empty() {
                return Err(ConvertError::InvalidArguments(format!(
                    "第 {} 段缺音频流（audio = Aac 要求每段自带音频；段级静音填充归后续批次）",
                    seg_index + 1
                )));
            }
            if indices.len() > 1 {
                return Err(ConvertError::InvalidArguments(format!(
                    "第 {} 段含 {} 路音频流（混音归后续批次）",
                    seg_index + 1,
                    indices.len()
                )));
            }
            let stream = &header.streams[indices[0]];
            run.ensure_audio_lane(codecs, stream, seg_index, settings)?;
            Some(indices[0])
        }
    };

    let mut video_decoder = codecs.create_decoder(&header.streams[video_index].codec)?;
    let mut audio_decoder = match audio_index {
        Some(index) => Some(codecs.create_decoder(&header.streams[index].codec)?),
        None => None,
    };

    let mut sampler = CfrSampler::new(
        seg_index,
        segment.trim_in_us,
        segment.trim_out_us,
        settings.frame_rate,
    )?;

    // 主循环：每包检查点（取消）→ 分流解码 → 帧过链/入累积。
    while let Some(packet) = demuxer.read_packet()? {
        if cancel.load(Ordering::Relaxed) {
            return Err(ConvertError::Cancelled);
        }
        let index = usize::try_from(packet.stream_index).unwrap_or(usize::MAX);
        if index == video_index {
            video_decoder.feed(&packet)?;
            loop {
                match video_decoder.pull()? {
                    Decoded::Frame(Frame::Video(frame)) => {
                        run.take_video_frame(
                            &mut sampler,
                            frame,
                            segment.trim_in_us,
                            segment.trim_out_us,
                            seg_index,
                        )?;
                    }
                    // 视频解码器出音频帧是实现违约：容忍忽略（convert 同款态度）。
                    Decoded::Frame(Frame::Audio(_)) => {}
                    Decoded::NeedInput | Decoded::Finished => break,
                }
            }
        } else if Some(index) == audio_index
            && let Some(decoder) = audio_decoder.as_mut()
        {
            decoder.feed(&packet)?;
            loop {
                match decoder.pull()? {
                    Decoded::Frame(Frame::Audio(frame)) => {
                        run.take_audio_frame(
                            frame,
                            segment.trim_in_us,
                            segment.trim_out_us,
                            seg_index,
                        )?;
                    }
                    // 音频解码器出视频帧是实现违约：容忍忽略。
                    Decoded::Frame(Frame::Video(_)) => {}
                    Decoded::NeedInput | Decoded::Finished => break,
                }
            }
        } // 其余轨道（字幕等）：丢弃。
    }

    // 冲尾解码器：重排缓冲的尾帧同样过窗（convert 同款）。
    video_decoder.flush();
    loop {
        match video_decoder.pull()? {
            Decoded::Frame(Frame::Video(frame)) => {
                run.take_video_frame(
                    &mut sampler,
                    frame,
                    segment.trim_in_us,
                    segment.trim_out_us,
                    seg_index,
                )?;
            }
            Decoded::Frame(Frame::Audio(_)) => {}
            Decoded::NeedInput | Decoded::Finished => break,
        }
    }
    if let Some(decoder) = audio_decoder.as_mut() {
        decoder.flush();
        loop {
            match decoder.pull()? {
                Decoded::Frame(Frame::Audio(frame)) => {
                    run.take_audio_frame(
                        frame,
                        segment.trim_in_us,
                        segment.trim_out_us,
                        seg_index,
                    )?;
                }
                Decoded::Frame(Frame::Video(_)) => {}
                Decoded::NeedInput | Decoded::Finished => break,
            }
        }
    }

    // 桶采样收尾：余下 tick hold-last 补尾；空窗（一帧都没有）显式拒绝。
    sampler.finish(&mut |frame| run.emit_cfr_frame(frame))
}

// ---------------------------------------------------------------------------
// CFR 桶采样（流式：帧按 pts 升序到达，逐 tick 定夺即发射）
// ---------------------------------------------------------------------------

/// CFR 桶采样器（每段一个）：把段窗口映射为 ⌈窗口×帧率⌉ 个输出 tick，
/// 每 tick 取「tick 时刻前（pts ≤ tick 时刻）最近的帧」（hold-last；段首
/// tick 前无帧取段内首帧）。帧假定按 pts 升序送入（`push`）；新帧到达即
/// 定夺并发射所有「时刻早于该帧 pts」的 tick，`finish` 冲尾部余量。
///
/// tick 时刻比较在 µs × 帧率分子 域做整数交叉相乘（i128），无 f64。
pub(crate) struct CfrSampler {
    /// 段号（0 起，报错用 1 起）。
    seg_index: usize,
    /// 窗口起点（µs）。
    trim_in_us: i64,
    /// tick 总数（⌈窗口×帧率⌉）。
    n_ticks: u64,
    /// 下一个待发射 tick。
    next_tick: u64,
    /// 帧率分子/分母（i64 域，比较公式用）。
    rate_num: i64,
    rate_den: i64,
    /// hold-last 的候选帧（最近的已过链帧）。
    held: Option<VideoFrame>,
    /// 是否已见过窗口内帧（空窗拒绝的依据）。
    saw_any: bool,
    black_leading: bool,
    valid_from: Option<Timestamp>,
}

impl CfrSampler {
    /// 构造（窗口与帧率校验 + tick 数推导）。
    pub(crate) fn new(
        seg_index: usize,
        trim_in_us: i64,
        trim_out_us: i64,
        frame_rate: Rational,
    ) -> Result<Self, ConvertError> {
        check_window(seg_index, trim_in_us, trim_out_us)?;
        let (rate_num, rate_den) = (i64::from(frame_rate.num()), i64::from(frame_rate.den()));
        if rate_num <= 0 || rate_den <= 0 {
            return Err(ConvertError::InvalidArguments(format!(
                "第 {} 段输出帧率须为正",
                seg_index + 1
            )));
        }
        // n_ticks = ⌈window_us × num / (den × 1e6)⌉（i128 中间量防溢出）。
        let window = i128::from(trim_out_us - trim_in_us);
        let n_ticks = ceil_div(
            window * i128::from(rate_num),
            i128::from(rate_den) * 1_000_000,
        );
        let n_ticks = u64::try_from(n_ticks).map_err(|_| {
            ConvertError::InvalidArguments(format!("第 {} 段 tick 数溢出 u64", seg_index + 1))
        })?;
        if n_ticks == 0 {
            return Err(ConvertError::InvalidArguments(format!(
                "第 {} 段窗口不足一个输出 tick",
                seg_index + 1
            )));
        }
        Ok(CfrSampler {
            seg_index,
            trim_in_us,
            n_ticks,
            next_tick: 0,
            rate_num,
            rate_den,
            held: None,
            saw_any: false,
            black_leading: false,
            valid_from: None,
        })
    }

    pub(crate) fn for_duration(
        duration: Timestamp,
        rate: Rational,
        valid_from: Timestamp,
    ) -> Result<Self, ConvertError> {
        if duration.ticks() <= 0 || duration.time_base().num() <= 0 || rate.num() <= 0 {
            return Err(ConvertError::InvalidArguments(
                "CFR 时长和帧率须为正".into(),
            ));
        }
        let numerator = i128::from(duration.ticks())
            * i128::from(duration.time_base().num())
            * i128::from(rate.num());
        let denominator = i128::from(duration.time_base().den()) * i128::from(rate.den());
        let count = u64::try_from(ceil_div(numerator, denominator))
            .map_err(|_| ConvertError::InvalidArguments("CFR 帧数溢出".into()))?;
        Ok(Self {
            seg_index: 0,
            trim_in_us: 0,
            n_ticks: count,
            next_tick: 0,
            rate_num: i64::from(rate.num()),
            rate_den: i64::from(rate.den()),
            held: None,
            saw_any: false,
            black_leading: true,
            valid_from: Some(valid_from),
        })
    }

    /// 送入一个窗口内帧（调用方已过滤窗口 + 过链；pts 升序）。触发发射
    /// 所有已定夺的 tick（`emit` 可能被调多次）。
    pub(crate) fn push(
        &mut self,
        frame: VideoFrame,
        emit: &mut dyn FnMut(&VideoFrame) -> Result<(), ConvertError>,
    ) -> Result<(), ConvertError> {
        self.advance(Some(&frame), emit)?;
        self.held = Some(frame);
        self.saw_any = true;
        Ok(())
    }

    /// 窗口结束：空窗（一帧未见）显式拒绝；余下 tick hold-last 补尾发射。
    pub(crate) fn finish(
        &mut self,
        emit: &mut dyn FnMut(&VideoFrame) -> Result<(), ConvertError>,
    ) -> Result<(), ConvertError> {
        if !self.saw_any {
            return Err(ConvertError::InvalidArguments(format!(
                "第 {} 段的窗口内没有任何视频帧，无从合成",
                self.seg_index + 1
            )));
        }
        self.advance(None, emit)
    }

    /// 发射推进：下一帧 pts（`next`）已知的 tick 立即定夺（时刻 < next.pts
    /// 的 tick 选择已定格）；`next = None`（窗口结束）时发射全部余量。
    /// 源帧选择：`held`（hold-last）优先，无 held（段首 tick 前无帧）取
    /// `next`（段内首帧）。
    fn advance(
        &mut self,
        next: Option<&VideoFrame>,
        emit: &mut dyn FnMut(&VideoFrame) -> Result<(), ConvertError>,
    ) -> Result<(), ConvertError> {
        let next_pts: Option<Timestamp> = match next {
            Some(frame) => Some(
                frame
                    .pts()
                    .ok_or_else(|| ConvertError::InvalidData("视频帧缺 PTS".into()))?,
            ),
            None => None,
        };
        while self.next_tick < self.n_ticks {
            let tick = Timestamp::from_frame_index(
                i64::try_from(self.next_tick)
                    .map_err(|_| ConvertError::InvalidArguments("CFR tick 溢出".into()))?,
                Rational::new(self.rate_num as i32, self.rate_den as i32),
            )?;
            let offset = Timestamp::new(self.trim_in_us, MICRO);
            if let Some(next_pts) = next_pts
                && Timestamp::compare_sum(tick, offset, next_pts)?.is_ge()
            {
                break; // 该 tick 的选择尚未定格（next 可能就是它）。
            }
            let Some(source) = self.held.as_ref().or(next) else {
                // finish 路径 held 必在（saw_any 已查）；防御性错误。
                return Err(ConvertError::InvalidData(format!(
                    "第 {} 段桶采样状态破损（tick {} 无源帧）",
                    self.seg_index + 1,
                    self.next_tick
                )));
            };
            if (self.black_leading && self.held.is_none())
                || match self.valid_from {
                    Some(start) => Timestamp::compare_sum(tick, offset, start)?.is_lt(),
                    None => false,
                }
            {
                let mut black = source.clone();
                let luma = if black.color().range == ye_pixfmt::ColorRange::Jpeg {
                    0
                } else {
                    16
                };
                for plane in 0..3 {
                    black
                        .plane_mut(plane)
                        .fill(if plane == 0 { luma } else { 128 });
                }
                emit(&black)?;
            } else {
                emit(source)?;
            }
            self.next_tick += 1;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 泵内状态（音频全局轨 / 封装头延迟 / 主游标）
// ---------------------------------------------------------------------------

/// 封装头状态（与 convert.rs 的 HeaderState 同构）：视频首包未到 → 缓冲
/// 音频包（流号先写标记值，落头冲出时改写）；落头后 → 直写。
enum HeaderState {
    /// 头未写：缓冲的音频包。
    Pending(Vec<Packet>),
    /// 头已写：后续包直写 muxer。
    Written,
}

/// 缓冲音频包的流号占位（冲出时改写为真实输出流号）。
const PENDING_AUDIO_TAG: u32 = u32::MAX;

/// 全局音频轨（AAC-LC 重编码）：唯一编码器 + 跨段连续采样累积。
struct AudioLane {
    encoder: Box<dyn Encoder>,
    /// 输出模板（esds extradata 构造即可用；时间线从 0 起算无平移）。
    template: StreamTemplate,
    /// 采样累积缓冲（packed f32 交错；凑满 `AAC_FRAME_SAMPLES × channels`
    /// 出一帧）。
    samples: Vec<f32>,
    sample_rate: u32,
    layout: ChannelLayout,
    channels: usize,
    /// 输出流号（`open_output_tracks` 时分配）。
    out_index: u32,
}

/// 合成泵游标。
struct ComposeRun<'a> {
    muxer: Box<dyn Muxer>,
    /// 全局唯一视频编码器（跨段不重置）。
    video_encoder: Box<dyn Encoder>,
    /// avcC（首 IDR 带内 SPS/PPS 构造；`None` = 尚无输出包）。
    video_extradata: Option<Vec<u8>>,
    video_out_index: u32,
    video_time_base: Rational,
    frame_rate: Rational,
    width: u32,
    height: u32,
    pixels: PixelOps,
    /// 全局音频轨（首段建立；`None` = Drop 或尚未见到音频段）。
    audio: Option<AudioLane>,
    header: HeaderState,
    processed_us: i64,
    total_us: i64,
    presentation_us: Option<i64>,
    progress: ConvertProgress<'a>,
    video_frames: u64,
    audio_frames: u64,
    /// 输出流数（落头时定）。
    streams_out: usize,
}

impl ComposeRun<'_> {
    /// 视频帧入口：窗口过滤 → 过链（scale → 强制 yuv420p）→ 入桶采样器
    /// （发射经 [`Self::emit_cfr_frame`]）。窗口外的帧直接丢弃。
    fn take_video_frame(
        &mut self,
        sampler: &mut CfrSampler,
        frame: VideoFrame,
        trim_in_us: i64,
        trim_out_us: i64,
        seg_index: usize,
    ) -> Result<(), ConvertError> {
        let pts_us = frame_pts_us(seg_index, frame.pts())?;
        if !in_window(pts_us, trim_in_us, trim_out_us) {
            return Ok(()); // 窗口外：丢弃（不做 seek 优化，见模块文档）。
        }
        // 过链：scale 到输出画幅（尺寸相同跳过）→ 强制 yuv420p（已是则跳过）。
        let frame = if (frame.width(), frame.height()) != (self.width, self.height) {
            (self.pixels.scale)(&frame, self.width, self.height)?
        } else {
            frame
        };
        let frame = if frame.format() != PixelFormat::Yuv420p {
            (self.pixels.convert)(&frame, PixelFormat::Yuv420p)?
        } else {
            frame
        };
        sampler.push(frame, &mut |frame| self.emit_cfr_frame(frame))
    }

    /// 单个 CFR tick 的源帧：喂全局编码器（帧克隆一次——同一帧可占多个
    /// tick，零拷贝优化归后续批次）→ 排空编码器。
    fn emit_cfr_frame(&mut self, frame: &VideoFrame) -> Result<(), ConvertError> {
        self.video_encoder.feed(&Frame::Video(frame.clone()))?;
        self.video_frames += 1;
        self.drain_video_encoder()
    }

    /// 排空视频编码器输出队列（编码逐帧同步，常态一帧一包）。
    fn drain_video_encoder(&mut self) -> Result<(), ConvertError> {
        loop {
            let Some(mut packet) = (match self.video_encoder.pull()? {
                Encoded::Packet(p) => Some(p),
                Encoded::NeedInput | Encoded::Finished => None,
            }) else {
                return Ok(());
            };
            if self.video_extradata.is_none() {
                self.video_extradata = Some(crate::convert::avcc_from_idr(&packet)?);
            }
            if matches!(self.header, HeaderState::Pending(_)) {
                self.open_output_tracks()?;
            }
            packet.stream_index = self.video_out_index;
            let pts = packet.pts;
            self.muxer.write_packet(&packet)?;
            self.report(pts)?;
        }
    }

    /// 音频帧入口：逐采样窗口过滤并入全局累积，凑满即成帧编码。
    fn take_audio_frame(
        &mut self,
        frame: AudioFrame,
        trim_in_us: i64,
        trim_out_us: i64,
        seg_index: usize,
    ) -> Result<(), ConvertError> {
        let pts_us = frame_pts_us(seg_index, frame.pts())?;
        if frame.format() != SampleFormat::F32 {
            return Err(ConvertError::InvalidData(format!(
                "第 {} 段音频解码帧须为 packed f32（AAC 重编码链的输入面），收到 {:?}",
                seg_index + 1,
                frame.format()
            )));
        }
        let (sample_rate, channels) = match self.audio.as_ref() {
            // Drop：不建轨也不建解码器，不可达；防御性丢弃。
            None => return Ok(()),
            Some(lane) => (lane.sample_rate, lane.channels),
        };
        let bytes = frame.data();
        if !bytes.len().is_multiple_of(4) {
            return Err(ConvertError::InvalidData(
                "音频解码帧字节数非 4 的倍数（packed f32 破损）".into(),
            ));
        }
        let total_samples = bytes.len() / 4;
        if channels == 0 || !total_samples.is_multiple_of(channels) {
            return Err(ConvertError::InvalidData(format!(
                "第 {} 段音频解码帧采样数（{total_samples}）不是声道数（{channels}）的整数倍",
                seg_index + 1
            )));
        }
        let rate = i64::from(sample_rate);
        if rate == 0 {
            return Err(ConvertError::InvalidData("音频轨采样率为 0".into()));
        }
        // 逐采样窗口过滤（含头不含尾）：采样时刻 = pts + g/rate，整数比较
        // 在 µs × rate 域（pts_us·rate + g·1e6 ∈ [in·rate, out·rate)）。
        let (lo, hi) = (
            trim_in_us.saturating_mul(rate),
            trim_out_us.saturating_mul(rate),
        );
        let base = pts_us.saturating_mul(rate);
        let mut kept: Vec<f32> = Vec::new();
        for (g, chunk) in bytes.as_chunks::<4>().0.chunks(channels).enumerate() {
            let at = base.saturating_add(i64::saturating_mul(g as i64, 1_000_000));
            if at < lo || at >= hi {
                continue;
            }
            kept.extend(chunk.iter().map(|c| f32::from_le_bytes(*c)));
        }
        if let Some(lane) = self.audio.as_mut() {
            lane.samples.extend(kept);
        }
        self.flush_audio_chunks()
    }

    /// 累积缓冲凑满 1024×channels 即成帧喂编码器并排空输出（可能多帧）。
    fn flush_audio_chunks(&mut self) -> Result<(), ConvertError> {
        loop {
            let frame = {
                let Some(lane) = self.audio.as_mut() else {
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
            if let Some(lane) = self.audio.as_mut() {
                lane.encoder.feed(&frame)?;
            }
            self.drain_audio_encoder()?;
        }
    }

    /// 排空音频编码器输出队列（编码逐帧同步，常态一帧一包）。
    fn drain_audio_encoder(&mut self) -> Result<(), ConvertError> {
        loop {
            let Some(packet) = (match self.audio.as_mut() {
                Some(lane) => match lane.encoder.pull()? {
                    Encoded::Packet(p) => Some(p),
                    Encoded::NeedInput | Encoded::Finished => None,
                },
                None => None,
            }) else {
                return Ok(());
            };
            let pts = packet.pts;
            self.emit_audio_packet(packet, pts)?;
        }
    }

    /// 落一个音频输出包：计数 → 进度 → 缓冲（头未落）或直写。
    fn emit_audio_packet(
        &mut self,
        mut packet: Packet,
        report_at: Option<Timestamp>,
    ) -> Result<(), ConvertError> {
        self.audio_frames += 1;
        self.report(report_at)?;
        if let HeaderState::Pending(buffer) = &mut self.header {
            packet.stream_index = PENDING_AUDIO_TAG;
            buffer.push(packet);
            return Ok(());
        }
        let Some(out_index) = self.audio.as_ref().map(|lane| lane.out_index) else {
            return Err(ConvertError::InvalidData(
                "音频包无输出轨（状态破损）".into(),
            ));
        };
        packet.stream_index = out_index;
        self.muxer.write_packet(&packet)?;
        Ok(())
    }

    /// 建立或校验全局音频轨（首段建：能力面校验 + esds + 编码器；后续段
    /// 只比对采样率/声道与前段一致）。
    fn ensure_audio_lane(
        &mut self,
        codecs: &CodecRegistry,
        stream: &Stream,
        seg_index: usize,
        settings: &ComposeSettings,
    ) -> Result<(), ConvertError> {
        let AudioEncodeSettings::Aac { bitrate_bps } = settings.audio else {
            return Ok(()); // Drop：不建轨（调用方已分支，不可达）。
        };
        let audio = stream.codec.audio.as_ref().ok_or_else(|| {
            ConvertError::InvalidArguments(format!(
                "第 {} 段音频流 #{} 缺采样率/声道参数（容器未携带），无法配置 AAC 编码器",
                seg_index + 1,
                stream.index
            ))
        })?;
        let sample_rate = audio.sample_rate;
        if sample_rate == 0 {
            return Err(ConvertError::InvalidArguments(format!(
                "第 {} 段音频流采样率为 0",
                seg_index + 1
            )));
        }
        let Some(layout) = audio.channel_layout else {
            return Err(ConvertError::InvalidArguments(format!(
                "第 {} 段音频流缺声道布局（容器未携带）",
                seg_index + 1
            )));
        };
        let channels = layout.channels();
        if channels == 0 || channels > 2 {
            return Err(ConvertError::InvalidArguments(format!(
                "第 {} 段音频为 {channels} 声道，超出 AAC-LC 能力面（单/双声道），混音归后续批次",
                seg_index + 1
            )));
        }
        match &self.audio {
            Some(lane) => {
                if lane.sample_rate != sample_rate || lane.layout.channels() != channels {
                    return Err(ConvertError::InvalidArguments(format!(
                        "第 {} 段音频参数与前段不一致（采样率 {sample_rate}Hz × {channels} 声道 vs 首段 {}Hz × {} 声道），重采样/混音归后续批次",
                        seg_index + 1,
                        lane.sample_rate,
                        lane.layout.channels()
                    )));
                }
                Ok(())
            }
            None => {
                // AAC 访问单元无带内参数集：esds 按流参数构造（正本在
                // ye-codec-aac，经 convert 复用）；采样率/声道超能力面 →
                // InvalidArguments（指明段号）。
                let channels_u8 = u8::try_from(channels)
                    .map_err(|_| ConvertError::InvalidArguments("AAC 声道数超 u8".into()))?;
                let extradata = crate::convert::aac_esds(sample_rate, channels_u8).map_err(
                    |err| match err {
                        ConvertError::InvalidArguments(msg) => ConvertError::InvalidArguments(
                            format!("第 {} 段音频{msg}，重采样归后续批次", seg_index + 1),
                        ),
                        other => other,
                    },
                )?;
                let rate = i32::try_from(sample_rate).map_err(|_| {
                    ConvertError::InvalidArguments(format!(
                        "第 {} 段音频采样率超 i32 时基域",
                        seg_index + 1
                    ))
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
                params.audio = Some(ye_codec::AudioParams {
                    sample_rate,
                    channel_layout: Some(layout),
                    sample_format: None,
                });
                self.audio = Some(AudioLane {
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
                    channels: channels as usize,
                    out_index: 0,
                });
                Ok(())
            }
        }
    }

    /// 落封装头：视频模板（avcC 须已回收）+ 音频模板（esds），分配输出
    /// 流号（视频 0、音频 1），冲出缓冲的音频包。
    fn open_output_tracks(&mut self) -> Result<(), ConvertError> {
        let HeaderState::Pending(buffer) =
            std::mem::replace(&mut self.header, HeaderState::Written)
        else {
            return Ok(()); // 已落头：幂等。
        };
        let mut templates = Vec::new();
        let mut next_index = 0u32;
        if let Some(avcc) = self.video_extradata.clone() {
            self.video_out_index = next_index;
            next_index += 1;
            let mut params = CodecParameters::new(CodecId::H264, MediaType::Video);
            params.extradata = Some(avcc);
            params.video = Some(VideoParams {
                width: self.width,
                height: self.height,
                pixel_format: Some(PixelFormat::Yuv420p),
                frame_rate: Some(self.frame_rate),
            });
            templates.push(StreamTemplate {
                media_type: MediaType::Video,
                codec: params,
                time_base: self.video_time_base,
                frame_rate: Some(self.frame_rate),
                metadata: crate::convert::presentation_metadata(
                    self.presentation_us
                        .map(|us| Timestamp::new(us, MICRO).rescale(self.video_time_base))
                        .transpose()?
                        .map(|t| t.ticks()),
                ),
            });
        }
        if let Some(lane) = self.audio.as_mut() {
            lane.out_index = next_index;
            templates.push(lane.template.clone());
        }
        self.streams_out = templates.len();
        self.muxer.write_header(&templates)?;
        // 冲出缓冲包（流号从占位标记改写为真实输出流号）——必须在
        // write_header 之后（convert 同款顺序）。
        if let Some(out_index) = self.audio.as_ref().map(|lane| lane.out_index) {
            for mut packet in buffer {
                packet.stream_index = out_index;
                self.muxer.write_packet(&packet)?;
            }
        }
        Ok(())
    }

    /// 进度上报：时戳（自带时基）换 µs 取单调最大值后回调（convert 同款）。
    fn report(&mut self, pts: Option<Timestamp>) -> Result<(), ConvertError> {
        if let Some(pts) = pts {
            let us = pts.rescale(MICRO)?.ticks();
            self.processed_us = self.processed_us.max(us);
        }
        (self.progress)(self.processed_us, self.total_us);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 单元测试（CFR 桶采样纯语义 / 校验纯函数）
// ---------------------------------------------------------------------------

/// Shared output pump for already rendered frames. Input audio is 48kHz stereo.
pub(crate) struct RenderedOutput<'a> {
    run: ComposeRun<'a>,
}
impl RenderedOutput<'_> {
    pub(crate) fn new(
        containers: &ContainerRegistry,
        codecs: &CodecRegistry,
        format: &str,
        output: Box<dyn WriteSeek>,
        settings: &ComposeSettings,
        total_us: i64,
    ) -> Result<Self, ConvertError> {
        validate_settings(settings)?;
        let fps = settings.frame_rate;
        let time_base = Rational::new(1, fps.num());
        let config = EncoderConfig {
            codec: CodecId::H264,
            time_base,
            bit_rate: settings.bit_rate,
            video: Some(ye_codec::VideoEncoderConfig {
                width: settings.width,
                height: settings.height,
                format: PixelFormat::Yuv420p,
                frame_rate: fps,
                gop: settings.gop,
                quality: settings.quality,
            }),
            audio: None,
        };
        let mut run = ComposeRun {
            muxer: containers.create_muxer(format, output)?,
            video_encoder: codecs.create_encoder(&config)?,
            video_extradata: None,
            video_out_index: 0,
            video_time_base: time_base,
            frame_rate: fps,
            width: settings.width,
            height: settings.height,
            pixels: crate::convert::filter_ops(),
            audio: None,
            header: HeaderState::Pending(Vec::new()),
            processed_us: 0,
            total_us,
            presentation_us: Some(total_us),
            progress: &|_, _| {},
            video_frames: 0,
            audio_frames: 0,
            streams_out: 0,
        };
        if matches!(settings.audio, AudioEncodeSettings::Aac { .. }) {
            let mut params = CodecParameters::new(CodecId::Aac, MediaType::Audio);
            params.audio = Some(ye_codec::AudioParams {
                sample_rate: 48000,
                channel_layout: Some(ChannelLayout::STEREO),
                sample_format: Some(SampleFormat::F32),
            });
            run.ensure_audio_lane(
                codecs,
                &Stream {
                    index: 0,
                    media_type: MediaType::Audio,
                    codec: params,
                    time_base: Rational::new(1, 48000),
                    start_time: None,
                    duration: None,
                    nb_frames: None,
                    metadata: Metadata::new(),
                },
                0,
                settings,
            )?;
            if let Some(audio) = run.audio.as_mut() {
                audio.template.metadata = crate::convert::presentation_metadata(Some(
                    ((i128::from(total_us) * 48000 + 999999) / 1_000_000) as i64,
                ));
            }
        }
        Ok(Self { run })
    }
    pub(crate) fn write(&mut self, frame: Frame) -> Result<(), ConvertError> {
        match frame {
            Frame::Video(v) => {
                let v = (self.run.pixels.convert)(&v, PixelFormat::Yuv420p)?;
                self.run.emit_cfr_frame(&v)
            }
            Frame::Audio(a) => self.run.take_audio_frame(a, 0, self.run.total_us, 0),
        }
    }
    pub(crate) fn finish(mut self) -> Result<ConvertStats, ConvertError> {
        let run = &mut self.run;
        run.video_encoder.flush();
        run.drain_video_encoder()?;
        if let Some(lane) = run.audio.as_mut()
            && !lane.samples.is_empty()
        {
            lane.samples.resize(AAC_FRAME_SAMPLES * lane.channels, 0.0);
        }
        run.flush_audio_chunks()?;
        if let Some(lane) = run.audio.as_mut() {
            lane.encoder.flush();
        }
        run.drain_audio_encoder()?;
        if matches!(run.header, HeaderState::Pending(_)) {
            run.open_output_tracks()?;
        }
        run.muxer.write_trailer()?;
        Ok(ConvertStats {
            streams: run.streams_out,
            video_frames: run.video_frames,
            audio_frames: run.audio_frames,
            processed_us: run.total_us,
            total_us: run.total_us,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ye_pixfmt::ColorDesc;

    #[test]
    fn cfr_fractional_clock_preserves_every_frame_and_exact_count() {
        for rate in [Rational::new(30000, 1001), Rational::new(2997, 100)] {
            let mut sampler = CfrSampler::for_duration(
                Timestamp::from_frame_index(11, rate).unwrap(),
                rate,
                Timestamp::new(0, MICRO),
            )
            .unwrap();
            let mut marks = Vec::new();
            for i in 0..11 {
                let mut frame = mark_frame(i as u8, 0);
                frame.set_pts(Some(Timestamp::from_frame_index(i, rate).unwrap()));
                sampler
                    .push(frame, &mut |f| {
                        marks.push(f.plane(0)[0]);
                        Ok(())
                    })
                    .unwrap();
            }
            sampler
                .finish(&mut |f| {
                    marks.push(f.plane(0)[0]);
                    Ok(())
                })
                .unwrap();
            assert_eq!(marks, (0..11).collect::<Vec<u8>>(), "rate={rate:?}");
        }
    }

    /// 造一帧 4x4 gray8 测试帧（luma = 标记，pts 按给定 µs）。
    fn mark_frame(marker: u8, pts_us: i64) -> VideoFrame {
        let mut frame = VideoFrame::new(
            PixelFormat::Gray8,
            4,
            4,
            ColorDesc::UNSPECIFIED,
            Some(Timestamp::new(pts_us, MICRO)),
            None,
        )
        .expect("测试帧构造");
        for byte in frame.plane_mut(0).iter_mut() {
            *byte = marker;
        }
        frame
    }

    /// 跑一遍采样：送帧序列（标记, pts µs）→ finish → 返回发射标记序列。
    fn sample(
        trim_in_us: i64,
        trim_out_us: i64,
        rate: Rational,
        frames: &[(u8, i64)],
    ) -> Result<Vec<u8>, ConvertError> {
        let mut sampler = CfrSampler::new(0, trim_in_us, trim_out_us, rate)?;
        let marks: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());
        let mut emit = |frame: &VideoFrame| -> Result<(), ConvertError> {
            marks.lock().unwrap().push(frame.plane(0)[0]);
            Ok(())
        };
        for (marker, pts_us) in frames {
            sampler.push(mark_frame(*marker, *pts_us), &mut emit)?;
        }
        sampler.finish(&mut emit)?;
        Ok(marks.into_inner().unwrap())
    }

    /// hold-last：空 tick 补前一帧。0.5s 一帧、窗口 2s @2fps → 4 tick，
    /// 帧仅覆盖前 1s → [f0, f1, f1, f1]。
    #[test]
    fn cfr_hold_last_fills_empty_ticks() {
        let marks =
            sample(0, 2_000_000, Rational::new(2, 1), &[(0, 0), (1, 500_000)]).expect("采样应成功");
        assert_eq!(marks, vec![0, 1, 1, 1]);
    }

    /// 段首 tick 前无帧：取段内首帧（t=0 无帧 ≤ 0，首帧 0.3s）→ 两个
    /// tick 都取 f0（tick1=0.5s 仍被 f0 覆盖，f1=0.8s 只接 tick2）。
    #[test]
    fn cfr_leading_gap_takes_first_frame() {
        let marks = sample(
            0,
            1_000_000,
            Rational::new(2, 1),
            &[(5, 300_000), (6, 800_000)],
        )
        .expect("采样应成功");
        assert_eq!(marks, vec![5, 5], "tick0/1 均取段内首帧 f0");
    }

    /// 窗口含头不含尾（`in_window` 口径）：pts == trim_in 在窗内、
    /// pts == trim_out 在窗外。
    #[test]
    fn window_is_half_open() {
        assert!(in_window(500_000, 500_000, 1_500_000), "头包含");
        assert!(!in_window(499_999, 500_000, 1_500_000), "窗外（前）");
        assert!(!in_window(1_500_000, 500_000, 1_500_000), "尾不包含");
        assert!(in_window(1_499_999, 500_000, 1_500_000));
        // 头包含联动采样：trim_in 处的帧即 tick0 的源。
        let marks =
            sample(500_000, 1_500_000, Rational::new(1, 1), &[(9, 500_000)]).expect("采样应成功");
        assert_eq!(marks, vec![9], "pts==trim_in 的帧参与采样");
    }

    /// tick 数 = ⌈窗口 × 帧率⌉：2.1s @15fps → ⌈31.5⌉ = 32；单帧
    /// hold-last 补满全部 tick。
    #[test]
    fn cfr_tick_count_ceils() {
        let marks = sample(0, 2_100_000, Rational::new(15, 1), &[(3, 0)]).expect("采样应成功");
        assert_eq!(marks.len(), 32, "⌈2.1s × 15fps⌉ = 32 tick");
        assert!(marks.iter().all(|&m| m == 3));
        // 恰好整除：2s @15fps → 30 tick。
        let marks = sample(0, 2_000_000, Rational::new(15, 1), &[(3, 0)]).expect("采样应成功");
        assert_eq!(marks.len(), 30);
    }

    /// 帧选择语义：每 tick 取 tick 时刻前最近的帧（pts ≤ t）。15fps 源、
    /// 15fps 出：帧 j 覆盖 tick j。帧 pts 取略小于 1/15s、2/15s 的整 µs
    /// （66666.67 → 66666）：验证 tick 边界的精确有理数比较（66667 会
    /// 落在 t1=66666.67 之后而被 tick1 越过——hold 到 f0）。
    #[test]
    fn cfr_selects_latest_frame_at_or_before_tick() {
        let marks = sample(
            0,
            200_000,
            Rational::new(15, 1),
            &[(0, 0), (1, 66_666), (2, 133_333)],
        )
        .expect("采样应成功");
        // tick 数 ⌈0.2s×15⌉=3；每 tick 选 pts ≤ t 的最近帧。
        assert_eq!(marks, vec![0, 1, 2]);
        // 反例：pts 恰在 tick 边界之后（66667 > 66666.67）→ tick1 仍 hold f0。
        let marks = sample(
            0,
            200_000,
            Rational::new(15, 1),
            &[(0, 0), (1, 66_667), (2, 133_333)],
        )
        .expect("采样应成功");
        assert_eq!(marks, vec![0, 0, 2], "pts 越过 tick 边界即不接管该 tick");
    }

    /// 空窗拒绝：一帧未见 → finish 显式 InvalidArguments（指明段号）。
    #[test]
    fn cfr_empty_window_rejected() {
        let result = sample(0, 1_000_000, Rational::new(2, 1), &[]);
        assert!(
            matches!(&result, Err(ConvertError::InvalidArguments(msg)) if msg.contains("第 1 段")),
            "应指明段号拒绝，实际：{result:?}"
        );
    }

    /// 窗口参数校验：空窗/倒窗/负起点拒绝。
    #[test]
    fn window_validation_rejects_degenerate() {
        assert!(check_window(0, 500_000, 500_000).is_err(), "空窗");
        assert!(check_window(0, 600_000, 500_000).is_err(), "倒窗");
        assert!(check_window(2, -1, 500_000).is_err(), "负起点");
        assert!(check_window(0, 0, 500_000).is_ok(), "0 起点合法");
    }

    /// 设置校验：奇数画幅/零画幅/非正帧率/Copy 全拒。
    #[test]
    fn settings_validation() {
        let ok = |settings: &ComposeSettings| validate_settings(settings).is_ok();
        let base = || ComposeSettings {
            width: 320,
            height: 240,
            frame_rate: Rational::new(15, 1),
            gop: 250,
            quality: QualityMode::Crf(23),
            bit_rate: None,
            audio: AudioEncodeSettings::Aac {
                bitrate_bps: 128_000,
            },
        };
        assert!(ok(&base()));
        assert!(
            !ok(&ComposeSettings {
                width: 321,
                ..base()
            }),
            "奇数宽"
        );
        assert!(
            !ok(&ComposeSettings {
                height: 0,
                ..base()
            }),
            "零高"
        );
        assert!(
            !ok(&ComposeSettings {
                frame_rate: Rational::new(0, 1),
                ..base()
            }),
            "零帧率"
        );
        let copy = ComposeSettings {
            audio: AudioEncodeSettings::Copy,
            ..base()
        };
        let err = validate_settings(&copy).unwrap_err();
        assert!(
            matches!(&err, ConvertError::InvalidArguments(msg) if msg.contains("Copy")),
            "Copy 应明确拒绝：{err:?}"
        );
    }

    /// i128 上取整公式锁死：正数域与数学 ⌈⌉ 一致。
    #[test]
    fn ceil_div_matches_math() {
        assert_eq!(ceil_div(30, 1), 30);
        assert_eq!(ceil_div(31, 2), 16); // ⌈15.5⌉
        assert_eq!(ceil_div(32, 2), 16); // 整除不动
        assert_eq!(ceil_div(1, 3), 1);
    }
}
