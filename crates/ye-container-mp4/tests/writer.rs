//! Mp4Writer 单元测试：写 `Cursor<Vec<u8>>`，用 [`Mp4Reader`] 读回断言
//! + 定点字节断言。自包含（不依赖 helpers/）。
//!
//! 帧时戳全部给全局 tick（tick 基数 7,056,000）：1/1000 时基下
//! 1 单位 = 70_560 tick，1/48000 下 1 单位 = 147 tick。

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

/// 一条龙：建 writer → 加流 → 写包 → finalize → 字节。
fn mux(
    opts: Mp4WriterOptions,
    streams: Vec<TrackConfig>,
    packets: Vec<Packet>,
) -> Result<Vec<u8>, WriterError> {
    let mut w = Mp4Writer::new(Cursor::new(Vec::new()), opts);
    for s in streams {
        w.add_stream(s)?;
    }
    for p in &packets {
        w.write_packet(p)?;
    }
    let cur = w.finalize()?;
    Ok(cur.into_inner())
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

/// 第 trak_i 路轨的 stbl 节点。
fn stbl_of(r: &Mp4Reader<Cursor<Vec<u8>>>, trak_i: usize) -> &BoxNode {
    let moov = find(r.box_tree(), "moov").unwrap();
    let trak = moov
        .children
        .iter()
        .filter(|c| c.info.kind.as_str() == "trak")
        .nth(trak_i)
        .unwrap();
    let mdia = find(&trak.children, "mdia").unwrap();
    let minf = find(&mdia.children, "minf").unwrap();
    find(&minf.children, "stbl").unwrap()
}

/// box 载荷（8 字节普通头；本封装器产出的表 box 均非 largesize）。
fn payload<'a>(data: &'a [u8], node: &BoxNode) -> &'a [u8] {
    let s = node.info.offset as usize;
    &data[s + 8..s + node.info.size as usize]
}

fn stts_entries(p: &[u8]) -> Vec<(u32, u32)> {
    let n = u32b(p, 4) as usize;
    (0..n)
        .map(|i| (u32b(p, 8 + i * 8), u32b(p, 12 + i * 8)))
        .collect()
}

fn ctts_entries(p: &[u8]) -> (u32, Vec<(u32, i32)>) {
    let version = u32b(p, 0);
    let n = u32b(p, 4) as usize;
    (
        version >> 24,
        (0..n)
            .map(|i| (u32b(p, 8 + i * 8), i32b(p, 12 + i * 8)))
            .collect(),
    )
}

fn stss_entries(p: &[u8]) -> Vec<u32> {
    let n = u32b(p, 4) as usize;
    (0..n).map(|i| u32b(p, 8 + i * 4)).collect()
}

fn stsc_entries(p: &[u8]) -> Vec<(u32, u32, u32)> {
    let n = u32b(p, 4) as usize;
    (0..n)
        .map(|i| {
            (
                u32b(p, 8 + i * 12),
                u32b(p, 12 + i * 12),
                u32b(p, 16 + i * 12),
            )
        })
        .collect()
}

fn stsz_table(p: &[u8]) -> (u32, u32, Vec<u32>) {
    let fixed = u32b(p, 4);
    let count = u32b(p, 8) as usize;
    let sizes = (0..count).map(|i| u32b(p, 12 + i * 4)).collect();
    (fixed, count as u32, sizes)
}

fn stco_entries(p: &[u8]) -> Vec<u32> {
    let n = u32b(p, 4) as usize;
    (0..n).map(|i| u32b(p, 8 + i * 4)).collect()
}

fn co64_entries(p: &[u8]) -> Vec<u64> {
    let n = u32b(p, 4) as usize;
    (0..n).map(|i| u64b(p, 8 + i * 8)).collect()
}

// —— ① 单视频轨黄金：布局 + stco 回填 + stts run-length 合并 ——

