//! 波形峰值：packed f32 交织样本 → 每桶每声道 min/max/rms。
//!
//! ## 结构分层
//!
//! - **纯核**（仅 std，无条件编译，进默认门禁基线）：桶数入口
//!   [`peaks_by_bucket_count`] 与桶长入口 [`peaks_by_bucket_size`]——
//!   共享同一条分桶边界公式（见下），f64 中间量累计（长素材防漂移）；
//! - **消费助手**（`mp4`+`aac` 双 feature 函数级门，不新增 feature 名）：
//!   [`audio_peaks`]（桶长流式喂桶）与 [`audio_peaks_with_count`]（桶数
//!   收全样本再分桶）——前置流布局校验 + [`convert_frames`] 解码泵接线，
//!   解码 Err 原样透传不兜底（回退归 jian 命令层）。
//!
//! ## 分桶边界公式（两入口共享，一处分义）
//!
//! 每声道样本数 `N`、桶长 `L` 时，桶 `k` 覆盖帧区间
//! `[min(k·L, N), min((k+1)·L, N))`（u128 乘法防 usize 溢出后钳制到 N）：
//!
//! - **桶长入口**：`L` = 请求值，恰 `ceil(N/L)` 桶，尾桶可不满；
//! - **桶数入口**：`L = ceil(N/桶数)`，实桶 `ceil(N/L)` 个，**不足请求
//!   桶数的尾部以静默零值桶（min=max=rms=0）补齐**——返回桶数恒等于
//!   请求值（桶数 > 样本数时亦然；空桶零值即波形显示的「此处无音频」
//!   语义）；
//! - `N == 0` 两入口都返回空 Vec；桶数/桶长/声道数为 0 →
//!   [`ConvertError::InvalidArguments`]；样本数非声道数整倍数（packed
//!   交织须整帧）→ [`ConvertError::InvalidData`]。
//!
//! m2-jian-plan §2.1 草案原文为 `floor(N·k/B)` 等分式，实施定为上式
//! min 形，理由：桶长入口的消费助手须**流式喂桶**（样本到达时不知 N，
//! 无法重放 floor 边界），「尾桶可不满」「桶数入口返回桶数=请求值」
//! 「两入口共享同一条边界公式」三条约束仅在均匀桶长下自洽，桶数入口
//! 改以零值桶补齐保持请求值契约。对拍口径（T3）参考侧与 ye 侧喂同一
//! 份纯核，公式形态不影响自洽性。
//!
//! ## 消费助手行为契约
//!
//! - **前置流布局校验**（[`convert_frames`] 给每条音/视频流都建解码器、
//!   [`Frame`] 不携带流标识——多音轨素材若不前置校验，两路样本会混进
//!   同一组桶，是静默错数据，比 Err 更坏）：[`ContainerRegistry::probe`]
//!   读 [`MediaHeader`]，要求**恰一条音频流**（视频流可有可无，视频帧
//!   解码后在 sink 丢弃；0 条 → Err，≥2 条 → 明确 Err 绝不静默混桶）；
//! - **回卷**：probe 按值消费输入，底层输入经 [`SharedInput`]（`Arc<
//!   Mutex>` 共享）同时供 probe 与泵持有；probe 读头后游标停在头解析
//!   处，喂泵前显式 seek 回 0（泵内 probe 窗口从当前位置读起，不回卷
//!   会拿 mdat 中段当探查窗口）；
//! - **解码 Err 原样透传**（PNS/多声道/非 LC 等能力面外）：全程 `?`
//!   无兜底分支；
//! - **桶数入口收全样本**再按数分桶（f32 × 声道 × 帧数随素材长线性
//!   增长——长素材内存局限，流式化归 M4，m2-jian-plan §6）；
//! - **注册表内部自建**：容器 MP4 + 解码 AAC；`h264` feature 开启时
//!   顺带注册 H.264（含视频素材的视频帧解码后丢弃），未开启时视频流
//!   以 `NotRegistered` 透传——能力面语义，回退归 jian 命令层。
//!
//! ## 全库约定
//!
//! 零第三方依赖（仅 std + workspace crate）、不含 unsafe；数据驱动失败
//! 一律 `Result`，无 panic 路径；错误复用 [`ConvertError`]（手写
//! `Display`/`Error` 见 convert 模块），doc 注释中文并写明 derive 清单。

use crate::convert::ConvertError;

/// 单桶单声道峰值统计。
///
/// 空桶（桶数入口样本不足时的补齐桶）按静默零值（三字段全 0）表示。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq`（f32 三元组纯值载荷；
/// 不作 `Eq`——f32 无全序，含 NaN 样本时逐字段相等断言无意义）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PeakBucket {
    /// 桶内最小样本值。
    pub min: f32,
    /// 桶内最大样本值。
    pub max: f32,
    /// 桶内均方根 `sqrt(mean(x²))`：f64 累计平方和后回 f32。
    pub rms: f32,
}

impl PeakBucket {
    /// 静默零值桶（空桶补齐/无音频的显示语义）。
    fn silence() -> Self {
        PeakBucket {
            min: 0.0,
            max: 0.0,
            rms: 0.0,
        }
    }
}

