//! 契约 trait 接线：[`Demuxer`]/[`Muxer`] 实现与 [`ContainerRegistry`] 注册。
//!
//! 2026-10-24 移植裁决第 7/8 条预留的本阶段工作（见 docs/contracts.md）：
//! 把移植完成的 [`Mp4Reader`]/[`Mp4Writer`] 挂到 develop 契约 trait 上。
//! 本文件只做包装与映射，不改 reader/writer 的任何行为：
//!
//! - [`Mp4Demuxer`] 包 `Mp4Reader<Box<dyn ReadSeek>>`：`read_packet` 把
//!   随机访问 API 折成顺序游标语义（跨轨按 dts 归并，比较走
//!   `Timestamp::compare` 精确交叉相乘）；`seek` 按契约定位到指定流时基
//!   下 `target` 的**前一个关键帧**——二分找到 ≤ target 的最大 dts 样例，
//!   再经只读查询 [`Mp4Reader::keyframe_index_before`] 回溯关键帧，全程
//!   不依赖可变游标状态（合流裁决第 3 条）；
//! - [`Mp4Muxer`] 包 `Mp4Writer<Box<dyn WriteSeek>>`（progressive 布局）：
//!   trait 状态机（未写头/写入中/已收尾）映射 `ContainerError::InvalidState`；
//!   fMP4 等布局选项走具体类型 [`Mp4Writer`]，不经 trait（裁决第 7 条）；
//! - [`Mp4DemuxerFactory::probe`]：ftyp/styp box 头嗅探——size 合法 +
//!   类型四字符命中 + major brand 在常见集内 → [`ProbeScore::MAXIMUM`]，
//!   brand 可读但不在集内 → [`ProbeScore::EXTENSION`]，其余 → NONE；
//! - [`register_mp4`]：向注册表注册 demuxer/muxer 工厂（auto-probe 由
//!   [`ContainerRegistry::open_demuxer`] 驱动）。
//!
//! 包打戳契约（合流裁决第 4 条）由 [`Mp4Reader::read_packet`] 原生满足：
//! 产出的包已按流时基（`1/mdhd:timescale`）打戳并填 `stream_index`，
//! 本层零换算透传。

use std::cmp::Ordering;

use ye_codec::Packet;
use ye_container::{
    ContainerError, ContainerRegistry, Demuxer, DemuxerFactory, MediaHeader, Muxer, MuxerFactory,
    ProbeScore, ReadSeek, StreamTemplate, WriteSeek,
};
use ye_time::{Rational, Timestamp};

use crate::error::Mp4Error;
use crate::reader::Mp4Reader;
use crate::writer::{Layout, Mp4Writer, Mp4WriterOptions, TrackConfig, WriterError};

/// 工厂名（demuxer/muxer 同名，注册表按它点名）。
pub const FORMAT_NAME: &str = "mp4";

// —— 错误映射 ——

/// [`Mp4Error`] → [`ContainerError`]（trait 面）。
fn map_read_err(e: Mp4Error) -> ContainerError {
    match e {
        Mp4Error::Io(io) => ContainerError::Io(io),
        // 流索引语义在 trait 面是 StreamNotFound(u32)。
        Mp4Error::InvalidStreamIndex { index, .. } => {
            ContainerError::StreamNotFound(u32::try_from(index).unwrap_or(u32::MAX))
        }
        // Malformed/Unsupported/SampleIndexOutOfRange → InvalidData（带原文案）。
        other => ContainerError::InvalidData(other.to_string()),
    }
}

/// [`WriterError`] → [`ContainerError`]（trait 面）。
fn map_write_err(e: WriterError) -> ContainerError {
    match e {
        WriterError::Io(io) => ContainerError::Io(io),
        WriterError::Misuse(what) => ContainerError::InvalidState(what),
        // 数据驱动的时戳重标度失败 → Time（Muxer trait 契约：「重标度
        // 失败以 Err(ContainerError::Time(..)) 透出」；2026-10-24 评审
        // 统一——此前误归 Misuse → InvalidState，与 mock 实现分叉）。
        WriterError::Time(_) => ContainerError::Time(ye_time::TimeError::Overflow),
        WriterError::InvalidStreamIndex { index, .. } => {
            ContainerError::StreamNotFound(u32::try_from(index).unwrap_or(u32::MAX))
        }
        WriterError::Unsupported(what) => {
            ContainerError::InvalidData(format!("不支持的封装特性：{what}"))
        }
    }
}

// —— 探测 ——

/// ftyp/styp major brand 常见集（对齐 FFmpeg mov probe 的 brand 表的
/// 常用子集；命中即视作魔数完全命中。注意 `m4a `/`m4v `/`qt  ` 含空格）。
/// 不含 `heic` 等 HEIF 静态图片 brand（2026-10-24 评审：HEIC 是 ISOBMFF
/// 家族但无 moov/trak，命中 MAXIMUM 会让照片被当 MP4 假阳性录取——
/// 现在 heic 走未知 brand 路径（EXTENSION 半命中），再被读取端
/// 「至少一条 trak」校验在 open 阶段拒绝）。
const KNOWN_BRANDS: [[u8; 4]; 13] = [
    *b"isom", *b"iso2", *b"mp41", *b"mp42", *b"mp71", *b"avc1", *b"avc3", *b"mp4a", *b"m4v ",
    *b"dash", *b"msdh", *b"mse6", *b"qt  ",
];

