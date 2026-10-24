//! MP4 读取器：box 树解析、trak 元数据提取与样例随机访问。
//!
//! 解析规则（硬契约）：
//! 1. 所有整数字段大端。
//! 2. box 头 size u32 + type；size==0 ⇒ 延伸到父容器末（顶层到 EOF）；
//!    size==1 ⇒ 后跟 8 字节 largesize(u64)；size 小于头长 ⇒ Malformed；
//!    box 越出父容器 ⇒ Malformed。
//! 3. 递归容器 moov/trak/mdia/minf/stbl 与 moof/traf/mvex；其余未知 box
//!    （udta/edts/free/wide/mfra/tfra/mfro/sidx/styp/skip/…，顶层与容器内
//!    同样处理）按大小跳过；容器嵌套深度上限 64 层，超限 ⇒ Malformed
//!    （防栈溢出）。
//! 4. stsd entry_count 必须为 1；宽高/声道采样率取 entry 固定字段，
//!    第一个子 box 按其声明 size 截取的载荷进 extra_data
//!    （size==0 ⇒ 到 entry 末；越出 entry ⇒ Malformed）。
//! 5. mdhd/tkhd 先读版本字节再按 v0/v1 字段宽度解析。
//! 6. read_packet 分配前校验样例 offset+size ≤ 文件长度，越界 ⇒ Malformed。
//!
//! 时戳语义（2026-10-24 移植裁决）：样例时戳一律以流时基
//! （`1/mdhd:timescale`）的**原生轨道单位**打戳（`Timestamp::new(单位,
//! 流时基)`），无全局 tick 换算、无舍入；`pts` 恒 `Some(dts+cts)`
//! （ctts 缺省 ⇒ cts=0）。取代 m1 的全局 tick（7,056,000）方案。
//!
//! fMP4（fragment）硬契约：
//! 7. mvex 按 track_id 关联 trak：mehd 可选（跳过）；trex 记录
//!    default_sample_description_index/duration/size/flags。moof 递归
//!    mfhd（跳过）/traf；traf 递归 tfhd/tfdt/trun，tfhd 须先于 tfdt/trun
//!    出现（tfdt/trun 的 track_id 由 tfhd 携带）。
//! 8. 基值解析：tfhd 0x000001 置位 ⇒ 显式 base_data_offset；否则
//!    0x020000 default-base-is-moof ⇒ moof 起点；否则 ⇒ 前一 trun 数据
//!    末尾（解析期按轨维护游标）；首个 traf 无任何基值信息 ⇒ 兜底取
//!    moof 起点（罕见形态）。trun data-offset-present==0 ⇒ 样例数据接
//!    当前游标（前一 trun 数据末尾）。
//! 9. 样例字段优先链：trun 逐样例显式（存在位 duration=0x100/size=0x200/
//!    flags=0x400/cts=0x800，24 位 flags 内）> tfhd 默认 > trex 默认。
//!    duration/size 全缺省 ⇒ 0（破损文件按 0 容忍）；flags 另有首样例
//!    特例——首样例且 trun 置 first-sample-flags-present(0x000004) ⇒
//!    first_sample_flags 覆盖该样例整个 flags；全缺省 ⇒ flags 未知。
//!    实测 ffmpeg 形态即 first_sample_flags + tfhd 默认链（trun flags
//!    =0x00000a05：data-offset|first-sample-flags|size|cts，无逐样例
//!    flags；tfhd 默认 flags=0x01010000）。
//! 10. keyframe = !(有效 flags & 0x00010000)（sample_is_non_sync_sample
//!     位；实测 ffmpeg default flags 0x01010000 标非关键帧、
//!     first_sample_flags 0x02000000 标关键帧，判定位即 0x00010000）；
//!     flags 全缺省 ⇒ true（与 progressive 无 stss 表恒关键帧的语义
//!     对齐，见 [`TrackTables::is_keyframe`]）；音频轨恒 true。
//! 11. tfdt v0(u32)/v1(u64) 均解析，为 fragment 首 dts（轨道时间单位，
//!     缺 tfdt 则接前一 fragment 尾 dts）；trun v0 cts 无符号、v1 有符号。
//! 12. 形态判定：无 moof ⇒ 纯 progressive 语义，mvex/trex 仅记录不参与
//!     （mvex+非空 stbl+无 moof 的文件正常打开读样例）；有 moof ⇒
//!     fragment 样例接在轨样例序之后统一编址，sample_count =
//!     Σ(stbl+fragment) 样例数，Stream.duration = 末 fragment 尾
//!     dts+dur（mdhd/mehd 为 0 时亦然）；同轨 stbl 非空样例又遇 moof
//!     样例 ⇒ Unsupported("混合 progressive/fragmented 轨道")。moov 内
//!     空样例表（entry_count=0，ffmpeg empty_moov 实测形态）在 mvex
//!     存在时合法。
//! 13. hdlr 兼容两种布局：handler 紧随 version/flags（本 crate 早期手工
//!     测试字节、Mp4Writer 的 pre_defined 双写）与 ISO 布局（pre_defined
//!     之后才是 handler_type，ffmpeg 实测产物）；mp4a entry 兼容 20 字节
//!     基础布局与 28 字节扩展布局（声道/采样率前多 8 字节，ffmpeg 产物
//!     与 Mp4Writer 输出均为后者），按采样率/声道合理区间判别。
//!
//! 媒体类别映射：hdlr 非 vide/soun 的轨内部按 [`TrackKind::Other`]
//! 处理（entry 布局取 8 字节基座、关键帧走 stss 链），公开面映射
//! `MediaType::Subtitle` + `CodecId::Unknown`（develop 的 `MediaType`
//! 无 Other；裁决见 docs/contracts.md）。

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

use ye_codec::{AudioParams, CodecParameters, MediaType, Packet, PacketFlags, VideoParams};
use ye_container::{Metadata, Stream};
use ye_time::{Rational, Timestamp};

use crate::box_tree::{BoxInfo, BoxNode};
use crate::error::{Mp4Error, malformed};
use crate::types::{FourCc, codec_id_of, default_channel_layout};

type Res<T> = std::result::Result<T, Mp4Error>;

/// 轨道媒体类别（内部三值；公开面映射 [`MediaType`]，Other→Subtitle）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum TrackKind {
    Video,
    Audio,
    Other,
}

impl TrackKind {
    fn media_type(self) -> MediaType {
        match self {
            TrackKind::Video => MediaType::Video,
            TrackKind::Audio => MediaType::Audio,
            TrackKind::Other => MediaType::Subtitle,
        }
    }
}

/// 轨道时间值（timescale 单位，u64 源）→ i64 tick 数。
/// 单位域即 tick 域，无换算无舍入；落不回 i64 ⇒ Malformed。
fn u64_ticks(v: u64, path: &str, what: &str) -> Res<i64> {
    i64::try_from(v).map_err(|_| malformed(path, format!("{what} 超出可表示范围")))
}

// —— 样例表数据 ——

/// stts 的一段：(样例数, 每样例时长)。
struct SttsRun {
    count: u64,
    delta: u64,
}

/// ctts 的一段：(样例数, pts 相对 dts 的偏移；v1 可为负)。
struct CttsRun {
    count: u64,
    offset: i64,
}

/// stsc 的一段：(首 chunk 序号 1-based, 每 chunk 样例数)。
struct StscEntry {
    first_chunk: u64,
    samples_per_chunk: u64,
}

/// stsz：定长（sample_size!=0）或变长表。
struct StszData {
    fixed: Option<u64>,
    sizes: Vec<u64>,
    count: u64,
}

// —— fragment（fMP4）数据 ——

/// mvex/trex 记录的缺省值，按 track_id 关联 trak。
#[derive(Clone, Copy)]
struct Trex {
    track_id: u32,
    /// 契约要求记录；本 crate stsd entry_count 恒 1，不参与样例定位。
    #[allow(dead_code)]
    default_sample_description_index: u32,
    default_sample_duration: u32,
    default_sample_size: u32,
    default_sample_flags: u32,
}

/// 一个 fragment 样例（统一编址时接在 stbl 样例之后）。
struct FragSample {
    /// dts（轨道时间单位，tfdt/前一 fragment 尾累计）。
    dts: u64,
    /// 时长（轨道时间单位）。
    duration: u64,
    /// 绝对文件偏移与大小。
    offset: u64,
    size: u64,
    /// 样例 flags；None ⇒ 三级缺省后仍未知。
    flags: Option<u32>,
    /// pts 相对 dts 的偏移（轨道单位）；None ⇒ pts==dts。
    cts: Option<i64>,
}

/// 解析期按 track_id 累积的 fragment 状态（moof 可能出现在 moov 之前，
/// 故不挂在 TrackBuilder 上，from_reader 末尾统一关联）。
struct FragTrack {
    track_id: u32,
    samples: Vec<FragSample>,
    /// 上一 trun 数据末尾（绝对偏移）——基值缺省时的接续游标；
    /// None ⇒ 尚未解析任何数据。
    cursor: Option<u64>,
    /// 下一 fragment 样例的 dts（轨道单位）；tfdt 覆盖，否则接续累计。
    next_dts: u64,
    /// 最近一条 trun 的 box 路径（错误定位）。
    last_path: String,
}