#[test]
fn golden_layout_ftyp_free_mdat_moov() {
    let opts = Mp4WriterOptions {
        free_size: 16,
        ..Default::default()
    };
    // 3 样例：durations 10/10/20 单位（tb=1/1000 ⇒ 70_560/70_560/141_120 tick），
    // 数据 4+5+6=15 字节，全关键帧。
    let data = mux(
        opts,
        vec![video_cfg(1000)],
        vec![
            pkt(0, 0, 0, 70_560, true, &[0xA0, 0xA1, 0xA2, 0xA3]),
            pkt(0, 1, 70_560, 70_560, true, &[0xA4, 0xA5, 0xA6, 0xA7, 0xA8]),
            pkt(
                0,
                2,
                141_120,
                141_120,
                true,
                &[0xA9, 0xAA, 0xAB, 0xAC, 0xAD, 0xAE],
            ),
        ],
    )
    .unwrap();

    let mut r = Mp4Reader::from_vec(data.clone()).unwrap();
    // 顶层 box 序列：ftyp → free → mdat → moov。
    let kinds: Vec<String> = r.box_tree().iter().map(|n| n.info.kind.as_str()).collect();
    assert_eq!(kinds, ["ftyp", "free", "mdat", "moov"]);
    // ftyp 32 字节；free 8 头 + 16 载荷。
    assert_eq!(r.box_tree()[0].info.size, 32);
    assert_eq!(r.box_tree()[1].info.size, 24);
    // mdat 恒 16 字节 largesize 头：size 字段==1，largesize==16+15 载荷。
    let mdat = &r.box_tree()[2];
    let mo = mdat.info.offset as usize;
    assert_eq!(u32b(&data, mo), 1);
    assert_eq!(u64b(&data, mo + 8), 16 + 15);
    assert_eq!(mdat.info.size, 16 + 15);
    // moov 在 mdat 之后。
    assert_eq!(
        r.box_tree()[3].info.offset,
        mdat.info.offset + mdat.info.size
    );

    let stbl = stbl_of(&r, 0);
    // stco 回填：3 样例连续 ⇒ 单 chunk，偏移 = mdat 载荷起点。
    let stco = find(&stbl.children, "stco").unwrap();
    assert!(find(&stbl.children, "co64").is_none());
    assert_eq!(stco_entries(payload(&data, stco)), vec![(mo + 16) as u32]);
    // stts run-length 合并：10,10,20 → [(2,10),(1,20)]。
    let stts = find(&stbl.children, "stts").unwrap();
    assert_eq!(stts_entries(payload(&data, stts)), vec![(2, 10), (1, 20)]);
    // 全关键帧 ⇒ 无 stss；pts 全等 dts ⇒ 无 ctts。
    assert!(find(&stbl.children, "stss").is_none());
    assert!(find(&stbl.children, "ctts").is_none());
    // stsz 变长（4/5/6 不等）。
    let stsz = find(&stbl.children, "stsz").unwrap();
    assert_eq!(stsz_table(payload(&data, stsz)), (0, 3, vec![4, 5, 6]));

    // 读回：dts/duration/数据逐包一致。
    assert_eq!(r.sample_count(0).unwrap(), 3);
    let p0 = r.read_packet(0, 0).unwrap();
    assert_eq!((p0.dts, p0.duration), (t(0), t(70_560)));
    assert_eq!(p0.data, vec![0xA0, 0xA1, 0xA2, 0xA3]);
    let p2 = r.read_packet(0, 2).unwrap();
    assert_eq!((p2.dts, p2.duration), (t(141_120), t(141_120)));
    assert_eq!(p2.data, (0xA9..=0xAE).collect::<Vec<u8>>());
}

/// free_size=0（默认）⇒ 不写 free，顶层即 ftyp → mdat → moov。
#[test]
fn zero_free_size_omits_free_box() {
    let data = mux(
        Mp4WriterOptions {
            ..Default::default()
        },
        vec![video_cfg(1000)],
        vec![pkt(0, 0, 0, 70_560, true, &[1, 2, 3])],
    )
    .unwrap();
    let r = Mp4Reader::from_vec(data).unwrap();
    let kinds: Vec<String> = r.box_tree().iter().map(|n| n.info.kind.as_str()).collect();
    assert_eq!(kinds, ["ftyp", "mdat", "moov"]);
}

// —— ② avc1 extra_data 原样回写 ——

#[test]
fn avc1_extra_data_roundtrip() {
    let data = mux(
        Mp4WriterOptions {
            ..Default::default()
        },
        vec![video_cfg(1000)],
        vec![pkt(0, 0, 0, 70_560, true, &[0xAA; 8])],
    )
    .unwrap();
    let mut r = Mp4Reader::from_vec(data).unwrap();
    let s = r.stream(0).unwrap();
    assert_eq!(s.kind, MediaKind::Video);
    assert_eq!(s.codec.as_str(), "avc1");
    assert_eq!(s.timebase, Rational::new(1, 1000).unwrap());
    assert_eq!(
        s.video,
        Some(VideoInfo {
            width: 64,
            height: 96
        })
    );
    // 原样回写：逐字节相等。
    assert_eq!(s.extra_data, AVCC.to_vec());
    assert_eq!(r.read_packet(0, 0).unwrap().data, vec![0xAA; 8]);
}