/// 单桶累计器（纯核区间统计与消费助手流式喂桶共用的最小单元）。
///
/// f64 中间量累计平方和（模块文档「f64 防漂」）；min/max 用 [`f32::min`]
/// /[`f32::max`]（NaN 容忍——含 NaN 样本按「取另一侧」语义，不传播）。
///
/// derive 清单：`Clone, Copy`（`vec![..; n]` 批量初始化需要；私有类型
/// 不公开 derive 面，`Debug` 由 [`PeakBucket`] 承担）。
#[derive(Clone, Copy)]
struct PeakAccum {
    /// 迄今最小值（初值 +∞，空累计由 `count == 0` 判定）。
    min: f32,
    /// 迄今最大值（初值 -∞）。
    max: f32,
    /// 平方和（f64 累计）。
    sumsq: f64,
    /// 样本计数。
    count: u64,
}

impl PeakAccum {
    /// 空累计器。
    fn new() -> Self {
        PeakAccum {
            min: f32::INFINITY,
            max: f32::NEG_INFINITY,
            sumsq: 0.0,
            count: 0,
        }
    }

    /// 喂一个样本。
    fn feed(&mut self, x: f32) {
        self.min = self.min.min(x);
        self.max = self.max.max(x);
        let wide = f64::from(x);
        self.sumsq += wide * wide;
        self.count += 1;
    }

    /// 结算为峰值桶（空累计 → 静默零值）。
    fn peak(&self) -> PeakBucket {
        if self.count == 0 {
            return PeakBucket::silence();
        }
        PeakBucket {
            min: self.min,
            max: self.max,
            rms: (self.sumsq / self.count as f64).sqrt() as f32,
        }
    }
}

/// 共享分桶边界公式：桶 `k` 覆盖 `[min(k·L, N), min((k+1)·L, N))`。
///
/// 「两入口共享同一条边界公式」的落点——桶数/桶长入口都经本函数取
/// 边界；u128 乘法防 `k·L` 溢出 usize（两者均可近 `usize::MAX`），
/// 结果钳制到 `N`。
fn bucket_bounds(index: usize, len: usize, total: usize) -> (usize, usize) {
    let (index, len, total) = (index as u128, len as u128, total as u128);
    let start = (index * len).min(total);
    let end = (index * len + len).min(total);
    (start as usize, end as usize)
}

/// 单声道在帧区间 `[start, end)` 上的统计（交织样本按 `stride` 步进
/// 取本声道：声道 `c` 第 `i` 帧样本 = `samples[i·stride + c]`）。
fn range_peak(
    samples: &[f32],
    stride: usize,
    channel: usize,
    start: usize,
    end: usize,
) -> PeakBucket {
    let mut accum = PeakAccum::new();
    for frame in start..end {
        accum.feed(samples[frame * stride + channel]);
    }
    accum.peak()
}

/// 公共入参校验：声道数非 0、样本数为声道数整倍数（packed 交织须整帧）。
///
/// 返回每声道帧数。
fn validate_interleaved(samples: &[f32], channels: usize) -> Result<usize, ConvertError> {
    if channels == 0 {
        return Err(ConvertError::InvalidArguments(
            "波形分桶要求声道数 ≥ 1，收到 0".into(),
        ));
    }
    if !samples.len().is_multiple_of(channels) {
        return Err(ConvertError::InvalidData(format!(
            "packed 交织样本数须为声道数整倍数：{} 样本 ÷ {} 声道余 {}",
            samples.len(),
            channels,
            samples.len() % channels
        )));
    }
    Ok(samples.len() / channels)
}

/// 逐声道逐桶统计（`[声道][桶]`；`out_buckets` 含桶数入口的零值补齐）。
fn bucket_columns(
    samples: &[f32],
    channels: usize,
    len: usize,
    out_buckets: usize,
) -> Vec<Vec<PeakBucket>> {
    let frames = samples.len() / channels;
    (0..channels)
        .map(|channel| {
            (0..out_buckets)
                .map(|k| {
                    let (start, end) = bucket_bounds(k, len, frames);
                    range_peak(samples, channels, channel, start, end)
                })
                .collect()
        })
        .collect()
}

/// 纯核·桶数入口：按请求桶数分桶（边界公式见模块文档）。
///
/// 返回 `[声道][桶]`——内层恰 `buckets` 个（实桶不足请求桶数时尾部以
/// 静默零值桶补齐，**返回桶数恒等于请求值**，桶数 > 样本数时亦然）。
/// 每声道样本数为 0 → 空 Vec。
///
/// # Errors
/// `buckets == 0` 或 `channels == 0` → [`ConvertError::InvalidArguments`]；
/// 样本数非声道数整倍数 → [`ConvertError::InvalidData`]。
pub fn peaks_by_bucket_count(
    samples: &[f32],
    channels: usize,
    buckets: usize,
) -> Result<Vec<Vec<PeakBucket>>, ConvertError> {
    let frames = validate_interleaved(samples, channels)?;
    if buckets == 0 {
        return Err(ConvertError::InvalidArguments(
            "波形分桶要求桶数 ≥ 1，收到 0".into(),
        ));
    }
    if frames == 0 {
        return Ok(Vec::new());
    }
    let len = frames.div_ceil(buckets);
    Ok(bucket_columns(samples, channels, len, buckets))
}

