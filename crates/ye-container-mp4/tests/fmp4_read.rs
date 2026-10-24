//! fMP4（fragment）读取：手工字节、构造器内联自包含（不动 tests/helpers）。
//!
//! 覆盖矩阵：
//! ① 最小 fMP4 黄金（default-base-is-moof + tfdt v0 + trun v0 全显式字段）；
//! ② tfdt v1、trun v1（负 cts）；③ tfhd 显式 base_data_offset；
//! ④ 无 data_offset 的 trun 接续前一 trun 数据末尾；
//! ⑤ first_sample_flags 路径（实测 ffmpeg 形态）；
//! ⑥ 全缺省 flags ⇒ keyframe=true；⑦ trex 默认 duration/size 兜底；
//! ⑧ 多 moof、多 traf/轨（音视频交错）；⑨ 空 stbl+mvex 跨 fragment 编址；
//! ⑩ mvex+非空 stbl+无 moof ⇒ 正常 progressive 读；
//! ⑪ 顶层 mfra/sidx/styp 跳过后仍正确读；⑫ 错误路径。
//!
//! 移植后语义：时戳为流时基（1/timescale）原生单位——`t(units)` 即
//! `Timestamp::new(units, 1/1000)`，pts 恒 Some（无 cts 字段 ⇒ pts==dts）。

use ye_container_mp4::{CodecId, MediaType, Mp4Error, Mp4Reader, PacketFlags, Rational, Timestamp};

// —— 字节构造（helpers 风格的内联副本）——

const AVCC: [u8; 14] = [
    0x01, 0x64, 0x00, 0x1E, 0xFF, 0xE1, 0x00, 0x08, 0x1A, 0x2B, 0x3C, 0x4D, 0x5E, 0x6F,
];
const ESDS: [u8; 29] = [
    0x00, 0x00, 0x00, 0x00, 0x03, 0x19, 0x00, 0x01, 0x00, 0x04, 0x11, 0x40, 0x15, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x02, 0x10, 0x88, 0x06, 0x01, 0x02,
];

fn be16(v: u16) -> [u8; 2] {
    v.to_be_bytes()
}
fn be32(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}
fn be64(v: u64) -> [u8; 8] {
    v.to_be_bytes()
}
fn push(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend_from_slice(bytes);
}

fn kind4(kind: &str) -> [u8; 4] {
    kind.as_bytes().try_into().unwrap()
}

fn r#box(kind: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    push(&mut out, &be32(8 + payload.len() as u32));
    push(&mut out, &kind4(kind));
    push(&mut out, payload);
    out
}

fn wrap(kind: &str, children: &[Vec<u8>]) -> Vec<u8> {
    let mut payload = Vec::new();
    for c in children {
        push(&mut payload, c);
    }
    r#box(kind, &payload)
}

fn ftyp() -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, b"isom");
    push(&mut p, &be32(0x200));
    push(&mut p, b"isomiso2avc1mp4a");
    r#box("ftyp", &p)
}

fn mvhd() -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be32(0));
    push(&mut p, &be32(0));
    push(&mut p, &be32(0)); // creation/modification
    push(&mut p, &be32(1000)); // timescale
    push(&mut p, &be32(0)); // duration
    push(&mut p, &be32(0x0001_0000)); // rate
    push(&mut p, &be16(0x0100)); // volume
    push(&mut p, &be16(0));
    push(&mut p, &be32(0));
    push(&mut p, &be32(0)); // reserved
    for v in [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        push(&mut p, &be32(v));
    }
    for _ in 0..6 {
        push(&mut p, &be32(0));
    }
    push(&mut p, &be32(4)); // next_track_ID
    r#box("mvhd", &p)
}

fn tkhd(track_id: u32) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be32(0x0000_0003)); // version 0 + flags
    push(&mut p, &be32(0));
    push(&mut p, &be32(0)); // creation/modification
    push(&mut p, &be32(track_id));
    push(&mut p, &be32(0)); // reserved
    push(&mut p, &be32(0)); // duration（fMP4 常为 0，读取端不用）
    push(&mut p, &be32(0));
    push(&mut p, &be32(0)); // reserved
    push(&mut p, &be16(0)); // layer
    push(&mut p, &be16(0)); // alternate_group
    push(&mut p, &be16(0)); // volume
    push(&mut p, &be16(0)); // reserved
    for v in [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        push(&mut p, &be32(v));
    }
    push(&mut p, &be32(0)); // width 16.16
    push(&mut p, &be32(0)); // height 16.16
    r#box("tkhd", &p)
}

fn mdhd(timescale: u32, duration: u32) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be32(0)); // version 0 + flags
    push(&mut p, &be32(0));
    push(&mut p, &be32(0)); // creation/modification
    push(&mut p, &be32(timescale));
    push(&mut p, &be32(duration));
    push(&mut p, &be16(0x55C4)); // language=und
    push(&mut p, &be16(0)); // pre_defined
    r#box("mdhd", &p)
}

fn hdlr(handler: &str) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be32(0));
    push(&mut p, &kind4(handler));
    push(&mut p, &[0; 12]);
    push(&mut p, b"Handler\0");
    r#box("hdlr", &p)
}

fn visual_entry(w: u16, h: u16) -> Vec<u8> {
    let mut e = Vec::with_capacity(78);
    push(&mut e, &[0; 6]); // reserved
    push(&mut e, &be16(1)); // data_reference_index
    push(&mut e, &be16(0));
    push(&mut e, &be16(0));
    push(&mut e, &[0; 12]);
    push(&mut e, &be16(w));
    push(&mut e, &be16(h));
    push(&mut e, &be32(0x0048_0000));
    push(&mut e, &be32(0x0048_0000));
    push(&mut e, &be32(0));
    push(&mut e, &be16(1)); // frame_count
    push(&mut e, &be32(0));
    push(&mut e, &[0; 28]); // compressorname
    push(&mut e, &be16(24)); // depth
    push(&mut e, &be16(0xFFFF));
    assert_eq!(e.len(), 78);
    e
}

fn stsd_video() -> Vec<u8> {
    let avcc = r#box("avcC", &AVCC);
    let mut entry = Vec::new();
    push(&mut entry, &be32(8 + 78 + avcc.len() as u32));
    push(&mut entry, b"avc1");
    push(&mut entry, &visual_entry(64, 96));
    push(&mut entry, &avcc);
    let mut p = Vec::new();
    push(&mut p, &be32(0));
    push(&mut p, &be32(1)); // entry_count
    push(&mut p, &entry);
    r#box("stsd", &p)
}