// —— ③ mp4a/esds 原样回写 ——
//
// 注：writer 按 ISO 14496-12 写 28 字节 AudioSampleEntry（声道在 +16、
// 采样率在 +24）；读取端 extract_entry_fields 现按 20 字节旧布局解析
// （声道在 +8、采样率在 +16），故本测试的声道/采样率/extra_data 断言在
// 读取端修复前为预期失败（与 oracle_mux 同根因，修法见 contracts.md
// Mp4Writer 节与交付 notes）。
#[test]
fn mp4a_esds_roundtrip() {
    // tb=1/48000 ⇒ 1 单位 = 147 tick。
    let data = mux(
        Mp4WriterOptions {
            ..Default::default()
        },
        vec![audio_cfg(48_000)],
        vec![
            pkt(0, 0, 0, 147, true, &[0xC0, 0xC1]),
            pkt(0, 1, 147, 147, true, &[0xC2, 0xC3]),
        ],
    )
    .unwrap();
    let mut r = Mp4Reader::from_vec(data).unwrap();
    let s = r.stream(0).unwrap();
    assert_eq!(s.kind, MediaKind::Audio);
    assert_eq!(s.codec.as_str(), "mp4a");
    assert_eq!(s.timebase, Rational::new(1, 48_000).unwrap());
    assert_eq!(
        s.audio,
        Some(AudioInfo {
            channel_count: 2,
            sample_rate: 48_000
        })
    );
    // esds 载荷（含 4 字节版本/标志）原样回写。
    assert_eq!(s.extra_data, ESDS.to_vec());
    // 音频轨无 stss（恒关键帧）。
    let stbl = stbl_of(&r, 0);
    assert!(find(&stbl.children, "stss").is_none());
    let p1 = r.read_packet(0, 1).unwrap();
    assert_eq!((p1.dts, p1.duration), (t(147), t(147)));
    assert!(p1.keyframe);
}

// —— ④ 音视频交错：同流连续样例合一 chunk，stsc 段最小 ——

#[test]
fn interleaved_av_chunks_minimal_stsc() {
    // 写序：v0 v1 | a0 a1 | v2 | a2 —— 同流相邻包在文件内连续 ⇒ 各合成
    // 一个 chunk；v2/a2 单样例 chunk。
    let v = video_cfg(1000);
    let a = audio_cfg(1000);
    let u = 70_560i64; // 1 单位 @1/1000
    let packets = vec![
        pkt(0, 0, 0, u, true, &[0xA0, 0xA1]),
        pkt(0, 1, u, u, false, &[0xA2, 0xA3]),
        pkt(1, 0, 0, u, true, &[0xC0]),
        pkt(1, 1, u, u, true, &[0xC1]),
        pkt(0, 2, 2 * u, u, false, &[0xA4, 0xA5]),
        pkt(1, 2, 2 * u, u, true, &[0xC2]),
    ];
    let data = mux(
        Mp4WriterOptions {
            ..Default::default()
        },
        vec![v, a],
        packets,
    )
    .unwrap();
    let mut r = Mp4Reader::from_vec(data.clone()).unwrap();
    assert_eq!(r.streams().len(), 2);

    // 两轨各两个 chunk：stsc 均为 [(1,2),(2,1)]，段最小。
    for trak_i in 0..2 {
        let stbl = stbl_of(&r, trak_i);
        let stsc = find(&stbl.children, "stsc").unwrap();
        assert_eq!(
            stsc_entries(payload(&data, stsc)),
            vec![(1, 2, 1), (2, 1, 1)],
            "trak {trak_i}"
        );
        let stco = find(&stbl.children, "stco").unwrap();
        assert_eq!(stco_entries(payload(&data, stco)).len(), 2, "trak {trak_i}");
    }

    // 交错读回：数据逐包一致。
    let expect = [
        (0usize, vec![0xA0, 0xA1]),
        (0, vec![0xA2, 0xA3]),
        (1, vec![0xC0]),
        (1, vec![0xC1]),
        (0, vec![0xA4, 0xA5]),
        (1, vec![0xC2]),
    ];
    // read_packet 按轨内样例序读：先按轨收齐再比对。
    let mut got = Vec::new();
    for si in 0..2 {
        for ii in 0..r.sample_count(si).unwrap() {
            let p = r.read_packet(si, ii).unwrap();
            got.push((p.stream_index, p.data));
        }
    }
    let mut want = expect.to_vec();
    got.sort_by_key(|(si, _)| *si);
    want.sort_by_key(|(si, _)| *si);
    assert_eq!(got, want);
}