/// 纯核·桶长入口：按每桶帧数分桶（边界公式见模块文档）。
///
/// 返回 `[声道][桶]`——内层恰 `ceil(N/桶长)` 个，**尾桶可不满**，无补齐。
/// 每声道样本数为 0 → 空 Vec。
///
/// # Errors
/// `samples_per_bucket == 0` 或 `channels == 0` →
/// [`ConvertError::InvalidArguments`]；样本数非声道数整倍数 →
/// [`ConvertError::InvalidData`]。
pub fn peaks_by_bucket_size(
    samples: &[f32],
    channels: usize,
    samples_per_bucket: usize,
) -> Result<Vec<Vec<PeakBucket>>, ConvertError> {
    let frames = validate_interleaved(samples, channels)?;
    if samples_per_bucket == 0 {
        return Err(ConvertError::InvalidArguments(
            "波形分桶要求桶长 ≥ 1，收到 0".into(),
        ));
    }
    if frames == 0 {
        return Ok(Vec::new());
    }
    Ok(bucket_columns(
        samples,
        channels,
        samples_per_bucket,
        frames.div_ceil(samples_per_bucket),
    ))
}

// ---------------------------------------------------------------------------
// 消费助手（mp4 + aac feature；纯核上方为无条件编译区）
// ---------------------------------------------------------------------------

#[cfg(all(feature = "mp4", feature = "aac"))]
use std::io::{Read, Seek, SeekFrom};
#[cfg(all(feature = "mp4", feature = "aac"))]
use std::sync::atomic::AtomicBool;
#[cfg(all(feature = "mp4", feature = "aac"))]
use std::sync::{Arc, Mutex, MutexGuard};

#[cfg(all(feature = "mp4", feature = "aac"))]
use ye_codec::{AudioFrame, CodecId, CodecRegistry, Frame, MediaType};
#[cfg(all(feature = "mp4", feature = "aac"))]
use ye_container::{ContainerError, ContainerRegistry, MediaHeader, ReadSeek};
#[cfg(all(feature = "mp4", feature = "aac"))]
use ye_pixfmt::SampleFormat;

#[cfg(all(feature = "mp4", feature = "aac"))]
use crate::convert::{VideoChain, convert_frames, filter_ops};

/// 波形结果（消费助手成功载荷）。
///
/// derive 清单：`Debug, Clone, PartialEq`（全纯值字段；不作 `Eq`——
/// [`PeakBucket`] 含 f32；不作 `Copy`——peaks 矩阵按值传递）。
#[cfg(all(feature = "mp4", feature = "aac"))]
#[derive(Debug, Clone, PartialEq)]
pub struct AudioPeaks {
    /// 采样率（Hz；首个解码帧口径，无解码帧为 0）。
    pub sample_rate: u32,
    /// 声道数（首个解码帧口径，无解码帧为 0）。
    pub channels: usize,
    /// 桶长（名义帧数：桶长入口 = 请求值；桶数入口 = `ceil(N/桶数)`。
    /// 实际桶边界按共享公式，桶间可有 ±1 帧差）。
    pub bucket_samples: usize,
    /// 桶数（桶数入口恒等于请求值；桶长入口 = `ceil(N/桶长)`；
    /// 无解码帧为 0）。
    pub buckets: usize,
    /// 峰值矩阵 `[声道][桶]`。
    pub peaks: Vec<Vec<PeakBucket>>,
}

/// 共享输入句柄：同一底层输入（`Arc<Mutex>`）供 probe 与解码泵各持
/// 一份克隆——probe 按值消费 `Box<dyn ReadSeek>`，不经共享则喂泵前
/// 无法回卷（模块文档「回卷」）。顺序使用（probe 先、泵后），锁无
/// 竞争。
#[cfg(all(feature = "mp4", feature = "aac"))]
struct SharedInput {
    /// 底层输入。
    inner: Arc<Mutex<Box<dyn ReadSeek>>>,
}

#[cfg(all(feature = "mp4", feature = "aac"))]
impl SharedInput {
    /// 包一份底层输入的克隆视图（喂给 probe / 泵的 `Box<dyn ReadSeek>`）。
    fn share(inner: &Arc<Mutex<Box<dyn ReadSeek>>>) -> Box<dyn ReadSeek> {
        Box::new(SharedInput {
            inner: inner.clone(),
        })
    }
}

#[cfg(all(feature = "mp4", feature = "aac"))]
impl Read for SharedInput {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        lock_input(&self.inner).read(buf)
    }
}

#[cfg(all(feature = "mp4", feature = "aac"))]
impl Seek for SharedInput {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        lock_input(&self.inner).seek(pos)
    }
}

/// 锁底层输入（中毒容忍：read/seek 无 panic 路径，锁中毒仅因其它线程
/// panic 中断 IO——输入是纯数据游标，无不变式可破坏，取内部句柄续用）。
#[cfg(all(feature = "mp4", feature = "aac"))]
fn lock_input(shared: &Arc<Mutex<Box<dyn ReadSeek>>>) -> MutexGuard<'_, Box<dyn ReadSeek>> {
    shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 消费助手注册表：容器 MP4 + 解码 AAC（`h264` 开启时顺带注册——
/// 含视频素材的视频帧解码后丢弃；未开时视频流以 `NotRegistered`
/// 透传，见模块文档）。
#[cfg(all(feature = "mp4", feature = "aac"))]
fn decode_registries() -> (ContainerRegistry, CodecRegistry) {
    let mut containers = ContainerRegistry::new();
    crate::container::mp4::register_mp4(&mut containers);
    let mut codecs = CodecRegistry::new();
    codecs.register_decoder(CodecId::Aac, crate::codec_aac::decoder_factory);
    #[cfg(feature = "h264")]
    codecs.register_decoder(CodecId::H264, crate::codec_h264::decoder_factory);
    (containers, codecs)
}