fn stsd_audio() -> Vec<u8> {
    let mut e = Vec::with_capacity(20);
    push(&mut e, &[0; 6]);
    push(&mut e, &be16(1)); // data_reference_index
    push(&mut e, &be16(2)); // channelcount
    push(&mut e, &be16(16)); // samplesize
    push(&mut e, &be16(0));
    push(&mut e, &be16(0));
    push(&mut e, &be32(48_000 << 16)); // samplerate 16.16
    let esds = r#box("esds", &ESDS);
    let mut entry = Vec::new();
    push(&mut entry, &be32(8 + 20 + esds.len() as u32));
    push(&mut entry, b"mp4a");
    push(&mut entry, &e);
    push(&mut entry, &esds);
    let mut p = Vec::new();
    push(&mut p, &be32(0));
    push(&mut p, &be32(1));
    push(&mut p, &entry);
    r#box("stsd", &p)
}

fn stts(runs: &[(u32, u32)]) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be32(0));
    push(&mut p, &be32(runs.len() as u32));
    for &(count, delta) in runs {
        push(&mut p, &be32(count));
        push(&mut p, &be32(delta));
    }
    r#box("stts", &p)
}

fn ctts_v1(runs: &[(u32, i32)]) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be32(1 << 24));
    push(&mut p, &be32(runs.len() as u32));
    for &(count, offset) in runs {
        push(&mut p, &be32(count));
        push(&mut p, &(offset as u32).to_be_bytes());
    }
    r#box("ctts", &p)
}

fn stsc(runs: &[(u32, u32, u32)]) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be32(0));
    push(&mut p, &be32(runs.len() as u32));
    for &(first_chunk, spc, desc) in runs {
        push(&mut p, &be32(first_chunk));
        push(&mut p, &be32(spc));
        push(&mut p, &be32(desc));
    }
    r#box("stsc", &p)
}

fn stsz_var(sizes: &[u32]) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be32(0));
    push(&mut p, &be32(0)); // sample_size = 0
    push(&mut p, &be32(sizes.len() as u32));
    for &s in sizes {
        push(&mut p, &be32(s));
    }
    r#box("stsz", &p)
}

fn stco(offsets: &[u32]) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be32(0));
    push(&mut p, &be32(offsets.len() as u32));
    for &o in offsets {
        push(&mut p, &be32(o));
    }
    r#box("stco", &p)
}

/// 空样例表（entry_count 全 0，ffmpeg empty_moov 实测形态）。
fn empty_stbl(stsd: Vec<u8>) -> Vec<Vec<u8>> {
    vec![stsd, stts(&[]), stsc(&[]), stsz_var(&[]), stco(&[])]
}

fn trak(track_id: u32, handler: &str, timescale: u32, stbl_children: &[Vec<u8>]) -> Vec<u8> {
    wrap(
        "trak",
        &[
            tkhd(track_id),
            wrap(
                "mdia",
                &[
                    mdhd(timescale, 0),
                    hdlr(handler),
                    wrap("minf", &[wrap("stbl", stbl_children)]),
                ],
            ),
        ],
    )
}

/// mvex 包若干 trex。
fn mvex(trexes: &[(u32, u32, u32, u32)]) -> Vec<u8> {
    let children: Vec<Vec<u8>> = trexes
        .iter()
        .map(|&(track_id, dur, size, flags)| {
            let mut p = Vec::new();
            push(&mut p, &be32(0)); // version/flags
            push(&mut p, &be32(track_id));
            push(&mut p, &be32(1)); // default_sample_description_index
            push(&mut p, &be32(dur));
            push(&mut p, &be32(size));
            push(&mut p, &be32(flags));
            r#box("trex", &p)
        })
        .collect();
    wrap("mvex", &children)
}

fn mfhd(seq: u32) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be32(0));
    push(&mut p, &be32(seq));
    r#box("mfhd", &p)
}

/// tfhd：flags 原样写入；base/默认字段按 Some 与否追加。
fn tfhd(
    track_id: u32,
    flags: u32,
    base: Option<u64>,
    def: (Option<u32>, Option<u32>, Option<u32>),
) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be32(flags));
    push(&mut p, &be32(track_id));
    if let Some(b) = base {
        assert_eq!(flags & 0x0000_0001, 1, "base 须与 0x01 标志一致");
        push(&mut p, &be64(b));
    }
    if let Some(d) = def.0 {
        push(&mut p, &be32(d));
    }
    if let Some(s) = def.1 {
        push(&mut p, &be32(s));
    }
    if let Some(f) = def.2 {
        push(&mut p, &be32(f));
    }
    r#box("tfhd", &p)
}

/// tfdt：version 0 用 u32、1 用 u64。
fn tfdt(version: u32, base: u64) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be32(version << 24));
    if version == 1 {
        push(&mut p, &be64(base));
    } else {
        push(&mut p, &be32(base as u32));
    }
    r#box("tfdt", &p)
}

/// trun 样例：字段按 Some 与否决定存在（全样例须一致）。
#[derive(Clone, Copy)]
struct TrunS {
    dur: Option<u32>,
    size: Option<u32>,
    flags: Option<u32>,
    cts: Option<i32>,
}

fn trun(
    version: u32,
    samples: &[TrunS],
    data_offset: Option<i32>,
    first_flags: Option<u32>,
) -> Vec<u8> {
    let all_or_none = |f: &dyn Fn(&TrunS) -> bool, name: &str| {
        let any = samples.iter().any(f);
        let all = samples.iter().all(f);
        assert_eq!(any, all, "trun 测试数据 {name} 字段须全有或全无");
        any
    };
    let mut flags = 0u32;
    if data_offset.is_some() {
        flags |= 0x0000_0001;
    }
    if first_flags.is_some() {
        flags |= 0x0000_0004;
    }
    let f_dur = all_or_none(&|s: &TrunS| s.dur.is_some(), "duration");
    let f_size = all_or_none(&|s: &TrunS| s.size.is_some(), "size");
    let f_flags = all_or_none(&|s: &TrunS| s.flags.is_some(), "flags");
    let f_cts = all_or_none(&|s: &TrunS| s.cts.is_some(), "cts");
    if f_dur {
        flags |= 0x0000_0100;
    }
    if f_size {
        flags |= 0x0000_0200;
    }
    if f_flags {
        flags |= 0x0000_0400;
    }
    if f_cts {
        flags |= 0x0000_0800;
    }
    let mut p = Vec::new();
    push(&mut p, &be32(version << 24 | flags));
    push(&mut p, &be32(samples.len() as u32));
    if let Some(d) = data_offset {
        push(&mut p, &be32(d as u32));
    }
    if let Some(f) = first_flags {
        push(&mut p, &be32(f));
    }
    for s in samples {
        if let Some(d) = s.dur {
            push(&mut p, &be32(d));
        }
        if let Some(z) = s.size {
            push(&mut p, &be32(z));
        }
        if let Some(f) = s.flags {
            push(&mut p, &be32(f));
        }
        if let Some(c) = s.cts {
            if version == 1 {
                push(&mut p, &(c as u32).to_be_bytes());
            } else {
                push(&mut p, &be32(c as u32)); // v0 无符号
            }
        }
    }
    r#box("trun", &p)
}

fn traf(children: &[Vec<u8>]) -> Vec<u8> {
    wrap("traf", children)
}

fn moof(children: &[Vec<u8>]) -> Vec<u8> {
    wrap("moof", children)
}