/// traf 解析的瞬态状态：进入新 traf 时重置，tfhd 填充，trun 消费。
#[derive(Clone, Copy)]
struct TrafState {
    track_id: u32,
    /// 本 traf 的基值（tfhd 决定：显式/default-base-is-moof/接续游标）。
    base: u64,
    def_duration: Option<u32>,
    def_size: Option<u32>,
    def_flags: Option<u32>,
}

/// 一路 trak 的样例表，`read_packet` 随机访问的依据。
struct TrackTables {
    kind: TrackKind,
    track_id: u32,
    /// stsd 首 entry 类型（avc1/mp4a/…）。
    codec: FourCc,
    timebase: Rational,
    /// 如 `"moov.trak[0]"`，错误定位用。
    path: String,
    stts: Vec<SttsRun>,
    ctts: Option<Vec<CttsRun>>,
    /// 1-based 样号集合；None ⇒ 无 stss 表。
    stss: Option<HashSet<u64>>,
    stsc: Vec<StscEntry>,
    fixed_size: Option<u64>,
    sizes: Vec<u64>,
    /// stbl 样例数：样例序号小于它走 stbl 表，否则走 fragment 样例。
    stbl_count: u64,
    /// 统一样例总数（stbl + fragment）。
    sample_count: u64,
    chunk_offsets: Vec<u64>,
    /// "stco" 或 "co64"，错误路径用。
    chunk_box: &'static str,
    /// fragment 样例（统一编址接在 stbl 样例之后；无 moof 则空）。
    frags: Vec<FragSample>,
    /// 最近一条 trun 的 box 路径（fragment 样例错误定位）。
    frag_path: String,
}

impl TrackTables {
    /// 第 sample 个样例（0-based）的 (dts, 时长)，轨道时间单位。
    /// dts 为 stts 前缀和。
    fn stts_at(&self, sample: u64) -> Res<(u64, u64)> {
        let path = format!("{}.mdia.minf.stbl.stts", self.path);
        let mut base = 0u64;
        let mut acc = 0u64;
        for r in &self.stts {
            let end = base
                .checked_add(r.count)
                .ok_or_else(|| malformed(&path, "样例计数溢出"))?;
            if sample < end {
                // dts = 前段累计 + 段内位置 × delta。
                let within = sample - base;
                let dts = acc
                    .checked_add(
                        within
                            .checked_mul(r.delta)
                            .ok_or_else(|| malformed(&path, "时长累计溢出"))?,
                    )
                    .ok_or_else(|| malformed(&path, "时长累计溢出"))?;
                return Ok((dts, r.delta));
            }
            base = end;
            acc = acc
                .checked_add(
                    r.delta
                        .checked_mul(r.count)
                        .ok_or_else(|| malformed(&path, "时长累计溢出"))?,
                )
                .ok_or_else(|| malformed(&path, "时长累计溢出"))?;
        }
        Err(malformed(
            &path,
            format!("样例 {sample} 超出 stts 覆盖范围"),
        ))
    }

    /// ctts 偏移（轨道时间单位）；无 ctts 表 ⇒ None。
    fn ctts_at(&self, sample: u64) -> Res<Option<i64>> {
        let Some(runs) = &self.ctts else {
            return Ok(None);
        };
        let path = format!("{}.mdia.minf.stbl.ctts", self.path);
        let mut base = 0u64;
        for r in runs {
            let end = base
                .checked_add(r.count)
                .ok_or_else(|| malformed(&path, "样例计数溢出"))?;
            if sample < end {
                return Ok(Some(r.offset));
            }
            base = end;
        }
        Err(malformed(
            &path,
            format!("样例 {sample} 超出 ctts 覆盖范围"),
        ))
    }

    fn size_of(&self, sample: u64) -> Res<u64> {
        if let Some(f) = self.fixed_size {
            return Ok(f);
        }
        self.sizes.get(sample as usize).copied().ok_or_else(|| {
            malformed(
                &format!("{}.mdia.minf.stbl.stsz", self.path),
                format!("样例 {sample} 超出 stsz 表"),
            )
        })
    }

    /// 样例绝对文件偏移与大小：
    /// chunk_offset + chunk 内前序样例 size 之和。
    /// `file_len` 用于求和前的整体界校验（定长 size 场景 O(1)）：
    /// stsz 定长 count 与 stsc 的 samples_per_chunk 都是纯声明、与文件
    /// 字节无关，逐样例求和的代价会随攻击者声明的样例序号线性增长。
    fn locate(&self, sample: u64, file_len: u64) -> Res<(u64, u64)> {
        let stsc_path = format!("{}.mdia.minf.stbl.stsc", self.path);
        let chunk_count = self.chunk_offsets.len() as u64;
        let mut base = 0u64;
        let mut placed = None;
        for (i, e) in self.stsc.iter().enumerate() {
            let next_first = self
                .stsc
                .get(i + 1)
                .map(|n| n.first_chunk)
                .unwrap_or(chunk_count + 1);
            let run_chunks = next_first
                .checked_sub(e.first_chunk)
                .ok_or_else(|| malformed(&stsc_path, "stsc first_chunk 未递增"))?;
            let run_samples = run_chunks
                .checked_mul(e.samples_per_chunk)
                .ok_or_else(|| malformed(&stsc_path, "每段样例数溢出"))?;
            let end = base
                .checked_add(run_samples)
                .ok_or_else(|| malformed(&stsc_path, "样例计数溢出"))?;
            if sample < end {
                let within = sample - base;
                placed = Some((
                    e.first_chunk - 1 + within / e.samples_per_chunk,
                    within % e.samples_per_chunk,
                ));
                break;
            }
            base = end;
        }
        let (chunk_idx, in_chunk) = placed
            .ok_or_else(|| malformed(&stsc_path, format!("样例 {sample} 超出 stsc 覆盖范围")))?;
        if chunk_idx >= chunk_count {
            return Err(malformed(
                &format!("{}.mdia.minf.stbl.{}", self.path, self.chunk_box),
                format!("chunk {chunk_idx} 越界（共 {chunk_count} 个）"),
            ));
        }
        let chunk_off = self.chunk_offsets[chunk_idx as usize];
        let off = if let Some(f) = self.fixed_size {
            // 定长 size：前序样例总跨度 O(1) 可算，先按文件长度整体校验
            // （同 read_packet 分配前校验的原则：数据必须装进文件），
            // 再算偏移——不逐样例求和。变长表的循环则受表自身字节约束
            // （count×4 字节须在文件内），代价与文件长度成比例。
            let precede = in_chunk
                .checked_mul(f)
                .ok_or_else(|| malformed(&stsc_path, "样例偏移累计溢出"))?;
            let end = chunk_off
                .checked_add(precede)
                .ok_or_else(|| malformed(&stsc_path, "样例偏移累计溢出"))?;
            if end > file_len {
                return Err(malformed(
                    &stsc_path,
                    format!(
                        "chunk 样例数据越出文件（offset={chunk_off}, 前序跨度={precede}, 文件长={file_len}）"
                    ),
                ));
            }
            end
        } else {
            let mut off = chunk_off;
            for s in (sample - in_chunk)..sample {
                off = off
                    .checked_add(self.size_of(s)?)
                    .ok_or_else(|| malformed(&stsc_path, "样例偏移累计溢出"))?;
            }
            off
        };
        let size = self.size_of(sample)?;
        Ok((off, size))
    }

    /// stss 含此样例（1-based）⇒ 关键帧；无表 ⇒ 恒 true；音频 ⇒ 恒 true。
    fn is_keyframe(&self, sample: u64) -> bool {
        match self.kind {
            TrackKind::Audio => true,
            TrackKind::Video | TrackKind::Other => match &self.stss {
                Some(set) => set.contains(&(sample + 1)),
                None => true,
            },
        }
    }
}

/// fragment 样例关键帧：有效 flags 的 sample_is_non_sync_sample 位
/// （0x00010000）清零 ⇒ 关键帧；flags 全缺省 ⇒ true（与 stbl 无 stss 表
/// 恒关键帧的语义对齐）；音频 ⇒ 恒 true。
fn is_frag_keyframe(kind: TrackKind, flags: Option<u32>) -> bool {
    match kind {
        TrackKind::Audio => true,
        TrackKind::Video | TrackKind::Other => flags.is_none_or(|f| f & 0x0001_0000 == 0),
    }
}

// —— 读取器 ——

/// MP4 只读读取器。
pub struct Mp4Reader<R: Read + Seek> {
    reader: R,
    /// 底层文件总长（打开时探测）：读样例前校验 offset+size 不越界，
    /// 防损坏文件的巨型 stsz 声明触发巨型分配。
    file_len: u64,
    boxes: Vec<BoxNode>,
    streams: Vec<Stream>,
    tracks: Vec<TrackTables>,
}

