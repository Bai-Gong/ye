//! MP4 封装器（progressive 与 fMP4/fragmented）。
//!
//! progressive 布局硬契约：`ftyp → free(可选) → mdat → moov`。mdat 头恒用
//! largesize 形式（size==1 + 8 字节 u64，共 16 字节）：封装期占位、finalize
//! 回填真实总长——头长恒定 ⇒ 样例 chunk 偏移写入即定、免回填位移，文件
//! >4 GiB 天然正确（自有 reader 支持 largesize box）。moov 后置同理。
//!
//! 样例表：单 stsd entry（H264→avc1+avcC、Aac→mp4a+esds，`extra_data`
//! 原样回写不解释）；stts run-length 合并；ctts v1 仅当存在 pts≠dts（负
//! 偏移合法）；stss 仅视频轨且非全关键帧；stsz 全等且非 0 时定长否则
//! 变长；任一 chunk offset 超 u32::MAX 或 force_co64 ⇒ 全文件 co64。
//! 同轨在文件内连续的样例自动合成一个 chunk（stsc 段最小）。
//!
//! fMP4 布局硬契约（[`Layout::Fragmented`]）：`ftyp → 前置 moov（完整
//! stsd + 空样例表 stts/stsc/stsz/stco + mvex(trex 默认全 0)）→
//! (moof + mdat)*`，free 不写。样例数据在内存攒批（moof 前置 ⇒ 数据须
//! 等 moof 定长后才能落盘），`flush_fragment`（手动 / finalize 冲尾 /
//! `fragment_duration` 关键帧自动切）时切出：moof = mfhd(序号自 1 递增)
//! 加上每有 pending 数据的轨一个 traf（tfhd 恒 default-base-is-moof 且
//! 写 sample_description_index=1；tfdt 恒 v1 = 本 traf 首样例 dts；trun
//! 恒 v1 且 per-sample 显式 duration/size/flags/cts）。moof 在内存两遍
//! 拼装（data_offset 定宽 i32，第二遍回填「该轨数据起点 − moof 起点」）；
//! mdat 恒 16 字节 size==1+largesize 头（flush 时总长已知，无需回填）。
//! finalize 冲尾并回填前置 moov 的 mvhd/tkhd/mdhd duration——moov 写出
//! 时以 >u32::MAX 占位值强制 v1（u64 定宽）⇒ 字段偏移恒定，从 box size
//! 逐层定位后原位改写。
//!
//! 时戳换算（2026-10-24 移植裁决）：包自带任意时基（自描述
//! `Timestamp`），入表前一律重标度到轨道时基单位域
//! （`Timestamp::rescale`，[`Rounding::NearInf`]——对齐 FFmpeg
//! `av_rescale_q` 默认）。dts 衔接校验与 stts delta 因此都在单位域
//! 进行；包时基与轨道时基一致（remux 常态）时重标度恒等、逐包精确。
//! mvhd/tkhd 的 movie 时基（1/1000）换算同取 NearInf。
//! `fragment_duration` 阈值比较走 `Timestamp::compare` 精确交叉相乘，
//! 无舍入。两布局共用同一条 write_packet 校验链（负 dts/间隙/回退一律
//! Misuse）；区别仅在首包：progressive 须 dts==0（stts 隐式 0 起算），
//! fragmented 由 tfdt 承载任意非负首 dts。

use std::cmp::Ordering;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use ye_codec::{AudioParams, CodecId, MediaType, Packet, PacketFlags, VideoParams};

use crate::types::{FourCc, fourcc_of_codec};
use ye_time::{Rational, Timestamp};

/// mdat 恒定 largesize 头长（size u32 + type 4 字节 + largesize u64）。
const MDAT_HEADER: u64 = 16;

/// mvhd/movie 时基（毫秒）。
const MOVIE_TIMESCALE: u32 = 1000;

// —— 错误 ——

/// MP4 封装错误（独立于读取端 [`crate::Mp4Error`]，两模块零文件交集）。
#[derive(Debug)]
pub enum WriterError {
    /// 底层 I/O 失败。
    Io(std::io::Error),
    /// 明确不支持的特性（未知 codec/类别等）。
    Unsupported(&'static str),
    /// 流索引越界。
    InvalidStreamIndex { index: usize, count: usize },
    /// 调用方违反写入契约（时基/时戳/调用顺序，见 contracts.md）。
    Misuse(&'static str),
    /// 时戳/时基换算失败（数据驱动的溢出，如包时基重标度到轨道时基
    /// 落不回 i64——区别于调用顺序违约的 [`WriterError::Misuse`]；
    /// trait 面按 [`ye_container::muxer`] 契约映射
    /// `ContainerError::Time`，2026-10-24 评审统一）。
    Time(&'static str),
}

impl std::fmt::Display for WriterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriterError::Io(e) => write!(f, "I/O 错误：{e}"),
            WriterError::Unsupported(what) => write!(f, "不支持的封装特性：{what}"),
            WriterError::InvalidStreamIndex { index, count } => {
                write!(f, "流索引越界：{index}（共 {count} 路）")
            }
            WriterError::Misuse(what) => write!(f, "写入契约违规：{what}"),
            WriterError::Time(what) => write!(f, "时戳换算失败：{what}"),
        }
    }
}

impl std::error::Error for WriterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WriterError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for WriterError {
    fn from(e: std::io::Error) -> Self {
        WriterError::Io(e)
    }
}

// —— 布局与选项 ——

/// 封装布局。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// ftyp → free → mdat → moov 一次成片。
    Progressive,
    /// ftyp → 前置 moov（空样例表 + mvex/trex）→ (moof + mdat)* 分片流。
    Fragmented,
}

/// 封装选项。
///
/// 硬性构造约定（写入 contracts.md）：任何代码（含全部测试）构造本类型
/// 一律 `Mp4WriterOptions { layout: .., ..Default::default() }` 结构更新
/// 语法，禁止全字段字面量——后续加字段不破调用方编译。形状本模块一次性
/// 定形（含 `fragment_duration`）。
#[derive(Clone, Debug)]
pub struct Mp4WriterOptions {
    pub layout: Layout,
    /// ftyp 后 free 载荷字节数（仅 Progressive 生效）；0 ⇒ 不写 free；
    /// 上限 u64::MAX-8（free box 总长 8+free_size 不得溢出 u64，超出在
    /// 首个写出点报 Misuse）。
    pub free_size: u64,
    /// 强制 co64（测试/显式控制）；否则任一 chunk offset 超 u32::MAX 才升级。
    pub force_co64: bool,
    /// 仅 Fragmented 生效：新到关键帧且本轨待切跨度（轨道时基）≥ 此值时
    /// 自动 `flush_fragment`；None ⇒ 仅手动 flush / finalize 冲尾。
    /// 自描述 `Timestamp`，任意时基——比较走精确交叉相乘，无舍入。
    pub fragment_duration: Option<Timestamp>,
}

impl Default for Mp4WriterOptions {
    fn default() -> Self {
        Mp4WriterOptions {
            layout: Layout::Progressive,
            free_size: 0,
            force_co64: false,
            fragment_duration: None,
        }
    }
}

/// 一路待封装轨的配置（`add_stream` 入参）。develop 契约类型版：
/// `media_type`/`codec`（[`CodecId`]）取代 m1 的 MediaKind/FourCc，
/// `video`/`audio` 直接用 ye-codec 参数（stsd 表达不了的字段忽略）。
#[derive(Clone, Debug)]
pub struct TrackConfig {
    /// 仅 Video/Audio；Subtitle → [`WriterError::Unsupported`]。
    pub media_type: MediaType,
    /// 仅 H264（Video，写 avc1）/ Aac（Audio，写 mp4a）；其他 → Unsupported。
    pub codec: CodecId,
    /// 须 num==1（`1/mdhd:timescale`）；否则 Misuse。
    pub timebase: Rational,
    /// media_type==Video 须 Some（width/height 进 stsd entry）。
    pub video: Option<VideoParams>,
    /// media_type==Audio 须 Some（声道布局/采样率 16.16）。
    pub audio: Option<AudioParams>,
    /// avcC/esds 载荷（esds 含 4 字节版本/标志），原样写回不解释。
    pub extra_data: Vec<u8>,
}

impl TrackConfig {
    /// 从读取端 [`ye_container::Stream`] 直取封装配置（remux 场景）。
    /// extra_data 取 `codec.extradata`（缺省为空——avc1/mp4a 轨正常
    /// 均携带；缺失时产出无子 box 的 entry，合法性由 add_stream 校验）。
    pub fn from_stream(s: &ye_container::Stream) -> TrackConfig {
        TrackConfig {
            media_type: s.media_type,
            codec: s.codec.codec,
            timebase: s.time_base,
            video: s.codec.video.clone(),
            audio: s.codec.audio.clone(),
            extra_data: s.codec.extradata.clone().unwrap_or_default(),
        }
    }
}