/// 前置流布局校验：**恰一条音频流**（模块文档「前置流布局校验」）。
#[cfg(all(feature = "mp4", feature = "aac"))]
fn validate_single_audio_stream(header: &MediaHeader) -> Result<(), ConvertError> {
    let audio = header
        .streams
        .iter()
        .filter(|stream| stream.media_type == MediaType::Audio)
        .count();
    match audio {
        0 => Err(ConvertError::InvalidArguments(format!(
            "波形要求恰一条音频流，{} 容器无音频流",
            header.format_name
        ))),
        1 => Ok(()),
        n => Err(ConvertError::InvalidArguments(format!(
            "波形要求恰一条音频流，检测到 {n} 条（Frame 不带流标识，多轨会静默混桶——拒绝）"
        ))),
    }
}

/// 消费助手公共前奏：probe 校验流布局 → 共享游标回卷 0（喂泵前）。
///
/// 返回共享句柄（供 [`convert_frames`] 经 [`SharedInput::share`] 持有）。
#[cfg(all(feature = "mp4", feature = "aac"))]
fn probe_and_rewind(
    containers: &ContainerRegistry,
    input: Box<dyn ReadSeek>,
) -> Result<Arc<Mutex<Box<dyn ReadSeek>>>, ConvertError> {
    let shared = Arc::new(Mutex::new(input));
    let (_format, header) = containers.probe(SharedInput::share(&shared))?;
    validate_single_audio_stream(&header)?;
    // probe 读头后游标停在头解析处：显式回卷 0 再喂泵（泵内 probe 窗口
    // 从当前位置读起，不回卷会拿 mdat 中段当探查窗口）。
    lock_input(&shared)
        .seek(SeekFrom::Start(0))
        .map_err(|err| ConvertError::Container(ContainerError::Io(err)))?;
    Ok(shared)
}

/// 帧级定标与样本解包（两入口共用）。
///
/// 首帧把 `scale.channels` 从 0 定为该帧声道数并记录采样率；中途声道
/// 数变化 → [`ConvertError::InvalidData`]（波形无法续算）。返回本帧的
/// packed f32 样本（小端解包；帧数据长度与 `帧数×声道数` 交叉校验）。
#[cfg(all(feature = "mp4", feature = "aac"))]
fn scaled_samples(scale: &mut (usize, u32), audio: &AudioFrame) -> Result<Vec<f32>, ConvertError> {
    if audio.format() != SampleFormat::F32 {
        return Err(ConvertError::InvalidData(format!(
            "波形助手仅支持 packed f32 音频帧，收到 {:?}",
            audio.format()
        )));
    }
    let channels = audio.channel_layout().channels() as usize;
    if scale.0 == 0 {
        scale.0 = channels;
        scale.1 = audio.sample_rate();
    } else if channels != scale.0 {
        return Err(ConvertError::InvalidData(format!(
            "音频声道数中途变化（{} → {}），波形无法续算",
            scale.0, channels
        )));
    }
    // as_chunks 的余数尾巴丢弃：帧缓冲长度经 AudioFrame 不变式与
    // `帧数×声道数×4` 一致，尾巴理论上不存在，长度交叉校验兜底在下方。
    let samples: Vec<f32> = audio
        .data()
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        .collect();
    if samples.len() != audio.frames() * channels {
        return Err(ConvertError::InvalidData(format!(
            "音频帧数据长度与 帧数×声道数 不符（{} ≠ {}×{}）",
            samples.len(),
            audio.frames(),
            channels
        )));
    }
    Ok(samples)
}

/// 流式分桶游标（桶长入口）：帧到达即喂、满桶即闭，EOF 冲刷尾桶
/// （不满也闭——与纯核桶长入口边界一致）。
#[cfg(all(feature = "mp4", feature = "aac"))]
struct StreamBuckets {
    /// 桶长（请求值）。
    bucket_samples: usize,
    /// 定标（声道数 0 = 首帧未到；采样率）。
    scale: (usize, u32),
    /// 已闭合桶（`[声道][桶]`；首帧时按声道数建列）。
    columns: Vec<Vec<PeakBucket>>,
    /// 当前桶逐声道累计。
    current: Vec<PeakAccum>,
    /// 当前桶已收帧数。
    filled: usize,
}

#[cfg(all(feature = "mp4", feature = "aac"))]
impl StreamBuckets {
    /// 空游标（bucket_samples 已由入口校验非 0）。
    fn new(bucket_samples: usize) -> Self {
        StreamBuckets {
            bucket_samples,
            scale: (0, 0),
            columns: Vec::new(),
            current: Vec::new(),
            filled: 0,
        }
    }

    /// 喂一帧（定标/解包见 [`scaled_samples`]）。
    fn feed(&mut self, audio: &AudioFrame) -> Result<(), ConvertError> {
        let samples = scaled_samples(&mut self.scale, audio)?;
        let channels = self.scale.0;
        if self.columns.is_empty() {
            self.columns = vec![Vec::new(); channels];
            self.current = vec![PeakAccum::new(); channels];
        }
        for frame in 0..audio.frames() {
            for channel in 0..channels {
                self.current[channel].feed(samples[frame * channels + channel]);
            }
            self.filled += 1;
            if self.filled == self.bucket_samples {
                self.close_bucket();
            }
        }
        Ok(())
    }

    /// 闭合当前桶（各声道列各推进一个 PeakBucket）。
    fn close_bucket(&mut self) {
        for (column, accum) in self.columns.iter_mut().zip(&self.current) {
            column.push(accum.peak());
        }
        for accum in &mut self.current {
            *accum = PeakAccum::new();
        }
        self.filled = 0;
    }