/// MP4 轨道特有信息（develop `Stream` 放不下的字段，只读查询）：
/// tkhd 的 track_id 与 stsd 首 entry 的原始 fourcc。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrackInfo {
    /// tkhd track_ID。
    pub track_id: u32,
    /// stsd 首 entry 类型（avc1/mp4a/…；`CodecId` 之外的原始四字符码）。
    pub codec: FourCc,
}

/// 样例时戳（流时基；`Mp4Reader::sample_timing` 的返回）。
/// pts 恒有值（ctts 缺省 ⇒ cts=0 ⇒ pts==dts）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SampleTiming {
    /// 解码时戳。
    pub dts: Timestamp,
    /// 展示时戳（dts + ctts 偏移）。
    pub pts: Timestamp,
    /// 样例时长。
    pub duration: Timestamp,
}

/// 手写 Debug（R 未必可 Debug；只显示流表摘要，测试里 `unwrap_err` 需要）。
impl<R: Read + Seek> std::fmt::Debug for Mp4Reader<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mp4Reader")
            .field("streams", &self.streams)
            .finish_non_exhaustive()
    }
}

impl Mp4Reader<std::io::BufReader<std::fs::File>> {
    /// 打开磁盘文件。
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, Mp4Error> {
        let file = std::fs::File::open(path)?;
        Self::from_reader(std::io::BufReader::new(file))
    }
}

impl Mp4Reader<std::io::Cursor<Vec<u8>>> {
    /// 从内存数据构造（测试与内存数据入口）。
    pub fn from_vec(data: Vec<u8>) -> Result<Self, Mp4Error> {
        Self::from_reader(std::io::Cursor::new(data))
    }
}

impl<R: Read + Seek> Mp4Reader<R> {
    /// 泛型入口：解析完整 MP4（progressive 与 fMP4 均可）。
    pub fn from_reader(mut reader: R) -> Result<Self, Mp4Error> {
        let len = reader.seek(SeekFrom::End(0))?;
        reader.seek(SeekFrom::Start(0))?;
        let mut ctx = ParseCtx {
            file_len: len,
            ..Default::default()
        };
        let boxes = walk(&mut reader, 0, len, Scope::Top, "", &mut ctx, 0)?;
        // fragment 样例按 track_id 并入对应 trak：解析期只按 track_id 累积
        // （moof 可能出现在 moov 之前），此处统一关联。
        for ft in std::mem::take(&mut ctx.frag_tracks) {
            // 无样例的 traf（无 trun 或 sample_count=0）不触发任何判定。
            if ft.samples.is_empty() {
                continue;
            }
            let idx = ctx
                .builders
                .iter()
                .position(|b| b.track_id == Some(ft.track_id))
                .ok_or_else(|| {
                    malformed(
                        &ft.last_path,
                        format!("track_id={} 无对应 trak", ft.track_id),
                    )
                })?;
            let b = &mut ctx.builders[idx];
            // 同轨 stbl 非空样例又遇 moof 样例：两种编址并存，不支持。
            if b.stsz.as_ref().is_some_and(|s| s.count > 0) {
                return Err(Mp4Error::Unsupported("混合 progressive/fragmented 轨道"));
            }
            b.frag = Some(FragMerge {
                samples: ft.samples,
                end_dts: ft.next_dts,
                path: ft.last_path,
            });
        }
        let mut streams = Vec::with_capacity(ctx.builders.len());
        let mut tracks = Vec::with_capacity(ctx.builders.len());
        for (i, b) in ctx.builders.into_iter().enumerate() {
            let (s, t) = finish_track(i, b)?;
            streams.push(s);
            tracks.push(t);
        }
        Ok(Mp4Reader {
            reader,
            file_len: len,
            boxes,
            streams,
            tracks,
        })
    }

    pub fn streams(&self) -> &[Stream] {
        &self.streams
    }

    pub fn stream(&self, index: usize) -> Option<&Stream> {
        self.streams.get(index)
    }

    /// MP4 轨道特有信息（track_id / 原始 fourcc）——develop `Stream`
    /// 无处安放的字段，经此只读查询（流索引越界同 [`Mp4Error::InvalidStreamIndex`]）。
    pub fn track_info(&self, stream_index: usize) -> Result<TrackInfo, Mp4Error> {
        let t = self.track_tables(stream_index)?;
        Ok(TrackInfo {
            track_id: t.track_id,
            codec: t.codec,
        })
    }

    pub fn sample_count(&self, stream_index: usize) -> Result<usize, Mp4Error> {
        match self.tracks.get(stream_index) {
            Some(t) => Ok(t.sample_count as usize),
            None => Err(Mp4Error::InvalidStreamIndex {
                index: stream_index,
                count: self.streams.len(),
            }),
        }
    }

    /// 轨道表引用（越界统一 InvalidStreamIndex）。
    fn track_tables(&self, stream_index: usize) -> Result<&TrackTables, Mp4Error> {
        self.tracks
            .get(stream_index)
            .ok_or(Mp4Error::InvalidStreamIndex {
                index: stream_index,
                count: self.streams.len(),
            })
    }

    /// 第 sample 个样例（0-based）的时戳三元组（dts/pts/duration，
    /// 流时基轨道单位；不读样例数据）。progressive 与 fragment 统一编址。
    /// 错误语义与 [`Mp4Reader::read_packet`] 一致。
    pub fn sample_timing(
        &self,
        stream_index: usize,
        sample_index: usize,
    ) -> Result<SampleTiming, Mp4Error> {
        let t = self.track_tables(stream_index)?;
        let si = self.check_sample_index(t, stream_index, sample_index)?;
        let (dts, pts, duration) = self.sample_times(t, si)?;
        Ok(SampleTiming { dts, pts, duration })
    }

    /// ≤ sample_index 的最近关键帧样例序号（含自身；自身即关键帧则返回
    /// 自身）。音频轨/无 stss 表恒关键帧 ⇒ 恒返回 sample_index。
    /// 为接线阶段 `Demuxer::seek`（前一关键帧语义）预置的只读查询，
    /// 不改变任何读取状态。错误语义与 [`Mp4Reader::read_packet`] 一致。
    pub fn keyframe_index_before(
        &self,
        stream_index: usize,
        sample_index: usize,
    ) -> Result<usize, Mp4Error> {
        let t = self.track_tables(stream_index)?;
        let si = self.check_sample_index(t, stream_index, sample_index)?;
        for i in (0..=si).rev() {
            let key = if i < t.stbl_count {
                t.is_keyframe(i)
            } else {
                let f = &t.frags[(i - t.stbl_count) as usize];
                is_frag_keyframe(t.kind, f.flags)
            };
            if key {
                return Ok(i as usize);
            }
        }
        // 到不了这里：0 号样例要么在 stss 表（1-based 样号 1 必要时含 0
        // 之前无样例），要么无表恒关键帧。防御性返回 0。
        Ok(0)
    }

    /// 样例序号越界检查（0-based，统一编址）。
    fn check_sample_index(
        &self,
        t: &TrackTables,
        stream_index: usize,
        sample_index: usize,
    ) -> Result<u64, Mp4Error> {
        if sample_index as u64 >= t.sample_count {
            return Err(Mp4Error::SampleIndexOutOfRange {
                stream: stream_index,
                sample: sample_index,
                count: t.sample_count as usize,
            });
        }
        Ok(sample_index as u64)
    }

    /// 第 si 个样例的 (dts, pts, duration)（流时基）。progressive 走
    /// stts/ctts 前缀计算，fragment 走解析期已算好的字段。
    fn sample_times(&self, t: &TrackTables, si: u64) -> Res<(Timestamp, Timestamp, Timestamp)> {
        if si < t.stbl_count {
            // —— progressive 分支：stbl 样例表 ——
            let ctts_path = format!("{}.mdia.minf.stbl.ctts", t.path);
            let stts_path = format!("{}.mdia.minf.stbl.stts", t.path);
            let (dts_track, delta) = t.stts_at(si)?;
            let dts_v = u64_ticks(dts_track, &stts_path, "dts")?;
            let dts = Timestamp::new(dts_v, t.timebase);
            // ctts 偏移可有符号（v1 负偏移合法，pts 可早于 dts/为负）。
            let pts_v = match t.ctts_at(si)? {
                Some(off) => dts_v
                    .checked_add(off)
                    .ok_or_else(|| malformed(&ctts_path, "pts 溢出"))?,
                None => dts_v,
            };
            let pts = Timestamp::new(pts_v, t.timebase);
            let duration = Timestamp::new(u64_ticks(delta, &stts_path, "样例时长")?, t.timebase);
            Ok((dts, pts, duration))
        } else {
            // —— fragment 分支：tfdt/trun 解析期已算好的字段 ——
            let f = &t.frags[(si - t.stbl_count) as usize];
            let path = t.frag_path.clone();
            let dts_v = u64_ticks(f.dts, &path, "dts")?;
            let dts = Timestamp::new(dts_v, t.timebase);
            let pts_v = match f.cts {
                Some(c) => dts_v
                    .checked_add(c)
                    .ok_or_else(|| malformed(&path, "pts 溢出"))?,
                None => dts_v,
            };
            let pts = Timestamp::new(pts_v, t.timebase);
            let duration = Timestamp::new(u64_ticks(f.duration, &path, "样例时长")?, t.timebase);
            Ok((dts, pts, duration))
        }
    }