fn mdat(payload: &[u8]) -> Vec<u8> {
    r#box("mdat", payload)
}

// —— 布局组装 ——

/// 部件生产者：收到自身起始绝对偏移（moof 的 data_offset 回填用）。
type Part = Box<dyn Fn(u64) -> Vec<u8>>;

/// 与位置无关的固定部件。
fn fixed(bytes: Vec<u8>) -> Part {
    Box::new(move |_| bytes.clone())
}

/// moof 部件：make(data_offset_rel_to_moof) 生成 moof 字节。data_offset
/// 自动指向紧随其后的 mdat 载荷起点——相对本 moof 起点（default-base-is-moof
/// 的基值）的偏移即 moof 长度 + mdat 头 8；测试内再加样例的 inner 偏移。
/// 字段 u32 定宽 ⇒ 两遍构造布局不变。
fn moof_part<F: Fn(i32) -> Vec<u8> + 'static>(make: F) -> Part {
    Box::new(move |_start: u64| {
        let len = make(0).len() as i64;
        make((len + 8) as i32)
    })
}

/// 组装 ftyp + 各部件（moov 一般是首个 fixed 部件）。
fn layout(parts: Vec<Part>) -> Vec<u8> {
    let sizes: Vec<usize> = parts.iter().map(|p| p(0).len()).collect();
    let mut f = Vec::new();
    push(&mut f, &ftyp());
    let mut start = f.len() as u64;
    for (p, sz) in parts.into_iter().zip(sizes) {
        let bytes = p(start);
        assert_eq!(
            bytes.len(),
            sz,
            "部件尺寸不得依赖位置（data_offset 须 u32 定宽回填）"
        );
        start += sz as u64;
        push(&mut f, &bytes);
    }
    f
}

/// 单视频轨（track_id 7，timescale 1000）+ 空样例表 + mvex 的 moov。
fn video_fmp4_moov(trexes: &[(u32, u32, u32, u32)]) -> Vec<u8> {
    wrap(
        "moov",
        &[
            mvhd(),
            trak(7, "vide", 1000, &empty_stbl(stsd_video())),
            mvex(trexes),
        ],
    )
}

/// 视频轨时基（1/1000）下的时间戳。
fn t(units: i64) -> Timestamp {
    Timestamp::new(units, Rational::new(1, 1000))
}

fn find_top<'a>(
    r: &'a Mp4Reader<std::io::Cursor<Vec<u8>>>,
    kind: &str,
) -> &'a ye_container_mp4::BoxNode {
    r.box_tree()
        .iter()
        .find(|n| n.info.kind.as_str() == kind)
        .unwrap()
}

fn child<'a>(
    node: &'a ye_container_mp4::BoxNode,
    kind: &str,
) -> Option<&'a ye_container_mp4::BoxNode> {
    node.children.iter().find(|c| c.info.kind.as_str() == kind)
}

fn kinds(node: &ye_container_mp4::BoxNode) -> Vec<String> {
    node.children.iter().map(|c| c.info.kind.as_str()).collect()
}

// —— ① 最小 fMP4 黄金 + ⑪ 顶层未知 box ——

/// ① 的标准 moof 构造器：trun v0 全显式字段（dur/size/flags/cts + data_offset），
/// tfdt v0 base=100，default-base-is-moof。
fn golden_moof_make() -> impl Fn(i32) -> Vec<u8> {
    move |rel: i32| {
        moof(&[
            mfhd(1),
            traf(&[
                tfhd(7, 0x0002_0000, None, (None, None, None)),
                tfdt(0, 100),
                trun(
                    0,
                    &[
                        TrunS {
                            dur: Some(10),
                            size: Some(4),
                            flags: Some(0x0200_0000),
                            cts: Some(0),
                        },
                        TrunS {
                            dur: Some(10),
                            size: Some(5),
                            flags: Some(0x0101_0000),
                            cts: Some(5),
                        },
                    ],
                    Some(rel),
                    None,
                ),
            ]),
        ])
    }
}

fn golden_fmp4() -> Vec<u8> {
    // trex 缺省值刻意「错」（777/888/0x02000000）：trun 全显式时不得采用。
    layout(vec![
        fixed(video_fmp4_moov(&[(7, 777, 888, 0x0200_0000)])),
        moof_part(golden_moof_make()),
        fixed(mdat(&(0xA0..=0xA8).collect::<Vec<u8>>())),
    ])
}

/// ①/⑪ 共用的逐样例断言。
fn assert_golden_reads(r: &mut Mp4Reader<std::io::Cursor<Vec<u8>>>) {
    let s = r.stream(0).unwrap();
    assert_eq!(r.track_info(0).unwrap().track_id, 7);
    assert_eq!(s.media_type, MediaType::Video);
    assert_eq!(s.codec.codec, CodecId::H264);
    assert_eq!(r.track_info(0).unwrap().codec.as_str(), "avc1");
    assert_eq!(s.time_base, Rational::new(1, 1000));
    // Stream.duration = 末 fragment 尾 dts+dur = 100+10+10 = 120 单位
    // （mdhd 为 0 亦然）。
    assert_eq!(s.duration, Some(t(120)));
    assert_eq!(r.sample_count(0).unwrap(), 2);

    let p0 = r.read_packet(0, 0).unwrap();
    assert_eq!(p0.dts, Some(t(100))); // tfdt v0 base=100
    assert_eq!(p0.pts, Some(t(100))); // cts=0 ⇒ pts=dts
    assert_eq!(p0.duration, Some(t(10)));
    assert!(
        p0.flags.contains(PacketFlags::KEYFRAME),
        "flags=0x02000000（非 sync 位清零）⇒ 关键帧"
    );
    assert_eq!(&*p0.data, &[0xA0, 0xA1, 0xA2, 0xA3]);

    let p1 = r.read_packet(0, 1).unwrap();
    assert_eq!(p1.dts, Some(t(110)));
    assert_eq!(p1.pts, Some(t(115))); // cts=5
    assert_eq!(p1.duration, Some(t(10)));
    assert!(
        !p1.flags.contains(PacketFlags::KEYFRAME),
        "flags=0x01010000（0x00010000 置位）⇒ 非关键帧"
    );
    assert_eq!(&*p1.data, &(0xA4..=0xA8).collect::<Vec<u8>>());

    // 乱序随机访问重读。
    let again = r.read_packet(0, 0).unwrap();
    assert_eq!(&*again.data, &*p0.data);
}

/// ① 最小 fMP4 黄金：default-base-is-moof + tfdt v0 + trun v0 全显式字段。
#[test]
fn fmp4_golden_default_base_is_moof() {
    let mut r = Mp4Reader::from_vec(golden_fmp4()).unwrap();
    assert_golden_reads(&mut r);

    // box 树包含 moof/traf 子树。
    let top: Vec<String> = r.box_tree().iter().map(|n| n.info.kind.as_str()).collect();
    assert_eq!(top, vec!["ftyp", "moov", "moof", "mdat"]);
    let moof_node = find_top(&r, "moof");
    assert_eq!(kinds(moof_node), vec!["mfhd", "traf"]);
    let traf_node = child(moof_node, "traf").unwrap();
    assert_eq!(kinds(traf_node), vec!["tfhd", "tfdt", "trun"]);
    assert!(child(traf_node, "trun").unwrap().children.is_empty());
}