// —— ⑤ pts≠dts 生成 ctts（v1，含负偏移）；全等则无 ctts ——

#[test]
fn ctts_v1_written_only_when_pts_shifts() {
    let u = 70_560i64;
    let mut packets = vec![
        pkt(0, 0, 0, u, true, &[0xA0]),
        pkt(0, 1, u, u, false, &[0xA1]),
        pkt(0, 2, 2 * u, u, false, &[0xA2]),
    ];
    // 偏移（单位域）：+1 / 0 / −1（负 pts−dts 合法）。
    packets[0].pts = Some(t(u));
    packets[2].pts = Some(t(2 * u - u));
    let data = mux(
        Mp4WriterOptions {
            ..Default::default()
        },
        vec![video_cfg(1000)],
        packets,
    )
    .unwrap();

    let mut r = Mp4Reader::from_vec(data.clone()).unwrap();
    let stbl = stbl_of(&r, 0);
    let ctts = find(&stbl.children, "ctts").unwrap();
    let (version, entries) = ctts_entries(payload(&data, ctts));
    assert_eq!(version, 1, "ctts 须 v1（有符号偏移）");
    // 偏移为轨道时基单位：u=70_560 tick = 10 单位。
    assert_eq!(entries, vec![(1, 10), (1, 0), (1, -10)]);
    // 读回 pts：dts + 偏移。
    let p0 = r.read_packet(0, 0).unwrap();
    assert_eq!(p0.pts, Some(t(u)));
    let p1 = r.read_packet(0, 1).unwrap();
    assert_eq!(p1.pts, Some(t(u))); // ctts 存在 ⇒ pts 显式 == dts
    let p2 = r.read_packet(0, 2).unwrap();
    assert_eq!(p2.pts, Some(t(u))); // dts 2u + (−1u)

    // 对照组：pts 全等 dts（None）⇒ box 树无 ctts，读回 pts 全 None。
    let plain = mux(
        Mp4WriterOptions {
            ..Default::default()
        },
        vec![video_cfg(1000)],
        vec![pkt(0, 0, 0, u, true, &[0]), pkt(0, 1, u, u, false, &[1])],
    )
    .unwrap();
    let mut r2 = Mp4Reader::from_vec(plain).unwrap();
    assert!(find(&stbl_of(&r2, 0).children, "ctts").is_none());
    assert_eq!(r2.read_packet(0, 0).unwrap().pts, None);
}

// —— ⑥ stss：部分关键帧才写；全关键帧/音频轨不写 ——

#[test]
fn stss_partial_keyframes_only() {
    let u = 70_560i64;
    let packets = vec![
        pkt(0, 0, 0, u, true, &[0xA0]),
        pkt(0, 1, u, u, false, &[0xA1]),
        pkt(0, 2, 2 * u, u, true, &[0xA2]),
    ];
    let data = mux(
        Mp4WriterOptions {
            ..Default::default()
        },
        vec![video_cfg(1000)],
        packets,
    )
    .unwrap();
    let mut r = Mp4Reader::from_vec(data.clone()).unwrap();
    let stbl = stbl_of(&r, 0);
    let stss = find(&stbl.children, "stss").unwrap();
    // 1-based 样号，仅关键帧样例。
    assert_eq!(stss_entries(payload(&data, stss)), vec![1, 3]);
    assert!(!r.read_packet(0, 1).unwrap().keyframe);
    assert!(r.read_packet(0, 2).unwrap().keyframe);

    // 全关键帧视频轨 ⇒ 无 stss（缺省语义即全关键帧）。
    let all_key = mux(
        Mp4WriterOptions {
            ..Default::default()
        },
        vec![video_cfg(1000)],
        vec![pkt(0, 0, 0, u, true, &[0]), pkt(0, 1, u, u, true, &[1])],
    )
    .unwrap();
    let mut r2 = Mp4Reader::from_vec(all_key).unwrap();
    assert!(find(&stbl_of(&r2, 0).children, "stss").is_none());
    assert!(r2.read_packet(0, 0).unwrap().keyframe);

    // 音频轨即使 keyframe=false 也不写 stss，读回恒关键帧。
    let audio = mux(
        Mp4WriterOptions {
            ..Default::default()
        },
        vec![audio_cfg(1000)],
        vec![
            pkt(0, 0, 0, u, false, &[0xC0]),
            pkt(0, 1, u, u, false, &[0xC1]),
        ],
    )
    .unwrap();
    let mut r3 = Mp4Reader::from_vec(audio).unwrap();
    assert!(find(&stbl_of(&r3, 0).children, "stss").is_none());
    assert!(r3.read_packet(0, 0).unwrap().keyframe, "音频恒关键帧");
}