    /// 随机访问：按 sample_index 读一个样例。序号统一编址——小于 stbl
    /// 样例数走样例表（拼 dts/pts/keyframe，seek 到 chunk_offset+前序样例
    /// 偏移）；否则走 fragment 样例（tfdt/trun 已在解析期算好偏移与时戳）。
    /// 按序解封装 = 调用方 sample_index 递增。产出的包按流时基打戳
    /// （demuxer 打戳契约），keyframe 进 `PacketFlags::KEYFRAME`。
    pub fn read_packet(
        &mut self,
        stream_index: usize,
        sample_index: usize,
    ) -> Result<Packet, Mp4Error> {
        let t = self.track_tables(stream_index)?;
        let si = self.check_sample_index(t, stream_index, sample_index)?;
        let (dts, pts, duration) = self.sample_times(t, si)?;
        let keyframe = if si < t.stbl_count {
            t.is_keyframe(si)
        } else {
            let f = &t.frags[(si - t.stbl_count) as usize];
            is_frag_keyframe(t.kind, f.flags)
        };
        let (offset, size, bound_path) = if si < t.stbl_count {
            let stsz_path = format!("{}.mdia.minf.stbl.stsz", t.path);
            let (offset, size) = t.locate(si, self.file_len)?;
            (offset, size, stsz_path)
        } else {
            let f = &t.frags[(si - t.stbl_count) as usize];
            (f.offset, f.size, t.frag_path.clone())
        };
        // 分配前先按文件长度校验样例范围：stsz/trun 声明可达近 4 GiB，
        // 不校验就 vec![0u8; size] 会被损坏文件拖进巨型分配。
        let sample_end = offset
            .checked_add(size)
            .ok_or_else(|| malformed(&bound_path, "样例 offset+size 溢出"))?;
        if sample_end > self.file_len {
            return Err(malformed(
                &bound_path,
                format!(
                    "样例范围越出文件（offset={offset}, size={size}, 文件长={}）",
                    self.file_len
                ),
            ));
        }
        let mut data = vec![0u8; size as usize];
        self.reader.seek(SeekFrom::Start(offset))?;
        self.reader.read_exact(&mut data)?;
        let mut flags = PacketFlags::empty();
        if keyframe {
            flags.insert(PacketFlags::KEYFRAME);
        }
        Ok(Packet {
            data: Arc::from(data),
            stream_index: stream_index as u32,
            pts: Some(pts),
            dts: Some(dts),
            duration: Some(duration),
            flags,
        })
    }

    pub fn box_tree(&self) -> &[BoxNode] {
        &self.boxes
    }
}

// —— box 树与 trak 解析 ——

/// 容器递归深度上限。合法 MP4 深度 ≤ 5（moov/trak/mdia/minf/stbl），
/// 远超即恶意构造：超限返回 Malformed，防递归栈溢出。
const MAX_BOX_DEPTH: u32 = 64;

/// 递归范围：决定叶子 box 归属哪路 trak / 哪个 fragment 结构。
#[derive(Clone, Copy)]
enum Scope {
    Top,
    Moov,
    Trak(usize),
    Mdia(usize),
    Minf(usize),
    Stbl(usize),
    /// 顶层 moof 容器：start 为 moof box 起始绝对偏移
    /// （default-base-is-moof 与隐式基值兜底的基值）。
    Moof {
        start: u64,
    },
    /// moof 内的 traf。
    Traf {
        moof_start: u64,
    },
    /// moov 内的 mvex（trex 解析）。
    Mvex,
    /// 非法嵌套（如 trak 里出现 moov）：只走树，不取数据。
    Dead,
}

#[derive(Default)]
struct ParseCtx {
    /// 底层文件总长：trun 解析期按「样例数据必须装进文件」原则做长度界
    /// 校验用（from_reader 进入前填入）。
    file_len: u64,
    builders: Vec<TrackBuilder>,
    /// mvex 内的 trex（按 track_id 查缺省值）。
    trex: Vec<Trex>,
    /// 按 track_id 累积的 fragment 样例与游标。
    frag_tracks: Vec<FragTrack>,
    /// 已见顶层 moof 的个数（box 路径编号）。
    moof_count: usize,
    /// 当前 moof 内已见 traf 的个数（box 路径编号；进入 moof 时清零）。
    traf_count: usize,
    /// 当前 traf 的瞬态状态（进入 traf 时置 None，tfhd 填充）。
    /// walk 串行解析无重入，放这里即可。
    cur_traf: Option<TrafState>,
}