/// ftyp/box 嗅探（[`DemuxerFactory::probe`] 的实现）。
///
/// 规则：≥8 字节、首 box size ≥ 8 且类型为 `ftyp`/`styp` 才继续；
/// major brand（box 载荷前 4 字节）在 [`KNOWN_BRANDS`] → MAXIMUM，
/// 可打印但不在集内 → EXTENSION（**部分魔数命中**：box 头可信但
/// brand 未知，如 heic 照片或罕见 brand 的真 MP4——探查管线无文件名
/// 输入，该档语义见 [`ProbeScore::EXTENSION`]；录取后的硬保证由读取端
/// 「至少一条 trak」校验兜底，非 MP4 结构的文件 open 即 InvalidData），
/// 不可打印 → NONE（乱数据）。窗口不足 12 字节（brand 读不到）按
/// EXTENSION 给分（box 头已可信；后续 open 同样受最少轨道校验约束）。
fn probe_mp4(head: &[u8]) -> ProbeScore {
    if head.len() < 8 {
        return ProbeScore::NONE;
    }
    let size = u32::from_be_bytes(head[0..4].try_into().unwrap()) as u64;
    if size < 8 {
        return ProbeScore::NONE;
    }
    match &head[4..8] {
        b"ftyp" | b"styp" => {}
        _ => return ProbeScore::NONE,
    }
    if head.len() < 12 {
        return ProbeScore::EXTENSION;
    }
    let brand: [u8; 4] = head[8..12].try_into().unwrap();
    if KNOWN_BRANDS.contains(&brand) {
        return ProbeScore::MAXIMUM;
    }
    let printable = brand.iter().all(|&b| (0x20..=0x7e).contains(&b));
    if printable {
        ProbeScore::EXTENSION
    } else {
        ProbeScore::NONE
    }
}

// —— 解封装 ——

/// MP4 的 [`Demuxer`] 实现：[`Mp4Reader`] 的顺序游标包装。
///
/// `read_packet` 的交错序：跨轨取「下一样例 dts 最小者」（并列取流号
/// 小者），dts 比较走 `Timestamp::compare`（i128 精确交叉相乘，无舍入）。
/// EOF 后幂等返回 `Ok(None)`（全部游标到尾即 EOF，状态不再变化）。
///
/// `seek(stream_index, target)`：二分找 ≤ target 的最大 dts 样例（同轨
/// dts 单调不减），经 [`Mp4Reader::keyframe_index_before`] 回溯关键帧，
/// 返回落地时刻（该流时基）。**其他轨**同时重定位到「首个 dts ≥ 落地
/// 时刻」的样例——seek 后继续 `read_packet` 不重放已过内容（契约只
/// 约束 seek 目标轨，其余轨的处置归本实现，前向恢复是对齐 FFmpeg
/// 丢弃语义的选择）。
pub struct Mp4Demuxer {
    reader: Mp4Reader<Box<dyn ReadSeek>>,
    /// 每轨下一待读样例序号（0-based，统一编址）。
    cursors: Vec<u64>,
}

impl Mp4Demuxer {
    /// 绑定输入并完成初始化解析（[`DemuxerFactory::open`] 的实现体）。
    pub fn new(input: Box<dyn ReadSeek>) -> Result<Self, ContainerError> {
        let reader = Mp4Reader::from_reader(input).map_err(map_read_err)?;
        let cursors = vec![0u64; reader.streams().len()];
        Ok(Mp4Demuxer { reader, cursors })
    }

    /// 该轨下一待读样例的 dts；`Ok(None)` = 本轨已读尽。
    fn next_dts(&self, si: usize) -> Result<Option<Timestamp>, ContainerError> {
        let count = self.reader.sample_count(si).map_err(map_read_err)?;
        if self.cursors[si] >= count as u64 {
            return Ok(None);
        }
        let timing = self
            .reader
            .sample_timing(si, self.cursors[si] as usize)
            .map_err(map_read_err)?;
        Ok(Some(timing.dts))
    }

    /// ≤ target 的最大 dts 样例序号（同轨 dts 单调不减，二分；
    /// target 早于首样例 → 0）。
    fn sample_at_or_before(&self, si: usize, target: Timestamp) -> Result<usize, ContainerError> {
        let count = self.reader.sample_count(si).map_err(map_read_err)?;
        if count == 0 {
            return Err(ContainerError::InvalidData(format!(
                "流 {si} 无样例，无法定位"
            )));
        }
        let dts_of = |i: usize| -> Result<Timestamp, ContainerError> {
            Ok(self.reader.sample_timing(si, i).map_err(map_read_err)?.dts)
        };
        if Timestamp::compare(dts_of(0)?, target) == Ordering::Greater {
            return Ok(0);
        }
        let (mut lo, mut hi) = (0usize, count - 1);
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2);
            if Timestamp::compare(dts_of(mid)?, target) != Ordering::Greater {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        Ok(lo)
    }