    /// EOF 冲刷尾桶（不满也闭）并产出结果。
    fn finish(mut self) -> AudioPeaks {
        if self.scale.0 > 0 && self.filled > 0 {
            self.close_bucket();
        }
        let buckets = self.columns.first().map_or(0, Vec::len);
        AudioPeaks {
            sample_rate: self.scale.1,
            channels: self.scale.0,
            bucket_samples: self.bucket_samples,
            buckets,
            peaks: self.columns,
        }
    }
}

/// 波形消费助手·桶长入口：解码全流程 + **流式喂桶**（满桶即闭，内存
/// 与素材长无关）。
///
/// 行为契约见模块文档（probe 校验恰一音频流 → 回卷 0 →
/// [`convert_frames`] → 音频帧喂 [`StreamBuckets`]，视频帧解码后丢弃；
/// 解码 Err 原样透传）。
///
/// # Errors
/// `bucket_samples == 0` → [`ConvertError::InvalidArguments`]；流布局
/// 校验/解封装/解码失败原样透传（不兜底）。
#[cfg(all(feature = "mp4", feature = "aac"))]
pub fn audio_peaks(
    input: Box<dyn ReadSeek>,
    bucket_samples: usize,
    cancel: &AtomicBool,
) -> Result<AudioPeaks, ConvertError> {
    if bucket_samples == 0 {
        return Err(ConvertError::InvalidArguments(
            "波形桶长入口要求桶长 ≥ 1，收到 0".into(),
        ));
    }
    let (containers, codecs) = decode_registries();
    let shared = probe_and_rewind(&containers, input)?;
    let mut buckets = StreamBuckets::new(bucket_samples);
    let mut sink = |frame: Frame| match frame {
        Frame::Audio(audio) => buckets.feed(&audio),
        // 前置校验已限恰一音频流；视频流可有可无，帧解码后在此丢弃
        //（Baseline 可解、High 解码 Err 整体透传——回退归 jian 命令层）。
        Frame::Video(_) => Ok(()),
    };
    convert_frames(
        &containers,
        &codecs,
        SharedInput::share(&shared),
        VideoChain {
            format: None,
            size: None,
        },
        filter_ops(),
        &mut sink,
        &|_processed, _total| {},
        cancel,
    )?;
    Ok(buckets.finish())
}

/// 波形消费助手·桶数入口：解码收全样本再按数分桶（纯核
/// [`peaks_by_bucket_count`] 语义——**返回桶数恒等于请求值**，尾桶可
/// 不满）。
///
/// 内存局限：收全样本（f32 × 声道 × 帧数）随素材长线性增长——长素材
/// 优化归 M4（m2-jian-plan §6）。
///
/// # Errors
/// `bucket_count == 0` → [`ConvertError::InvalidArguments`]；流布局
/// 校验/解封装/解码失败原样透传（不兜底）。
#[cfg(all(feature = "mp4", feature = "aac"))]
pub fn audio_peaks_with_count(
    input: Box<dyn ReadSeek>,
    bucket_count: usize,
    cancel: &AtomicBool,
) -> Result<AudioPeaks, ConvertError> {
    if bucket_count == 0 {
        return Err(ConvertError::InvalidArguments(
            "波形桶数入口要求桶数 ≥ 1，收到 0".into(),
        ));
    }
    let (containers, codecs) = decode_registries();
    let shared = probe_and_rewind(&containers, input)?;
    let mut samples: Vec<f32> = Vec::new();
    let mut scale = (0usize, 0u32);
    let mut sink = |frame: Frame| match frame {
        Frame::Audio(audio) => {
            let frame_samples = scaled_samples(&mut scale, &audio)?;
            samples.extend_from_slice(&frame_samples);
            Ok(())
        }
        Frame::Video(_) => Ok(()),
    };
    convert_frames(
        &containers,
        &codecs,
        SharedInput::share(&shared),
        VideoChain {
            format: None,
            size: None,
        },
        filter_ops(),
        &mut sink,
        &|_processed, _total| {},
        cancel,
    )?;
    let (channels, sample_rate) = scale;
    let peaks = peaks_by_bucket_count(&samples, channels, bucket_count)?;
    // channels == 0（无解码帧）→ 0 帧；peaks 亦为空 Vec。
    let frames = samples.len().checked_div(channels).unwrap_or(0);
    Ok(AudioPeaks {
        sample_rate,
        channels,
        bucket_samples: if frames > 0 {
            frames.div_ceil(bucket_count)
        } else {
            0
        },
        buckets: peaks.first().map_or(0, Vec::len),
        peaks,
    })
}

/// Presentation-aligned, bounded waveform for product timelines. Decode only audio;
/// retain priming for overlap reconstruction, but exclude it from visible buckets.
#[cfg(all(feature = "mp4", feature = "aac"))]
pub fn audio_peaks_aligned_with_count(
    input: Box<dyn ReadSeek>,
    bucket_count: usize,
    cancel: &AtomicBool,
) -> Result<AudioPeaks, ConvertError> {
    audio_peaks_aligned_stream_with_count(input, None, bucket_count, cancel)
}