/// ⑪ 顶层 mfra/sidx/styp 跳过后仍正确读（与 ① 完全同值）。
#[test]
fn fmp4_unknown_top_boxes_are_skipped() {
    let mfra = r#box("mfra", &r#box("tfra", &[0u8; 8])); // 整体按未知 box 跳过
    let f = layout(vec![
        fixed(r#box("styp", &[0; 8])), // fragment 流头，moov 之前
        fixed(video_fmp4_moov(&[(7, 777, 888, 0x0200_0000)])),
        moof_part(golden_moof_make()),
        fixed(mdat(&(0xA0..=0xA8).collect::<Vec<u8>>())),
        fixed(r#box("sidx", &[0; 12])),
        fixed(mfra),
    ]);
    let mut r = Mp4Reader::from_vec(f).unwrap();
    assert_golden_reads(&mut r);

    let top: Vec<String> = r.box_tree().iter().map(|n| n.info.kind.as_str()).collect();
    assert_eq!(
        top,
        vec!["ftyp", "styp", "moov", "moof", "mdat", "sidx", "mfra"]
    );
    // mfra 作为未知 leaf 跳过：树里存在但无子节点（不递归 tfra）。
    let mfra_node = find_top(&r, "mfra");
    assert!(mfra_node.children.is_empty());
}

// —— ② tfdt v1 / trun v1 ——

/// tfdt v1（u64 base）+ trun v1（cts 有符号，负值合法）。
#[test]
fn fmp4_tfdt_v1_trun_v1_signed_cts() {
    let make = |rel: i32| {
        moof(&[
            mfhd(1),
            traf(&[
                tfhd(7, 0x0002_0000, None, (None, None, None)),
                tfdt(1, 100), // v1
                trun(
                    1,
                    &[
                        TrunS {
                            dur: Some(10),
                            size: Some(4),
                            flags: Some(0x0200_0000),
                            cts: Some(-8),
                        },
                        TrunS {
                            dur: Some(10),
                            size: Some(4),
                            flags: Some(0x0200_0000),
                            cts: Some(3),
                        },
                    ],
                    Some(rel),
                    None,
                ),
            ]),
        ])
    };
    let f = layout(vec![
        fixed(video_fmp4_moov(&[])),
        moof_part(make),
        fixed(mdat(&[0xA0, 0xA1, 0xA2, 0xA3, 0xB0, 0xB1, 0xB2, 0xB3])),
    ]);
    let mut r = Mp4Reader::from_vec(f).unwrap();
    assert_eq!(r.sample_count(0).unwrap(), 2);
    let p0 = r.read_packet(0, 0).unwrap();
    assert_eq!(p0.dts, Some(t(100)));
    assert_eq!(p0.pts, Some(t(92)), "v1 负 cts：100-8");
    assert_eq!(&*p0.data, &[0xA0, 0xA1, 0xA2, 0xA3]);
    let p1 = r.read_packet(0, 1).unwrap();
    assert_eq!(p1.dts, Some(t(110)));
    assert_eq!(p1.pts, Some(t(113)), "v1 正 cts：110+3");
    assert_eq!(&*p1.data, &[0xB0, 0xB1, 0xB2, 0xB3]);
}

// —— ③ tfhd 显式 base_data_offset ——

/// moof 与 mdat 之间隔一个 free box：显式基值必须指向 mdat 载荷起点，
/// 若误用 moof 起点（default-base-is-moof 语义）会错位 16 字节读进 free。
#[test]
fn fmp4_explicit_base_data_offset() {
    let free = r#box("free", &[0; 8]); // 16 字节
    let make = move |base_abs: u64| {
        moof(&[
            mfhd(1),
            traf(&[
                tfhd(7, 0x0000_0001, Some(base_abs), (None, None, None)),
                tfdt(0, 0),
                trun(
                    0,
                    &[TrunS {
                        dur: Some(10),
                        size: Some(4),
                        flags: Some(0x0200_0000),
                        cts: None,
                    }],
                    Some(0), // 相对显式基值
                    None,
                ),
            ]),
        ])
    };
    let f = layout(vec![
        fixed(video_fmp4_moov(&[])),
        Box::new(move |start: u64| {
            let len = make(0).len() as u64;
            // mdat 载荷起点 = moof 末 + free 16 + mdat 头 8。
            make(start + len + 16 + 8)
        }),
        fixed(free),
        fixed(mdat(&[0xC0, 0xC1, 0xC2, 0xC3])),
    ]);
    let mut r = Mp4Reader::from_vec(f).unwrap();
    let p0 = r.read_packet(0, 0).unwrap();
    assert_eq!(
        &*p0.data,
        &[0xC0, 0xC1, 0xC2, 0xC3],
        "显式基值须越过 free 指向 mdat"
    );
    assert_eq!(p0.dts, Some(t(0)));
}

// —— ④ 无 data_offset 的 trun 接续前一 trun 数据末尾 ——

/// 布局 moov/moof1/moof2/mdat：moof1 的 trunA 显式 data_offset 指向 mdat 载荷
/// 起点；同 traf 的 trunB 无 data_offset ⇒ 接 trunA 数据末尾；moof2 的 traf
/// 无任何基值信息（tfhd 仅 track_id），其 trunC 无 data_offset ⇒ 跨 moof 接
/// 续 trunB 数据末尾。19 字节载荷 = 4+5（trunA）+6（trunB）+4（trunC）。
#[test]
fn fmp4_trun_without_data_offset_continues() {
    let moof2 = moof(&[
        mfhd(2),
        traf(&[
            tfhd(7, 0, None, (None, None, None)), // 无基值信息 ⇒ 接续游标
            trun(
                0,
                &[TrunS {
                    dur: Some(10),
                    size: Some(4),
                    flags: Some(0x0200_0000),
                    cts: None,
                }],
                None, // 无 data_offset ⇒ 接前一 trun 数据末尾
                None,
            ),
        ]),
    ]);
    let moof2_len = moof2.len() as i64;
    let make = move |rel: i32| {
        moof(&[
            mfhd(1),
            traf(&[
                tfhd(7, 0x0002_0000, None, (None, None, None)),
                tfdt(0, 0),
                trun(
                    0,
                    &[
                        TrunS {
                            dur: Some(10),
                            size: Some(4),
                            flags: Some(0x0200_0000),
                            cts: None,
                        },
                        TrunS {
                            dur: Some(10),
                            size: Some(5),
                            flags: Some(0x0200_0000),
                            cts: None,
                        },
                    ],
                    Some(rel), // 指向 mdat 载荷起点
                    None,
                ),
                trun(
                    0,
                    &[TrunS {
                        dur: Some(10),
                        size: Some(6),
                        flags: Some(0x0200_0000),
                        cts: None,
                    }],
                    None, // 同 traf 内接续：trunA 数据末尾（载荷 +9）
                    None,
                ),
            ]),
        ])
    };
    let f = layout(vec![
        fixed(video_fmp4_moov(&[])),
        Box::new(move |_start: u64| {
            let len = make(0).len() as i64;
            // mdat 在 moof2 之后：载荷起点相对 moof1 起点 = moof1_len + moof2_len + 8。
            make((len + moof2_len + 8) as i32)
        }),
        fixed(moof2),
        fixed(mdat(&(0xA0..=0xB2).collect::<Vec<u8>>())), // 19 字节连续
    ]);
    let mut r = Mp4Reader::from_vec(f).unwrap();
    assert_eq!(r.sample_count(0).unwrap(), 4);
    assert_eq!(
        &*r.read_packet(0, 0).unwrap().data,
        &[0xA0, 0xA1, 0xA2, 0xA3]
    );
    assert_eq!(
        &*r.read_packet(0, 1).unwrap().data,
        &(0xA4..=0xA8).collect::<Vec<u8>>()
    );
    // trunB：接 trunA 数据末尾（载荷偏移 9）。
    assert_eq!(
        &*r.read_packet(0, 2).unwrap().data,
        &(0xA9..=0xAE).collect::<Vec<u8>>()
    );
    // trunC（moof2，无基值信息无 data_offset）：跨 moof 接 trunB 末尾（+15）。
    assert_eq!(
        &*r.read_packet(0, 3).unwrap().data,
        &(0xAF..=0xB2).collect::<Vec<u8>>()
    );
    // dts 无 tfdt 接续：0/10/20/30。
    for (i, dts) in [0, 10, 20, 30].into_iter().enumerate() {
        assert_eq!(r.read_packet(0, i).unwrap().dts, Some(t(dts)), "样例 {i}");
    }
}

// —— ⑤ first_sample_flags（实测 ffmpeg 形态）——

/// trun flags=0x00000a05：data-offset|first-sample-flags|size|cts，无逐样例
/// duration/flags；tfhd 带三默认（ffmpeg tfhd flags=0x020038 形态）；
/// trex 缺省值刻意「错」以验证优先级：tfhd 默认 > trex 默认。
#[test]
fn fmp4_first_sample_flags_with_tfhd_defaults() {
    let make = |rel: i32| {
        moof(&[
            mfhd(1),
            traf(&[
                tfhd(
                    7,
                    0x0002_0038,
                    None,
                    (Some(10), Some(999), Some(0x0101_0000)),
                ),
                tfdt(0, 0),
                trun(
                    0,
                    &[
                        TrunS {
                            dur: None,
                            size: Some(4),
                            flags: None,
                            cts: Some(0),
                        },
                        TrunS {
                            dur: None,
                            size: Some(5),
                            flags: None,
                            cts: Some(1),
                        },
                        TrunS {
                            dur: None,
                            size: Some(6),
                            flags: None,
                            cts: Some(2),
                        },
                    ],
                    Some(rel),
                    Some(0x0200_0000), // first_sample_flags
                ),
            ]),
        ])
    };
    let f = layout(vec![
        fixed(video_fmp4_moov(&[(7, 777, 888, 0x0200_0000)])), // trex 兜底（不得采用）
        moof_part(make),
        fixed(mdat(&(0xA0..=0xAE).collect::<Vec<u8>>())),
    ]);
    let mut r = Mp4Reader::from_vec(f).unwrap();
    assert_eq!(r.sample_count(0).unwrap(), 3);
    for i in 0..3 {
        let p = r.read_packet(0, i).unwrap();
        assert_eq!(
            p.duration,
            Some(t(10)),
            "时长取 tfhd 默认 10（非 trex 777）——样例 {i}"
        );
        assert_eq!(p.dts, Some(t(10 * i as i64)), "样例 {i}");
        assert_eq!(
            p.pts,
            Some(t(10 * i as i64 + i as i64)),
            "cts=0/1/2——样例 {i}"
        );
        let expect: Vec<u8> = match i {
            0 => vec![0xA0, 0xA1, 0xA2, 0xA3],
            1 => (0xA4..=0xA8).collect(),
            _ => (0xA9..=0xAE).collect(),
        };
        assert_eq!(
            &*p.data, &expect,
            "大小取逐样例 size（非 tfhd 默认 999）——样例 {i}"
        );
        assert_eq!(
            p.flags.contains(PacketFlags::KEYFRAME),
            i == 0,
            "首样例用 first_sample_flags，其余落 tfhd 默认链"
        );
    }
}

// —— ⑥ 全缺省 flags ⇒ keyframe=true ——

/// 无逐样例 flags、无 tfhd 默认、trex 仅服务另一 track（未引用）⇒ flags 未知。
#[test]
fn fmp4_unknown_flags_default_to_keyframe() {
    let make = |rel: i32| {
        moof(&[
            mfhd(1),
            traf(&[
                tfhd(7, 0x0002_0000, None, (None, None, None)),
                tfdt(0, 0),
                trun(
                    0,
                    &[TrunS {
                        dur: Some(10),
                        size: Some(4),
                        flags: None,
                        cts: None,
                    }; 3],
                    Some(rel),
                    None,
                ),
            ]),
        ])
    };
    let f = layout(vec![
        // trex 服务 track 8（不存在）：验证未引用 trex 无害且不参与。
        fixed(video_fmp4_moov(&[(8, 777, 888, 0x0101_0000)])),
        moof_part(make),
        fixed(mdat(&[0xA0; 12])),
    ]);
    let mut r = Mp4Reader::from_vec(f).unwrap();
    assert_eq!(r.sample_count(0).unwrap(), 3);
    for i in 0..3 {
        let p = r.read_packet(0, i).unwrap();
        assert!(
            p.flags.contains(PacketFlags::KEYFRAME),
            "flags 全缺省 ⇒ true（样例 {i}）"
        );
        assert_eq!(p.pts, p.dts, "无 cts 字段 ⇒ pts==dts（恒 Some）");
    }
}

// —— ⑦ trex 默认 duration/size 兜底 ——

/// trun 无任何逐样例字段：duration/size/flags 全落 trex 默认。
#[test]
fn fmp4_trex_defaults() {
    let make = |rel: i32| {
        moof(&[
            mfhd(1),
            traf(&[
                tfhd(7, 0x0002_0000, None, (None, None, None)),
                tfdt(0, 0),
                trun(
                    0,
                    &[TrunS {
                        dur: None,
                        size: None,
                        flags: None,
                        cts: None,
                    }; 3],
                    Some(rel),
                    None,
                ),
            ]),
        ])
    };
    let f = layout(vec![
        fixed(video_fmp4_moov(&[(7, 10, 4, 0x0200_0000)])),
        moof_part(make),
        fixed(mdat(&(0xA0..=0xAB).collect::<Vec<u8>>())),
    ]);
    let mut r = Mp4Reader::from_vec(f).unwrap();
    assert_eq!(r.sample_count(0).unwrap(), 3);
    for (i, bytes) in [
        vec![0xA0, 0xA1, 0xA2, 0xA3],
        (0xA4..=0xA7).collect(),
        (0xA8..=0xAB).collect(),
    ]
    .into_iter()
    .enumerate()
    {
        let p = r.read_packet(0, i).unwrap();
        assert_eq!(p.duration, Some(t(10)), "trex 默认 duration——样例 {i}");
        assert_eq!(&*p.data, &bytes, "trex 默认 size——样例 {i}");
        assert!(
            p.flags.contains(PacketFlags::KEYFRAME),
            "trex 默认 flags=0x02000000 ⇒ 关键帧"
        );
        assert_eq!(p.dts, Some(t(10 * i as i64)));
    }
    // Stream.duration = 末 fragment 尾 = 30 单位。
    assert_eq!(r.stream(0).unwrap().duration, Some(t(30)));
}

// —— ⑧ 多 moof、多 traf/轨（音视频交错）+ ⑨ 空 stbl 跨 fragment ——

/// 视频（track 7，ts 1000）+ 音频（track 3，ts 48000）各 1 样例/frag，
/// 两个 moof（各随一 mdat），交错 traf；音频 trun flags 带 0x00010000
/// 仍恒关键帧；moof2 无 tfdt ⇒ dts 接续。
#[test]
fn fmp4_interleaved_av_two_moofs() {
    let audio_stbl = empty_stbl(stsd_audio());
    let moov = wrap(
        "moov",
        &[
            mvhd(),
            trak(7, "vide", 1000, &empty_stbl(stsd_video())),
            trak(3, "soun", 48_000, &audio_stbl),
            mvex(&[(7, 0, 0, 0), (3, 0, 0, 0)]),
        ],
    );
    // make(rel_v, rel_a)：rel 为相对本 moof 起点到 mdat 载荷的偏移，
    // 音频样例在视频样例之后（载荷内 +4）。
    let make_frag = |seq: u32, v_flags: u32| {
        move |rel_v: i32, rel_a: i32| {
            moof(&[
                mfhd(seq),
                traf(&[
                    tfhd(7, 0x0002_0000, None, (None, None, None)),
                    tfdt(0, if seq == 1 { 0 } else { 10 }),
                    trun(
                        0,
                        &[TrunS {
                            dur: Some(10),
                            size: Some(4),
                            flags: Some(v_flags),
                            cts: None,
                        }],
                        Some(rel_v),
                        None,
                    ),
                ]),
                traf(&[
                    tfhd(3, 0x0002_0000, None, (None, None, None)),
                    // moof1 带 tfdt=0；moof2 无 tfdt ⇒ 音频 dts 接续前一 fragment 尾。
                    if seq == 1 {
                        tfdt(0, 0)
                    } else {
                        r#box("skpx", &[0u8; 8])
                    },
                    trun(
                        0,
                        &[TrunS {
                            dur: Some(48),
                            size: Some(2),
                            flags: Some(0x0101_0000),
                            cts: None,
                        }],
                        Some(rel_a),
                        None,
                    ),
                ]),
            ])
        }
    };
    let part1 = {
        let make = make_frag(1, 0x0200_0000);
        Box::new(move |_start: u64| {
            let len = make(0, 0).len() as i64;
            let rel_v = len + 8;
            make(rel_v as i32, (rel_v + 4) as i32)
        }) as Part
    };
    let part2 = {
        let make = make_frag(2, 0x0101_0000);
        Box::new(move |_start: u64| {
            let len = make(0, 0).len() as i64;
            let rel_v = len + 8;
            make(rel_v as i32, (rel_v + 4) as i32)
        }) as Part
    };
    let f = layout(vec![
        fixed(moov),
        part1,
        fixed(mdat(&[0xA0, 0xA1, 0xA2, 0xA3, 0xC0, 0xC1])),
        part2,
        fixed(mdat(&[0xA4, 0xA5, 0xA6, 0xA7, 0xC2, 0xC3])),
    ]);
    let mut r = Mp4Reader::from_vec(f).unwrap();
    assert_eq!(r.streams().len(), 2);
    let (v, a) = (r.stream(0).unwrap(), r.stream(1).unwrap());
    assert_eq!(
        (v.media_type, a.media_type),
        (MediaType::Video, MediaType::Audio)
    );
    assert_eq!(a.time_base, Rational::new(1, 48_000));
    // 空 stbl ⇒ sample_count 完全来自 fragment。
    assert_eq!(r.sample_count(0).unwrap(), 2);
    assert_eq!(r.sample_count(1).unwrap(), 2);

    let v0 = r.read_packet(0, 0).unwrap();
    assert!(v0.flags.contains(PacketFlags::KEYFRAME));
    assert_eq!(&*v0.data, &[0xA0, 0xA1, 0xA2, 0xA3]);
    assert_eq!(v0.dts, Some(t(0)));
    let v1 = r.read_packet(0, 1).unwrap();
    assert!(
        !v1.flags.contains(PacketFlags::KEYFRAME),
        "0x01010000 ⇒ 非关键帧"
    );
    assert_eq!(&*v1.data, &[0xA4, 0xA5, 0xA6, 0xA7]);
    assert_eq!(v1.dts, Some(t(10)), "moof2 tfdt=10");

    let at = |u: i64| Timestamp::new(u, Rational::new(1, 48_000));
    let a0 = r.read_packet(1, 0).unwrap();
    assert!(
        a0.flags.contains(PacketFlags::KEYFRAME),
        "音频轨恒关键帧（flags 带 0x00010000 亦然）"
    );
    assert_eq!(&*a0.data, &[0xC0, 0xC1]);
    assert_eq!(a0.dts, Some(at(0)));
    assert_eq!(a0.duration, Some(at(48)));
    let a1 = r.read_packet(1, 1).unwrap();
    assert!(a1.flags.contains(PacketFlags::KEYFRAME));
    assert_eq!(&*a1.data, &[0xC2, 0xC3]);
    assert_eq!(a1.dts, Some(at(48)), "无 tfdt ⇒ 接前一 fragment 尾 dts");
}

/// ⑨ 同轨两个 moof×2 样例：统一编址 0..4，dts 跨 moof 连续（moof2 无 tfdt）。
#[test]
fn fmp4_same_track_two_moofs_continuous_addressing() {
    let make_moof = |seq: u32, rel: i32| {
        moof(&[
            mfhd(seq),
            traf(&[
                tfhd(7, 0x0002_0000, None, (None, None, None)),
                if seq == 1 {
                    tfdt(0, 5)
                } else {
                    r#box("xxxx", &[])
                }, // moof2 无 tfdt
                trun(
                    0,
                    &[
                        TrunS {
                            dur: Some(10),
                            size: Some(4),
                            flags: Some(0x0200_0000),
                            cts: None,
                        },
                        TrunS {
                            dur: Some(10),
                            size: Some(4),
                            flags: Some(0x0200_0000),
                            cts: None,
                        },
                    ],
                    Some(rel),
                    None,
                ),
            ]),
        ])
    };
    // 单 mdat 在两个 moof 之后：moof2 长度先探测。
    let moof2_probe = make_moof(2, 0);
    let moof2_len = moof2_probe.len() as i64;
    let f = layout(vec![
        fixed(video_fmp4_moov(&[(7, 0, 0, 0)])),
        Box::new(move |_start: u64| {
            let len = make_moof(1, 0).len() as i64;
            // moof1 两个样例在 mdat 载荷 0/4；mdat 在 moof2 后，
            // 相对 moof1 起点的偏移 = moof1_len + moof2_len + 8。
            make_moof(1, (len + moof2_len + 8) as i32)
        }),
        Box::new(move |_start: u64| {
            let len = make_moof(2, 0).len() as i64;
            // moof2 两个样例在载荷 8/12。
            make_moof(2, (len + 8 + 8) as i32)
        }),
        fixed(mdat(&(0xB0..=0xBF).collect::<Vec<u8>>())),
    ]);
    let mut r = Mp4Reader::from_vec(f).unwrap();
    assert_eq!(
        r.sample_count(0).unwrap(),
        4,
        "空 stbl ⇒ sample_count 跨 fragment 累计"
    );
    for (i, (dts, bytes)) in [
        (5i64, vec![0xB0, 0xB1, 0xB2, 0xB3]),
        (15, vec![0xB4, 0xB5, 0xB6, 0xB7]),
        (25, vec![0xB8, 0xB9, 0xBA, 0xBB]),
        (35, vec![0xBC, 0xBD, 0xBE, 0xBF]),
    ]
    .into_iter()
    .enumerate()
    {
        let p = r.read_packet(0, i).unwrap();
        assert_eq!(p.dts, Some(t(dts)), "样例 {i}：dts 跨 moof 连续");
        assert_eq!(&*p.data, &bytes, "样例 {i}");
    }
    assert_eq!(
        r.stream(0).unwrap().duration,
        Some(t(45)),
        "末 fragment 尾 dts+dur=45"
    );
}

// —— ⑩ mvex+非空 stbl+无 moof ⇒ 纯 progressive ——

/// 黄金式 progressive 样例表（3 样例，ctts v1 首段 -5）+ mvex：
/// 无 moof ⇒ trex 仅记录，读路径与 progressive 完全一致。
#[test]
fn fmp4_mvex_without_moof_reads_progressive() {
    let mdat_payload: Vec<u8> = (0xA0..=0xAF).collect();
    let file = |c1: u32, c2: u32| {
        let stbl = vec![
            stsd_video(),
            stts(&[(3, 10)]),
            ctts_v1(&[(1, -5), (2, 0)]),
            stsc(&[(1, 2, 1), (2, 1, 1)]),
            stsz_var(&[4, 5, 6]),
            stco(&[c1, c2]),
        ];
        let moov = wrap(
            "moov",
            &[
                mvhd(),
                trak(7, "vide", 1000, &stbl),
                mvex(&[(7, 777, 888, 0x0101_0000)]),
            ],
        );
        let mut f = Vec::new();
        push(&mut f, &ftyp());
        push(&mut f, &moov);
        push(&mut f, &mdat(&mdat_payload));
        f
    };
    // 占位探测须量整文件（含 ftyp/mvex，它们决定 mdat 载荷起点）。
    let probe = file(0, 0);
    let c1 = (probe.len() - mdat_payload.len()) as u32;
    let c2 = c1 + 9; // chunk1 含样例 0/1（4+5）
    let f = file(c1, c2);

    let mut r = Mp4Reader::from_vec(f).unwrap();
    assert_eq!(r.sample_count(0).unwrap(), 3);
    let p0 = r.read_packet(0, 0).unwrap();
    assert_eq!(p0.dts, Some(t(0)));
    assert_eq!(p0.pts, Some(t(-5)), "ctts 不受 mvex 影响");
    assert_eq!(p0.duration, Some(t(10)));
    assert!(
        p0.flags.contains(PacketFlags::KEYFRAME),
        "无 stss 表 ⇒ 恒关键帧（trex flags 不参与）"
    );
    assert_eq!(&*p0.data, &[0xA0, 0xA1, 0xA2, 0xA3]);
    let p2 = r.read_packet(0, 2).unwrap();
    assert_eq!(&*p2.data, &(0xA9..=0xAE).collect::<Vec<u8>>());
    // 无 moof ⇒ duration 沿用 mdhd（构造为 0）。
    assert_eq!(r.stream(0).unwrap().duration, Some(t(0)));
}

// —— ⑫ 错误路径 ——

/// traf 内 trun 先于 tfhd（tfhd 缺失）⇒ Malformed。
#[test]
fn fmp4_traf_without_tfhd_is_malformed() {
    let make = |rel: i32| {
        moof(&[
            mfhd(1),
            traf(&[
                tfdt(0, 0),
                trun(
                    0,
                    &[TrunS {
                        dur: Some(10),
                        size: Some(4),
                        flags: None,
                        cts: None,
                    }],
                    Some(rel),
                    None,
                ),
            ]),
        ])
    };
    let f = layout(vec![
        fixed(video_fmp4_moov(&[])),
        moof_part(make),
        fixed(mdat(&[0xA0, 0xA1, 0xA2, 0xA3])),
    ]);
    match Mp4Reader::from_vec(f) {
        Err(Mp4Error::Malformed { reason, .. }) => {
            assert!(reason.contains("tfhd"), "reason={reason}");
        }
        other => panic!("应为 Malformed，实为 {other:?}"),
    }
}

/// trun data_offset 越出文件：解析成功，读样例时按文件长度校验 ⇒ Malformed。
#[test]
fn fmp4_data_offset_beyond_file_rejected_on_read() {
    let make = |rel: i32| {
        moof(&[
            mfhd(1),
            traf(&[
                tfhd(7, 0x0002_0000, None, (None, None, None)),
                tfdt(0, 0),
                trun(
                    0,
                    &[TrunS {
                        dur: Some(10),
                        size: Some(4),
                        flags: None,
                        cts: None,
                    }],
                    Some(rel),
                    None,
                ),
            ]),
        ])
    };
    let f = layout(vec![
        fixed(video_fmp4_moov(&[])),
        Box::new(move |start: u64| {
            let _ = start;
            make(1_000_000) // 远超文件长度
        }),
        fixed(mdat(&[0xA0, 0xA1, 0xA2, 0xA3])),
    ]);
    let mut r = Mp4Reader::from_vec(f).unwrap(); // 解析期不报错
    match r.read_packet(0, 0) {
        Err(Mp4Error::Malformed { reason, .. }) => {
            assert!(reason.contains("越出文件"), "reason={reason}");
        }
        other => panic!("应为 Malformed，实为 {other:?}"),
    }
}

/// trun 声明 2 个样例但只给 1 个样例的字段 ⇒ Malformed（截断）。
#[test]
fn fmp4_truncated_trun_is_malformed() {
    let mut p = Vec::new();
    push(&mut p, &be32(0x0000_0301)); // data-offset(0x1)|duration(0x100)|size(0x200)
    push(&mut p, &be32(2)); // sample_count = 2
    push(&mut p, &be32(0)); // data_offset（占位）
    push(&mut p, &be32(10)); // 样例 0 duration
    push(&mut p, &be32(4)); // 样例 0 size —— 样例 1 的字段缺失
    let moof_bytes = moof(&[
        mfhd(1),
        traf(&[
            tfhd(7, 0x0002_0000, None, (None, None, None)),
            r#box("trun", &p),
        ]),
    ]);
    let mut f = Vec::new();
    push(&mut f, &ftyp());
    push(&mut f, &video_fmp4_moov(&[]));
    push(&mut f, &moof_bytes);
    push(&mut f, &mdat(&[0; 8]));

    match Mp4Reader::from_vec(f) {
        Err(Mp4Error::Malformed { reason, path, .. }) => {
            assert!(path.ends_with("trun"), "path={path}");
            assert!(reason.contains("截断"), "reason={reason}");
        }
        other => panic!("应为 Malformed，实为 {other:?}"),
    }
}

/// 同轨 stbl 非空样例又遇 moof 样例 ⇒ Unsupported（混合编址不支持）。
#[test]
fn fmp4_mixed_progressive_fragmented_is_unsupported() {
    let stbl = vec![
        stsd_video(),
        stts(&[(3, 10)]),
        stsc(&[(1, 3, 1)]),
        stsz_var(&[4, 5, 6]),
        stco(&[0]), // 占位；读不到样例（先报 Unsupported），偏移不必回真值
    ];
    let moov = wrap(
        "moov",
        &[mvhd(), trak(7, "vide", 1000, &stbl), mvex(&[(7, 0, 0, 0)])],
    );
    let make = |rel: i32| {
        moof(&[
            mfhd(1),
            traf(&[
                tfhd(7, 0x0002_0000, None, (None, None, None)),
                tfdt(0, 30),
                trun(
                    0,
                    &[TrunS {
                        dur: Some(10),
                        size: Some(4),
                        flags: None,
                        cts: None,
                    }],
                    Some(rel),
                    None,
                ),
            ]),
        ])
    };
    let f = layout(vec![
        fixed(moov),
        moof_part(make),
        fixed(mdat(&[0xA0, 0xA1, 0xA2, 0xA3])),
    ]);
    match Mp4Reader::from_vec(f) {
        Err(Mp4Error::Unsupported(what)) => assert_eq!(what, "混合 progressive/fragmented 轨道"),
        other => panic!("应为 Unsupported，实为 {other:?}"),
    }
}

/// moof 内声明 size=4 的 box（小于 8 字节头）⇒ Malformed（沿用既有头部校验）。
#[test]
fn fmp4_box_size_below_8_in_moof_is_malformed() {
    let mut traf_bytes = Vec::new();
    push(&mut traf_bytes, &be32(8 + 8)); // traf 头
    push(&mut traf_bytes, b"traf");
    push(&mut traf_bytes, &be32(4)); // 声明 size=4
    push(&mut traf_bytes, b"trun");
    let mut moof_payload = mfhd(1);
    push(&mut moof_payload, &traf_bytes);
    let mut moof_bytes = Vec::new();
    push(&mut moof_bytes, &be32(8 + moof_payload.len() as u32));
    push(&mut moof_bytes, b"moof");
    push(&mut moof_bytes, &moof_payload);

    let mut f = Vec::new();
    push(&mut f, &ftyp());
    push(&mut f, &video_fmp4_moov(&[]));
    push(&mut f, &moof_bytes);
    push(&mut f, &mdat(&[0; 8]));
    match Mp4Reader::from_vec(f) {
        Err(Mp4Error::Malformed { reason, .. }) => {
            assert!(reason.contains("小于 8"), "reason={reason}");
        }
        other => panic!("应为 Malformed，实为 {other:?}"),
    }
}

/// 首 traf 无任何基值信息（tfhd 无 0x01/0x020000）：兜底取 moof 起点，
/// trun data_offset 相对 moof 起点 ⇒ 正确读 mdat。
#[test]
fn fmp4_implicit_base_falls_back_to_moof_start() {
    let make = |rel: i32| {
        moof(&[
            mfhd(1),
            traf(&[
                tfhd(7, 0, None, (None, None, None)), // 仅 track_id
                tfdt(0, 0),
                trun(
                    0,
                    &[
                        TrunS {
                            dur: Some(10),
                            size: Some(4),
                            flags: None,
                            cts: None,
                        },
                        TrunS {
                            dur: Some(10),
                            size: Some(4),
                            flags: None,
                            cts: None,
                        },
                    ],
                    Some(rel),
                    None,
                ),
            ]),
        ])
    };
    let f = layout(vec![
        // 无 mvex 亦合法（trun 全显式不依赖 trex）。
        fixed(wrap(
            "moov",
            &[mvhd(), trak(7, "vide", 1000, &empty_stbl(stsd_video()))],
        )),
        moof_part(make),
        fixed(mdat(&[0xD0, 0xD1, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7])),
    ]);
    let mut r = Mp4Reader::from_vec(f).unwrap();
    assert_eq!(r.sample_count(0).unwrap(), 2);
    assert_eq!(
        &*r.read_packet(0, 0).unwrap().data,
        &[0xD0, 0xD1, 0xD2, 0xD3]
    );
    assert_eq!(
        &*r.read_packet(0, 1).unwrap().data,
        &[0xD4, 0xD5, 0xD6, 0xD7]
    );
}

/// tfhd 引用不存在的 track_id ⇒ Malformed（moof 无法关联 trak）。
#[test]
fn fmp4_tfhd_unknown_track_id_is_malformed() {
    let make = |rel: i32| {
        moof(&[
            mfhd(1),
            traf(&[
                tfhd(99, 0x0002_0000, None, (None, None, None)),
                tfdt(0, 0),
                trun(
                    0,
                    &[TrunS {
                        dur: Some(10),
                        size: Some(4),
                        flags: None,
                        cts: None,
                    }],
                    Some(rel),
                    None,
                ),
            ]),
        ])
    };
    let f = layout(vec![
        fixed(video_fmp4_moov(&[])),
        moof_part(make),
        fixed(mdat(&[0xA0, 0xA1, 0xA2, 0xA3])),
    ]);
    match Mp4Reader::from_vec(f) {
        Err(Mp4Error::Malformed { reason, .. }) => {
            assert!(reason.contains("track_id=99"), "reason={reason}");
        }
        other => panic!("应为 Malformed，实为 {other:?}"),
    }
}