// —— ⑦ force_co64 出 co64 ——

#[test]
fn force_co64_writes_co64() {
    let data = mux(
        Mp4WriterOptions {
            force_co64: true,
            ..Default::default()
        },
        vec![video_cfg(1000)],
        vec![
            pkt(0, 0, 0, 70_560, true, &[0xA0, 0xA1]),
            pkt(0, 1, 70_560, 70_560, false, &[0xA2, 0xA3]),
        ],
    )
    .unwrap();
    let mut r = Mp4Reader::from_vec(data.clone()).unwrap();
    let stbl = stbl_of(&r, 0);
    assert!(find(&stbl.children, "co64").is_some());
    assert!(find(&stbl.children, "stco").is_none());
    let co64 = find(&stbl.children, "co64").unwrap();
    // 单 chunk（同流连续），偏移 = mdat 载荷起点。
    let mdat = find(r.box_tree(), "mdat").unwrap();
    assert_eq!(
        co64_entries(payload(&data, co64)),
        vec![mdat.info.offset + 16]
    );
    // co64 不影响数据读取。
    assert_eq!(r.read_packet(0, 1).unwrap().data, vec![0xA2, 0xA3]);
}

// —— ⑧ Misuse / Unsupported / InvalidStreamIndex 全清单 ——

fn misuse(err: WriterError, what: &str) {
    match &err {
        WriterError::Misuse(msg) => assert!(
            msg.contains(what),
            "Misuse 文案应含「{what}」，实为「{msg}」"
        ),
        other => panic!("应为 Misuse，实为 {other:?}"),
    }
}

#[test]
fn misuse_timebase_num_not_one() {
    let mut cfg = video_cfg(1000);
    // Rational 构造即约分：30/1001 的 num=30 ≠ 1。
    cfg.timebase = Rational::new(30, 1001).unwrap();
    let mut w = Mp4Writer::new(
        Cursor::new(Vec::new()),
        Mp4WriterOptions {
            ..Default::default()
        },
    );
    misuse(w.add_stream(cfg).unwrap_err(), "num==1");
}

#[test]
fn unsupported_other_kind_and_codec() {
    let mut w = Mp4Writer::new(
        Cursor::new(Vec::new()),
        Mp4WriterOptions {
            ..Default::default()
        },
    );
    // kind=Other → Unsupported。
    let mut cfg = video_cfg(1000);
    cfg.kind = MediaKind::Other;
    match w.add_stream(cfg).unwrap_err() {
        WriterError::Unsupported(what) => assert_eq!(what, "仅支持视频/音频轨（vide/soun）"),
        other => panic!("应为 Unsupported，实为 {other:?}"),
    }
    // Video + 非 avc1 → Unsupported。
    let mut cfg = video_cfg(1000);
    cfg.codec = FourCc::new(*b"hev1");
    match w.add_stream(cfg).unwrap_err() {
        WriterError::Unsupported(what) => assert_eq!(what, "仅支持 avc1（视频）与 mp4a（音频）"),
        other => panic!("应为 Unsupported，实为 {other:?}"),
    }
    // Audio + 非 mp4a → Unsupported。
    let mut cfg = audio_cfg(48_000);
    cfg.codec = FourCc::new(*b"ac-3");
    match w.add_stream(cfg).unwrap_err() {
        WriterError::Unsupported(what) => assert_eq!(what, "仅支持 avc1（视频）与 mp4a（音频）"),
        other => panic!("应为 Unsupported，实为 {other:?}"),
    }
}

#[test]
fn misuse_kind_info_mismatch() {
    let mut w = Mp4Writer::new(
        Cursor::new(Vec::new()),
        Mp4WriterOptions {
            ..Default::default()
        },
    );
    // Video 缺 video 信息。
    let mut cfg = video_cfg(1000);
    cfg.video = None;
    misuse(w.add_stream(cfg).unwrap_err(), "video 信息");
    // Video 多带 audio 信息。
    let mut cfg = video_cfg(1000);
    cfg.audio = Some(AudioInfo {
        channel_count: 2,
        sample_rate: 48_000,
    });
    misuse(w.add_stream(cfg).unwrap_err(), "audio 信息");
    // Audio 缺 audio 信息。
    let mut cfg = audio_cfg(48_000);
    cfg.audio = None;
    misuse(w.add_stream(cfg).unwrap_err(), "audio 信息");
    // Audio 多带 video 信息。
    let mut cfg = audio_cfg(48_000);
    cfg.video = Some(VideoInfo {
        width: 64,
        height: 96,
    });
    misuse(w.add_stream(cfg).unwrap_err(), "video 信息");
}

