//! Mp4Writer fMP4（Fragmented）单元测试：写 `Cursor<Vec<u8>>`，自包含
//! 字节断言 + 自产自读（`Mp4Reader`，零 ffmpeg）。
//!
//! 覆盖：①黄金字节（box 序列 / tfhd / tfdt v1 / trun v1 / data_offset
//! 算术 / mdat largesize 头）②多 fragment（tfdt 接续、mfhd 递增）
//! ③fragment_duration 关键帧自动切与 None 仅手动 ④flush + finalize 冲尾
//! （含空 flush no-op）⑤自产自读逐包 roundtrip（音视频交错、负 pts−dts）
//! ⑥Misuse（Progressive 下 flush、空轨 finalize 产合法可读文件）
//! ⑦模块二校验链在 Fragmented 下同样生效（负 dts/间隙/回退；首 dts
//! 非零合法——tfdt 承载）。
//!
//! 帧时戳全局 tick（基数 7,056,000）：1/1000 时基下 1 单位 = 70_560 tick，
//! 1/48000 下 1 单位 = 147 tick。options 一律 `..Default::default()` 构造。

use std::io::Cursor;
use ye_container_mp4::{
    AudioInfo, BoxNode, FourCc, Layout, MediaKind, Mp4Reader, Mp4Writer, Mp4WriterOptions, Packet,
    Rational, Timestamp, TrackConfig, VideoInfo, WriterError,
};

/// avcC 载荷（AVCDecoderConfigurationRecord 本体；内容任意但定长便于断言）。
const AVCC: [u8; 14] = [
    0x01, 0x64, 0x00, 0x1E, 0xFF, 0xE1, 0x00, 0x08, 0x1A, 0x2B, 0x3C, 0x4D, 0x5E, 0x6F,
];

/// esds 载荷（含 4 字节版本/标志；内容任意但定长便于断言）。
const ESDS: [u8; 29] = [
    0x00, 0x00, 0x00, 0x00, // version/flags
    0x03, 0x19, 0x00, 0x01, 0x00, // ES_Descriptor 头
    0x04, 0x11, 0x40, 0x15, // DecoderConfigDescriptor 头
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // avgBitrate 等
    0x05, 0x02, 0x10, 0x88, // DecspecificInfo
    0x06, 0x01, 0x02, // SLConfigDescriptor
];

fn t(v: i64) -> Timestamp {
    Timestamp::from_ticks(v)
}

fn video_cfg(tb_den: i64) -> TrackConfig {
    TrackConfig {
        kind: MediaKind::Video,
        codec: FourCc::new(*b"avc1"),
        timebase: Rational::new(1, tb_den).unwrap(),
        video: Some(VideoInfo {
            width: 64,
            height: 96,
        }),
        audio: None,
        extra_data: AVCC.to_vec(),
    }
}

fn audio_cfg(tb_den: i64) -> TrackConfig {
    TrackConfig {
        kind: MediaKind::Audio,
        codec: FourCc::new(*b"mp4a"),
        timebase: Rational::new(1, tb_den).unwrap(),
        video: None,
        audio: Some(AudioInfo {
            channel_count: 2,
            sample_rate: 48_000,
        }),
        extra_data: ESDS.to_vec(),
    }
}

fn pkt(si: usize, idx: usize, dts: i64, dur: i64, key: bool, data: &[u8]) -> Packet {
    Packet {
        stream_index: si,
        sample_index: idx,
        dts: t(dts),
        pts: None,
        duration: t(dur),
        keyframe: key,
        data: data.to_vec(),
    }
}

fn frag_opts(fd: Option<Timestamp>) -> Mp4WriterOptions {
    Mp4WriterOptions {
        layout: Layout::Fragmented,
        fragment_duration: fd,
        ..Default::default()
    }
}

// —— 定点字节断言的小工具 ——

fn u32b(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

fn u64b(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

fn i32b(b: &[u8], at: usize) -> i32 {
    u32b(b, at) as i32
}

fn find<'a>(nodes: &'a [BoxNode], kind: &str) -> Option<&'a BoxNode> {
    nodes.iter().find(|n| n.info.kind.as_str() == kind)
}

/// box 载荷（8 字节普通头；本封装器产出的 moof 内表 box 均非 largesize）。
fn payload<'a>(data: &'a [u8], node: &BoxNode) -> &'a [u8] {
    let s = node.info.offset as usize;
    &data[s + 8..s + node.info.size as usize]
}