/// [`TrackConfig`] 视频判定（stss/handler/volume 走视频分支的判定点）。
fn is_video_cfg(cfg: &TrackConfig) -> bool {
    cfg.media_type == MediaType::Video
}

// —— pub(crate) 组装件（fMP4 组装的编译契约）——

/// 大端字节追加器 + box 包装工具。
/// 注意 `into_box`/`into_box64` 取 `self`（按值），不能接在返回
/// `&mut Self` 的追加链尾直接调用——先追加完、再以所有权变量消费。
pub(crate) struct BoxBuf {
    buf: Vec<u8>,
}

impl Default for BoxBuf {
    fn default() -> Self {
        BoxBuf::new()
    }
}

impl BoxBuf {
    pub(crate) fn new() -> BoxBuf {
        BoxBuf { buf: Vec::new() }
    }

    pub(crate) fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub(crate) fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub(crate) fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub(crate) fn i32(&mut self, v: i32) -> &mut Self {
        self.buf.extend_from_slice(&(v as u32).to_be_bytes());
        self
    }

    pub(crate) fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(b);
        self
    }

    /// 包装为普通 box：u32 size + type + 载荷。
    /// 总长装不进 u32 时自动降级为 largesize 头（读端皆支持）。
    pub(crate) fn into_box(self, kind: &[u8; 4]) -> Vec<u8> {
        let total = self.buf.len() as u64 + 8;
        if u32::try_from(total).is_err() {
            return self.into_box64(kind);
        }
        let mut out = Vec::with_capacity(self.buf.len() + 8);
        out.extend_from_slice(&(total as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(&self.buf);
        out
    }

    /// 包装为 largesize box：size==1 + type + 8 字节真实总长（恒 16 字节头）。
    pub(crate) fn into_box64(self, kind: &[u8; 4]) -> Vec<u8> {
        let total = self.buf.len() as u64 + 16;
        let mut out = Vec::with_capacity(self.buf.len() + 16);
        out.extend_from_slice(&1u32.to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(&total.to_be_bytes());
        out.extend_from_slice(&self.buf);
        out
    }
}

/// ftyp：major isom / minor 0x200 / 兼容 brands isom,iso2,avc1,mp4a。
pub(crate) fn build_ftyp() -> Vec<u8> {
    let mut b = BoxBuf::new();
    b.bytes(b"isom").u32(0x200).bytes(b"isomiso2avc1mp4a");
    b.into_box(b"ftyp")
}

/// timebase 须为 `1/timescale`（num==1，den ∈ [1, u32::MAX∩i32 正域]）→
/// 返回 timescale。
pub(crate) fn validate_timebase(tb: &Rational) -> Result<u32, WriterError> {
    if tb.num() != 1 {
        return Err(WriterError::Misuse("timebase 须为 1/timescale（num==1）"));
    }
    // Rational 分母为正 i32（不变式保证 den > 0），天然落在 mdhd 的 u32
    // 字段域内（i32::MAX < u32::MAX）——仅防御性拒绝非正值。
    if tb.den() <= 0 {
        return Err(WriterError::Misuse(
            "timescale 须在 1..=u32::MAX（mdhd 字段宽度）",
        ));
    }
    Ok(tb.den() as u32)
}

/// stsd 单 entry（avc1+avcC / mp4a+esds），extra_data 原样回写。
/// 返回 entry 本身（含 8 字节 box 头），由 [`build_stsd_box`] 包进 stsd。
pub(crate) fn build_stsd_entry(cfg: &TrackConfig) -> Result<Vec<u8>, WriterError> {
    match cfg.media_type {
        MediaType::Video => {
            if fourcc_of_codec(cfg.codec) != Some(FourCc::new(*b"avc1")) {
                return Err(WriterError::Unsupported(
                    "仅支持 avc1（视频）与 mp4a（音频）",
                ));
            }
            let Some(v) = &cfg.video else {
                return Err(WriterError::Misuse("Video 流须带 video 信息"));
            };
            // visual entry 固定字段 78 字节，宽高为 u16 字段。
            if v.width > u16::MAX as u32 || v.height > u16::MAX as u32 {
                return Err(WriterError::Misuse(
                    "宽/高超出 u16（stsd visual entry 字段宽度）",
                ));
            }
            let mut e = BoxBuf::new();
            e.bytes(&[0; 6]) // reserved
                .u16(1) // data_reference_index
                .u16(0) // pre_defined
                .u16(0) // reserved
                .bytes(&[0; 12]) // pre_defined[3]
                .u16(v.width as u16)
                .u16(v.height as u16)
                .u32(0x0048_0000) // horizresolution 72dpi
                .u32(0x0048_0000) // vertresolution
                .u32(0) // reserved
                .u16(1) // frame_count
                .u32(0) // compressorname 长度前缀
                .bytes(&[0; 28]) // compressorname 本体（32 字节）
                .u16(24) // depth
                .u16(0xFFFF); // pre_defined = -1（i16）
            let mut avcc = BoxBuf::new();
            avcc.bytes(&cfg.extra_data);
            e.bytes(&avcc.into_box(b"avcC"));
            Ok(e.into_box(b"avc1"))
        }
        MediaType::Audio => {
            if fourcc_of_codec(cfg.codec) != Some(FourCc::new(*b"mp4a")) {
                return Err(WriterError::Unsupported(
                    "仅支持 avc1（视频）与 mp4a（音频）",
                ));
            }
            let Some(a) = &cfg.audio else {
                return Err(WriterError::Misuse("Audio 流须带 audio 信息"));
            };
            // 声道数取布局的 channels()（stsd 只收一个 u16）。
            let channel_count = a.channel_layout.map(|l| l.channels()).unwrap_or(0);
            if channel_count == 0 || channel_count > u16::MAX as u32 {
                return Err(WriterError::Misuse("声道数须在 1..=u16（stsd 字段宽度）"));
            }
            // samplerate 为 16.16 定点，整数段即高 16 位。
            if a.sample_rate == 0 || a.sample_rate > u16::MAX as u32 {
                return Err(WriterError::Misuse(
                    "采样率须在 1..=u16（16.16 定点整数段）",
                ));
            }
            let mut e = BoxBuf::new();
            // ISO 14496-12 AudioSampleEntry：SampleEntry 基座(8) +
            // reserved u32[2](8) + 声道/样宽/pre_defined/reserved(8) +
            // 采样率 16.16(4) = 固定 28 字节，然后 esds 子 box。
            // （读取端 extract_entry_fields 已按 20/28 双布局自动判别，
            // ffmpeg 产物与本封装器输出均为 28 字节布局。）
            e.bytes(&[0; 6]) // reserved（SampleEntry 基座）
                .u16(1) // data_reference_index
                .u32(0)
                .u32(0) // reserved u32[2]
                .u16(channel_count as u16)
                .u16(16) // samplesize
                .u16(0) // pre_defined
                .u16(0) // reserved
                .u32(a.sample_rate << 16);
            let mut esds = BoxBuf::new();
            esds.bytes(&cfg.extra_data);
            e.bytes(&esds.into_box(b"esds"));
            Ok(e.into_box(b"mp4a"))
        }
        MediaType::Subtitle => Err(WriterError::Unsupported("仅支持视频/音频轨（vide/soun）")),
    }
}

/// stsd box：version/flags + entry_count=1 + 单 entry。
fn build_stsd_box(cfg: &TrackConfig) -> Result<Vec<u8>, WriterError> {
    let entry = build_stsd_entry(cfg)?;
    let mut b = BoxBuf::new();
    b.u32(0).u32(1).bytes(&entry);
    Ok(b.into_box(b"stsd"))
}

/// mvhd（时长装得进 u32 用 v0，否则 v1）。
pub(crate) fn build_mvhd(movie_timescale: u32, duration_units: u64, next_track_id: u32) -> Vec<u8> {
    let mut b = BoxBuf::new();
    match u32::try_from(duration_units) {
        Ok(d) => {
            b.u32(0) // version 0 + flags
                .u32(0) // creation_time
                .u32(0) // modification_time
                .u32(movie_timescale)
                .u32(d);
        }
        Err(_) => {
            b.u32(1 << 24) // version 1 + flags
                .u64(0)
                .u64(0)
                .u32(movie_timescale)
                .u64(duration_units);
        }
    }
    b.u32(0x0001_0000) // rate 1.0
        .u16(0x0100) // volume 1.0
        .u16(0) // reserved
        .u32(0)
        .u32(0) // reserved
        // unity matrix
        .u32(0x0001_0000)
        .u32(0)
        .u32(0)
        .u32(0)
        .u32(0)
        .u32(0x0001_0000)
        .u32(0)
        .u32(0)
        .u32(0)
        .u32(0x4000_0000);
    for _ in 0..6 {
        b.u32(0); // pre_defined
    }
    b.u32(next_track_id);
    b.into_box(b"mvhd")
}

/// trak 骨架：tkhd + mdia(mdhd/hdlr/minf(stbl(stsd+给定样例表)))。
///
/// `duration_units` 写 mdhd（轨道时基域）；`tkhd_duration_units` 写
/// tkhd（movie 时基域，ISO 14496-12 §8.3.2——tkhd duration 以 mvhd
/// timescale=1000 表达，ffmpeg movenc 同此）——两参数分开（2026-10-24
/// 评审：此前同值直写轨道单位，timescale≠1000 的轨会错报时长，如
/// 48kHz 音频 1 秒写成 48000 即 480 秒）。fMP4 复用本骨架：传空样例表
/// + 占位 duration（>u32::MAX 强制 v1 定宽，finalize 回填真实时长）。
pub(crate) fn build_trak_skeleton(
    track_id: u32,
    cfg: &TrackConfig,
    timescale: u32,
    duration_units: u64,
    tkhd_duration_units: u64,
    stbl_children: Vec<Vec<u8>>,
    edit: bool,
) -> Vec<u8> {
    let is_video = is_video_cfg(cfg);
    // tkhd 16.16 宽高（clamp 到 u16 段，字段上限）。
    let (w, h) = cfg.video.as_ref().map_or((0u32, 0u32), |v| {
        (v.width.min(0xFFFF), v.height.min(0xFFFF))
    });

    let mut t = BoxBuf::new();
    match u32::try_from(tkhd_duration_units) {
        Ok(d) => {
            t.u32(0x0000_0003) // version 0 + flags=3（enabled|in_movie）
                .u32(0)
                .u32(0) // creation/modification
                .u32(track_id)
                .u32(0) // reserved
                .u32(d);
        }
        Err(_) => {
            t.u32(0x0100_0003) // version 1 + flags=3
                .u64(0)
                .u64(0)
                .u32(track_id)
                .u32(0)
                .u64(tkhd_duration_units);
        }
    }
    t.u32(0)
        .u32(0) // reserved
        .u16(0) // layer
        .u16(0) // alternate_group
        .u16(if is_video { 0 } else { 0x0100 }) // volume（音频 1.0）
        .u16(0) // reserved
        // unity matrix
        .u32(0x0001_0000)
        .u32(0)
        .u32(0)
        .u32(0)
        .u32(0x0001_0000)
        .u32(0)
        .u32(0)
        .u32(0)
        .u32(0x4000_0000)
        .u32(w << 16) // width 16.16
        .u32(h << 16); // height 16.16
    let tkhd = t.into_box(b"tkhd");

    let mut m = BoxBuf::new();
    match u32::try_from(duration_units) {
        Ok(d) => {
            m.u32(0).u32(0).u32(0).u32(timescale).u32(d);
        }
        Err(_) => {
            m.u32(1 << 24)
                .u64(0)
                .u64(0)
                .u32(timescale)
                .u64(duration_units);
        }
    }
    m.u16(0x55C4) // language=und
        .u16(0); // pre_defined
    let mdhd = m.into_box(b"mdhd");

    let mut hd = BoxBuf::new();
    let handler: &[u8; 4] = if is_video { b"vide" } else { b"soun" };
    let name: &[u8] = if is_video {
        b"VideoHandler\0"
    } else {
        b"SoundHandler\0"
    };
    // handler 双写：ISO 14496-12 的 handler_type 在 pre_defined 之后（+8），
    // 本 crate 读取端 parse_hdlr 只跳 version/flags（+4）——两处都写同一
    // 四字符，两代读取端（修复 pre_defined 前后）都读到正确 handler。
    hd.u32(0) // version + flags
        .bytes(handler) // pre_defined 槽位（读取端兼容双写）
        .bytes(handler) // handler_type（ISO 规范槽位）
        .bytes(&[0; 12]) // reserved
        .bytes(name);
    let hdlr = hd.into_box(b"hdlr");

    // stsd 由 cfg 现建（cfg 已在 add_stream 校验通过，此处重建不会失败）。
    let stsd = build_stsd_box(cfg).expect("TrackConfig 已在 add_stream 校验通过");
    let mut sb = BoxBuf::new();
    sb.bytes(&stsd);
    for c in &stbl_children {
        sb.bytes(c);
    }
    let stbl = sb.into_box(b"stbl");

    let mut mf = BoxBuf::new();
    mf.bytes(&stbl);
    let minf = mf.into_box(b"minf");

    let mut md = BoxBuf::new();
    md.bytes(&mdhd).bytes(&hdlr).bytes(&minf);
    let mdia = md.into_box(b"mdia");

    let mut tk = BoxBuf::new();
    tk.bytes(&tkhd);
    if edit {
        let mut e = BoxBuf::new();
        e.u32(0x01000000)
            .u32(1)
            .u64(tkhd_duration_units)
            .u64(0)
            .u16(1)
            .u16(0);
        let elst = e.into_box(b"elst");
        let mut edts = BoxBuf::new();
        edts.bytes(&elst);
        tk.bytes(&edts.into_box(b"edts"));
    }
    tk.bytes(&mdia);
    tk.into_box(b"trak")
}

// —— 轨内累积状态 ——

/// 单样例记录。单位域 = 轨道时基单位；offset 为绝对文件偏移。
/// （时戳不再保留 tick 域副本——单位 + 轨道时基即完备表达，
/// movie 时基换算在 finalize 一次完成。）
struct SampleRec {
    dts_units: u64,
    /// 本样例时长（单位域）；stts 尾段 delta 直接用它。
    dur_units: u64,
    offset: u64,
    size: u32,
    /// pts−dts（单位域，i64 可负 → ctts v1）。
    cts_offset_units: i64,
    keyframe: bool,
}

/// Fragmented 布局：单个待切样例的元数据（数据在
/// `TrackState::frag_pending_data`，轨内连续）。
#[derive(Clone, Copy)]
struct PendingSample {
    dts_units: u64,
    dur_units: u64,
    size: u32,
    /// pts−dts（单位域，可负 → trun v1 有符号 cts）。
    cts_units: i64,
    keyframe: bool,
}

/// 一路已添加轨的累积状态。
struct TrackState {
    cfg: TrackConfig,
    timescale: u32,
    presentation_duration: Option<u64>,
    /// Progressive：已落盘样例。
    samples: Vec<SampleRec>,
    // —— Fragmented ——
    /// 本 fragment 待切样例元数据。
    frag_pending: Vec<PendingSample>,
    /// 本 fragment 待切样例数据（轨内连续；flush 时按轨序拼进 mdat，
    /// 同轨连续 ⇒ 单 trun 的 data_offset 走读成立）。
    frag_pending_data: Vec<u8>,
    /// 跨 fragment 的 (本轨末样例 dts, 下一期望 dts)（单位域）。
    frag_last: Option<(u64, u64)>,
    /// 已写样例 (首 dts, 尾 dts+dur)（单位域；finalize 回填 mdhd 用，
    /// tkhd/mvhd 经轨道时基重标度后回填）。
    frag_span_units: Option<(u64, u64)>,
}

/// run-length 合并相邻相等值：(count, value) 序列。
fn runs_of<T: PartialEq + Copy>(vals: &[T]) -> Vec<(u64, T)> {
    let mut out: Vec<(u64, T)> = Vec::new();
    for &v in vals {
        match out.last_mut() {
            Some(last) if last.1 == v => last.0 += 1,
            _ => out.push((1, v)),
        }
    }
    out
}

/// 同轨在文件内连续的样例合成一个 chunk：(chunk 起始偏移, 样例数)。
/// 连续判定：offset[i] == offset[i-1] + size[i-1]。
fn chunk_runs(samples: &[SampleRec]) -> Vec<(u64, u64)> {
    let mut chunks: Vec<(u64, u64)> = Vec::new();
    for (i, s) in samples.iter().enumerate() {
        let contiguous = i > 0 && samples[i - 1].offset + samples[i - 1].size as u64 == s.offset;
        match chunks.last_mut() {
            Some(last) if contiguous => last.1 += 1,
            _ => chunks.push((s.offset, 1)),
        }
    }
    chunks
}

/// 组装一路 trak 的样例表
/// （stts + ctts? + stss? + stsc + stsz + stco/co64，按此顺序）。
/// stsd 不在内——由 [`build_trak_skeleton`] 从 cfg 现建并置于首位。
fn build_stbl_children(t: &TrackState, use_co64: bool) -> Result<Vec<Vec<u8>>, WriterError> {
    let u32c = |v: u64, what: &'static str| -> Result<u32, WriterError> {
        u32::try_from(v).map_err(|_| WriterError::Misuse(what))
    };
    let mut children: Vec<Vec<u8>> = Vec::new();

    // —— stts：delta 一律取单位域 dts 差，尾包取 duration ——
    let mut deltas: Vec<u64> = Vec::with_capacity(t.samples.len());
    for i in 0..t.samples.len() {
        let d = if i + 1 < t.samples.len() {
            t.samples[i + 1].dts_units - t.samples[i].dts_units
        } else {
            t.samples[i].dur_units
        };
        deltas.push(d);
    }
    let stts_runs = runs_of(&deltas);
    let mut stts = BoxBuf::new();
    stts.u32(0)
        .u32(u32c(stts_runs.len() as u64, "stts 段数超出 u32")?);
    for &(count, delta) in &stts_runs {
        stts.u32(u32c(count, "样例数超出 u32")?)
            .u32(u32c(delta, "stts delta 超出 u32")?);
    }
    children.push(stts.into_box(b"stts"));

    // —— ctts v1：仅当存在 pts≠dts（0 偏移也入表，段合并）——
    if t.samples.iter().any(|s| s.cts_offset_units != 0) {
        let offsets: Vec<i64> = t.samples.iter().map(|s| s.cts_offset_units).collect();
        let ctts_runs = runs_of(&offsets);
        let mut ctts = BoxBuf::new();
        ctts.u32(1 << 24) // version 1（有符号偏移）
            .u32(u32c(ctts_runs.len() as u64, "ctts 段数超出 u32")?);
        for &(count, offset) in &ctts_runs {
            ctts.u32(u32c(count, "样例数超出 u32")?).i32(
                i32::try_from(offset)
                    .map_err(|_| WriterError::Misuse("ctts 偏移超出 i32 可表示范围"))?,
            );
        }
        children.push(ctts.into_box(b"ctts"));
    }

    // —— stss：仅视频轨且非全关键帧（缺省语义 = 全关键帧）——
    if is_video_cfg(&t.cfg) && !t.samples.is_empty() && !t.samples.iter().all(|s| s.keyframe) {
        let mut stss = BoxBuf::new();
        let keys: Vec<u64> = t
            .samples
            .iter()
            .enumerate()
            .filter(|&(_, s)| s.keyframe)
            .map(|(i, _)| i as u64 + 1) // 1-based 样号
            .collect();
        stss.u32(0)
            .u32(u32c(keys.len() as u64, "stss 条目数超出 u32")?);
        for k in keys {
            stss.u32(u32c(k, "stss 样号超出 u32")?);
        }
        children.push(stss.into_box(b"stss"));
    }

    // —— stsc：相邻等样例数 chunk 合成一段 ——
    let chunks = chunk_runs(&t.samples);
    let mut stsc_runs: Vec<(u64, u64)> = Vec::new(); // (first_chunk 1-based, samples_per_chunk)
    for (ci, &(_, spc)) in chunks.iter().enumerate() {
        match stsc_runs.last_mut() {
            Some(last) if last.1 == spc => {}
            _ => stsc_runs.push((ci as u64 + 1, spc)),
        }
    }
    let mut stsc = BoxBuf::new();
    stsc.u32(0)
        .u32(u32c(stsc_runs.len() as u64, "stsc 段数超出 u32")?);
    for &(first, spc) in &stsc_runs {
        stsc.u32(u32c(first, "stsc first_chunk 超出 u32")?)
            .u32(u32c(spc, "stsc samples_per_chunk 超出 u32")?)
            .u32(1); // sample_description_index
    }
    children.push(stsc.into_box(b"stsc"));

    // —— stsz：全等且非 0 定长（sample_size），否则变长（0 + 逐样例表）——
    // size==0 恒落变长分支：sample_size=0 是变长表标记（读取端以
    // fixed!=0 判定），定长写出缺逐样例条目会产出损坏文件（全空包轨）。
    let fixed = !t.samples.is_empty()
        && t.samples[0].size != 0
        && t.samples.iter().all(|s| s.size == t.samples[0].size);
    let mut stsz = BoxBuf::new();
    stsz.u32(0); // version + flags
    if fixed {
        stsz.u32(t.samples[0].size); // sample_size ≠ 0 ⇒ 定长
        stsz.u32(u32c(t.samples.len() as u64, "样例数超出 u32")?);
    } else {
        stsz.u32(0); // sample_size = 0 ⇒ 变长
        stsz.u32(u32c(t.samples.len() as u64, "样例数超出 u32")?);
        for s in &t.samples {
            stsz.u32(s.size);
        }
    }
    children.push(stsz.into_box(b"stsz"));

    // —— stco/co64 ——
    let mut off = BoxBuf::new();
    off.u32(0)
        .u32(u32c(chunks.len() as u64, "chunk 数超出 u32")?);
    for &(start, _) in &chunks {
        if use_co64 {
            off.u64(start);
        } else {
            off.u32(u32c(start, "chunk 偏移超出 u32")?);
        }
    }
    children.push(off.into_box(if use_co64 { b"co64" } else { b"stco" }));

    Ok(children)
}

// —— 封装器 ——

/// fMP4 前置 moov 各 duration 字段的占位值：>u32::MAX ⇒ build_mvhd/
/// build_trak_skeleton 恒选 v1（u64 定宽）⇒ finalize 回填不改 box 布局。
const FRAG_DURATION_PLACEHOLDER: u64 = u64::MAX;

/// MP4 封装器（progressive 与 fragmented）。
///
/// 用法：`create/new → add_stream* → write_packet* →（Fragmented 可
/// flush_fragment*）→ finalize`。`W` 从当前写位置起独占输出（请传全新
/// writer，偏移按 0 起算）；finalize 返回底层 `W`（已 flush、定位在文件
/// 末尾）。中途 drop = 丢弃半成品。**首个 I/O 错误后封装器封闭
/// （poisoned）**：write_packet/flush_fragment/finalize 此后一律返回该
/// 错误——错误后复用（重试）会静默产出损坏文件（重复 ftyp、中段重写
/// moof、stco 偏移错位），一律拒绝（2026-10-24 评审）。
///
/// 两布局差异：progressive 的 mdat 头随首包惰性写出、finalize 后置 moov
/// 并回填 mdat largesize；fragmented 的样例数据攒在内存，`flush_fragment`
/// （或 finalize 冲尾 / `fragment_duration` 关键帧自动切）时写 moof+mdat，
/// 前置 moov 在首个实际写出点惰性落盘、finalize 回填其时长字段。
pub struct Mp4Writer<W> {
    w: W,
    options: Mp4WriterOptions,
    movie_timescale: u32,
    tracks: Vec<TrackState>,
    /// 布局固定前缀（progressive：ftyp+free+mdat 头；fragmented：
    /// ftyp+moov）是否已落盘（首包/首 flush/finalize 惰性写出，
    /// 让 `new` 保持无 I/O、错误可上报）。
    prologue_done: bool,
    /// 任一轨已写首包（此后禁止 add_stream）。
    wrote_packet: bool,
    /// progressive：mdat box 起始绝对偏移（fragmented 不用，恒 0）。
    mdat_pos: u64,
    /// 当前绝对写位置（progressive：mdat 载荷内；fragmented：文件末尾）。
    pos: u64,
    /// fragmented：已写 fragment 数（mfhd sequence_number = frag_seq+1）。
    frag_seq: u32,
    /// fragmented：前置 moov 内 duration 字段（u64，v1 定宽）的绝对
    /// 偏移，顺序 [mvhd, (tkhd, mdhd)×轨]，finalize 逐个回填。
    frag_dur_offsets: Vec<usize>,
    /// 首个底层 I/O 错误的锁存（poisoned 封闭，2026-10-24 评审）：
    /// 写一半失败后继续使用会静默产出损坏文件（重试重写 ftyp、
    /// 中段再写一份 moof、stco 偏移与 mdat 内容错位），而
    /// `flush_fragment(&mut self) -> Result` 的形状天然邀请重试。
    /// 锁存后 write_packet/flush_fragment/finalize 一律返回首个错误。
    poisoned: Option<WriterError>,
}

impl Mp4Writer<std::io::BufWriter<std::fs::File>> {
    /// 新建磁盘文件封装（progressive 与 fragmented 均可）。
    pub fn create<P: AsRef<Path>>(path: P, options: Mp4WriterOptions) -> Result<Self, WriterError> {
        let file = std::fs::File::create(path)?;
        Ok(Mp4Writer::new(std::io::BufWriter::new(file), options))
    }
}

impl<W: Write + Seek> Mp4Writer<W> {
    /// 从给定 writer 起封装（假定写位置 0 起）。
    pub fn new(w: W, options: Mp4WriterOptions) -> Mp4Writer<W> {
        let ftyp_len = build_ftyp().len() as u64;
        // free 仅 Progressive 生效。超限值（8+free_size 溢出 u64）由首个
        // 写出点 ensure_prologue 以 Misuse 拒写；此处布局占位用
        // saturating 算术，杜绝 debug 加法 panic / release 静默回绕
        // （超限时 pos 为饱和占位值，任何写出路径都先过校验，不会消费）。
        let free_total = if options.free_size > 0 && options.layout == Layout::Progressive {
            options.free_size.saturating_add(8)
        } else {
            0
        };
        let (mdat_pos, pos) = if options.layout == Layout::Fragmented {
            // 前缀（ftyp+moov）惰性写出，位置届时按实长推进。
            (0, 0)
        } else {
            let mdat_pos = ftyp_len.saturating_add(free_total);
            (mdat_pos, mdat_pos.saturating_add(MDAT_HEADER))
        };
        Mp4Writer {
            w,
            options,
            movie_timescale: MOVIE_TIMESCALE,
            tracks: Vec::new(),
            prologue_done: false,
            wrote_packet: false,
            mdat_pos,
            pos,
            frag_seq: 0,
            frag_dur_offsets: Vec::new(),
            poisoned: None,
        }
    }

    /// poisoned 状态的对外错误（保留变体与文案；`io::Error` 不可 Clone，
    /// 以同 kind + 文案重建——Io 变体语义即「底层 I/O 失败」不因重建
    /// 改变，调用方按 kind 分流不受影响）。
    fn poison_err(e: &WriterError) -> WriterError {
        match e {
            WriterError::Io(io) => WriterError::Io(std::io::Error::new(io.kind(), io.to_string())),
            WriterError::Unsupported(w) => WriterError::Unsupported(w),
            WriterError::InvalidStreamIndex { index, count } => WriterError::InvalidStreamIndex {
                index: *index,
                count: *count,
            },
            WriterError::Misuse(w) => WriterError::Misuse(w),
            WriterError::Time(w) => WriterError::Time(w),
        }
    }

    /// poisoned 守卫：封闭后任何写路径调用一律返回锁存的错误。
    fn check_poisoned(&self) -> Result<(), WriterError> {
        match &self.poisoned {
            Some(e) => Err(Self::poison_err(e)),
            None => Ok(()),
        }
    }

    /// 底层写 + poisoned 锁存：失败即刻封闭封装器。
    fn io_write(&mut self, buf: &[u8]) -> Result<(), WriterError> {
        self.check_poisoned()?;
        io_write_poisoned(&mut self.w, &mut self.poisoned, buf)
    }

    /// 底层寻址 + poisoned 锁存（同 [`Self::io_write`]）。
    fn io_seek(&mut self, pos: SeekFrom) -> Result<u64, WriterError> {
        self.check_poisoned()?;
        match self.w.seek(pos) {
            Ok(v) => Ok(v),
            Err(e) => Err(lock_poison(&mut self.poisoned, WriterError::Io(e))),
        }
    }

    /// 底层 flush + poisoned 锁存（同 [`Self::io_write`]）。
    fn io_flush(&mut self) -> Result<(), WriterError> {
        self.check_poisoned()?;
        if let Err(e) = self.w.flush() {
            return Err(lock_poison(&mut self.poisoned, WriterError::Io(e)));
        }
        Ok(())
    }

    /// 添加一路轨，返回 stream_index（0 起，按添加顺序）。
    /// 首包已写后再调用 → Misuse。
    pub fn add_stream(&mut self, cfg: TrackConfig) -> Result<usize, WriterError> {
        if self.wrote_packet {
            return Err(WriterError::Misuse("首包已写后不可再 add_stream"));
        }
        let timescale = validate_timebase(&cfg.timebase)?;
        match cfg.media_type {
            MediaType::Video => {
                if cfg.video.is_none() {
                    return Err(WriterError::Misuse("Video 流须带 video 信息"));
                }
                if cfg.audio.is_some() {
                    return Err(WriterError::Misuse("Video 流不应带 audio 信息"));
                }
            }
            MediaType::Audio => {
                if cfg.audio.is_none() {
                    return Err(WriterError::Misuse("Audio 流须带 audio 信息"));
                }
                if cfg.video.is_some() {
                    return Err(WriterError::Misuse("Audio 流不应带 video 信息"));
                }
            }
            MediaType::Subtitle => {
                return Err(WriterError::Unsupported("仅支持视频/音频轨（vide/soun）"));
            }
        }
        // codec 合法性与字段范围校验（结果在 finalize 重建）。
        build_stsd_entry(&cfg)?;
        self.tracks.push(TrackState {
            cfg,
            timescale,
            presentation_duration: None,
            samples: Vec::new(),
            frag_pending: Vec::new(),
            frag_pending_data: Vec::new(),
            frag_last: None,
            frag_span_units: None,
        });
        Ok(self.tracks.len() - 1)
    }

    pub fn stream_count(&self) -> usize {
        self.tracks.len()
    }

    /// Trim encoder padding in the presentation timeline, preserving raw packet durations.
    /// A fine movie clock represents microseconds and 44.1/48kHz samples exactly.
    pub fn set_presentation_duration(
        &mut self,
        index: usize,
        ticks: u64,
    ) -> Result<(), WriterError> {
        if self.prologue_done || self.options.layout != Layout::Progressive || ticks == 0 {
            return Err(WriterError::Misuse(
                "presentation duration requires progressive output before packets",
            ));
        }
        let count = self.tracks.len();
        let track = self
            .tracks
            .get_mut(index)
            .ok_or(WriterError::InvalidStreamIndex { index, count })?;
        track.presentation_duration = Some(ticks);
        self.movie_timescale = 882_000_000;
        Ok(())
    }

    /// 顺序写一个样例（`pkt.stream_index` 定轨）。
    /// 写入契约见模块头注与 contracts.md；校验失败即拒，不落半字节数据。
    /// 两布局共用同一条校验链，仅分摊方式不同：progressive 直接落盘
    /// mdat，fragmented 攒进待切 fragment。
    ///
    /// 时戳契约：包时基任意（自描述）；入轨前按 NearInf 重标度到轨道
    /// 时基单位域，校验与入表均在单位域进行。
    pub fn write_packet(&mut self, pkt: &Packet) -> Result<(), WriterError> {
        self.check_poisoned()?;
        let si = usize::try_from(pkt.stream_index).unwrap_or(usize::MAX);
        if si >= self.tracks.len() {
            return Err(WriterError::InvalidStreamIndex {
                index: si,
                count: self.tracks.len(),
            });
        }
        let dts = pkt.dts.ok_or(WriterError::Misuse(
            "缺 dts（M1 封装不推断时戳，请调用方补齐）",
        ))?;
        let dur = pkt.duration.ok_or(WriterError::Misuse(
            "缺 duration（M1 封装不推断时长，请调用方补齐）",
        ))?;
        // 符号在重标度下不变，先按包时基判负。
        if dts.ticks() < 0 {
            return Err(WriterError::Misuse(
                "负 dts 不可表达（stts 0 起/tfdt u64），请调用方整体平移",
            ));
        }
        if dur.ticks() < 0 {
            return Err(WriterError::Misuse("duration 不可为负"));
        }
        let tb = self.tracks[si].cfg.timebase;
        // 包时基 → 轨道时基单位域（NearInf；dts/dur 非负 ⇒ 单位域非负）。
        // 重标度溢出是数据驱动的换算失败（非调用顺序违约）——按
        // Muxer trait 契约以 Time 变体透出（2026-10-24 评审统一，
        // 此前误归 Misuse → trait 面 InvalidState）。
        let rescale = |t: Timestamp| -> Result<i64, WriterError> {
            t.rescale(tb)
                .map(|v| v.ticks())
                .map_err(|_| WriterError::Time("时戳重标度到轨道时基溢出"))
        };
        let dts_units = rescale(dts)?;
        let dur_units = rescale(dur)?;
        let cts_offset = match pkt.pts {
            None => 0i64,
            Some(p) => {
                let p_units = rescale(p)?;
                p_units
                    .checked_sub(dts_units)
                    .ok_or(WriterError::Misuse("pts−dts 偏移溢出"))?
            }
        };
        let keyframe = pkt.flags.contains(PacketFlags::KEYFRAME);
        // 单位域单调/衔接校验：单位域是 stts/trun 的真实存储域，跨时基
        // 舍入不参与（包时基与轨道时基一致时重标度恒等）。
        match self.last_of_track(si)? {
            None => {
                // progressive 的 stts 隐式从 0 起算；fragmented 由 tfdt
                // 承载任意非负首 dts，故不加此约束。
                if self.options.layout == Layout::Progressive && dts_units != 0 {
                    return Err(WriterError::Misuse(
                        "首包 dts 须为 0（stts 从 0 起算，偏移请调用方平移）",
                    ));
                }
            }
            Some((last_dts, expect)) => {
                // dts_units ≥ 0（dts 非负已校验），可安全入 u64 域比较。
                if (dts_units as u64) < last_dts {
                    return Err(WriterError::Misuse("同轨 dts 须单调不减"));
                }
                if dts_units as u64 != expect {
                    return Err(WriterError::Misuse(
                        "dts 间隙/重叠与 duration 不一致，M1 不自动补样例",
                    ));
                }
            }
        }
        let size = u32::try_from(pkt.data.len())
            .map_err(|_| WriterError::Misuse("样例大小超出 u32（stsz 限制）"))?;
        match self.options.layout {
            Layout::Progressive => {
                self.ensure_prologue()?;
                let offset = self.pos;
                self.io_write(&pkt.data)?;
                self.pos += pkt.data.len() as u64;
                self.tracks[si].samples.push(SampleRec {
                    dts_units: dts_units as u64,
                    dur_units: dur_units as u64,
                    offset,
                    size,
                    cts_offset_units: cts_offset,
                    keyframe,
                });
            }
            Layout::Fragmented => {
                // 字段宽度前置校验（trun duration u32 / cts i32）——先于
                // fragment_duration 自动切（2026-10-24 评审：此前一个注定
                // 被拒的包若恰是到达阈值的关键帧，会先把攒的样例切成
                // fragment 落盘再返回 Misuse，被拒包改变了写入器状态），
                // 保证「校验失败即拒」的完全无副作用语义。
                if dur_units > u32::MAX as i64 {
                    return Err(WriterError::Misuse(
                        "样例时长超出 u32（trun duration 字段宽度）",
                    ));
                }
                i32::try_from(cts_offset).map_err(|_| {
                    WriterError::Misuse("pts−dts 偏移超出 i32（trun v1 cts 字段宽度）")
                })?;
                // fragment_duration 自动切：新到的关键帧 + 本轨待切跨度
                // 已达阈值 ⇒ 先切再攒（片段起点落在关键帧上）。阈值比较
                // 走 Timestamp::compare 精确交叉相乘，无舍入。
                if let Some(threshold) = self.options.fragment_duration {
                    let span = frag_pending_span(&self.tracks[si], tb);
                    if keyframe
                        && span.is_some_and(|s| Timestamp::compare(s, threshold) != Ordering::Less)
                    {
                        self.flush_fragment()?;
                    }
                }
                let t = &mut self.tracks[si];
                t.frag_pending_data.extend_from_slice(&pkt.data);
                t.frag_pending.push(PendingSample {
                    dts_units: dts_units as u64,
                    dur_units: dur_units as u64,
                    size,
                    cts_units: cts_offset,
                    keyframe,
                });
            }
        }
        self.wrote_packet = true;
        Ok(())
    }

    /// 同轨衔接基准（单位域）：Some((末样例 dts, 下一期望 dts))；
    /// None ⇒ 本轨尚未写过样例。progressive 看已落盘表，fragmented
    /// 优先看待切批、批空则看跨 fragment 游标。
    fn last_of_track(&self, si: usize) -> Result<Option<(u64, u64)>, WriterError> {
        let t = &self.tracks[si];
        let last = match self.options.layout {
            Layout::Progressive => t.samples.last().map(|s| (s.dts_units, s.dur_units)),
            Layout::Fragmented => t.frag_pending.last().map(|s| (s.dts_units, s.dur_units)),
        };
        match last {
            Some((dts, dur)) => {
                let next = dts
                    .checked_add(dur)
                    .ok_or(WriterError::Misuse("dts 累计溢出"))?;
                Ok(Some((dts, next)))
            }
            None => Ok(match self.options.layout {
                Layout::Progressive => None,
                Layout::Fragmented => t.frag_last,
            }),
        }
    }

    /// progressive：惰性写出 ftyp + free + mdat 16 字节占位头。
    fn ensure_prologue(&mut self) -> Result<(), WriterError> {
        if self.prologue_done {
            return Ok(());
        }
        // free box 总长（8+free_size）先做 checked 校验且置于任何写出
        // 之前：溢出即 Misuse，不落半字节（8+free_size 曾为未检加法，
        // debug panic / release 回绕成错位布局）。
        let free_total = if self.options.free_size > 0 {
            Some(
                self.options
                    .free_size
                    .checked_add(8)
                    .ok_or(WriterError::Misuse(
                        "free_size 超出可表示范围（8+free_size 溢出 u64）",
                    ))?,
            )
        } else {
            None
        };
        self.io_write(&build_ftyp())?;
        if let Some(total) = free_total {
            match u32::try_from(total) {
                Ok(sz) => {
                    self.io_write(&sz.to_be_bytes())?;
                    self.io_write(b"free")?;
                }
                Err(_) => {
                    // 总长装不进 u32：size==1 + largesize 头。
                    self.io_write(&1u32.to_be_bytes())?;
                    self.io_write(b"free")?;
                    self.io_write(&total.to_be_bytes())?;
                }
            }
            // 零载荷分块写，避免巨型 free 一次性分配。
            let zeros = [0u8; 4096];
            let mut left = self.options.free_size;
            while left > 0 {
                let n = (left as usize).min(zeros.len());
                self.io_write(&zeros[..n])?;
                left -= n as u64;
            }
        }
        // mdat 占位头（size==1 + largesize=16）：finalize 回填真实总长。
        let mdat = BoxBuf::new();
        self.io_write(&mdat.into_box64(b"mdat"))?;
        self.prologue_done = true;
        Ok(())
    }

    /// 收尾：progressive 写后置 moov、回填 mdat largesize 头；fragmented
    /// 冲尾 fragment、回填前置 moov 的时长字段。返回底层 W
    /// （已 flush、定位在文件末尾）。0 流 → Misuse；0 包轨合法。
    pub fn finalize(mut self) -> Result<W, WriterError> {
        self.check_poisoned()?;
        if self.tracks.is_empty() {
            return Err(WriterError::Misuse(
                "finalize 时无流（至少一路 trak；0 包轨合法）",
            ));
        }
        match self.options.layout {
            Layout::Progressive => self.finalize_progressive(),
            Layout::Fragmented => {
                self.flush_fragment()?;
                self.ensure_frag_prologue()?;
                self.backfill_frag_durations()?;
                self.io_seek(SeekFrom::End(0))?;
                self.io_flush()?;
                Ok(self.w)
            }
        }
    }

    /// progressive 收尾（moov 后置 + mdat 头回填）。
    fn finalize_progressive(mut self) -> Result<W, WriterError> {
        self.ensure_prologue()?;
        // co64 判定：任一 chunk 起始偏移超 u32::MAX 或显式强制。
        let use_co64 = self.options.force_co64
            || self
                .tracks
                .iter()
                .any(|t| chunk_runs(&t.samples).iter().any(|c| c.0 > u32::MAX as u64));

        let movie_tb = Rational::new(1, self.movie_timescale as i32);
        let mut movie_dur_max: u64 = 0;
        let mut traks = Vec::with_capacity(self.tracks.len());
        for (i, t) in self.tracks.iter().enumerate() {
            let stbl_children = build_stbl_children(t, use_co64)?;
            // dts_units[0]==0 ⇒ mdhd 时长 = 末样例 dts + 尾 delta（前缀和）。
            let duration_units = t
                .samples
                .last()
                .map(|s| {
                    s.dts_units
                        .checked_add(s.dur_units)
                        .ok_or(WriterError::Misuse("mdhd 时长累计溢出"))
                })
                .transpose()?
                .unwrap_or(0);
            let presented = t.presentation_duration.unwrap_or(duration_units);
            if presented > duration_units {
                return Err(WriterError::Misuse("presentation exceeds encoded samples"));
            }
            let end =
                i64::try_from(presented).map_err(|_| WriterError::Misuse("mdhd 时长累计溢出"))?;
            let movie_end = Timestamp::new(end, t.cfg.timebase)
                .rescale(movie_tb)
                .map_or(u32::MAX as u64 + 1, |v| v.ticks().max(0) as u64);
            movie_dur_max = movie_dur_max.max(movie_end);
            traks.push(build_trak_skeleton(
                i as u32 + 1,
                &t.cfg,
                t.timescale,
                duration_units,
                // tkhd 取 movie 时基（与 backfill_frag_durations 同一换算）。
                movie_end,
                stbl_children,
                t.presentation_duration.is_some(),
            ));
        }
        let mut moov = BoxBuf::new();
        moov.bytes(&build_mvhd(
            self.movie_timescale,
            movie_dur_max,
            self.tracks.len() as u32 + 1,
        ));
        for t in &traks {
            moov.bytes(t);
        }
        let moov_bytes = moov.into_box(b"moov");
        self.io_write(&moov_bytes)?;

        // 回填 mdat largesize（头恒 16 字节 ⇒ 无位移）。
        let payload = self.pos - (self.mdat_pos + MDAT_HEADER);
        let total = MDAT_HEADER + payload;
        self.io_seek(SeekFrom::Start(self.mdat_pos + 8))?;
        self.io_write(&total.to_be_bytes())?;
        self.io_seek(SeekFrom::End(0))?;
        self.io_flush()?;
        Ok(self.w)
    }

    // —— fMP4（Fragmented）——

    /// Fragmented：把 add_stream/write_packet 已攒进度样例切出为一个
    /// moof + mdat 对；finalize 自动冲尾。Progressive 下调用 →
    /// `WriterError::Misuse("flush_fragment 仅 Fragmented 布局可用")`。
    /// 无待切样例时为 no-op（不产空 moof）。
    pub fn flush_fragment(&mut self) -> Result<(), WriterError> {
        self.check_poisoned()?;
        if self.options.layout != Layout::Fragmented {
            return Err(WriterError::Misuse("flush_fragment 仅 Fragmented 布局可用"));
        }
        if self.tracks.iter().all(|t| t.frag_pending.is_empty()) {
            return Ok(());
        }
        self.ensure_frag_prologue()?;
        let seq = self.frag_seq + 1;
        for t in &self.tracks {
            u32::try_from(t.frag_pending.len())
                .map_err(|_| WriterError::Misuse("fragment 样例数超出 u32（trun 字段宽度）"))?;
        }
        // 两遍构造：data_offset 定宽 i32 ⇒ 第一遍取 moof 总长（占位 0），
        // 第二遍回填「该轨数据起点 − moof 起点」。mdat 按轨序紧随 moof，
        // 轨内数据连续 ⇒ data_offset = moof 长 + mdat 头 16 + 轨前数据量。
        let zeros = vec![0i32; self.tracks.len()];
        let probe = self.build_moof(seq, &zeros);
        let moof_len = probe.len() as u64;
        let mut inner: u64 = 0;
        let mut offsets: Vec<i32> = Vec::with_capacity(self.tracks.len());
        for t in &self.tracks {
            if t.frag_pending.is_empty() {
                offsets.push(0); // 无待切数据的轨不产 traf，占位不消费
                continue;
            }
            let off = moof_len + MDAT_HEADER + inner;
            inner += t.frag_pending_data.len() as u64;
            offsets.push(i32::try_from(off).map_err(|_| {
                WriterError::Misuse("data_offset 超出 i32（moof+mdat 距起点过远）")
            })?);
        }
        let moof_bytes = self.build_moof(seq, &offsets);
        let data_total = inner;
        self.io_write(&moof_bytes)?;
        // mdat 恒 16 字节 size==1 + largesize 头（总长已知，直接写实值；
        // 逐字段写——into_box64 会把 BoxBuf 载荷当 mdat 内容）。
        self.io_write(&1u32.to_be_bytes())?;
        self.io_write(b"mdat")?;
        self.w
            .write_all(&(MDAT_HEADER + data_total).to_be_bytes())?;
        for t in &self.tracks {
            if t.frag_pending_data.is_empty() {
                continue;
            }
            // 字段级拆分借用（迭代 tracks 的同时写 w + 维护 poisoned）。
            io_write_poisoned(&mut self.w, &mut self.poisoned, &t.frag_pending_data)?;
        }
        // 轨状态推进：跨 fragment 游标 / 首尾跨度（finalize 回填用）。
        for t in &mut self.tracks {
            if t.frag_pending.is_empty() {
                continue;
            }
            let first = t.frag_pending[0];
            let last = *t.frag_pending.last().expect("非空已判");
            let end_units = last
                .dts_units
                .checked_add(last.dur_units)
                .ok_or(WriterError::Misuse("fragment 尾 dts 累计溢出"))?;
            t.frag_last = Some((last.dts_units, end_units));
            let start_units = t.frag_span_units.map_or(first.dts_units, |(f, _)| f);
            t.frag_span_units = Some((start_units, end_units));
            t.frag_pending.clear();
            t.frag_pending_data.clear();
        }
        self.pos += moof_bytes.len() as u64 + MDAT_HEADER + data_total;
        self.frag_seq = seq;
        Ok(())
    }

    /// moof = mfhd(序号) + 每有 pending 数据的轨一个 traf。
    /// `offsets` 按轨索引给 trun data_offset（无 traf 的轨不消费）。
    fn build_moof(&self, seq: u32, offsets: &[i32]) -> Vec<u8> {
        let mut m = BoxBuf::new();
        let mut mf = BoxBuf::new();
        mf.u32(0).u32(seq);
        m.bytes(&mf.into_box(b"mfhd"));
        for (i, t) in self.tracks.iter().enumerate() {
            if t.frag_pending.is_empty() {
                continue;
            }
            m.bytes(&build_traf(i as u32 + 1, t, offsets[i]));
        }
        m.into_box(b"moof")
    }

    /// fragmented：惰性写出 ftyp + 前置 moov（完整 stsd + 空样例表 +
    /// mvex/trex），并记录各 duration 字段偏移（finalize 回填）。
    /// 仅在真实写出点调用（flush 有数据 / finalize），此前 add_stream
    /// 仍可继续 ⇒ 落盘时轨表已冻结。
    fn ensure_frag_prologue(&mut self) -> Result<(), WriterError> {
        if self.prologue_done {
            return Ok(());
        }
        let ftyp = build_ftyp();
        self.io_write(&ftyp)?;
        // duration 占位 >u32::MAX ⇒ build_mvhd/build_trak_skeleton 恒选
        // v1（u64 定宽）⇒ 回填不改布局。
        let mut moov = BoxBuf::new();
        moov.bytes(&build_mvhd(
            MOVIE_TIMESCALE,
            FRAG_DURATION_PLACEHOLDER,
            self.tracks.len() as u32 + 1,
        ));
        let empty_tables = build_empty_stbl_children();
        for (i, t) in self.tracks.iter().enumerate() {
            moov.bytes(&build_trak_skeleton(
                i as u32 + 1,
                &t.cfg,
                t.timescale,
                FRAG_DURATION_PLACEHOLDER,
                FRAG_DURATION_PLACEHOLDER,
                empty_tables.clone(),
                false,
            ));
        }
        moov.bytes(&build_mvex_trex(&self.tracks));
        let moov_bytes = moov.into_box(b"moov");
        let offsets = frag_moov_dur_offsets(&moov_bytes, self.tracks.len()).ok_or(
            WriterError::Misuse("前置 moov 时长字段定位失败（内部不变量破坏）"),
        )?;
        self.frag_dur_offsets = offsets.into_iter().map(|o| o + ftyp.len()).collect();
        self.io_write(&moov_bytes)?;
        self.pos = ftyp.len() as u64 + moov_bytes.len() as u64;
        self.prologue_done = true;
        Ok(())
    }

    /// 回填前置 moov 的时长：mvhd/tkhd（movie 时基单位，取各轨轨道时基
    /// 跨度经 NearInf 重标度）与 mdhd（轨道时基单位）。0 包 ⇒ 全 0。
    fn backfill_frag_durations(&mut self) -> Result<(), WriterError> {
        let movie_tb = Rational::new(1, MOVIE_TIMESCALE as i32);
        let to_movie = |span_units: u64, track_tb: Rational| -> u64 {
            // 换算失败（时长天文数字）按 0 写——布局已是 v1，无需触发升级。
            let Ok(end) = i64::try_from(span_units) else {
                return 0;
            };
            Timestamp::new(end, track_tb)
                .rescale(movie_tb)
                .map_or(0, |v| v.ticks().max(0) as u64)
        };
        let spans: Vec<(u64, Rational)> = self
            .tracks
            .iter()
            .map(|t| (t.frag_span_units.map_or(0, |(f, e)| e - f), t.cfg.timebase))
            .collect();
        let mut vals = Vec::with_capacity(1 + 2 * self.tracks.len());
        vals.push(
            spans
                .iter()
                .map(|&(units, tb)| to_movie(units, tb))
                .max()
                .unwrap_or(0),
        );
        for &(units, tb) in &spans {
            vals.push(to_movie(units, tb)); // tkhd（movie 时基）
            vals.push(units); // mdhd（轨道时基）
        }
        let offsets = std::mem::take(&mut self.frag_dur_offsets);
        for (off, v) in offsets.iter().zip(vals) {
            self.io_seek(SeekFrom::Start(*off as u64))?;
            self.io_write(&v.to_be_bytes())?;
        }
        self.frag_dur_offsets = offsets;
        Ok(())
    }
}

// —— fMP4 组装辅助 ——

/// 锁存首个 I/O 错误（poisoned 封闭）并返回对外错误。
fn lock_poison(poisoned: &mut Option<WriterError>, err: WriterError) -> WriterError {
    let out = Mp4Writer::<std::io::Cursor<Vec<u8>>>::poison_err(&err);
    *poisoned = Some(out);
    err
}

/// 底层写 + poisoned 锁存（自由函数版：供字段级拆分借用的调用点——
/// 迭代 `self.tracks` 的同时写 `self.w`，方法调用会借整个 `self` 冲突）。
fn io_write_poisoned<W: Write>(
    w: &mut W,
    poisoned: &mut Option<WriterError>,
    buf: &[u8],
) -> Result<(), WriterError> {
    if let Some(e) = poisoned {
        return Err(Mp4Writer::<std::io::Cursor<Vec<u8>>>::poison_err(e));
    }
    if let Err(e) = w.write_all(buf) {
        return Err(lock_poison(poisoned, WriterError::Io(e)));
    }
    Ok(())
}

/// 本轨待切批的跨度（末样例尾 − 首样例 dts，轨道时基时间戳）；
/// 空批 → None。供 fragment_duration 阈值的精确比较。
/// 饱和算术（同 m1）：天文数字跨度只会令比较判大，不 panic。
fn frag_pending_span(t: &TrackState, tb: Rational) -> Option<Timestamp> {
    let first = t.frag_pending.first()?;
    let last = t.frag_pending.last()?;
    let end = last.dts_units.saturating_add(last.dur_units);
    let span = end.checked_sub(first.dts_units)?;
    let ticks = i64::try_from(span).ok()?;
    Some(Timestamp::new(ticks, tb))
}

/// traf = tfhd + tfdt + trun（单 trun；轨内数据连续是前提）。
///
/// tfhd 恒置 default-base-is-moof(0x020000) 且不写 base_data_offset，
/// 另写 sample_description_index=1；trex 已兜底全 0 默认，不再写默认值。
/// tfdt 恒 v1 = 本 traf 首样例 dts。trun 恒 v1 且 per-sample 显式
/// duration/size/flags/cts——duration 取 dts 差（尾包取 pkt.duration，
/// 衔接校验下两者同值）、flags 关键帧 0x02000000 / 非关键帧 0x01010000
/// （读取端判定位 0x00010000 清零即关键帧）、cts = pts−dts（可负）。
/// first_sample_flags 不用：首样例也写 per-sample flags，与读取端
/// 优先链兼容。
fn build_traf(track_id: u32, t: &TrackState, data_offset: i32) -> Vec<u8> {
    let mut tf = BoxBuf::new();
    tf.u32(0x0002_0002) // default-base-is-moof | sample-description-index
        .u32(track_id)
        .u32(1); // sample_description_index
    let tfhd = tf.into_box(b"tfhd");

    let mut td = BoxBuf::new();
    td.u32(1 << 24) // version 1
        .u64(t.frag_pending[0].dts_units);
    let tfdt = td.into_box(b"tfdt");

    let n = t.frag_pending.len();
    let mut tr = BoxBuf::new();
    // v1 + data-offset(0x1)|duration(0x100)|size(0x200)|flags(0x400)|cts(0x800)
    tr.u32((1 << 24) | 0x0000_0F01)
        .u32(n as u32)
        .i32(data_offset);
    for i in 0..n {
        let s = &t.frag_pending[i];
        let dur = if i + 1 < n {
            t.frag_pending[i + 1].dts_units - s.dts_units
        } else {
            s.dur_units
        };
        let flags: u32 = if s.keyframe { 0x0200_0000 } else { 0x0101_0000 };
        tr.u32(dur as u32)
            .u32(s.size)
            .u32(flags)
            .i32(s.cts_units as i32);
    }
    let trun = tr.into_box(b"trun");

    let mut traf = BoxBuf::new();
    traf.bytes(&tfhd).bytes(&tfdt).bytes(&trun);
    traf.into_box(b"traf")
}

/// 前置 moov 的空样例表：stts/stsc/stsz/stco 各 entry_count=0
/// （stsz 为变长表形态：sample_size=0 + count=0）。
fn build_empty_stbl_children() -> Vec<Vec<u8>> {
    let mut stts = BoxBuf::new();
    stts.u32(0).u32(0);
    let mut stsc = BoxBuf::new();
    stsc.u32(0).u32(0);
    let mut stsz = BoxBuf::new();
    stsz.u32(0).u32(0).u32(0);
    let mut stco = BoxBuf::new();
    stco.u32(0).u32(0);
    vec![
        stts.into_box(b"stts"),
        stsc.into_box(b"stsc"),
        stsz.into_box(b"stsz"),
        stco.into_box(b"stco"),
    ]
}

/// mvex：每轨一个 trex（track_id 按 add_stream 顺序），缺省字段全 0
/// （trun 全显式 + tfhd 带 index，缺省链仅供读取端兜底）。
fn build_mvex_trex(tracks: &[TrackState]) -> Vec<u8> {
    let mut x = BoxBuf::new();
    for i in 0..tracks.len() {
        let mut trex = BoxBuf::new();
        trex.u32(0) // version/flags
            .u32(i as u32 + 1) // track_ID
            .u32(0) // default_sample_description_index
            .u32(0) // default_sample_duration
            .u32(0) // default_sample_size
            .u32(0); // default_sample_flags
        x.bytes(&trex.into_box(b"trex"));
    }
    x.into_box(b"mvex")
}

/// 前置 moov 各 duration 字段（u64，v1 定宽）在 moov 字节内的偏移：
/// [mvhd, (tkhd, mdhd)×轨]。布局由本模块拼装（mvhd → trak* → mvex，
/// trak 首子 box 恒为 tkhd、mdia 首子 box 恒为 mdhd），从字节里的
/// box size 逐层读取定位，不依赖各 builder 的内部尺寸；版本字节非
/// v1（占位失效）⇒ None。
fn frag_moov_dur_offsets(moov_bytes: &[u8], track_count: usize) -> Option<Vec<usize>> {
    let rd32 = |at: usize| -> Option<u32> {
        moov_bytes
            .get(at..at + 4)
            .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
    };
    // v1 校验：占位 >u32::MAX 必然选出 version 1。
    if rd32(8 + 8)? >> 24 != 1 {
        return None;
    }
    let mut out = Vec::with_capacity(1 + 2 * track_count);
    let mvhd_len = rd32(8)? as usize;
    out.push(8 + 8 + 4 + 8 + 8 + 4); // mvhd：头8+vf4+creation8+modification8+timescale4
    let mut p = 8 + mvhd_len;
    for _ in 0..track_count {
        let trak_len = rd32(p)? as usize;
        let tkhd_len = rd32(p + 8)? as usize;
        if rd32(p + 8 + 8)? >> 24 != 1 {
            return None;
        }
        out.push(p + 8 + 8 + 4 + 8 + 8 + 4 + 4); // tkhd：…+track_id4+reserved4
        let mdia_at = p + 8 + tkhd_len;
        if rd32(mdia_at + 8 + 8)? >> 24 != 1 {
            return None;
        }
        out.push(mdia_at + 8 + 8 + 4 + 8 + 8 + 4); // mdhd：头8+vf4+c8+m8+timescale4
        p += trak_len;
    }
    Some(out)
}