#[test]
fn misuse_add_stream_after_first_packet() {
    let mut w = Mp4Writer::new(
        Cursor::new(Vec::new()),
        Mp4WriterOptions {
            ..Default::default()
        },
    );
    w.add_stream(video_cfg(1000)).unwrap();
    w.write_packet(&pkt(0, 0, 0, 70_560, true, &[0])).unwrap();
    misuse(w.add_stream(video_cfg(1000)).unwrap_err(), "首包已写");
}

#[test]
fn misuse_dts_regression_gap_and_overlap() {
    let u = 70_560i64;
    // 回退：dts[2] < dts[1]（首包 dts 恒 0，回退只能出现在第 2 包起）。
    let mut w = Mp4Writer::new(
        Cursor::new(Vec::new()),
        Mp4WriterOptions {
            ..Default::default()
        },
    );
    w.add_stream(video_cfg(1000)).unwrap();
    w.write_packet(&pkt(0, 0, 0, u, true, &[0])).unwrap();
    w.write_packet(&pkt(0, 1, u, u, true, &[1])).unwrap();
    misuse(
        w.write_packet(&pkt(0, 2, u / 2, u, true, &[2]))
            .unwrap_err(),
        "单调不减",
    );
    // 间隙：dts[1] > dts[0]+dur[0]。
    let mut w = Mp4Writer::new(
        Cursor::new(Vec::new()),
        Mp4WriterOptions {
            ..Default::default()
        },
    );
    w.add_stream(video_cfg(1000)).unwrap();
    w.write_packet(&pkt(0, 0, 0, u, true, &[0])).unwrap();
    misuse(
        w.write_packet(&pkt(0, 1, 2 * u, u, true, &[1]))
            .unwrap_err(),
        "间隙/重叠",
    );
    // 重叠：dts[0] < dts[1] < dts[0]+dur[0]（同一报错路径）。
    let mut w = Mp4Writer::new(
        Cursor::new(Vec::new()),
        Mp4WriterOptions {
            ..Default::default()
        },
    );
    w.add_stream(video_cfg(1000)).unwrap();
    w.write_packet(&pkt(0, 0, 0, u, true, &[0])).unwrap();
    misuse(
        w.write_packet(&pkt(0, 1, u / 2, u, true, &[1]))
            .unwrap_err(),
        "间隙/重叠",
    );
}

#[test]
fn misuse_negative_dts_and_duration() {
    let mut w = Mp4Writer::new(
        Cursor::new(Vec::new()),
        Mp4WriterOptions {
            ..Default::default()
        },
    );
    w.add_stream(video_cfg(1000)).unwrap();
    // 固定文案（契约）：负 dts。
    let err = w
        .write_packet(&pkt(0, 0, -147, 70_560, true, &[0]))
        .unwrap_err();
    match err {
        WriterError::Misuse(msg) => assert_eq!(
            msg,
            "负 dts 不可表达（stts 0 起/tfdt u64），请调用方整体平移"
        ),
        other => panic!("应为 Misuse，实为 {other:?}"),
    }
    // 负 duration。
    misuse(
        w.write_packet(&pkt(0, 0, 0, -1, true, &[0])).unwrap_err(),
        "负",
    );
}

#[test]
fn misuse_first_dts_nonzero() {
    // stts 隐式从 0 起算，非零首偏移无 elst 不可表达（契约补充规则）。
    let mut w = Mp4Writer::new(
        Cursor::new(Vec::new()),
        Mp4WriterOptions {
            ..Default::default()
        },
    );
    w.add_stream(video_cfg(1000)).unwrap();
    misuse(
        w.write_packet(&pkt(0, 0, 70_560, 70_560, true, &[0]))
            .unwrap_err(),
        "首包 dts 须为 0",
    );
}