/// 手工 box 构造（黄金 moof 逐字节比对用）。
fn be_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + payload.len());
    v.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
    v.extend_from_slice(kind);
    v.extend_from_slice(payload);
    v
}

fn be32(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

fn push(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend_from_slice(bytes);
}

fn kinds(nodes: &[BoxNode]) -> Vec<String> {
    nodes.iter().map(|n| n.info.kind.as_str()).collect()
}

/// 顶层序列里的全部 moof（按出现序）。
fn moofs(r: &Mp4Reader<Cursor<Vec<u8>>>) -> Vec<&BoxNode> {
    r.box_tree()
        .iter()
        .filter(|n| n.info.kind.as_str() == "moof")
        .collect()
}

/// moof 内第 i 个 traf。
fn traf_of(moof: &BoxNode, i: usize) -> &BoxNode {
    moof.children
        .iter()
        .filter(|c| c.info.kind.as_str() == "traf")
        .nth(i)
        .unwrap()
}

fn mfhd_seq(p: &[u8]) -> u32 {
    assert_eq!(u32b(p, 0), 0, "mfhd version/flags 应为 0");
    u32b(p, 4)
}

/// tfdt 载荷 → (version, base)。本封装器恒 v1。
fn tfdt_base(p: &[u8]) -> (u32, u64) {
    (u32b(p, 0) >> 24, u64b(p, 4))
}

/// trun 头 → (version|flags, sample_count, data_offset)。
fn trun_head(p: &[u8]) -> (u32, u32, i32) {
    (u32b(p, 0), u32b(p, 4), i32b(p, 8))
}

/// misère 断言：Misuse 且文案含 what。
fn misuse(err: WriterError, what: &str) {
    match &err {
        WriterError::Misuse(msg) => assert!(
            msg.contains(what),
            "Misuse 文案应含「{what}」，实为「{msg}」"
        ),
        other => panic!("应为 Misuse，实为 {other:?}"),
    }
}

// —— ①②④ 黄金字节：布局 / box 形态 / data_offset 算术 / mdat 头 ——

/// 期望的黄金 moof（单视频轨，track_id=1）。`base` 为 tfdt 基值；
/// `samples` 为 (dur_units, size, keyframe, cts_units)。
fn expect_moof(
    seq: u32,
    base: u64,
    data_offset: i32,
    samples: &[(u32, u32, bool, i32)],
) -> Vec<u8> {
    let mut mfhd_p = Vec::new();
    push(&mut mfhd_p, &be32(0));
    push(&mut mfhd_p, &be32(seq));
    let mfhd = be_box(b"mfhd", &mfhd_p);
    let mut tfhd_p = Vec::new();
    push(&mut tfhd_p, &be32(0x0002_0002)); // default-base-is-moof | sample-description-index
    push(&mut tfhd_p, &be32(1)); // track_ID
    push(&mut tfhd_p, &be32(1)); // sample_description_index
    let tfhd = be_box(b"tfhd", &tfhd_p);
    let mut tfdt_p = Vec::new();
    push(&mut tfdt_p, &be32(1 << 24)); // version 1
    push(&mut tfdt_p, &base.to_be_bytes());
    let tfdt = be_box(b"tfdt", &tfdt_p);
    let mut trun_p = Vec::new();
    push(&mut trun_p, &be32((1 << 24) | 0x0000_0F01)); // v1 | data-offset|dur|size|flags|cts
    push(&mut trun_p, &be32(samples.len() as u32));
    push(&mut trun_p, &(data_offset as u32).to_be_bytes());
    for &(dur, size, key, cts) in samples {
        push(&mut trun_p, &be32(dur));
        push(&mut trun_p, &be32(size));
        push(
            &mut trun_p,
            &be32(if key { 0x0200_0000 } else { 0x0101_0000 }),
        );
        push(&mut trun_p, &(cts as u32).to_be_bytes());
    }
    let trun = be_box(b"trun", &trun_p);
    let mut traf_p = Vec::new();
    push(&mut traf_p, &tfhd);
    push(&mut traf_p, &tfdt);
    push(&mut traf_p, &trun);
    let traf = be_box(b"traf", &traf_p);
    let mut moof_p = Vec::new();
    push(&mut moof_p, &mfhd);
    push(&mut moof_p, &traf);
    be_box(b"moof", &moof_p)
}

#[test]
fn golden_fragmented_bytes() {
    let u = 70_560i64; // 1 单位 @1/1000 = 10 单位域 units? 恰为 10 units。
    let mut w = Mp4Writer::new(Cursor::new(Vec::new()), frag_opts(None));
    w.add_stream(video_cfg(1000)).unwrap();
    w.write_packet(&pkt(0, 0, 0, u, true, &[0xA0, 0xA1, 0xA2, 0xA3]))
        .unwrap();
    w.write_packet(&pkt(0, 1, u, u, false, &[0xA4, 0xA5, 0xA6, 0xA7, 0xA8]))
        .unwrap();
    w.flush_fragment().unwrap();
    w.write_packet(&pkt(0, 2, 2 * u, u, true, &[0xB0, 0xB1]))
        .unwrap();
    let data = w.finalize().unwrap().into_inner();

    let mut r = Mp4Reader::from_vec(data.clone()).unwrap();
    // 顶层序列：ftyp → moov → (moof → mdat)×2，无 free。
    let top = kinds(r.box_tree());
    assert_eq!(top, ["ftyp", "moov", "moof", "mdat", "moof", "mdat"]);

    // —— 前置 moov：mvhd + trak(完整 stsd + 空表) + mvex(trex) ——
    let moov = find(r.box_tree(), "moov").unwrap();
    assert_eq!(kinds(&moov.children), ["mvhd", "trak", "mvex"]);
    let trak = find(&moov.children, "trak").unwrap();
    let mdia = find(&trak.children, "mdia").unwrap();
    let minf = find(&mdia.children, "minf").unwrap();
    let stbl = find(&minf.children, "stbl").unwrap();
    assert_eq!(
        kinds(&stbl.children),
        ["stsd", "stts", "stsc", "stsz", "stco"]
    );
    for name in ["stts", "stsc", "stco"] {
        let p = payload(&data, find(&stbl.children, name).unwrap());
        assert_eq!(p.len(), 8, "{name} 空表恰 version/flags + count");
        assert_eq!(u32b(p, 4), 0, "{name} entry_count=0");
    }
    let stsz = payload(&data, find(&stbl.children, "stsz").unwrap());
    assert_eq!(stsz.len(), 12);
    assert_eq!((u32b(stsz, 4), u32b(stsz, 8)), (0, 0), "stsz 变长形态空表");
    // stsd 完整：读回流参数原样。
    let s = r.stream(0).unwrap();
    assert_eq!(s.kind, MediaKind::Video);
    assert_eq!(s.codec.as_str(), "avc1");
    assert_eq!(
        s.video,
        Some(VideoInfo {
            width: 64,
            height: 96
        })
    );
    assert_eq!(s.extra_data, AVCC.to_vec());
    // mvex/trex：track_id=1、缺省全 0。
    let mvex = find(&moov.children, "mvex").unwrap();
    assert_eq!(kinds(&mvex.children), ["trex"]);
    let trex = payload(&data, find(&mvex.children, "trex").unwrap());
    assert_eq!(trex.len(), 24);
    assert_eq!(u32b(trex, 0), 0);
    assert_eq!(u32b(trex, 4), 1, "track_ID");
    for at in [8, 12, 16, 20] {
        assert_eq!(u32b(trex, at), 0, "trex 缺省字段全 0（+{at}）");
    }

    // —— moof1 黄金逐字节 ——
    let (moof1, mdat1) = (&r.box_tree()[2], &r.box_tree()[3]);
    // 手工尺寸：mfhd16 + tfhd20 + tfdt20 + trun(20+2×16=52) → traf100 → moof124。
    let expect1 = expect_moof(1, 0, 124 + 16, &[(10, 4, true, 0), (10, 5, false, 0)]);
    assert_eq!(moof1.info.size as usize, expect1.len());
    let got1 = &data[moof1.info.offset as usize..moof1.info.offset as usize + expect1.len()];
    assert_eq!(got1, expect1.as_slice(), "moof1 应与手工黄金字节一致");
    // data_offset 算术：moof 起点 + data_offset == mdat 载荷起点。
    let traf1 = traf_of(moof1, 0);
    let trun1 = find(&traf1.children, "trun").unwrap();
    let (vf, n, off) = trun_head(payload(&data, trun1));
    assert_eq!((vf, n), ((1 << 24) | 0xF01, 2));
    assert_eq!(
        moof1.info.offset as i64 + off as i64,
        mdat1.info.offset as i64 + 16,
        "data_offset 应指向 mdat 载荷起点"
    );
    // mdat 恒 16 字节 size==1 + largesize 头，载荷 4+5=9 字节。
    let mo1 = mdat1.info.offset as usize;
    assert_eq!(u32b(&data, mo1), 1);
    assert_eq!(u64b(&data, mo1 + 8), 16 + 9);
    assert_eq!(mdat1.info.size, 16 + 9);
    assert_eq!(
        &data[mo1 + 16..mo1 + 25],
        &[0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8]
    );

    // —— moof2：mfhd 递增、tfdt 接续前 fragment 尾（20 单位）——
    let (moof2, mdat2) = (&r.box_tree()[4], &r.box_tree()[5]);
    let expect2 = expect_moof(2, 20, 108 + 16, &[(10, 2, true, 0)]);
    assert_eq!(moof2.info.size as usize, expect2.len());
    let got2 = &data[moof2.info.offset as usize..moof2.info.offset as usize + expect2.len()];
    assert_eq!(got2, expect2.as_slice(), "moof2 应与手工黄金字节一致");
    let traf2 = traf_of(moof2, 0);
    let tfdt2 = payload(&data, find(&traf2.children, "tfdt").unwrap());
    assert_eq!(tfdt_base(tfdt2), (1, 20), "tfdt v1 基值接续前 fragment 尾");
    assert_eq!(
        mfhd_seq(payload(&data, find(&moof2.children, "mfhd").unwrap())),
        2
    );
    let mo2 = mdat2.info.offset as usize;
    assert_eq!(u64b(&data, mo2 + 8), 16 + 2);
    assert_eq!(&data[mo2 + 16..mo2 + 18], &[0xB0, 0xB1]);

    // —— finalize 回填时长（占位强制 v1 ⇒ u64 定宽）——
    let mvhd = find(&moov.children, "mvhd").unwrap();
    assert_eq!(
        u32b(&data, mvhd.info.offset as usize + 8) >> 24,
        1,
        "mvhd v1"
    );
    assert_eq!(
        u64b(&data, mvhd.info.offset as usize + 32),
        30,
        "mvhd 时长 3×10 单位"
    );
    let tkhd = find(&trak.children, "tkhd").unwrap();
    assert_eq!(
        u64b(&data, tkhd.info.offset as usize + 36),
        30,
        "tkhd 时长（movie 时基恰同值）"
    );
    let mdhd = find(&mdia.children, "mdhd").unwrap();
    assert_eq!(
        u64b(&data, mdhd.info.offset as usize + 32),
        30,
        "mdhd 时长（轨道时基）"
    );

    // —— 自产自读：逐包一致（pts 缺省语义 = dts）——
    assert_eq!(r.sample_count(0).unwrap(), 3);
    let want = [
        (0i64, true, vec![0xA0, 0xA1, 0xA2, 0xA3]),
        (u, false, vec![0xA4, 0xA5, 0xA6, 0xA7, 0xA8]),
        (2 * u, true, vec![0xB0, 0xB1]),
    ];
    for (i, (dts, key, bytes)) in want.iter().enumerate() {
        let p = r.read_packet(0, i).unwrap();
        assert_eq!(p.dts, t(*dts), "样例 {i} dts");
        assert_eq!(p.pts, Some(t(*dts)), "样例 {i} pts（cts=0 ⇒ Some(dts)）");
        assert_eq!(p.duration, t(u), "样例 {i} duration");
        assert_eq!(p.keyframe, *key, "样例 {i} keyframe");
        assert_eq!(&p.data, bytes, "样例 {i} data");
    }
}

// —— ② 多 fragment：tfdt 接续与 mfhd 序号递增（3 fragment）——

#[test]
fn multi_fragment_tfdt_and_mfhd_increment() {
    let u = 70_560i64;
    let mut w = Mp4Writer::new(Cursor::new(Vec::new()), frag_opts(None));
    w.add_stream(video_cfg(1000)).unwrap();
    for frag in 0..3u64 {
        for k in 0..2u64 {
            let idx = frag * 2 + k;
            w.write_packet(&pkt(
                0,
                idx as usize,
                idx as i64 * u,
                u,
                k == 0,
                &[(0xA0 + idx as u8); 3],
            ))
            .unwrap();
        }
        w.flush_fragment().unwrap();
    }
    let data = w.finalize().unwrap().into_inner();
    let mut r = Mp4Reader::from_vec(data.clone()).unwrap();
    let ms = moofs(&r);
    assert_eq!(
        ms.len(),
        3,
        "恰 3 个 moof（finalize 无尾可冲不产空 fragment）"
    );
    for (i, m) in ms.iter().enumerate() {
        assert_eq!(
            mfhd_seq(payload(&data, find(&m.children, "mfhd").unwrap())),
            i as u32 + 1,
            "mfhd 序号从 1 递增"
        );
        let traf = traf_of(m, 0);
        let tfdt = payload(&data, find(&traf.children, "tfdt").unwrap());
        assert_eq!(
            tfdt_base(tfdt),
            (1, (i as u64) * 20),
            "fragment {i} tfdt 接续"
        );
    }
    // 读回：6 样例 dts 跨 fragment 连续（0/10/20/30/40/50 单位）。
    assert_eq!(r.sample_count(0).unwrap(), 6);
    for i in 0..6 {
        assert_eq!(
            r.read_packet(0, i).unwrap().dts,
            t(i as i64 * u),
            "样例 {i}"
        );
    }
    assert_eq!(
        r.stream(0).unwrap().duration,
        t(6 * u),
        "末 fragment 尾 dts+dur"
    );
}

// —— ③ fragment_duration：关键帧边界自动切；None 仅手动 ——

/// 每包 10 单位、关键帧在 idx 0 与 3：阈值 20 单位 ⇒ 在 idx3（关键帧）
/// 切，得 [0..2]/[3..5] 两 fragment；None ⇒ 单 fragment。
#[test]
fn fragment_duration_cuts_at_keyframes() {
    let u = 70_560i64;
    let mk_packets = || {
        (0..6)
            .map(|i| {
                pkt(
                    0,
                    i,
                    i as i64 * u,
                    u,
                    i == 0 || i == 3,
                    &[(0xA0 + i as u8); 2],
                )
            })
            .collect::<Vec<_>>()
    };
    // Some(2u)：idx3 到达时待切跨度 30 单位 ≥ 20 ⇒ 切。
    let mut w = Mp4Writer::new(Cursor::new(Vec::new()), frag_opts(Some(t(2 * u))));
    w.add_stream(video_cfg(1000)).unwrap();
    for p in &mk_packets() {
        w.write_packet(p).unwrap();
    }
    let data = w.finalize().unwrap().into_inner();
    let r = Mp4Reader::from_vec(data.clone()).unwrap();
    let ms = moofs(&r);
    assert_eq!(ms.len(), 2, "自动切应在关键帧 idx3 处切一刀");
    let (t0, t3) = (
        payload(&data, find(&traf_of(ms[0], 0).children, "tfdt").unwrap()),
        payload(&data, find(&traf_of(ms[1], 0).children, "tfdt").unwrap()),
    );
    assert_eq!(tfdt_base(t0), (1, 0));
    assert_eq!(
        tfdt_base(t3),
        (1, 30),
        "第二 fragment 起点即关键帧 idx3 的 dts"
    );
    let (tr0, tr3) = (
        find(&traf_of(ms[0], 0).children, "trun").unwrap(),
        find(&traf_of(ms[1], 0).children, "trun").unwrap(),
    );
    assert_eq!(trun_head(payload(&data, tr0)).1, 3);
    assert_eq!(trun_head(payload(&data, tr3)).1, 3);
    assert_eq!(r.sample_count(0).unwrap(), 6);

    // None：同包序列不自动切，finalize 冲尾单 fragment。
    let mut w = Mp4Writer::new(Cursor::new(Vec::new()), frag_opts(None));
    w.add_stream(video_cfg(1000)).unwrap();
    for p in &mk_packets() {
        w.write_packet(p).unwrap();
    }
    let data = w.finalize().unwrap().into_inner();
    let r = Mp4Reader::from_vec(data).unwrap();
    assert_eq!(moofs(&r).len(), 1, "None ⇒ 仅手动/冲尾");

    // 阈值已过但下一包非关键帧 ⇒ 不切，直到下一关键帧：关键帧仅 idx0/5，
    // 阈值 20 单位 ⇒ 唯一一刀落在 idx5。
    let mut w = Mp4Writer::new(Cursor::new(Vec::new()), frag_opts(Some(t(2 * u))));
    w.add_stream(video_cfg(1000)).unwrap();
    for i in 0..6 {
        w.write_packet(&pkt(
            0,
            i,
            i as i64 * u,
            u,
            i == 0 || i == 5,
            &[(0xB0 + i as u8); 2],
        ))
        .unwrap();
    }
    let data = w.finalize().unwrap().into_inner();
    let r = Mp4Reader::from_vec(data.clone()).unwrap();
    let ms = moofs(&r);
    assert_eq!(ms.len(), 2, "只在关键帧切：[0..4]/[5]");
    let tfdt2 = payload(&data, find(&traf_of(ms[1], 0).children, "tfdt").unwrap());
    assert_eq!(tfdt_base(tfdt2), (1, 50));
    assert_eq!(
        trun_head(payload(
            &data,
            find(&traf_of(ms[1], 0).children, "trun").unwrap()
        ))
        .1,
        1
    );
}

// —— ④ flush_fragment + finalize 冲尾；空 flush 为 no-op ——

#[test]
fn flush_fragment_noop_and_finalize_tail() {
    let u = 70_560i64;
    let mut w = Mp4Writer::new(Cursor::new(Vec::new()), frag_opts(None));
    w.add_stream(video_cfg(1000)).unwrap();
    // 尚无任何包：flush 为 no-op（不产 moof，也不落 moov）。
    w.flush_fragment().unwrap();
    w.write_packet(&pkt(0, 0, 0, u, true, &[0xA0])).unwrap();
    w.flush_fragment().unwrap();
    // 无待切数据：再次 no-op。
    w.flush_fragment().unwrap();
    w.write_packet(&pkt(0, 1, u, u, false, &[0xA1])).unwrap();
    // 不手动 flush：finalize 冲尾（第二 fragment 仍切出，空 flush 未产 box）。
    let data = w.finalize().unwrap().into_inner();
    let mut r = Mp4Reader::from_vec(data).unwrap();
    let top = kinds(r.box_tree());
    assert_eq!(
        top,
        ["ftyp", "moov", "moof", "mdat", "moof", "mdat"],
        "两次空 flush 不产 box；尾批由 finalize 冲出"
    );
    assert_eq!(r.sample_count(0).unwrap(), 2, "冲尾 fragment 收走剩余样例");
    assert_eq!(r.read_packet(0, 1).unwrap().data, vec![0xA1]);
}

// —— ⑤ 自产自读：音视频交错 + 负 pts−dts（trun v1 有符号 cts）——

#[test]
fn roundtrip_interleaved_av_with_negative_cts() {
    let u = 70_560i64; // 视频 1/1000：1 单位 = 10 units。
    let ua = 147i64; // 音频 1/48000：1 单位。
    let mut w = Mp4Writer::new(Cursor::new(Vec::new()), frag_opts(None));
    w.add_stream(video_cfg(1000)).unwrap();
    w.add_stream(audio_cfg(48_000)).unwrap();
    let mut v0 = pkt(0, 0, 0, u, true, &[0xA0, 0xA1]);
    v0.pts = Some(t(-u)); // 负 pts−dts：cts = −10 单位。
    let mut v2 = pkt(0, 2, 2 * u, u, true, &[0xA4, 0xA5]);
    v2.pts = Some(t(2 * u + 7_056)); // +1 单位 cts。
    let a2 = pkt(1, 2, 2 * ua, ua, true, &[0xC4, 0xC5]);
    let input = vec![
        v0.clone(),
        pkt(1, 0, 0, ua, true, &[0xC0, 0xC1]),  // a0
        pkt(0, 1, u, u, false, &[0xA2, 0xA3]),  // v1
        pkt(1, 1, ua, ua, true, &[0xC2, 0xC3]), // a1
    ];
    for p in &input {
        w.write_packet(p).unwrap();
    }
    w.flush_fragment().unwrap(); // fragment1：v0/v1 + a0/a1
    w.write_packet(&v2).unwrap();
    w.write_packet(&a2).unwrap(); // fragment2（finalize 冲尾）
    let data = w.finalize().unwrap().into_inner();

    // mdat1 载荷按轨序拼接：[v0,v1][a0,a1]，音频 data_offset 含轨前数据量。
    let r = Mp4Reader::from_vec(data.clone()).unwrap();
    let ms = moofs(&r);
    assert_eq!(ms.len(), 2);
    let (m1, md1) = (ms[0], find(r.box_tree(), "mdat").unwrap());
    let (traf_v, traf_a) = (traf_of(m1, 0), traf_of(m1, 1));
    let (tr_v, tr_a) = (
        find(&traf_v.children, "trun").unwrap(),
        find(&traf_a.children, "trun").unwrap(),
    );
    let off_v = trun_head(payload(&data, tr_v)).2;
    let off_a = trun_head(payload(&data, tr_a)).2;
    assert_eq!(
        m1.info.offset as i64 + off_v as i64,
        md1.info.offset as i64 + 16,
        "视频轨 data_offset → mdat 载荷起点"
    );
    assert_eq!(
        m1.info.offset as i64 + off_a as i64,
        md1.info.offset as i64 + 16 + 4,
        "音频轨 data_offset → 视频轨数据（4 字节）之后"
    );
    let mo1 = md1.info.offset as usize;
    assert_eq!(
        &data[mo1 + 16..mo1 + 16 + 8],
        &[0xA0, 0xA1, 0xA2, 0xA3, 0xC0, 0xC1, 0xC2, 0xC3],
        "mdat 载荷按轨序：视频批在前、音频批在后"
    );

    // 逐包 roundtrip：dts/pts/duration/keyframe/data 与输入完全一致。
    let mut r = Mp4Reader::from_vec(data).unwrap();
    let v_all = vec![input[0].clone(), input[2].clone(), v2];
    let a_all = vec![input[1].clone(), input[3].clone(), a2];
    for (si, want) in [v_all, a_all].into_iter().enumerate() {
        assert_eq!(r.sample_count(si).unwrap(), want.len(), "流 {si} 样例数");
        for (i, wp) in want.iter().enumerate() {
            let p = r.read_packet(si, i).unwrap();
            assert_eq!(p.dts, wp.dts, "流 {si} 样例 {i} dts");
            assert_eq!(
                p.pts.unwrap_or(p.dts),
                wp.pts.unwrap_or(wp.dts),
                "流 {si} 样例 {i} pts（缺省语义 = dts）"
            );
            assert_eq!(p.duration, wp.duration, "流 {si} 样例 {i} duration");
            assert_eq!(p.keyframe, wp.keyframe, "流 {si} 样例 {i} keyframe");
            assert_eq!(p.data, wp.data, "流 {si} 样例 {i} data");
        }
    }
    // v0 的负 cts 走 trun v1 有符号字段：pts = dts + (−10 单位)。
    assert_eq!(r.read_packet(0, 0).unwrap().pts, Some(t(-u)));
    // 流元数据 roundtrip。
    let (sv, sa) = (r.stream(0).unwrap(), r.stream(1).unwrap());
    assert_eq!(sv.timebase, Rational::new(1, 1000).unwrap());
    assert_eq!(sa.timebase, Rational::new(1, 48_000).unwrap());
    assert_eq!(
        sa.audio,
        Some(AudioInfo {
            channel_count: 2,
            sample_rate: 48_000
        })
    );
    assert_eq!(sa.extra_data, ESDS.to_vec());
}

// —— ⑥ Misuse：Progressive 下 flush / 空轨 finalize ——

#[test]
fn flush_fragment_on_progressive_is_misuse() {
    let mut w = Mp4Writer::new(Cursor::new(Vec::new()), Mp4WriterOptions::default());
    w.add_stream(video_cfg(1000)).unwrap();
    match w.flush_fragment().unwrap_err() {
        WriterError::Misuse(msg) => {
            assert_eq!(msg, "flush_fragment 仅 Fragmented 布局可用")
        }
        other => panic!("应为 Misuse，实为 {other:?}"),
    }
    // Progressive 写入不受影响。
    w.write_packet(&pkt(0, 0, 0, 70_560, true, &[0])).unwrap();
}

/// Fragmented 空轨（无包 finalize）：产出仅 ftyp + moov 的合法文件。
#[test]
fn fragmented_zero_packet_finalize_is_readable() {
    let mut w = Mp4Writer::new(Cursor::new(Vec::new()), frag_opts(None));
    w.add_stream(video_cfg(1000)).unwrap();
    w.add_stream(audio_cfg(48_000)).unwrap();
    let data = w.finalize().unwrap().into_inner();
    let mut r = Mp4Reader::from_vec(data.clone()).unwrap();
    assert_eq!(kinds(r.box_tree()), ["ftyp", "moov"], "无 fragment 序列");
    assert_eq!(r.streams().len(), 2);
    assert_eq!(r.sample_count(0).unwrap(), 0);
    assert_eq!(r.sample_count(1).unwrap(), 0);
    assert_eq!(
        r.stream(0).unwrap().duration,
        Timestamp::ZERO,
        "0 包 ⇒ 时长回填 0"
    );
    match r.read_packet(0, 0) {
        Err(ye_container_mp4::Mp4Error::SampleIndexOutOfRange { .. }) => {}
        other => panic!("应为 SampleIndexOutOfRange，实为 {other:?}"),
    }
    // 时长字段全 0（v1 定宽已回填，不再是占位值）。
    let moov = find(r.box_tree(), "moov").unwrap();
    let mvhd = find(&moov.children, "mvhd").unwrap();
    assert_eq!(u64b(&data, mvhd.info.offset as usize + 32), 0);
}

// —— ⑦ 校验链在 Fragmented 下同样生效 ——

#[test]
fn fragmented_validation_chain() {
    let u = 70_560i64;
    // 负 dts：固定文案。
    let mut w = Mp4Writer::new(Cursor::new(Vec::new()), frag_opts(None));
    w.add_stream(video_cfg(1000)).unwrap();
    let err = w.write_packet(&pkt(0, 0, -147, u, true, &[0])).unwrap_err();
    match err {
        WriterError::Misuse(msg) => assert_eq!(
            msg,
            "负 dts 不可表达（stts 0 起/tfdt u64），请调用方整体平移"
        ),
        other => panic!("应为 Misuse，实为 {other:?}"),
    }
    // 间隙。
    let mut w = Mp4Writer::new(Cursor::new(Vec::new()), frag_opts(None));
    w.add_stream(video_cfg(1000)).unwrap();
    w.write_packet(&pkt(0, 0, 0, u, true, &[0])).unwrap();
    misuse(
        w.write_packet(&pkt(0, 1, 2 * u, u, true, &[1]))
            .unwrap_err(),
        "间隙/重叠",
    );
    // 跨 fragment 回退与间隙：flush 后游标接管校验。
    let mut w = Mp4Writer::new(Cursor::new(Vec::new()), frag_opts(None));
    w.add_stream(video_cfg(1000)).unwrap();
    w.write_packet(&pkt(0, 0, 0, u, true, &[0])).unwrap();
    w.write_packet(&pkt(0, 1, u, u, false, &[1])).unwrap();
    w.flush_fragment().unwrap();
    misuse(
        w.write_packet(&pkt(0, 2, u / 2, u, true, &[2]))
            .unwrap_err(),
        "单调不减",
    );
    misuse(
        w.write_packet(&pkt(0, 2, 3 * u, u, true, &[2]))
            .unwrap_err(),
        "间隙/重叠",
    );
    // 正常接续后 finalize 仍可收尾。
    w.write_packet(&pkt(0, 2, 2 * u, u, true, &[2])).unwrap();
    let data = w.finalize().unwrap().into_inner();
    let r = Mp4Reader::from_vec(data).unwrap();
    assert_eq!(r.sample_count(0).unwrap(), 3);

    // 首 dts 非零在 Fragmented 合法：tfdt 承载基值。
    let mut w = Mp4Writer::new(Cursor::new(Vec::new()), frag_opts(None));
    w.add_stream(video_cfg(1000)).unwrap();
    w.write_packet(&pkt(0, 0, u, u, true, &[0xD0])).unwrap();
    let data = w.finalize().unwrap().into_inner();
    let mut r = Mp4Reader::from_vec(data.clone()).unwrap();
    let moof = find(r.box_tree(), "moof").unwrap();
    let tfdt = payload(&data, find(&traf_of(moof, 0).children, "tfdt").unwrap());
    assert_eq!(tfdt_base(tfdt), (1, 10), "tfdt = 首样例 dts（10 单位）");
    assert_eq!(r.read_packet(0, 0).unwrap().dts, t(u));
}