    /// 首个 dts ≥ bound 的样例序号（二分；全早于 bound → count，即 EOF）。
    fn sample_at_or_after(&self, si: usize, bound: Timestamp) -> Result<usize, ContainerError> {
        let count = self.reader.sample_count(si).map_err(map_read_err)?;
        let dts_of = |i: usize| -> Result<Timestamp, ContainerError> {
            Ok(self.reader.sample_timing(si, i).map_err(map_read_err)?.dts)
        };
        let (mut lo, mut hi) = (0usize, count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if Timestamp::compare(dts_of(mid)?, bound) == Ordering::Less {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    }
}

impl Demuxer for Mp4Demuxer {
    fn format_name(&self) -> &str {
        FORMAT_NAME
    }

    /// 流表克隆 + 容器级时长（各轨时长重标度到 canonical 后取最大）。
    /// 重复调用合法（每次重建，读取器状态不受影响）。
    fn read_header(&mut self) -> Result<MediaHeader, ContainerError> {
        let streams = self.reader.streams().to_vec();
        let mut duration: Option<Timestamp> = None;
        for s in &streams {
            let Some(d) = s.duration else { continue };
            let canonical = d.to_canonical()?;
            duration = Some(match duration {
                None => canonical,
                Some(cur) => {
                    if Timestamp::compare(canonical, cur) == Ordering::Greater {
                        canonical
                    } else {
                        cur
                    }
                }
            });
        }
        Ok(MediaHeader {
            format_name: FORMAT_NAME.to_string(),
            streams,
            duration,
            start_time: None,
            bit_rate: None,
            metadata: ye_container::Metadata::new(),
        })
    }

    fn read_packet(&mut self) -> Result<Option<Packet>, ContainerError> {
        // 跨轨归并：下一样例 dts 最小者（并列取流号小者，输出确定）。
        let mut pick: Option<usize> = None;
        let mut pick_dts = Timestamp::new(0, Rational::ONE);
        for si in 0..self.cursors.len() {
            let Some(dts) = self.next_dts(si)? else {
                continue;
            };
            match pick {
                None => {
                    pick = Some(si);
                    pick_dts = dts;
                }
                Some(_) => {
                    if Timestamp::compare(dts, pick_dts) == Ordering::Less {
                        pick = Some(si);
                        pick_dts = dts;
                    }
                }
            }
        }
        let Some(si) = pick else {
            return Ok(None); // 全轨读尽：此后幂等
        };
        let cursor = self.cursors[si] as usize;
        let pkt = self.reader.read_packet(si, cursor).map_err(map_read_err)?;
        self.cursors[si] += 1;
        Ok(Some(pkt))
    }

    fn seek(&mut self, stream_index: u32, target: Timestamp) -> Result<Timestamp, ContainerError> {
        let si = usize::try_from(stream_index)
            .map_err(|_| ContainerError::StreamNotFound(stream_index))?;
        if si >= self.reader.streams().len() {
            return Err(ContainerError::StreamNotFound(stream_index));
        }
        let at_or_before = self.sample_at_or_before(si, target)?;
        let key = self
            .reader
            .keyframe_index_before(si, at_or_before)
            .map_err(map_read_err)?;
        let landed = self
            .reader
            .sample_timing(si, key)
            .map_err(map_read_err)?
            .dts;
        // 目标轨定位到关键帧样例；其余轨前向恢复到首个 dts ≥ 落地时刻。
        self.cursors[si] = key as u64;
        for j in 0..self.cursors.len() {
            if j == si {
                continue;
            }
            self.cursors[j] = self.sample_at_or_after(j, landed)? as u64;
        }
        Ok(landed)
    }
}

/// MP4 的 [`DemuxerFactory`]。
pub struct Mp4DemuxerFactory;

impl DemuxerFactory for Mp4DemuxerFactory {
    fn name(&self) -> &'static str {
        FORMAT_NAME
    }

    fn probe(&self, head: &[u8]) -> ProbeScore {
        probe_mp4(head)
    }

    fn open(&self, input: Box<dyn ReadSeek>) -> Result<Box<dyn Demuxer>, ContainerError> {
        Ok(Box::new(Mp4Demuxer::new(input)?))
    }
}

// —— 封装 ——

/// trait 面的封装状态机（[`Muxer`] 契约：头 → 包* → 尾）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum MuxState {
    /// 尚未 write_header。
    BeforeHeader,
    /// 写入中。
    Writing,
    /// 已 write_trailer（或失败后封闭）。
    Closed,
}

/// MP4 的 [`Muxer`] 实现：[`Mp4Writer`]（progressive 布局）的 trait 包装。
///
/// 布局固定 progressive（`ftyp → mdat → moov`）与默认选项——fMP4、
/// free_size、co64 等封装选项是 trait 面承载不了的 mp4 专属面，调用方
/// 直接用具体类型 [`Mp4Writer`]（移植裁决第 7 条）。时基重标度、dts
/// 衔接校验等写入契约由 [`Mp4Writer::write_packet`] 原生执行（包时基
/// 与模板不符时内部按 `Rounding::NearInf` 重标度）。
pub struct Mp4Muxer {
    /// `finalize` 按值消费 writer，故包 Option；Closed 后为 None。
    writer: Option<Mp4Writer<Box<dyn WriteSeek>>>,
    state: MuxState,
}