/// Presentation-aligned waveform for one explicit container audio stream.
#[cfg(all(feature = "mp4", feature = "aac"))]
pub fn audio_peaks_aligned_stream_with_count(
    input: Box<dyn ReadSeek>,
    stream_index: Option<u32>,
    bucket_count: usize,
    cancel: &AtomicBool,
) -> Result<AudioPeaks, ConvertError> {
    if bucket_count == 0 || bucket_count > 8192 {
        return Err(ConvertError::InvalidArguments("波形桶数须在1..8192".into()));
    }
    let (containers, codecs) = decode_registries();
    let mut source = crate::timeline::Source::open_selected(
        &containers,
        &codecs,
        input,
        MediaType::Audio,
        0,
        None,
        stream_index,
    )?;
    let params = source
        .stream
        .codec
        .audio
        .as_ref()
        .ok_or_else(|| ConvertError::InvalidData("缺音频参数".into()))?;
    let sample_rate = params.sample_rate;
    let channels = params.channel_layout.map_or(0, |l| l.channels() as usize);
    if sample_rate == 0 || !(1..=2).contains(&channels) {
        return Err(ConvertError::InvalidArguments("波形要求单/双声道".into()));
    }
    let duration = source
        .stream
        .duration
        .ok_or_else(|| ConvertError::InvalidArguments("波形要求已知呈现时长".into()))?;
    let end = duration.checked_add(
        source
            .stream
            .start_time
            .unwrap_or(ye_time::Timestamp::new(0, source.stream.time_base)),
    )?;
    let time_base = ye_time::Rational::new(1, sample_rate as i32);
    let valid_start = source
        .stream
        .start_time
        .unwrap_or(ye_time::Timestamp::new(0, time_base))
        .rescale_with(time_base, ye_time::Rounding::Up)?
        .ticks();
    let total = usize::try_from(end.rescale_with(time_base, ye_time::Rounding::Up)?.ticks())
        .map_err(|_| ConvertError::InvalidData("无效音频时长".into()))?;
    if total == 0 {
        return Err(ConvertError::InvalidData("音频时长为零".into()));
    }
    let len = total.div_ceil(bucket_count);
    let mut accum = vec![vec![PeakAccum::new(); bucket_count]; channels];
    let mut cursor = 0usize;
    let mut saw = false;
    fn silence(accum: &mut [Vec<PeakAccum>], cursor: &mut usize, end: usize, len: usize) {
        while *cursor < end {
            let bucket = *cursor / len;
            let stop = end.min((bucket + 1) * len);
            let n = (stop - *cursor) as u64;
            for row in accum.iter_mut() {
                let b = &mut row[bucket];
                b.min = b.min.min(0.0);
                b.max = b.max.max(0.0);
                b.count += n;
            }
            *cursor = stop;
        }
    }
    while let Some(frame) = source.next(cancel)? {
        let Frame::Audio(frame) = frame else {
            return Err(ConvertError::InvalidData("音频解码器返回视频".into()));
        };
        if frame.format() != ye_pixfmt::SampleFormat::F32
            || frame.sample_rate() != sample_rate
            || frame.channel_layout().channels() as usize != channels
            || !frame.data().len().is_multiple_of(4 * channels)
        {
            return Err(ConvertError::InvalidData("音频参数变化或 PCM 损坏".into()));
        }
        saw = true;
        let start = frame
            .pts()
            .ok_or_else(|| ConvertError::InvalidData("音频缺 PTS".into()))?
            .rescale(time_base)?
            .ticks();
        for (i, pcm) in frame.data().chunks_exact(4 * channels).enumerate() {
            let at = start.saturating_add(i as i64);
            if at < valid_start || at < 0 || at < cursor as i64 {
                continue;
            }
            if at >= total as i64 {
                break;
            }
            let at = at as usize;
            silence(&mut accum, &mut cursor, at, len);
            for (channel, bytes) in pcm.chunks_exact(4).enumerate() {
                let value = f32::from_le_bytes(
                    bytes
                        .try_into()
                        .map_err(|_| ConvertError::InvalidData("PCM".into()))?,
                );
                if !value.is_finite() {
                    return Err(ConvertError::InvalidData("非有限 PCM".into()));
                }
                accum[channel][at / len].feed(value);
            }
            cursor = at + 1;
        }
        if cursor >= total {
            break;
        }
    }
    if !saw {
        return Err(ConvertError::InvalidData("没有可解码的音频帧".into()));
    }
    silence(&mut accum, &mut cursor, total, len);
    Ok(AudioPeaks {
        sample_rate,
        channels,
        bucket_samples: len,
        buckets: bucket_count,
        peaks: accum
            .into_iter()
            .map(|row| row.iter().map(PeakAccum::peak).collect())
            .collect(),
    })
}