/// trak 解析的中间产物，walk 完成后 `finish_track` 收敛成 Stream+TrackTables。
struct TrackBuilder {
    /// 如 `"moov.trak[0]"`。
    path: String,
    track_id: Option<u32>,
    timescale: Option<u32>,
    mdhd_duration: Option<u64>,
    handler: Option<FourCc>,
    stsd_codec: Option<FourCc>,
    /// stsd 首 entry 内容（去掉 entry 的 8 字节 box 头）；
    /// finish 时按 kind 提取宽高/声道与子 box 载荷。
    stsd_content: Vec<u8>,
    stts: Option<Vec<SttsRun>>,
    ctts: Option<Vec<CttsRun>>,
    stss: Option<HashSet<u64>>,
    stsc: Option<Vec<StscEntry>>,
    stsz: Option<StszData>,
    chunk_offsets: Option<(Vec<u64>, &'static str)>,
    /// fragment 样例（from_reader 阶段按 track_id 并入）。
    frag: Option<FragMerge>,
}

/// 并入 TrackBuilder 的 fragment 结果。
struct FragMerge {
    samples: Vec<FragSample>,
    /// 全部 fragment 解析完的尾 dts（轨道单位）——Stream.duration 用。
    end_dts: u64,
    /// 最近一条 trun 的 box 路径（错误定位）。
    path: String,
}

impl TrackBuilder {
    fn new(path: String) -> TrackBuilder {
        TrackBuilder {
            path,
            track_id: None,
            timescale: None,
            mdhd_duration: None,
            handler: None,
            stsd_codec: None,
            stsd_content: Vec::new(),
            stts: None,
            ctts: None,
            stss: None,
            stsc: None,
            stsz: None,
            chunk_offsets: None,
            frag: None,
        }
    }
}

/// 读到的 box 头。`total` 含头的完整大小，`header` 为头长（8 或 16）。
struct BoxHeader {
    kind: FourCc,
    total: u64,
    header: u64,
}

fn read_box_header<R: Read + Seek>(
    r: &mut R,
    pos: u64,
    end: u64,
    parent_path: &str,
) -> Res<BoxHeader> {
    if end - pos < 8 {
        return Err(malformed(parent_path, "box 头截断"));
    }
    r.seek(SeekFrom::Start(pos))?;
    let mut buf = [0u8; 8];
    r.read_exact(&mut buf)?;
    let size = u32::from_be_bytes(buf[0..4].try_into().unwrap()) as u64;
    let kind = FourCc::new(buf[4..8].try_into().unwrap());
    let child_path = join_path(parent_path, &kind.as_str());
    let (total, header) = match size {
        // 延伸到父容器末（顶层则到 EOF）。
        0 => (end - pos, 8),
        1 => {
            let mut lb = [0u8; 8];
            r.read_exact(&mut lb)?;
            (u64::from_be_bytes(lb), 16)
        }
        n if n < 8 => return Err(malformed(&child_path, format!("size {n} 小于 8 字节头"))),
        n => (n, 8),
    };
    if total < header {
        return Err(malformed(
            &child_path,
            format!("size {total} 小于头长 {header}"),
        ));
    }
    match pos.checked_add(total) {
        Some(e) if e <= end => {}
        _ => return Err(malformed(&child_path, "box 越出父容器范围")),
    }
    Ok(BoxHeader {
        kind,
        total,
        header,
    })
}

/// 扫描 [start, end) 内的子 box，构建树并按 scope 提取 trak 数据。
/// `depth` 为容器嵌套深度（顶层 0），超过 [`MAX_BOX_DEPTH`] 报 Malformed。
fn walk<R: Read + Seek>(
    r: &mut R,
    start: u64,
    end: u64,
    scope: Scope,
    path: &str,
    ctx: &mut ParseCtx,
    depth: u32,
) -> Res<Vec<BoxNode>> {
    if depth > MAX_BOX_DEPTH {
        return Err(malformed(
            path,
            format!("box 嵌套深度超过 {MAX_BOX_DEPTH} 层"),
        ));
    }
    let mut nodes = Vec::new();
    let mut pos = start;
    while pos < end {
        let h = read_box_header(r, pos, end, path)?;
        let kind = h.kind.as_bytes();
        let info = BoxInfo {
            kind: h.kind,
            offset: pos,
            size: h.total,
        };
        let content_start = pos + h.header;
        let box_end = pos + h.total;

        if matches!(
            kind.as_slice(),
            b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"moof" | b"traf" | b"mvex"
        ) {
            let plain = join_path(path, &h.kind.as_str());
            let (child_scope, child_path) = if kind == *b"moov" {
                (Scope::Moov, plain)
            } else if kind == *b"trak" {
                let i = ctx.builders.len();
                let p = join_path(path, &format!("trak[{i}]"));
                ctx.builders.push(TrackBuilder::new(p.clone()));
                (Scope::Trak(i), p)
            } else if kind == *b"mdia" {
                match scope {
                    Scope::Trak(i) => (Scope::Mdia(i), plain),
                    _ => (Scope::Dead, plain),
                }
            } else if kind == *b"minf" {
                match scope {
                    Scope::Mdia(i) => (Scope::Minf(i), plain),
                    _ => (Scope::Dead, plain),
                }
            } else if kind == *b"stbl" {
                match scope {
                    Scope::Minf(i) => (Scope::Stbl(i), plain),
                    _ => (Scope::Dead, plain),
                }
            } else if kind == *b"mvex" {
                match scope {
                    Scope::Moov => (Scope::Mvex, plain),
                    _ => (Scope::Dead, plain),
                }
            } else if kind == *b"moof" {
                match scope {
                    Scope::Top => {
                        let i = ctx.moof_count;
                        ctx.moof_count += 1;
                        ctx.traf_count = 0;
                        (
                            Scope::Moof { start: pos },
                            join_path(path, &format!("moof[{i}]")),
                        )
                    }
                    _ => (Scope::Dead, plain),
                }
            } else {
                // traf：仅在 moof 内有效；进入时重置 traf 瞬态状态。
                match scope {
                    Scope::Moof { start } => {
                        let j = ctx.traf_count;
                        ctx.traf_count += 1;
                        ctx.cur_traf = None;
                        (
                            Scope::Traf { moof_start: start },
                            join_path(path, &format!("traf[{j}]")),
                        )
                    }
                    _ => (Scope::Dead, plain),
                }
            };
            let children = walk(
                r,
                content_start,
                box_end,
                child_scope,
                &child_path,
                ctx,
                depth + 1,
            )?;
            nodes.push(BoxNode { info, children });
        } else {
            let child_path = join_path(path, &h.kind.as_str());
            parse_leaf(r, scope, &h, content_start, box_end, &child_path, ctx)?;
            r.seek(SeekFrom::Start(box_end))?;
            nodes.push(BoxNode {
                info,
                children: Vec::new(),
            });
        }
        pos = box_end;
    }
    Ok(nodes)
}

/// 读取叶子 box 载荷并按 (scope, kind) 分发给 trak 解析器。
fn parse_leaf<R: Read + Seek>(
    r: &mut R,
    scope: Scope,
    h: &BoxHeader,
    content_start: u64,
    box_end: u64,
    path: &str,
    ctx: &mut ParseCtx,
) -> Res<()> {
    let kind = h.kind.as_bytes();
    let mut dispatch = |b: &mut TrackBuilder| -> Res<()> {
        if b.stsd_codec.is_some() && kind == *b"stsd"
            || b.stts.is_some() && kind == *b"stts"
            || b.stsc.is_some() && kind == *b"stsc"
            || b.stsz.is_some() && kind == *b"stsz"
            || b.chunk_offsets.is_some() && (kind == *b"stco" || kind == *b"co64")
        {
            return Err(malformed(path, "样例表 box 重复出现"));
        }
        let payload = read_payload(r, content_start, box_end)?;
        match &kind {
            b"tkhd" => parse_tkhd(&payload, path, b),
            b"mdhd" => parse_mdhd(&payload, path, b),
            b"hdlr" => parse_hdlr(&payload, path, b),
            b"stsd" => parse_stsd(&payload, path, b),
            b"stts" => parse_stts(&payload, path, b),
            b"ctts" => parse_ctts(&payload, path, b),
            b"stss" => parse_stss(&payload, path, b),
            b"stsc" => parse_stsc(&payload, path, b),
            b"stsz" => parse_stsz(&payload, path, b),
            b"stco" => parse_chunk_offsets(&payload, path, b, false),
            b"co64" => parse_chunk_offsets(&payload, path, b, true),
            _ => Ok(()),
        }
    };
    match scope {
        Scope::Trak(i) if kind == *b"tkhd" => dispatch(&mut ctx.builders[i]),
        Scope::Mdia(i) if kind == *b"mdhd" || kind == *b"hdlr" => dispatch(&mut ctx.builders[i]),
        Scope::Stbl(i) if is_sample_table(kind) => dispatch(&mut ctx.builders[i]),
        Scope::Mvex if kind == *b"trex" => {
            let payload = read_payload(r, content_start, box_end)?;
            parse_trex(&payload, path, ctx)
        }
        Scope::Traf { moof_start } => match kind.as_slice() {
            b"tfhd" => {
                let payload = read_payload(r, content_start, box_end)?;
                parse_tfhd(&payload, path, moof_start, ctx)
            }
            b"tfdt" => {
                let payload = read_payload(r, content_start, box_end)?;
                parse_tfdt(&payload, path, ctx)
            }
            b"trun" => {
                let payload = read_payload(r, content_start, box_end)?;
                parse_trun(&payload, path, ctx)
            }
            _ => Ok(()),
        },
        // 其余 box（udta/edts/free/wide/mehd/mfhd/…）一律跳过。
        _ => Ok(()),
    }
}

/// 是否 stbl 内我们关心的样例表 box。
fn is_sample_table(kind: [u8; 4]) -> bool {
    matches!(
        kind.as_slice(),
        b"stsd" | b"stts" | b"ctts" | b"stss" | b"stsc" | b"stsz" | b"stco" | b"co64"
    )
}

fn read_payload<R: Read + Seek>(r: &mut R, start: u64, end: u64) -> Res<Vec<u8>> {
    let len = (end - start) as usize;
    let mut buf = vec![0u8; len];
    r.seek(SeekFrom::Start(start))?;
    r.read_exact(&mut buf)?;
    Ok(buf)
}

fn join_path(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        child.to_string()
    } else {
        format!("{parent}.{child}")
    }
}

// —— 各 box 的载荷解析 ——

/// 载荷游标：越界读直接报 Malformed。
struct Cur<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> Cur<'a> {
    fn new(d: &'a [u8]) -> Cur<'a> {
        Cur { d, p: 0 }
    }

    fn need(&self, n: u64, path: &str, what: &str) -> Res<()> {
        let remain = (self.d.len() - self.p) as u64;
        if remain < n {
            Err(malformed(
                path,
                format!("{what} 截断（剩 {remain} 字节，需 {n}）"),
            ))
        } else {
            Ok(())
        }
    }

    fn skip(&mut self, n: u64, path: &str, what: &str) -> Res<()> {
        self.need(n, path, what)?;
        self.p += n as usize;
        Ok(())
    }

    fn u16(&mut self, path: &str, what: &str) -> Res<u16> {
        self.need(2, path, what)?;
        let v = u16::from_be_bytes(self.d[self.p..self.p + 2].try_into().unwrap());
        self.p += 2;
        Ok(v)
    }

    fn u32(&mut self, path: &str, what: &str) -> Res<u32> {
        self.need(4, path, what)?;
        let v = u32::from_be_bytes(self.d[self.p..self.p + 4].try_into().unwrap());
        self.p += 4;
        Ok(v)
    }

    fn u64(&mut self, path: &str, what: &str) -> Res<u64> {
        self.need(8, path, what)?;
        let v = u64::from_be_bytes(self.d[self.p..self.p + 8].try_into().unwrap());
        self.p += 8;
        Ok(v)
    }

    fn i32(&mut self, path: &str, what: &str) -> Res<i32> {
        Ok(self.u32(path, what)? as i32)
    }

    fn take4(&mut self, path: &str, what: &str) -> Res<[u8; 4]> {
        self.need(4, path, what)?;
        let v: [u8; 4] = self.d[self.p..self.p + 4].try_into().unwrap();
        self.p += 4;
        Ok(v)
    }
}

fn parse_tkhd(d: &[u8], path: &str, b: &mut TrackBuilder) -> Res<()> {
    let mut c = Cur::new(d);
    let vf = c.u32(path, "version/flags")?;
    // v0: creation/modification 各 4 字节；v1 各 8 字节。均已在 v+f 之后。
    let field = match vf >> 24 {
        0 => 8,
        1 => 16,
        v => return Err(malformed(path, format!("tkhd version {v} 不支持"))),
    };
    c.skip(field, path, "时间戳字段")?;
    b.track_id = Some(c.u32(path, "track_id")?);
    Ok(())
}