#[test]
fn misuse_invalid_stream_index() {
    let mut w = Mp4Writer::new(
        Cursor::new(Vec::new()),
        Mp4WriterOptions {
            ..Default::default()
        },
    );
    w.add_stream(video_cfg(1000)).unwrap();
    match w
        .write_packet(&pkt(5, 0, 0, 70_560, true, &[0]))
        .unwrap_err()
    {
        WriterError::InvalidStreamIndex { index, count } => assert_eq!((index, count), (5, 1)),
        other => panic!("应为 InvalidStreamIndex，实为 {other:?}"),
    }
}

#[test]
fn misuse_finalize_without_streams() {
    let w = Mp4Writer::new(
        Cursor::new(Vec::new()),
        Mp4WriterOptions {
            ..Default::default()
        },
    );
    misuse(w.finalize().unwrap_err(), "无流");
}

// —— ⑨ 0 包轨合法 finalize ——

#[test]
fn zero_packet_tracks_finalize() {
    let data = mux(
        Mp4WriterOptions {
            ..Default::default()
        },
        vec![video_cfg(1000), audio_cfg(48_000)],
        vec![],
    )
    .unwrap();
    let r = Mp4Reader::from_vec(data).unwrap();
    assert_eq!(r.streams().len(), 2);
    assert_eq!(r.sample_count(0).unwrap(), 0);
    assert_eq!(r.sample_count(1).unwrap(), 0);
    // 0 包轨照常有完整样例表（空表），box 树可解析。
    let stbl = stbl_of(&r, 0);
    for kind in ["stsd", "stts", "stsc", "stsz", "stco"] {
        assert!(find(&stbl.children, kind).is_some(), "缺 {kind}");
    }
}

// —— ⑩ 时基换算：1/90000（非整除，舍入路径）与 1/1000（整除，精确往返）——

/// 1/90000：7,056,000/90,000 = 78.4 tick/单位，非整除。
/// 单位可表示的 tick（784 的倍数）逐包精确往返；非表示值（785）在
/// 单位域舍入（to_units(785)=10 单位），读回 tick 落到 784——单位域
/// 严格一致、tick 域按舍入规则取整（见 contracts.md 时戳换算约定）。
#[test]
fn timebase_90000_rounding_path() {
    let data = mux(
        Mp4WriterOptions {
            ..Default::default()
        },
        vec![video_cfg(90_000)],
        vec![
            pkt(0, 0, 0, 784, true, &[0xA0]),
            pkt(0, 1, 784, 784, false, &[0xA1]),
            pkt(0, 2, 1568, 785, false, &[0xA2]), // 尾包 785 tick → 10 单位
        ],
    )
    .unwrap();
    let mut r = Mp4Reader::from_vec(data).unwrap();
    let tb = Rational::new(1, 90_000).unwrap();
    assert_eq!(r.stream(0).unwrap().timebase, tb);
    // 784 tick = 10 单位（精确），dts 逐包 tick 精确。
    let p0 = r.read_packet(0, 0).unwrap();
    assert_eq!(p0.dts, t(0));
    let p1 = r.read_packet(0, 1).unwrap();
    assert_eq!(p1.dts, t(784));
    // 尾包：入表 delta = to_units(785) = 10 单位 ⇒ 读回 duration 784 tick，
    // 单位域与调用侧一致（10）。
    let p2 = r.read_packet(0, 2).unwrap();
    assert_eq!(p2.dts, t(1568));
    assert_eq!(p2.duration, t(784), "785 tick 舍入到 10 单位 = 784 tick");
    assert_eq!(p2.duration.to_units(tb), Some(10), "单位域严格一致");
    assert_eq!(t(785).to_units(tb), Some(10));
}

/// 1/1000：7,056,000/1000 = 7,056 tick/单位，整除 ⇒ 单位↔tick 双向精确，
/// 任意单位可表示的 tick 值逐包严格往返（tick 域断言）。
#[test]
fn timebase_1000_exact_tick_roundtrip() {
    let durs = [70_560i64, 35_280, 105_840]; // 10/5/15 单位
    let mut packets = Vec::new();
    let mut dts = 0i64;
    for (i, &d) in durs.iter().enumerate() {
        packets.push(pkt(0, i, dts, d, i == 0, &[0xA0 + i as u8]));
        dts += d;
    }
    // 加一档 pts 偏移（+3 单位）验证 ctts 域同样精确。
    packets[1].pts = Some(t(70_560 + 3 * 7_056));
    let data = mux(
        Mp4WriterOptions {
            ..Default::default()
        },
        vec![video_cfg(1000)],
        packets,
    )
    .unwrap();
    let mut r = Mp4Reader::from_vec(data).unwrap();
    let mut dts = 0i64;
    for (i, &d) in durs.iter().enumerate() {
        let p = r.read_packet(0, i).unwrap();
        assert_eq!(p.dts, t(dts), "tick 域严格往返（样例 {i}）");
        assert_eq!(p.duration, t(d), "tick 域严格往返（样例 {i}）");
        if i == 1 {
            assert_eq!(p.pts, Some(t(70_560 + 3 * 7_056)));
        }
        dts += d;
    }
}