impl Mp4Muxer {
    /// 绑定输出（[`MuxerFactory::open`] 的实现体）。
    pub fn new(output: Box<dyn WriteSeek>) -> Self {
        Mp4Muxer {
            writer: Some(Mp4Writer::new(
                output,
                Mp4WriterOptions {
                    layout: Layout::Progressive,
                    ..Default::default()
                },
            )),
            state: MuxState::BeforeHeader,
        }
    }

    /// 状态守卫：非 `Writing` 态调用包/尾写入 → `InvalidState`。
    fn require_writing(&self) -> Result<(), ContainerError> {
        match self.state {
            MuxState::BeforeHeader => Err(ContainerError::InvalidState(
                "write_packet/write_trailer 先于 write_header",
            )),
            MuxState::Writing => Ok(()),
            MuxState::Closed => Err(ContainerError::InvalidState(
                "write_trailer 已调用，实例已收尾",
            )),
        }
    }

    /// 封装失败（add_stream/写包/收尾）后封闭实例：后续调用统一
    /// `InvalidState`，半成品状态不可再被误用。
    fn seal(&mut self) {
        self.state = MuxState::Closed;
        self.writer = None;
    }
}

/// [`StreamTemplate`] → [`TrackConfig`]（封装模板到 stsd 级配置）。
/// `frame_rate`/`metadata` 是 trait 面字段，stsd 表达不了，丢弃。
fn track_config_of(t: &StreamTemplate) -> TrackConfig {
    TrackConfig {
        media_type: t.media_type,
        codec: t.codec.codec,
        timebase: t.time_base,
        video: t.codec.video.clone(),
        audio: t.codec.audio.clone(),
        extra_data: t.codec.extradata.clone().unwrap_or_default(),
    }
}

impl Muxer for Mp4Muxer {
    fn write_header(&mut self, streams: &[StreamTemplate]) -> Result<(), ContainerError> {
        if self.state != MuxState::BeforeHeader {
            return Err(ContainerError::InvalidState(
                "write_header 已调用过（trait 面单次）",
            ));
        }
        let Some(writer) = self.writer.as_mut() else {
            return Err(ContainerError::InvalidState("封装器已封闭"));
        };
        for t in streams {
            if let Err(e) = writer.add_stream(track_config_of(t)) {
                self.seal();
                return Err(map_write_err(e));
            }
        }
        self.state = MuxState::Writing;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<(), ContainerError> {
        self.require_writing()?;
        let Some(writer) = self.writer.as_mut() else {
            return Err(ContainerError::InvalidState("封装器已封闭"));
        };
        match writer.write_packet(packet) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.seal();
                Err(map_write_err(e))
            }
        }
    }

    fn write_trailer(&mut self) -> Result<(), ContainerError> {
        self.require_writing()?;
        let Some(writer) = self.writer.take() else {
            return Err(ContainerError::InvalidState("封装器已封闭"));
        };
        match writer.finalize() {
            Ok(_output) => {
                self.state = MuxState::Closed;
                Ok(())
            }
            Err(e) => {
                self.seal();
                Err(map_write_err(e))
            }
        }
    }
}

/// MP4 的 [`MuxerFactory`]。
pub struct Mp4MuxerFactory;

impl MuxerFactory for Mp4MuxerFactory {
    fn name(&self) -> &'static str {
        FORMAT_NAME
    }

    fn open(&self, output: Box<dyn WriteSeek>) -> Result<Box<dyn Muxer>, ContainerError> {
        Ok(Box::new(Mp4Muxer::new(output)))
    }
}

// —— 注册 ——