fn parse_mdhd(d: &[u8], path: &str, b: &mut TrackBuilder) -> Res<()> {
    let mut c = Cur::new(d);
    let vf = c.u32(path, "version/flags")?;
    let (field, dur_width) = match vf >> 24 {
        0 => (8u64, 4u64),
        1 => (16, 8),
        v => return Err(malformed(path, format!("mdhd version {v} 不支持"))),
    };
    c.skip(field, path, "时间戳字段")?;
    let timescale = c.u32(path, "timescale")?;
    if timescale == 0 {
        return Err(malformed(path, "timescale 为 0"));
    }
    let duration = if dur_width == 4 {
        c.u32(path, "duration")? as u64
    } else {
        c.u64(path, "duration")?
    };
    b.timescale = Some(timescale);
    b.mdhd_duration = Some(duration);
    Ok(())
}

fn parse_hdlr(d: &[u8], path: &str, b: &mut TrackBuilder) -> Res<()> {
    let mut c = Cur::new(d);
    c.skip(4, path, "version/flags")?;
    let first = c.take4(path, "pre_defined/handler_type")?;
    // ISO 布局：version/flags 后是 pre_defined(4) 再是 handler_type
    // （ffmpeg 等真实封装器 pre_defined=0）。本 crate 早期手工构造的测试
    // 字节没有 pre_defined（handler 紧随 version/flags），两者都兼容：
    // 首 4 字节即已知 handler ⇒ 直接采用；否则视为 pre_defined 跳过。
    let handler = if is_known_handler(first) {
        first
    } else {
        c.take4(path, "handler_type")?
    };
    b.handler = Some(FourCc::new(handler));
    Ok(())
}

/// 是否常见 handler_type（vide/soun/text/subt/meta/hint…）。
fn is_known_handler(f: [u8; 4]) -> bool {
    matches!(
        f.as_slice(),
        b"vide" | b"soun" | b"text" | b"subt" | b"meta" | b"hint" | b"clcp" | b"dirr"
    )
}

fn parse_stsd(d: &[u8], path: &str, b: &mut TrackBuilder) -> Res<()> {
    let mut c = Cur::new(d);
    c.skip(4, path, "version/flags")?;
    let entry_count = c.u32(path, "entry_count")?;
    if entry_count != 1 {
        return Err(Mp4Error::Unsupported("多 sample description"));
    }
    // entry 本身是一个 box：size+type（size==1 时后跟 largesize）。
    let size = c.u32(path, "entry size")? as u64;
    let codec = FourCc::new(c.take4(path, "entry type")?);
    let (total, header) = match size {
        0 => ((d.len() - c.p) as u64 + 8, 8),
        1 => {
            let large = c.u64(path, "entry largesize")?;
            (large, 16)
        }
        n if n < 8 => return Err(malformed(path, format!("entry size {n} 小于头长"))),
        n => (n, 8),
    };
    if total < header {
        return Err(malformed(
            path,
            format!("entry size {total} 小于头长 {header}"),
        ));
    }
    let content_len = total - header;
    if ((d.len() - c.p) as u64) < content_len {
        return Err(malformed(path, "entry 越出 stsd 载荷"));
    }
    b.stsd_codec = Some(codec);
    b.stsd_content = d[c.p..c.p + content_len as usize].to_vec();
    Ok(())
}

fn parse_stts(d: &[u8], path: &str, b: &mut TrackBuilder) -> Res<()> {
    let mut c = Cur::new(d);
    c.skip(4, path, "version/flags")?;
    let n = c.u32(path, "entry_count")? as u64;
    c.need(n * 8, path, "stts 条目")?;
    let mut runs = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let count = c.u32(path, "sample_count")? as u64;
        let delta = c.u32(path, "sample_delta")? as u64;
        runs.push(SttsRun { count, delta });
    }
    b.stts = Some(runs);
    Ok(())
}

fn parse_ctts(d: &[u8], path: &str, b: &mut TrackBuilder) -> Res<()> {
    let mut c = Cur::new(d);
    let vf = c.u32(path, "version/flags")?;
    let version = vf >> 24;
    // v0 偏移无符号，v1 有符号（负偏移合法，pts 可早于 dts）。
    let signed = match version {
        0 => false,
        1 => true,
        v => return Err(malformed(path, format!("ctts version {v} 不支持"))),
    };
    let n = c.u32(path, "entry_count")? as u64;
    c.need(n * 8, path, "ctts 条目")?;
    let mut runs = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let count = c.u32(path, "sample_count")? as u64;
        let offset = if signed {
            c.i32(path, "sample_offset")? as i64
        } else {
            c.u32(path, "sample_offset")? as i64
        };
        runs.push(CttsRun { count, offset });
    }
    b.ctts = Some(runs);
    Ok(())
}

fn parse_stss(d: &[u8], path: &str, b: &mut TrackBuilder) -> Res<()> {
    let mut c = Cur::new(d);
    c.skip(4, path, "version/flags")?;
    let n = c.u32(path, "entry_count")? as u64;
    c.need(n * 4, path, "stss 条目")?;
    let mut set = HashSet::with_capacity(n as usize);
    for _ in 0..n {
        set.insert(c.u32(path, "sample_number")? as u64);
    }
    b.stss = Some(set);
    Ok(())
}

fn parse_stsc(d: &[u8], path: &str, b: &mut TrackBuilder) -> Res<()> {
    let mut c = Cur::new(d);
    c.skip(4, path, "version/flags")?;
    let n = c.u32(path, "entry_count")? as u64;
    c.need(n * 12, path, "stsc 条目")?;
    let mut runs: Vec<StscEntry> = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let first_chunk = c.u32(path, "first_chunk")? as u64;
        let samples_per_chunk = c.u32(path, "samples_per_chunk")? as u64;
        let _desc = c.u32(path, "sample_description_index")?;
        if first_chunk < 1 {
            return Err(malformed(path, "first_chunk 须从 1 起"));
        }
        if samples_per_chunk < 1 {
            return Err(malformed(path, "samples_per_chunk 须 ≥ 1"));
        }
        if let Some(prev) = runs.last()
            && first_chunk <= prev.first_chunk
        {
            return Err(malformed(path, "first_chunk 未严格递增"));
        }
        runs.push(StscEntry {
            first_chunk,
            samples_per_chunk,
        });
    }
    b.stsc = Some(runs);
    Ok(())
}

fn parse_stsz(d: &[u8], path: &str, b: &mut TrackBuilder) -> Res<()> {
    let mut c = Cur::new(d);
    c.skip(4, path, "version/flags")?;
    let fixed = c.u32(path, "sample_size")?;
    let count = c.u32(path, "sample_count")? as u64;
    let data = if fixed != 0 {
        StszData {
            fixed: Some(fixed as u64),
            sizes: Vec::new(),
            count,
        }
    } else {
        c.need(count * 4, path, "stsz 样例大小表")?;
        let mut sizes = Vec::with_capacity(count as usize);
        for _ in 0..count {
            sizes.push(c.u32(path, "entry_size")? as u64);
        }
        StszData {
            fixed: None,
            sizes,
            count,
        }
    };
    b.stsz = Some(data);
    Ok(())
}

fn parse_chunk_offsets(d: &[u8], path: &str, b: &mut TrackBuilder, is_co64: bool) -> Res<()> {
    let mut c = Cur::new(d);
    c.skip(4, path, "version/flags")?;
    let n = c.u32(path, "entry_count")? as u64;
    c.need(n * if is_co64 { 8 } else { 4 }, path, "chunk 偏移表")?;
    let mut offsets = Vec::with_capacity(n as usize);
    for _ in 0..n {
        offsets.push(if is_co64 {
            c.u64(path, "chunk_offset")?
        } else {
            c.u32(path, "chunk_offset")? as u64
        });
    }
    b.chunk_offsets = Some((offsets, if is_co64 { "co64" } else { "stco" }));
    Ok(())
}

// —— fMP4 box 的载荷解析 ——

/// 取（或惰性创建）某 track_id 的 fragment 累积状态。
fn frag_track_mut(ctx: &mut ParseCtx, track_id: u32) -> &mut FragTrack {
    if let Some(i) = ctx.frag_tracks.iter().position(|t| t.track_id == track_id) {
        &mut ctx.frag_tracks[i]
    } else {
        ctx.frag_tracks.push(FragTrack {
            track_id,
            samples: Vec::new(),
            cursor: None,
            next_dts: 0,
            last_path: String::new(),
        });
        ctx.frag_tracks.last_mut().unwrap()
    }
}