/// duration==0 合法：写 delta 0，dts 停留（同 dts 连包）。
#[test]
fn zero_duration_packet_is_legal() {
    let data = mux(
        Mp4WriterOptions {
            ..Default::default()
        },
        vec![video_cfg(1000)],
        vec![
            pkt(0, 0, 0, 0, true, &[0xA0]),
            pkt(0, 1, 0, 70_560, false, &[0xA1]),
        ],
    )
    .unwrap();
    let mut r = Mp4Reader::from_vec(data.clone()).unwrap();
    let stbl = stbl_of(&r, 0);
    let stts = find(&stbl.children, "stts").unwrap();
    // 70_560 tick @1/1000 = 10 单位。
    assert_eq!(stts_entries(payload(&data, stts)), vec![(1, 0), (1, 10)]);
    let p0 = r.read_packet(0, 0).unwrap();
    assert_eq!((p0.dts, p0.duration), (t(0), t(0)));
}

// —— ⑪ 评审回归 ——

/// 整轨全空包（data 为空，契约未禁止）：stsz 不得走定长分支——
/// sample_size=0 是变长表标记（读取端以 fixed!=0 判定），定长写出缺
/// 逐样例条目会产出损坏文件（回归：自有 reader 曾报「stsz 样例大小表
/// 截断」打不开、ffmpeg 把 stco 字节当样例大小误解析）。
#[test]
fn all_empty_packets_writeread_roundtrip() {
    let data = mux(
        Mp4WriterOptions {
            ..Default::default()
        },
        vec![video_cfg(1000)],
        vec![
            pkt(0, 0, 0, 70_560, true, &[]),
            pkt(0, 1, 70_560, 70_560, false, &[]),
        ],
    )
    .unwrap();
    let mut r = Mp4Reader::from_vec(data.clone()).unwrap();
    assert_eq!(r.sample_count(0).unwrap(), 2);
    // stsz 须为变长表（sample_size=0）且带逐样例条目 [0, 0]。
    let stbl = stbl_of(&r, 0);
    let stsz = find(&stbl.children, "stsz").unwrap();
    assert_eq!(stsz_table(payload(&data, stsz)), (0, 2, vec![0, 0]));
    // 逐样例可读回：空数据 + 时戳往返。
    let p0 = r.read_packet(0, 0).unwrap();
    assert_eq!(p0.data, Vec::<u8>::new());
    assert_eq!(p0.duration, t(70_560));
    let p1 = r.read_packet(0, 1).unwrap();
    assert_eq!(p1.data, Vec::<u8>::new());
    assert_eq!(p1.dts, t(70_560));
}

/// free_size 接近 u64::MAX：new 不得 panic（debug 下 8+free_size 加法
/// 溢出）/ release 不得静默回绕错位布局，首个写出点须 Misuse 拒写
/// 且不落半字节数据（回归：未检算术）。
#[test]
fn free_size_overflow_rejected_as_misuse() {
    let opts = Mp4WriterOptions {
        layout: Layout::Progressive,
        free_size: u64::MAX - 4, // 8 + free_size 回绕成 7
        ..Default::default()
    };
    // 修复点一：new 本体不得 panic。
    let mut w = Mp4Writer::new(Cursor::new(Vec::new()), opts.clone());
    w.add_stream(video_cfg(1000)).unwrap();
    // 修复点二：首个写出点（write_packet → ensure_prologue）Misuse 拒写。
    match w.write_packet(&pkt(0, 0, 0, 70_560, true, &[0xA0])) {
        Err(WriterError::Misuse(what)) => assert!(what.contains("free_size"), "what={what}"),
        other => panic!("应为 Misuse，实为 {other:?}"),
    }
    // finalize 路径（0 包轨）同样在 ensure_prologue 拒绝，不产出半成品。
    let mut w2 = Mp4Writer::new(Cursor::new(Vec::new()), opts);
    w2.add_stream(video_cfg(1000)).unwrap();
    match w2.finalize() {
        Err(WriterError::Misuse(what)) => assert!(what.contains("free_size"), "what={what}"),
        other => panic!("应为 Misuse，实为 {other:?}"),
    }
}