/// 向注册表注册 MP4 的 demuxer/muxer 工厂（同名替换语义见
/// [`ContainerRegistry::register_demuxer`]）。auto-probe 由
/// [`ContainerRegistry::open_demuxer`] 驱动，无需额外接线。
pub fn register_mp4(registry: &mut ContainerRegistry) {
    registry.register_demuxer(Box::new(Mp4DemuxerFactory));
    registry.register_muxer(Box::new(Mp4MuxerFactory));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    use ye_codec::{AudioParams, CodecId, CodecParameters, MediaType, PacketFlags, VideoParams};
    use ye_container::{Metadata, ReadSeek, Stream};
    use ye_pixfmt::ChannelLayout;
    use ye_time::{CANONICAL_TIME_BASE, Rational};

    // ---- 测试素材：Mp4Writer 造双轨小文件（零手工字节，往返自证） ----

    fn video_packet(si: u32, dts: i64, keyframe: bool, byte: u8) -> Packet {
        let mut p = Packet::empty(si);
        p.pts = Some(Timestamp::new(dts + 2, Rational::new(1, 10)));
        p.dts = Some(Timestamp::new(dts, Rational::new(1, 10)));
        p.duration = Some(Timestamp::new(1, Rational::new(1, 10)));
        p.data = std::sync::Arc::from(vec![byte; 4]);
        if keyframe {
            p.flags.insert(PacketFlags::KEYFRAME);
        }
        p
    }

    fn audio_packet(si: u32, dts: i64, byte: u8) -> Packet {
        let mut p = Packet::empty(si);
        p.pts = Some(Timestamp::new(dts, Rational::new(1, 100)));
        p.dts = Some(Timestamp::new(dts, Rational::new(1, 100)));
        p.duration = Some(Timestamp::new(10, Rational::new(1, 100)));
        p.data = std::sync::Arc::from(vec![byte; 2]);
        p.flags.insert(PacketFlags::KEYFRAME); // 音频恒关键帧
        p
    }

    /// 双轨 MP4：视频 1/10 时基 5 样例（关键帧 0/2 号），
    /// 音频 1/100 时基 5 样例（dts 0..40，步 10）。
    /// 布局：ftyp → mdat → moov（progressive）。
    fn build_two_track() -> Vec<u8> {
        let mut w = Mp4Writer::new(
            Cursor::new(Vec::<u8>::new()),
            Mp4WriterOptions {
                layout: Layout::Progressive,
                ..Default::default()
            },
        );
        w.add_stream(TrackConfig {
            media_type: MediaType::Video,
            codec: CodecId::H264,
            timebase: Rational::new(1, 10),
            video: Some(VideoParams {
                width: 64,
                height: 96,
                pixel_format: None,
                frame_rate: None,
            }),
            audio: None,
            extra_data: vec![0x01, 0x02, 0x03],
        })
        .unwrap();
        w.add_stream(TrackConfig {
            media_type: MediaType::Audio,
            codec: CodecId::Aac,
            timebase: Rational::new(1, 100),
            video: None,
            audio: Some(AudioParams {
                sample_rate: 48_000,
                channel_layout: Some(ChannelLayout::STEREO),
                sample_format: None,
            }),
            extra_data: vec![0x04, 0x05],
        })
        .unwrap();
        // 视频：样例 0/2 关键帧（stss 落 1/3 号，1-based）。
        for i in 0..5i64 {
            let key = i == 0 || i == 2;
            w.write_packet(&video_packet(0, i, key, 0xA0 + i as u8))
                .unwrap();
        }
        for i in 0..5i64 {
            w.write_packet(&audio_packet(1, i * 10, 0xB0 + i as u8))
                .unwrap();
        }
        let cursor = w.finalize().unwrap();
        cursor.into_inner()
    }

    fn registry() -> ContainerRegistry {
        let mut reg = ContainerRegistry::new();
        register_mp4(&mut reg);
        reg
    }

    fn open_vec(data: Vec<u8>) -> Box<dyn ReadSeek> {
        Box::new(Cursor::new(data))
    }

    // ---- probe 打分 ----

    fn ftyp_bytes(brand: &[u8; 4]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&24u32.to_be_bytes());
        v.extend_from_slice(b"ftyp");
        v.extend_from_slice(brand);
        v.extend_from_slice(&0u32.to_be_bytes());
        v.extend_from_slice(brand);
        v
    }

    #[test]
    fn probe_scoring() {
        let f = Mp4DemuxerFactory;
        // 已知 brand → MAXIMUM
        assert_eq!(f.probe(&ftyp_bytes(b"isom")), ProbeScore::MAXIMUM);
        assert_eq!(f.probe(&ftyp_bytes(b"qt  ")), ProbeScore::MAXIMUM);
        assert_eq!(f.probe(&ftyp_bytes(b"mp42")), ProbeScore::MAXIMUM);
        // 可读但未知 brand → EXTENSION
        assert_eq!(f.probe(&ftyp_bytes(b"zzzz")), ProbeScore::EXTENSION);
        // 不可打印 brand → NONE
        assert_eq!(
            f.probe(&ftyp_bytes(&[0x00, 0xFF, 0x01, 0x02])),
            ProbeScore::NONE
        );
        // box 头本身不合法 / 类型不对 → NONE
        assert_eq!(
            f.probe(&[0, 0, 0, 4, b'f', b't', b'y', b'p']),
            ProbeScore::NONE
        );
        assert_eq!(
            f.probe(&[0, 0, 0, 16, b'm', b'o', b'o', b'v']),
            ProbeScore::NONE
        );
        assert_eq!(f.probe(&[0u8; 7]), ProbeScore::NONE);
        assert_eq!(f.probe(&[]), ProbeScore::NONE);
        // 短窗口但 ftyp 头可信 → EXTENSION
        assert_eq!(f.probe(&ftyp_bytes(b"isom")[..8]), ProbeScore::EXTENSION);
        // styp（fMP4 分片起始 box）同 ftyp 处理
        let mut styp = ftyp_bytes(b"dash");
        styp[4..8].copy_from_slice(b"styp");
        assert_eq!(f.probe(&styp), ProbeScore::MAXIMUM);
    }

    // ---- 注册 + auto-probe ----

    /// heic 不再命中 MAXIMUM（HEIF 静态图片 major brand，2026-10-24
    /// 评审移出常见集）：降为未知 brand 的 EXTENSION 半命中。
    #[test]
    fn probe_heic_brand_is_not_maximum() {
        let f = Mp4DemuxerFactory;
        assert_eq!(f.probe(&ftyp_bytes(b"heic")), ProbeScore::EXTENSION);
    }

    /// 无 moov/trak 的文件（HEIC 照片形态：ftyp heic + free/mdat）经
    /// 注册表打开 ⇒ Err(InvalidData)，不得返回 0 路流的假成功
    /// （回归：此前 open_demuxer 返回 Ok(("mp4", 0 流))，ye-cli probe
    /// 输出「photo.heic: mp4, 0 路流」）。
    #[test]
    fn moovless_file_fails_at_open_not_fake_success() {
        let reg = registry();
        let mut data = ftyp_bytes(b"heic");
        // ftyp_bytes 声明 24 字节但只给 20：补齐（walk 须走到最少轨道
        // 校验，而不是先被 box 越界拦下）。
        data.extend_from_slice(&[0u8; 4]);
        data.extend_from_slice(&[0, 0, 0, 8, b'f', b'r', b'e', b'e']);
        data.extend_from_slice(&[0, 0, 0, 16, b'm', b'd', b'a', b't']);
        data.extend_from_slice(&[0u8; 8]);
        match reg.open_demuxer(open_vec(data)) {
            Err(ContainerError::InvalidData(msg)) => {
                assert!(msg.contains("无 trak 轨道"), "msg={msg}");
            }
            Err(_) => panic!("应为 InvalidData"),
            Ok(_) => panic!("moovless 文件不得打开成功"),
        }
    }

    /// 8~11 字节的短 ftyp 头「文件」：probe 可给 EXTENSION（box 头可信
    /// 的半命中），但 open 必须失败（回归：此前 0 轨返回 Ok）。
    #[test]
    fn short_ftyp_head_admitted_but_open_fails() {
        let reg = registry();
        match reg.open_demuxer(open_vec(vec![
            0, 0, 0, 24, b'f', b't', b'y', b'p', b'i', b's', b'o',
        ])) {
            Err(e) => assert!(
                matches!(e, ContainerError::InvalidData(_)),
                "应为 InvalidData，实为 {e:?}"
            ),
            Ok(_) => panic!("11 字节残件不得打开成功"),
        }
    }

    /// trait 面：数据驱动的时戳重标度溢出 ⇒ ContainerError::Time
    /// （Muxer 契约「重标度失败以 Time 透出」在真实实现侧的固化；
    /// mock 实现同语义。回归：2026-10-24 评审——此前 mp4 映射为
    /// InvalidState，与 mock 分叉）。
    #[test]
    fn muxer_rescale_overflow_surfaces_as_time_variant() {
        let reg = registry();
        let mut muxer = reg
            .create_muxer("mp4", Box::new(Cursor::new(Vec::<u8>::new())))
            .unwrap();
        let mut t = template();
        t.time_base = Rational::new(1, 1000);
        muxer.write_header(&[t]).unwrap();
        let mut p = Packet::empty(0);
        p.dts = Some(Timestamp::new(i64::MAX, Rational::ONE));
        p.duration = Some(Timestamp::new(1, Rational::ONE));
        let err = muxer.write_packet(&p).unwrap_err();
        assert!(matches!(err, ContainerError::Time(_)), "实为 {err:?}");
    }

    #[test]
    fn registry_registers_and_auto_probes() {
        let reg = registry();
        assert_eq!(reg.demuxer_names(), vec!["mp4"]);
        assert_eq!(reg.muxer_names(), vec!["mp4"]);
        // auto-probe：writer 产物（ftyp 头）被识别为 mp4
        let (name, demuxer) = reg
            .open_demuxer(open_vec(build_two_track()))
            .expect("auto-probe 应选中 mp4");
        assert_eq!(name, "mp4");
        assert_eq!(demuxer.format_name(), "mp4");
    }

    #[test]
    fn read_header_streams_and_canonical_duration() {
        let reg = registry();
        let (_, mut demuxer) = reg.open_demuxer(open_vec(build_two_track())).unwrap();
        let header = demuxer.read_header().unwrap();
        assert_eq!(header.format_name, "mp4");
        assert_eq!(header.streams.len(), 2);
        assert_eq!(header.streams[0].media_type, MediaType::Video);
        assert_eq!(header.streams[0].codec.codec, CodecId::H264);
        assert_eq!(header.streams[0].time_base, Rational::new(1, 10));
        assert_eq!(header.streams[0].nb_frames, Some(5));
        assert_eq!(header.streams[1].media_type, MediaType::Audio);
        assert_eq!(header.streams[1].time_base, Rational::new(1, 100));
        // 容器级时长在 canonical 时基：视频 5 单位@1/10 = 0.5s
        let duration = header.duration.expect("duration 应有值");
        assert_eq!(duration.time_base(), CANONICAL_TIME_BASE);
        assert_eq!(duration.ticks(), 7_056_000 / 2);
        // 重复 read_header 合法
        assert!(demuxer.read_header().is_ok());
    }

    // ---- read_packet：流时基打戳 + 跨轨 dts 归并 + EOF 幂等 ----

    #[test]
    fn read_packet_interleaves_by_dts_and_is_idempotent_at_eof() {
        let reg = registry();
        let (_, mut demuxer) = reg.open_demuxer(open_vec(build_two_track())).unwrap();
        let header = demuxer.read_header().unwrap();
        let mut got = Vec::new();
        while let Some(p) = demuxer.read_packet().unwrap() {
            // 打戳契约：时戳时基 == 流时基
            let tb = header.streams[p.stream_index as usize].time_base;
            assert_eq!(p.dts.unwrap().time_base(), tb, "dts 时基应为流时基");
            assert_eq!(p.pts.unwrap().time_base(), tb, "pts 时基应为流时基");
            assert_eq!(p.duration.unwrap().time_base(), tb);
            got.push((p.stream_index, p.dts.unwrap().ticks(), p.data[0]));
        }
        // 归并序：视频 dts 0,1,2,3,4 @1/10（0.0~0.4s）；
        // 音频 dts 0,10,20,30,40 @1/100（0.0~0.4s）。
        // 每步取当前头包 dts 最小者（并列取流号小者）：
        // 0.0s v=a 并列 → v；0.0s a 独小 → a；0.1s v=a 并列 → v；……
        assert_eq!(
            got,
            vec![
                (0, 0, 0xA0),  // 0.0s v（并列取流号小）
                (1, 0, 0xB0),  // 0.0s a
                (0, 1, 0xA1),  // 0.1s v（与 a1 同时，流号小者先）
                (1, 10, 0xB1), // 0.1s a
                (0, 2, 0xA2),  // 0.2s v
                (1, 20, 0xB2), // 0.2s a
                (0, 3, 0xA3),  // 0.3s v
                (1, 30, 0xB3), // 0.3s a
                (0, 4, 0xA4),  // 0.4s v
                (1, 40, 0xB4), // 0.4s a
            ],
            "跨轨应按下一头包 dts 归并（并列取流号小者）"
        );
        // EOF 幂等
        assert!(demuxer.read_packet().unwrap().is_none());
        assert!(demuxer.read_packet().unwrap().is_none());
    }

    // ---- seek：前一关键帧 + 返回值流时基 + 其余轨前向恢复 ----

    #[test]
    fn seek_lands_on_previous_keyframe() {
        let reg = registry();
        let (_, mut demuxer) = reg.open_demuxer(open_vec(build_two_track())).unwrap();
        let header = demuxer.read_header().unwrap();
        let v_tb = header.streams[0].time_base;

        // target 0.35s（用毫秒时基表达，验证跨时基比较）：
        // ≤0.35s 的最大视频样例是 3 号（0.3s，非关键帧）→ 回溯 2 号（0.2s）。
        let target = Timestamp::new(350, Rational::new(1, 1000));
        let landed = demuxer.seek(0, target).unwrap();
        assert_eq!(landed.time_base(), v_tb, "返回值应为流时基");
        assert_eq!(landed.ticks(), 2, "应落到 2 号关键帧（0.2s）");

        // seek 后继续读：从视频 2 号开始；音频前向恢复到首个 dts ≥ 0.2s
        // （即 20 单位处）——不重放 0/10 单位的旧音频。
        let p = demuxer.read_packet().unwrap().unwrap();
        assert_eq!((p.stream_index, p.dts.unwrap().ticks()), (0, 2));
        let mut audio_dts = Vec::new();
        while let Some(p) = demuxer.read_packet().unwrap() {
            if p.stream_index == 1 {
                audio_dts.push(p.dts.unwrap().ticks());
            }
        }
        assert_eq!(audio_dts, vec![20, 30, 40], "音频应前向恢复到 ≥ 落地时刻");

        // target 早于首样例 → 落 0 号（首样例即关键帧）
        let landed = demuxer.seek(0, Timestamp::new(-1, Rational::ONE)).unwrap();
        assert_eq!(landed.ticks(), 0);

        // 流不存在 → StreamNotFound
        let err = demuxer.seek(9, target).unwrap_err();
        assert!(matches!(err, ContainerError::StreamNotFound(9)));
    }

    // ---- muxer：状态机 + 经注册表往返 ----

    #[test]
    fn muxer_state_machine_rejects_out_of_order_calls() {
        let reg = registry();
        let mut muxer = reg
            .create_muxer("mp4", Box::new(Cursor::new(Vec::<u8>::new())))
            .unwrap();
        // 未写头先写包 → InvalidState
        let err = muxer.write_packet(&Packet::empty(0)).unwrap_err();
        assert!(matches!(err, ContainerError::InvalidState(_)));
        // 未写头先收尾 → InvalidState
        let err = muxer.write_trailer().unwrap_err();
        assert!(matches!(err, ContainerError::InvalidState(_)));
        // write_header 后再 write_header → InvalidState（trait 面单次）
        muxer.write_header(&[template()]).unwrap();
        let err = muxer.write_header(&[template()]).unwrap_err();
        assert!(matches!(err, ContainerError::InvalidState(_)));
    }

    #[test]
    fn muxer_rejects_packet_after_trailer() {
        let reg = registry();
        let mut muxer = reg
            .create_muxer("mp4", Box::new(Cursor::new(Vec::<u8>::new())))
            .unwrap();
        muxer.write_header(&[template()]).unwrap();
        muxer.write_packet(&video_packet(0, 0, true, 1)).unwrap();
        muxer.write_trailer().unwrap();
        let err = muxer
            .write_packet(&video_packet(0, 1, false, 2))
            .unwrap_err();
        assert!(matches!(err, ContainerError::InvalidState(_)));
    }

    fn template() -> StreamTemplate {
        StreamTemplate {
            media_type: MediaType::Video,
            codec: CodecParameters {
                codec: CodecId::H264,
                media_type: MediaType::Video,
                extradata: Some(vec![0x01, 0x02]),
                bit_rate: None,
                video: Some(VideoParams {
                    width: 64,
                    height: 96,
                    pixel_format: None,
                    frame_rate: None,
                }),
                audio: None,
            },
            time_base: Rational::new(1, 10),
            frame_rate: None,
            metadata: Metadata::new(),
        }
    }

    /// trait 写路径的可取回输出：Write+Seek 落在共享缓冲上
    /// （`write_trailer` 按值消费 writer，输出句柄必须外部共享）。
    #[derive(Clone)]
    struct SharedBuf {
        data: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
        pos: usize,
    }

    impl SharedBuf {
        fn new() -> (SharedBuf, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
            let data = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            (
                SharedBuf {
                    data: data.clone(),
                    pos: 0,
                },
                data,
            )
        }
    }

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut d = self.data.lock().unwrap();
            // 标准 Write 语义：原位覆写（finalize 会后向寻址回填 mdat
            // largesize——截断会抹掉已写数据），越过末尾则稀疏补零。
            if self.pos > d.len() {
                d.resize(self.pos, 0);
            }
            let end = self.pos + buf.len();
            if end > d.len() {
                d.resize(end, 0);
            }
            d[self.pos..end].copy_from_slice(buf);
            self.pos = end;
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl std::io::Seek for SharedBuf {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            let len = self.data.lock().unwrap().len() as i64;
            let new = match pos {
                std::io::SeekFrom::Start(s) => s as i64,
                std::io::SeekFrom::End(e) => len + e,
                std::io::SeekFrom::Current(c) => self.pos as i64 + c,
            };
            if new < 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "negative seek",
                ));
            }
            self.pos = new as usize;
            Ok(new as u64)
        }
    }

    /// trait 面全链路往返：Muxer 写（模板时基 1/1000、包时基 1/10——
    /// 验证 muxer 内部重标度）→ 注册表 auto-probe 读回 → 逐包比对。
    #[test]
    fn muxer_demuxer_round_trip_through_registry() {
        let (shared, handle) = SharedBuf::new();
        let reg = registry();
        let mut muxer = reg
            .create_muxer("mp4", Box::new(shared))
            .expect("create_muxer mp4");
        // 模板时基 1/1000；包自带 1/10 时基 → muxer 内部 NearInf 重标度。
        let mut t = template();
        t.time_base = Rational::new(1, 1000);
        muxer.write_header(&[t]).expect("write_header");
        for i in 0..3i64 {
            muxer
                .write_packet(&video_packet(0, i, i == 0, 0xC0 + i as u8))
                .expect("write_packet");
        }
        muxer.write_trailer().expect("write_trailer");
        drop(muxer);

        let bytes = handle.lock().unwrap().clone();
        let (_, mut demuxer) = reg
            .open_demuxer(Box::new(Cursor::new(bytes)))
            .expect("auto-probe 应识别 mp4");
        let header = demuxer.read_header().expect("read_header");
        assert_eq!(header.streams.len(), 1);
        // 输出流时基 = 模板时基（1/1000）。
        assert_eq!(header.streams[0].time_base, Rational::new(1, 1000));
        // 包 dts i @1/10 → i×100 单位 @1/1000；pts = dts+2@1/10 → +200。
        for i in 0..3i64 {
            let p = demuxer.read_packet().unwrap().expect("read_packet");
            let tb = Rational::new(1, 1000);
            assert_eq!(p.dts.unwrap(), Timestamp::new(i * 100, tb));
            assert_eq!(p.pts.unwrap(), Timestamp::new(i * 100 + 200, tb));
            assert_eq!(p.duration.unwrap(), Timestamp::new(100, tb));
            assert_eq!(p.data.as_ref(), &[0xC0 + i as u8; 4][..]);
            // 关键帧标志：仅首包（stss 只含样例 1）。
            assert_eq!(p.flags.contains(PacketFlags::KEYFRAME), i == 0);
        }
        assert!(demuxer.read_packet().unwrap().is_none());
    }

    // ---- 流表来源（Stream）与模板（StreamTemplate）的转换一致性 ----

    /// Demuxer 读出的 Stream 转 StreamTemplate 再喂 Muxer（remux 链路的
    /// 类型基础）：能 add_stream 成功且流数一致。
    #[test]
    fn stream_to_template_feeds_muxer() {
        let reg = registry();
        let (_, mut demuxer) = reg.open_demuxer(open_vec(build_two_track())).unwrap();
        let header = demuxer.read_header().unwrap();
        let templates: Vec<StreamTemplate> = header
            .streams
            .iter()
            .map(|s: &Stream| StreamTemplate {
                media_type: s.media_type,
                codec: s.codec.clone(),
                time_base: s.time_base,
                frame_rate: s.codec.video.as_ref().and_then(|v| v.frame_rate),
                metadata: s.metadata.clone(),
            })
            .collect();
        let mut muxer = reg
            .create_muxer("mp4", Box::new(Cursor::new(Vec::<u8>::new())))
            .unwrap();
        muxer.write_header(&templates).unwrap();
        // 写入一个包验证时基衔接（流时基即模板时基，恒等重标度）。
        muxer.write_packet(&video_packet(0, 0, true, 1)).unwrap();
        muxer.write_packet(&audio_packet(1, 0, 2)).unwrap();
        muxer.write_trailer().unwrap();
    }

    // ---- 线程安全（全库约定）----

    #[test]
    fn adapter_types_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Mp4Demuxer>();
        assert_send_sync::<Mp4Muxer>();
        assert_send_sync::<Mp4DemuxerFactory>();
        assert_send_sync::<Mp4MuxerFactory>();
    }
}