/// trex：按 track_id 记录缺省值（同 track 重复出现 ⇒ Malformed）。
fn parse_trex(d: &[u8], path: &str, ctx: &mut ParseCtx) -> Res<()> {
    let mut c = Cur::new(d);
    c.skip(4, path, "version/flags")?;
    let entry = Trex {
        track_id: c.u32(path, "track_ID")?,
        default_sample_description_index: c.u32(path, "default_sample_description_index")?,
        default_sample_duration: c.u32(path, "default_sample_duration")?,
        default_sample_size: c.u32(path, "default_sample_size")?,
        default_sample_flags: c.u32(path, "default_sample_flags")?,
    };
    if ctx.trex.iter().any(|t| t.track_id == entry.track_id) {
        return Err(malformed(path, "同 track_id 的 trex 重复出现"));
    }
    ctx.trex.push(entry);
    Ok(())
}

/// tfhd：确定本 traf 的基值与 tfhd 级缺省，填充 traf 瞬态状态。
fn parse_tfhd(d: &[u8], path: &str, moof_start: u64, ctx: &mut ParseCtx) -> Res<()> {
    let mut c = Cur::new(d);
    let vf = c.u32(path, "version/flags")?;
    let flags = vf & 0x00FF_FFFF;
    let track_id = c.u32(path, "track_ID")?;
    let explicit_base = if flags & 0x0000_0001 != 0 {
        Some(c.u64(path, "base_data_offset")?)
    } else {
        None
    };
    if flags & 0x0000_0002 != 0 {
        // sample_description_index：stsd entry_count 恒 1，记录后不参与。
        c.skip(4, path, "sample_description_index")?;
    }
    let def_duration = if flags & 0x0000_0008 != 0 {
        Some(c.u32(path, "default_sample_duration")?)
    } else {
        None
    };
    let def_size = if flags & 0x0000_0010 != 0 {
        Some(c.u32(path, "default_sample_size")?)
    } else {
        None
    };
    let def_flags = if flags & 0x0000_0020 != 0 {
        Some(c.u32(path, "default_sample_flags")?)
    } else {
        None
    };
    let base = if let Some(b) = explicit_base {
        b
    } else if flags & 0x0002_0000 != 0 {
        // default-base-is-moof（0x020000，24 位 flags 内）。
        moof_start
    } else {
        // 无基值信息：接本轨前一 trun 数据末尾；首个 traf 兜底取 moof 起点。
        ctx.frag_tracks
            .iter()
            .find(|t| t.track_id == track_id)
            .and_then(|t| t.cursor)
            .unwrap_or(moof_start)
    };
    ctx.cur_traf = Some(TrafState {
        track_id,
        base,
        def_duration,
        def_size,
        def_flags,
    });
    Ok(())
}

/// tfdt：fragment 首 dts（轨道单位），v0 u32 / v1 u64。
fn parse_tfdt(d: &[u8], path: &str, ctx: &mut ParseCtx) -> Res<()> {
    let mut c = Cur::new(d);
    let vf = c.u32(path, "version/flags")?;
    let base = match vf >> 24 {
        0 => c.u32(path, "baseMediaDecodeTime")? as u64,
        1 => c.u64(path, "baseMediaDecodeTime")?,
        v => return Err(malformed(path, format!("tfdt version {v} 不支持"))),
    };
    let Some(traf) = ctx.cur_traf else {
        return Err(malformed(path, "tfdt 先于 tfhd（tfhd 须为 traf 首个 box）"));
    };
    frag_track_mut(ctx, traf.track_id).next_dts = base;
    Ok(())
}

/// trun：逐样例解析并按优先链（trun > tfhd > trex）补缺省，
/// 数据偏移按「显式 data_offset 相对 traf 基值；缺省接前一 trun 数据末尾」。
fn parse_trun(d: &[u8], path: &str, ctx: &mut ParseCtx) -> Res<()> {
    let mut c = Cur::new(d);
    let vf = c.u32(path, "version/flags")?;
    let version = vf >> 24;
    let flags = vf & 0x00FF_FFFF;
    // v0 cts 无符号，v1 有符号（负偏移合法，pts 可早于 dts）。
    let signed_cts = match version {
        0 => false,
        1 => true,
        v => return Err(malformed(path, format!("trun version {v} 不支持"))),
    };
    let n = c.u32(path, "sample_count")? as u64;
    let data_offset = if flags & 0x0000_0001 != 0 {
        Some(c.i32(path, "data_offset")?)
    } else {
        None
    };
    let first_flags = if flags & 0x0000_0004 != 0 {
        Some(c.u32(path, "first_sample_flags")?)
    } else {
        None
    };
    // 逐样例字段存在位（24 位 flags 内）：0x100 duration、0x200 size、
    // 0x400 flags、0x800 cts（实测 ffmpeg trun flags=0x00000a05 即含 0x200|0x800）。
    let f_dur = flags & 0x0000_0100 != 0;
    let f_size = flags & 0x0000_0200 != 0;
    let f_flags = flags & 0x0000_0400 != 0;
    let f_cts = flags & 0x0000_0800 != 0;

    let Some(traf) = ctx.cur_traf else {
        return Err(malformed(path, "trun 所在 traf 缺 tfhd（tfhd 须先出现）"));
    };
    let trex = ctx
        .trex
        .iter()
        .find(|t| t.track_id == traf.track_id)
        .copied();
    // 声明的样例字段先整体校验长度，防巨型 sample_count 触发巨型分配。
    let per_sample = (f_dur as u64 + f_size as u64 + f_flags as u64 + f_cts as u64) * 4;
    c.need(n * per_sample, path, "trun 样例字段")?;

    let file_len = ctx.file_len;
    let ft = frag_track_mut(ctx, traf.track_id);
    // 本 trun 数据起点。
    let mut cursor = match data_offset {
        Some(off) => {
            // 显式 data_offset 相对 traf 基值（可有符号）。
            let abs = traf.base as i128 + off as i128;
            if abs < 0 {
                return Err(malformed(path, format!("data_offset {off} 使样例偏移为负")));
            }
            abs as u64
        }
        // 缺省接前一 trun 数据末尾；基值兜底（首 traf 无任何基值信息时
        // tfhd 已把 base 兜底到 moof 起点，此处再兜一次防御）。
        None => ft.cursor.unwrap_or(traf.base),
    };
    // 逐样例字段全缺时 per_sample==0，上方 need 空转——样例数失去字节
    // 约束。补长度界（同 read_packet 分配前校验的原则：数据必须装进
    // 文件），防无逐样例字段的 trun 声明 u32::MAX 样例令逐样例入表
    // 触发巨型分配 abort：缺省 size 已知时 O(1) 校验本 trun 数据总跨度；
    // size 全缺省 0 无字节可依时按样例数 ≤ file_len 兜底。逐样例显式
    // size 时每个样例至少占 trun 内 4 字节，样例数已被 need 约束。
    if !f_size {
        let def_size = traf
            .def_size
            .or(trex.map(|t| t.default_sample_size))
            .unwrap_or(0) as u64;
        if def_size > 0 {
            let end = n
                .checked_mul(def_size)
                .and_then(|total| cursor.checked_add(total))
                .ok_or_else(|| malformed(path, "fragment 样例数据总量溢出"))?;
            if end > file_len {
                return Err(malformed(
                    path,
                    format!("fragment 样例数据越出文件（需 {end} 字节，文件长 {file_len}）"),
                ));
            }
        } else if n > file_len {
            return Err(malformed(
                path,
                format!("fragment 样例数 {n} 超出文件长度 {file_len}（size 缺省 0 的兜底界）"),
            ));
        }
    }
    let mut dts = ft.next_dts;
    for i in 0..n {
        // duration/size：trun 显式 > tfhd 默认 > trex 默认 > 0（无表示「未知」的余地）。
        let duration = if f_dur {
            c.u32(path, "sample_duration")? as u64
        } else {
            traf.def_duration
                .or(trex.map(|t| t.default_sample_duration))
                .unwrap_or(0) as u64
        };
        let size = if f_size {
            c.u32(path, "sample_size")? as u64
        } else {
            traf.def_size
                .or(trex.map(|t| t.default_sample_size))
                .unwrap_or(0) as u64
        };
        // 逐样例 flags 字段存在就得消费（首样例的值可被 first_sample_flags 覆盖）。
        let per_sample_flags = if f_flags {
            Some(c.u32(path, "sample_flags")?)
        } else {
            None
        };
        let sample_flags = if i == 0 {
            match first_flags {
                // 首样例特例：first_sample_flags 覆盖整个 flags。
                Some(ff) => Some(ff),
                None => per_sample_flags
                    .or(traf.def_flags)
                    .or(trex.map(|t| t.default_sample_flags)),
            }
        } else {
            per_sample_flags
                .or(traf.def_flags)
                .or(trex.map(|t| t.default_sample_flags))
        };
        let cts = if f_cts {
            Some(if signed_cts {
                c.i32(path, "sample_composition_time_offset")? as i64
            } else {
                c.u32(path, "sample_composition_time_offset")? as i64
            })
        } else {
            None
        };
        ft.samples.push(FragSample {
            dts,
            duration,
            offset: cursor,
            size,
            flags: sample_flags,
            cts,
        });
        dts = dts
            .checked_add(duration)
            .ok_or_else(|| malformed(path, "fragment dts 累计溢出"))?;
        cursor = cursor
            .checked_add(size)
            .ok_or_else(|| malformed(path, "fragment 样例偏移累计溢出"))?;
    }
    ft.next_dts = dts;
    ft.cursor = Some(cursor);
    ft.last_path = path.to_string();
    Ok(())
}