// ---------------------------------------------------------------------------
// 单元测试（纯核无条件；消费助手件随 feature 门）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 纯核（无条件编译，进默认门禁基线） ----

    /// 空样本：两入口都返回空桶列表（无音频即无桶）。
    #[test]
    fn 空样本两入口都返回空桶列表() {
        assert!(
            peaks_by_bucket_count(&[], 2, 10)
                .expect("空样本分桶应成功")
                .is_empty()
        );
        assert!(
            peaks_by_bucket_size(&[], 2, 10)
                .expect("空样本分桶应成功")
                .is_empty()
        );
    }

    /// 桶长入口尾桶不满：10 帧桶长 4 → 3 桶（4/4/2），逐桶数值与手算一致。
    #[test]
    fn 桶长入口尾桶不满且逐桶数值精确() {
        // 样本 = 帧号（单声道），平方和可手算。
        let samples: Vec<f32> = (0..10).map(|i| i as f32).collect();
        let buckets = peaks_by_bucket_size(&samples, 1, 4).expect("分桶应成功");
        assert_eq!(buckets.len(), 1, "单声道一行");
        let row = &buckets[0];
        assert_eq!(row.len(), 3, "ceil(10/4) = 3 桶");
        let exact = |sumsq: f64, count: f64| (sumsq / count).sqrt() as f32;
        assert_eq!(
            row[0],
            PeakBucket {
                min: 0.0,
                max: 3.0,
                rms: exact(0.0 + 1.0 + 4.0 + 9.0, 4.0)
            }
        );
        assert_eq!(
            row[1],
            PeakBucket {
                min: 4.0,
                max: 7.0,
                rms: exact(16.0 + 25.0 + 36.0 + 49.0, 4.0)
            }
        );
        // 尾桶 [8,10)：只 2 帧（不满）。
        assert_eq!(
            row[2],
            PeakBucket {
                min: 8.0,
                max: 9.0,
                rms: exact(64.0 + 81.0, 2.0)
            }
        );
    }

    /// 桶数入口桶数超过样本数：实桶后补静默零值桶，返回桶数等于请求值。
    #[test]
    fn 桶数超过样本数时补齐静默桶且桶数等于请求值() {
        let samples = [1.0_f32, -2.0, 3.0];
        let buckets = peaks_by_bucket_count(&samples, 1, 5).expect("分桶应成功");
        assert_eq!(buckets[0].len(), 5, "返回桶数 = 请求值");
        // 桶长 = ceil(3/5) = 1：前三桶各 1 样本，后两桶静默零值。
        assert_eq!(
            buckets[0][0],
            PeakBucket {
                min: 1.0,
                max: 1.0,
                rms: 1.0
            }
        );
        assert_eq!(
            buckets[0][1],
            PeakBucket {
                min: -2.0,
                max: -2.0,
                rms: 2.0
            }
        );
        assert_eq!(
            buckets[0][2],
            PeakBucket {
                min: 3.0,
                max: 3.0,
                rms: 3.0
            }
        );
        assert_eq!(buckets[0][3], PeakBucket::silence());
        assert_eq!(buckets[0][4], PeakBucket::silence());
    }

    /// 桶数入口不整除时仍返回请求桶数：9 帧 4 桶 → 3 实桶 + 1 补齐。
    #[test]
    fn 桶数入口不整除时仍返回请求桶数() {
        let samples: Vec<f32> = (0..9).map(|i| i as f32).collect();
        let buckets = peaks_by_bucket_count(&samples, 1, 4).expect("分桶应成功");
        assert_eq!(buckets[0].len(), 4);
        // 桶长 = ceil(9/4) = 3：实桶 [0,3),[3,6),[6,9)，第 4 桶补齐。
        assert_eq!(buckets[0][0].max, 2.0);
        assert_eq!(buckets[0][2].max, 8.0);
        assert_eq!(buckets[0][3], PeakBucket::silence());
    }

    /// 非法入参被拒绝：桶数 0 / 桶长 0 / 声道数 0 / 样本非整帧。
    #[test]
    fn 桶数桶长声道数为零与非整帧样本都被拒绝() {
        let samples = [0.5_f32, -0.5, 0.25];
        assert!(matches!(
            peaks_by_bucket_count(&samples, 1, 0),
            Err(ConvertError::InvalidArguments(_))
        ));
        assert!(matches!(
            peaks_by_bucket_size(&samples, 1, 0),
            Err(ConvertError::InvalidArguments(_))
        ));
        assert!(matches!(
            peaks_by_bucket_count(&samples, 0, 4),
            Err(ConvertError::InvalidArguments(_))
        ));
        // 3 样本 2 声道余 1 → InvalidData（packed 交织须整帧）。
        assert!(matches!(
            peaks_by_bucket_count(&samples, 2, 4),
            Err(ConvertError::InvalidData(_))
        ));
        assert!(matches!(
            peaks_by_bucket_size(&samples, 2, 4),
            Err(ConvertError::InvalidData(_))
        ));
    }

    /// 立体声交织样本按声道独立分桶：两声道内容互不串桶。
    #[test]
    fn 立体声交织样本按声道独立分桶() {
        // 左 = 帧号，右 = -2×帧号，交织 [L0, R0, L1, R1, …]。
        let mut samples = Vec::new();
        for i in 0..8u32 {
            samples.push(i as f32);
            samples.push(-2.0 * (i as f32));
        }
        let buckets = peaks_by_bucket_size(&samples, 2, 4).expect("分桶应成功");
        assert_eq!(buckets.len(), 2, "两声道各一行");
        let exact = |sumsq: f64, count: f64| (sumsq / count).sqrt() as f32;
        // 桶 0：左 [0..4) min 0 max 3；右 [0,-2,-4,-6] min -6 max 0。
        assert_eq!(
            buckets[0][0],
            PeakBucket {
                min: 0.0,
                max: 3.0,
                rms: exact(14.0, 4.0)
            }
        );
        assert_eq!(buckets[1][0].min, -6.0);
        assert_eq!(buckets[1][0].max, 0.0);
        assert_eq!(buckets[1][0].rms, exact(0.0 + 4.0 + 16.0 + 36.0, 4.0));
        // 桶 1：左 [4..8)；右 [-8,-10,-12,-14]。
        assert_eq!(
            buckets[0][1],
            PeakBucket {
                min: 4.0,
                max: 7.0,
                rms: exact(126.0, 4.0)
            }
        );
        assert_eq!(buckets[1][1].min, -14.0);
        assert_eq!(buckets[1][1].max, -8.0);
        assert_eq!(buckets[1][1].rms, exact(504.0, 4.0));
    }

    /// 正弦样本每桶 min/max/rms 符合解析值（1kHz@48k、100 周期、10 桶）。
    #[test]
    fn 正弦样本每桶峰值符合解析值() {
        let (amp, freq, rate, frames) = (0.8_f32, 1000.0_f64, 48_000_u32, 4_800_usize);
        let samples: Vec<f32> = (0..frames)
            .map(|i| {
                let t = i as f64 / f64::from(rate);
                (f64::from(amp) * (std::f64::consts::TAU * freq * t).sin()) as f32
            })
            .collect();
        let buckets = peaks_by_bucket_count(&samples, 1, 10).expect("分桶应成功");
        assert_eq!(buckets[0].len(), 10);
        for (k, bucket) in buckets[0].iter().enumerate() {
            // 每桶 480 帧 = 10 个整周期：过峰 ±A，rms = A/√2。
            assert!((bucket.max - amp).abs() < 1e-4, "桶 {k} max {}", bucket.max);
            assert!((bucket.min + amp).abs() < 1e-4, "桶 {k} min {}", bucket.min);
            let want = f64::from(amp) / std::f64::consts::SQRT_2;
            assert!(
                (f64::from(bucket.rms) - want).abs() < 1e-3,
                "桶 {k} rms {}",
                bucket.rms
            );
        }
    }

    // ---- 消费助手（mp4 + aac feature） ----

    /// 造一个给定流类型的 MediaHeader（流布局校验单测用）。
    #[cfg(all(test, feature = "mp4", feature = "aac"))]
    fn header_of(types: &[MediaType]) -> MediaHeader {
        use ye_codec::{CodecId, CodecParameters};
        use ye_container::{Metadata, Stream};
        use ye_time::Rational;
        let streams = types
            .iter()
            .enumerate()
            .map(|(index, media_type)| Stream {
                index: index as u32,
                media_type: *media_type,
                codec: CodecParameters::new(
                    match media_type {
                        MediaType::Audio => CodecId::Aac,
                        MediaType::Video => CodecId::H264,
                        MediaType::Subtitle => CodecId::Aac,
                    },
                    *media_type,
                ),
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

    /// 流布局校验拒绝纯视频头（无音频流）。
    #[cfg(all(test, feature = "mp4", feature = "aac"))]
    #[test]
    fn 流布局校验拒绝纯视频头() {
        let header = header_of(&[MediaType::Video, MediaType::Video]);
        assert!(matches!(
            validate_single_audio_stream(&header),
            Err(ConvertError::InvalidArguments(_))
        ));
    }

    /// 流布局校验拒绝双音轨头（Frame 无流标识，多轨混桶是静默错数据）。
    #[cfg(all(test, feature = "mp4", feature = "aac"))]
    #[test]
    fn 流布局校验拒绝双音轨头() {
        let header = header_of(&[MediaType::Audio, MediaType::Audio]);
        assert!(matches!(
            validate_single_audio_stream(&header),
            Err(ConvertError::InvalidArguments(_))
        ));
    }

    /// 流布局校验放行恰一条音频流（纯音频或带视频轨均可）。
    #[cfg(all(test, feature = "mp4", feature = "aac"))]
    #[test]
    fn 流布局校验放行恰一条音频流() {
        let audio_only = header_of(&[MediaType::Audio]);
        assert!(validate_single_audio_stream(&audio_only).is_ok());
        let with_video = header_of(&[MediaType::Video, MediaType::Audio]);
        assert!(validate_single_audio_stream(&with_video).is_ok());
    }

    /// 流式喂桶与纯核桶长入口逐桶一致（桶边界跨帧也一致）。
    #[cfg(all(test, feature = "mp4", feature = "aac"))]
    #[test]
    fn 流式喂桶与纯核桶长入口逐桶一致() {
        use ye_codec::AudioFrame;
        use ye_pixfmt::{ChannelLayout, SampleFormat};
        use ye_time::Rational;
        // 3 帧 × 每帧 5 样本 × 2 声道，桶长 4：桶边界跨帧（5 不被 4 整除）。
        let mut all: Vec<f32> = Vec::new();
        let mut stream = StreamBuckets::new(4);
        for frame_index in 0..3u32 {
            let mut frame = AudioFrame::new(
                SampleFormat::F32,
                ChannelLayout::STEREO,
                48_000,
                5,
                Some(ye_time::Timestamp::new(
                    i64::from(frame_index) * 5,
                    Rational::new(1, 48_000),
                )),
            )
            .expect("构造音频帧");
            let mut bytes = Vec::with_capacity(40);
            for i in 0..5u32 {
                let left = (frame_index * 5 + i) as f32;
                let right = -left;
                bytes.extend_from_slice(&left.to_le_bytes());
                bytes.extend_from_slice(&right.to_le_bytes());
                all.push(left);
                all.push(right);
            }
            frame.data_mut().copy_from_slice(&bytes);
            stream.feed(&frame).expect("喂帧应成功");
        }
        let got = stream.finish();
        let want = peaks_by_bucket_size(&all, 2, 4).expect("纯核分桶应成功");
        assert_eq!(got.channels, 2);
        assert_eq!(got.sample_rate, 48_000);
        assert_eq!(got.bucket_samples, 4);
        assert_eq!(got.buckets, 4, "15 帧 ÷ 桶长 4 → 4 桶");
        assert_eq!(got.peaks, want, "流式与纯核逐桶一致");
    }
}