// —— 收敛 ——

fn finish_track(index: usize, b: TrackBuilder) -> Res<(Stream, TrackTables)> {
    let path = b.path;
    let missing = |name: &str| malformed(&format!("{path}.mdia.minf.stbl.{name}"), "缺失该表");
    let codec = b.stsd_codec.ok_or_else(|| missing("stsd"))?;
    let timescale = b
        .timescale
        .ok_or_else(|| malformed(&format!("{path}.mdia.mdhd"), "缺失 mdhd（timescale 未知）"))?;
    let stts = b.stts.ok_or_else(|| missing("stts"))?;
    let stsc = b.stsc.ok_or_else(|| missing("stsc"))?;
    let stsz = b.stsz.ok_or_else(|| missing("stsz"))?;
    let (chunk_offsets, chunk_box) = b
        .chunk_offsets
        .ok_or_else(|| malformed(&format!("{path}.mdia.minf.stbl.stco"), "缺失 stco/co64"))?;

    let kind = match &b.handler {
        Some(h) if h.as_bytes() == *b"vide" => TrackKind::Video,
        Some(h) if h.as_bytes() == *b"soun" => TrackKind::Audio,
        _ => TrackKind::Other,
    };
    let (video, audio, extra_data) = extract_entry_fields(kind, &b.stsd_content, &path)?;

    let mdhd_path = format!("{path}.mdia.mdhd");
    // timescale 为 u32；Rational 分母为 i32——超出 i32::MAX 的 timescale
    // 无法构造 1/timescale ⇒ Malformed（荒谬量级，正常文件不触）。
    if timescale > i32::MAX as u32 {
        return Err(malformed(&mdhd_path, "timescale 超出可表示范围"));
    }
    let timebase = Rational::new(1, timescale as i32);
    // 有 fragment 样例 ⇒ Stream.duration = 末 fragment 尾 dts+dur
    // （mdhd/mehd 为 0 时亦然）；否则沿用 mdhd 时长。流时基原生单位，
    // 无换算无舍入。
    let frag = b.frag;
    let frag_count = frag.as_ref().map_or(0, |f| f.samples.len() as u64);
    let sample_count = stsz
        .count
        .checked_add(frag_count)
        .ok_or_else(|| malformed(&mdhd_path, "样例计数溢出"))?;
    let duration = match &frag {
        Some(f) => Timestamp::new(u64_ticks(f.end_dts, &f.path, "fragment 尾 dts")?, timebase),
        None => Timestamp::new(
            u64_ticks(b.mdhd_duration.unwrap_or(0), &mdhd_path, "时长")?,
            timebase,
        ),
    };
    let (frags, frag_path) = match frag {
        Some(f) => (f.samples, f.path),
        None => (Vec::new(), String::new()),
    };

    let media_type = kind.media_type();
    let stream = Stream {
        index: index as u32,
        media_type,
        codec: CodecParameters {
            codec: codec_id_of(&codec),
            media_type,
            extradata: if extra_data.is_empty() {
                None
            } else {
                Some(extra_data)
            },
            bit_rate: None,
            video: video.map(|(width, height)| VideoParams {
                width,
                height,
                pixel_format: None,
                frame_rate: None,
            }),
            audio: audio.map(|(channel_count, sample_rate)| AudioParams {
                sample_rate,
                channel_layout: default_channel_layout(channel_count),
                sample_format: None,
            }),
        },
        time_base: timebase,
        start_time: None,
        duration: Some(duration),
        nb_frames: Some(sample_count),
        metadata: Metadata::new(),
    };
    let tables = TrackTables {
        kind,
        track_id: b.track_id.unwrap_or(0),
        codec,
        timebase,
        path,
        stts,
        ctts: b.ctts,
        stss: b.stss,
        stsc,
        fixed_size: stsz.fixed,
        sizes: stsz.sizes,
        stbl_count: stsz.count,
        sample_count,
        chunk_offsets,
        chunk_box,
        frags,
        frag_path,
    };
    Ok((stream, tables))
}

/// 按 kind 从 stsd entry 内容取宽高/声道采样率与首个子 box 载荷。
/// visual entry 固定字段 78 字节，audio 20 字节，其余按 8 字节
/// SampleEntry 基座找子 box。返回 (视频 (w,h)?, 音频 (声道,采样率)?, extra)。
type EntryFields = (Option<(u32, u32)>, Option<(u32, u32)>, Vec<u8>);

fn extract_entry_fields(kind: TrackKind, content: &[u8], trak_path: &str) -> Res<EntryFields> {
    let path = format!("{trak_path}.mdia.minf.stbl.stsd");
    let mut c = Cur::new(content);
    match kind {
        TrackKind::Video => {
            c.need(78, &path, "visual entry")?;
            c.skip(24, &path, "entry 固定字段")?;
            let width = c.u16(&path, "width")? as u32;
            let height = c.u16(&path, "height")? as u32;
            let extra = child_payload(content, 78, &path)?;
            Ok((Some((width, height)), None, extra))
        }
        TrackKind::Audio => {
            c.need(20, &path, "audio entry")?;
            // 两种 AudioSampleEntry 布局都支持，以 samplerate 高 16 位
            // （16.16 定点）与声道数的合理区间判别：
            // - ISO 基础布局（channelcount@8、samplerate@16，本 crate 手工
            //   构造测试字节的形态）；
            // - 扩展布局（本机实测 ffmpeg 渐进式 AAC：data_reference_index
            //   与 channelcount 之间多出 version/revision/vendor 共 8 字节，
            //   channelcount@16、samplerate@24；ffmpeg 自家 demux 靠 esds
            //   解码参数兜底，纯靠固定字段的读取方须识别此布局）。
            let rate_at = |off: usize| {
                content
                    .get(off..off + 4)
                    .map(|b| u32::from_be_bytes(b.try_into().unwrap()) >> 16)
            };
            let ch_at = |off: usize| {
                content
                    .get(off..off + 2)
                    .map(|b| u16::from_be_bytes(b.try_into().unwrap()))
            };
            let plausible_rate = |r: u32| (1000..=768_000).contains(&r);
            let plausible_ch = |ch: u16| (1..=64).contains(&ch);
            let form_a =
                rate_at(16).is_some_and(plausible_rate) && ch_at(8).is_some_and(plausible_ch);
            let form_b =
                rate_at(24).is_some_and(plausible_rate) && ch_at(16).is_some_and(plausible_ch);
            let (channel_count, sample_rate, fixed_len) = if form_a {
                (ch_at(8).unwrap() as u32, rate_at(16).unwrap(), 20)
            } else if form_b {
                (ch_at(16).unwrap() as u32, rate_at(24).unwrap(), 28)
            } else {
                return Err(malformed(&path, "audio entry 声道/采样率字段不可识别"));
            };
            let extra = child_payload(content, fixed_len, &path)?;
            Ok((None, Some((channel_count, sample_rate)), extra))
        }
        TrackKind::Other => {
            let extra = child_payload(content, 8, &path)?;
            Ok((None, None, extra))
        }
    }
}

/// entry 内第一个子 box 的载荷：按子 box 声明的 size 截取
/// （size==0 ⇒ 延伸到 entry 末；size==1 ⇒ 按 largesize）。声明越出
/// entry ⇒ Malformed。无子 box ⇒ 空。
fn child_payload(content: &[u8], at: usize, path: &str) -> Res<Vec<u8>> {
    if content.len() < at + 8 {
        return Ok(Vec::new());
    }
    let size = u32::from_be_bytes(content[at..at + 4].try_into().unwrap()) as u64;
    let header = match size {
        0 => 8,
        1 => {
            if content.len() < at + 16 {
                return Err(malformed(path, "子 box largesize 截断"));
            }
            16
        }
        n if n < 8 => return Err(malformed(path, format!("子 box size {n} 小于头长"))),
        _ => 8,
    };
    let end = match size {
        // size==0：子 box 延伸到 entry 末。
        0 => content.len() as u64,
        // size==1：真实大小在 largesize。
        1 => {
            let large = u64::from_be_bytes(content[at + 8..at + 16].try_into().unwrap());
            if large < 16 {
                return Err(malformed(
                    path,
                    format!("子 box largesize {large} 小于头长"),
                ));
            }
            at as u64 + large
        }
        n => at as u64 + n,
    };
    if end > content.len() as u64 {
        return Err(malformed(path, "子 box 越出 entry 范围"));
    }
    // 只取声明 size 内的载荷：兄弟 box（pasp/btrt/…）不得混进 extra_data。
    Ok(content[at + header..end as usize].to_vec())
}
