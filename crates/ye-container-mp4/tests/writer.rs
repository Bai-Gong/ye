//! Mp4Writer 单元测试：写 `Cursor<Vec<u8>>`，用 [`Mp4Reader`] 读回断言
//! + 定点字节断言。自包含（不依赖 helpers/）。
//!
//! 移植后语义：包时戳自带时基（自描述 `Timestamp`）。本文件主路径用
//! 轨道时基原生单位（1/1000 下 1 单位即 1 tick；1/48000 同理）——重标度
//! 恒等、逐包精确；另设跨时基重标度专组（NearInf 舍入路径）。

use std::io::Cursor;
use std::sync::Arc;
use ye_container_mp4::{
    AudioParams, CodecId, Layout, MediaType, Mp4Reader, Mp4Writer, Mp4WriterOptions, Packet,
    PacketFlags, Rational, Timestamp, TrackConfig, VideoParams, WriterError,
};
use ye_pixfmt::ChannelLayout;

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

/// 指定时基下的时间戳。
fn ts(units: i64, tb_den: i32) -> Timestamp {
    Timestamp::new(units, Rational::new(1, tb_den))
}

/// 视频轨时基（1/1000）便捷构造。
fn t(units: i64) -> Timestamp {
    ts(units, 1000)
}

fn video_cfg(tb_den: i32) -> TrackConfig {
    TrackConfig {
        media_type: MediaType::Video,
        codec: CodecId::H264,
        timebase: Rational::new(1, tb_den),
        video: Some(VideoParams {
            width: 64,
            height: 96,
            pixel_format: None,
            frame_rate: None,
        }),
        audio: None,
        extra_data: AVCC.to_vec(),
    }
}

fn audio_cfg(tb_den: i32) -> TrackConfig {
    TrackConfig {
        media_type: MediaType::Audio,
        codec: CodecId::Aac,
        timebase: Rational::new(1, tb_den),
        video: None,
        audio: Some(AudioParams {
            sample_rate: 48_000,
            channel_layout: Some(ChannelLayout::STEREO),
            sample_format: None,
        }),
        extra_data: ESDS.to_vec(),
    }
}

/// 轨道时基原生单位的包（主路径：重标度恒等）。
fn pkt(si: usize, dts_units: i64, dur_units: i64, key: bool, data: &[u8]) -> Packet {
    pkt_tb(si, dts_units, dur_units, key, data, 1000)
}

/// 指定包时基的包（跨时基重标度组用）。
fn pkt_tb(
    si: usize,
    dts_units: i64,
    dur_units: i64,
    key: bool,
    data: &[u8],
    tb_den: i32,
) -> Packet {
    let tb = Rational::new(1, tb_den);
    let mut flags = PacketFlags::empty();
    if key {
        flags.insert(PacketFlags::KEYFRAME);
    }
    Packet {
        data: Arc::from(data.to_vec()),
        stream_index: si as u32,
        pts: None,
        dts: Some(Timestamp::new(dts_units, tb)),
        duration: Some(Timestamp::new(dur_units, tb)),
        flags,
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

fn find<'a>(
    nodes: &'a [ye_container_mp4::BoxNode],
    kind: &str,
) -> Option<&'a ye_container_mp4::BoxNode> {
    nodes.iter().find(|n| n.info.kind.as_str() == kind)
}

/// 第 trak_i 路轨的 stbl 节点。
fn stbl_of(r: &Mp4Reader<Cursor<Vec<u8>>>, trak_i: usize) -> &ye_container_mp4::BoxNode {
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
fn payload<'a>(data: &'a [u8], node: &ye_container_mp4::BoxNode) -> &'a [u8] {
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

fn is_key(p: &Packet) -> bool {
    p.flags.contains(PacketFlags::KEYFRAME)
}

// —— ① 单视频轨黄金：布局 + stco 回填 + stts run-length 合并 ——

#[test]
fn golden_layout_ftyp_free_mdat_moov() {
    let opts = Mp4WriterOptions {
        free_size: 16,
        ..Default::default()
    };
    // 3 样例：durations 10/10/20 单位（tb=1/1000），数据 4+5+6=15 字节，
    // 全关键帧。
    let data = mux(
        opts,
        vec![video_cfg(1000)],
        vec![
            pkt(0, 0, 10, true, &[0xA0, 0xA1, 0xA2, 0xA3]),
            pkt(0, 10, 10, true, &[0xA4, 0xA5, 0xA6, 0xA7, 0xA8]),
            pkt(0, 20, 20, true, &[0xA9, 0xAA, 0xAB, 0xAC, 0xAD, 0xAE]),
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

    // 读回：dts/duration/数据逐包一致（流时基原生单位）。
    assert_eq!(r.sample_count(0).unwrap(), 3);
    let p0 = r.read_packet(0, 0).unwrap();
    assert_eq!((p0.dts, p0.duration), (Some(t(0)), Some(t(10))));
    assert_eq!(&*p0.data, &[0xA0, 0xA1, 0xA2, 0xA3]);
    let p2 = r.read_packet(0, 2).unwrap();
    assert_eq!((p2.dts, p2.duration), (Some(t(20)), Some(t(20))));
    assert_eq!(&*p2.data, &(0xA9..=0xAE).collect::<Vec<u8>>());
}

/// free_size=0（默认）⇒ 不写 free，顶层即 ftyp → mdat → moov。
#[test]
fn zero_free_size_omits_free_box() {
    let data = mux(
        Mp4WriterOptions {
            ..Default::default()
        },
        vec![video_cfg(1000)],
        vec![pkt(0, 0, 10, true, &[1, 2, 3])],
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
        vec![pkt(0, 0, 10, true, &[0xAA; 8])],
    )
    .unwrap();
    let mut r = Mp4Reader::from_vec(data).unwrap();
    let s = r.stream(0).unwrap();
    assert_eq!(s.media_type, MediaType::Video);
    assert_eq!(s.codec.codec, CodecId::H264);
    assert_eq!(r.track_info(0).unwrap().codec.as_str(), "avc1");
    assert_eq!(s.time_base, Rational::new(1, 1000));
    assert_eq!(
        s.codec.video,
        Some(VideoParams {
            width: 64,
            height: 96,
            pixel_format: None,
            frame_rate: None,
        })
    );
    // 原样回写：逐字节相等。
    assert_eq!(s.codec.extradata, Some(AVCC.to_vec()));
    assert_eq!(&*r.read_packet(0, 0).unwrap().data, &vec![0xAA; 8]);
}

// —— ③ mp4a/esds 原样回写 ——

#[test]
fn mp4a_esds_roundtrip() {
    // tb=1/48000，包时戳即音频原生单位。
    let data = mux(
        Mp4WriterOptions {
            ..Default::default()
        },
        vec![audio_cfg(48_000)],
        vec![
            pkt_tb(0, 0, 1, true, &[0xC0, 0xC1], 48_000),
            pkt_tb(0, 1, 1, true, &[0xC2, 0xC3], 48_000),
        ],
    )
    .unwrap();
    let mut r = Mp4Reader::from_vec(data).unwrap();
    let s = r.stream(0).unwrap();
    assert_eq!(s.media_type, MediaType::Audio);
    assert_eq!(s.codec.codec, CodecId::Aac);
    assert_eq!(r.track_info(0).unwrap().codec.as_str(), "mp4a");
    assert_eq!(s.time_base, Rational::new(1, 48_000));
    assert_eq!(
        s.codec.audio,
        Some(AudioParams {
            sample_rate: 48_000,
            channel_layout: Some(ChannelLayout::STEREO),
            sample_format: None,
        })
    );
    // esds 载荷（含 4 字节版本/标志）原样回写。
    assert_eq!(s.codec.extradata, Some(ESDS.to_vec()));
    // 音频轨无 stss（恒关键帧）。
    let stbl = stbl_of(&r, 0);
    assert!(find(&stbl.children, "stss").is_none());
    let p1 = r.read_packet(0, 1).unwrap();
    let at = |u: i64| Timestamp::new(u, Rational::new(1, 48_000));
    assert_eq!((p1.dts, p1.duration), (Some(at(1)), Some(at(1))));
    assert!(is_key(&p1));
}

// —— ④ 音视频交错：同流连续样例合一 chunk，stsc 段最小 ——

#[test]
fn interleaved_av_chunks_minimal_stsc() {
    // 写序：v0 v1 | a0 a1 | v2 | a2 —— 同流相邻包在文件内连续 ⇒ 各合成
    // 一个 chunk；v2/a2 单样例 chunk。两轨同为 1/1000。
    let v = video_cfg(1000);
    let a = audio_cfg(1000);
    let packets = vec![
        pkt(0, 0, 10, true, &[0xA0, 0xA1]),
        pkt(0, 10, 10, false, &[0xA2, 0xA3]),
        pkt(1, 0, 10, true, &[0xC0]),
        pkt(1, 10, 10, true, &[0xC1]),
        pkt(0, 20, 10, false, &[0xA4, 0xA5]),
        pkt(1, 20, 10, true, &[0xC2]),
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
        (0u32, vec![0xA0, 0xA1]),
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
            got.push((p.stream_index, p.data.to_vec()));
        }
    }
    let mut want = expect;
    got.sort_by_key(|(si, _)| *si);
    want.sort_by_key(|(si, _)| *si);
    assert_eq!(got, want);
}

// —— ⑤ pts≠dts 生成 ctts（v1，含负偏移）；全等则无 ctts ——

#[test]
fn ctts_v1_written_only_when_pts_shifts() {
    let mut packets = vec![
        pkt(0, 0, 10, true, &[0xA0]),
        pkt(0, 10, 10, false, &[0xA1]),
        pkt(0, 20, 10, false, &[0xA2]),
    ];
    // 偏移（轨道单位）：+10 / 0 / −10（负 pts−dts 合法）。
    packets[0].pts = Some(t(10));
    packets[2].pts = Some(t(10));
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
    assert_eq!(entries, vec![(1, 10), (1, 0), (1, -10)]);
    // 读回 pts：dts + 偏移。
    let p0 = r.read_packet(0, 0).unwrap();
    assert_eq!(p0.pts, Some(t(10)));
    let p1 = r.read_packet(0, 1).unwrap();
    assert_eq!(p1.pts, Some(t(10))); // ctts 存在 ⇒ pts 显式 == dts
    let p2 = r.read_packet(0, 2).unwrap();
    assert_eq!(p2.pts, Some(t(10))); // dts 20 + (−10)

    // 对照组：pts 缺省（None）⇒ box 树无 ctts，读回 pts==dts。
    let plain = mux(
        Mp4WriterOptions {
            ..Default::default()
        },
        vec![video_cfg(1000)],
        vec![pkt(0, 0, 10, true, &[0]), pkt(0, 10, 10, false, &[1])],
    )
    .unwrap();
    let mut r2 = Mp4Reader::from_vec(plain).unwrap();
    assert!(find(&stbl_of(&r2, 0).children, "ctts").is_none());
    assert_eq!(r2.read_packet(0, 0).unwrap().pts, Some(t(0)));
}

// —— ⑥ stss：部分关键帧才写；全关键帧/音频轨不写 ——

#[test]
fn stss_partial_keyframes_only() {
    let packets = vec![
        pkt(0, 0, 10, true, &[0xA0]),
        pkt(0, 10, 10, false, &[0xA1]),
        pkt(0, 20, 10, true, &[0xA2]),
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
    assert!(!is_key(&r.read_packet(0, 1).unwrap()));
    assert!(is_key(&r.read_packet(0, 2).unwrap()));

    // 全关键帧视频轨 ⇒ 无 stss（缺省语义即全关键帧）。
    let all_key = mux(
        Mp4WriterOptions {
            ..Default::default()
        },
        vec![video_cfg(1000)],
        vec![pkt(0, 0, 10, true, &[0]), pkt(0, 10, 10, true, &[1])],
    )
    .unwrap();
    let mut r2 = Mp4Reader::from_vec(all_key).unwrap();
    assert!(find(&stbl_of(&r2, 0).children, "stss").is_none());
    assert!(is_key(&r2.read_packet(0, 0).unwrap()));

    // 音频轨即使 keyframe=false 也不写 stss，读回恒关键帧。
    let audio = mux(
        Mp4WriterOptions {
            ..Default::default()
        },
        vec![audio_cfg(1000)],
        vec![
            pkt(0, 0, 10, false, &[0xC0]),
            pkt(0, 10, 10, false, &[0xC1]),
        ],
    )
    .unwrap();
    let mut r3 = Mp4Reader::from_vec(audio).unwrap();
    assert!(find(&stbl_of(&r3, 0).children, "stss").is_none());
    assert!(is_key(&r3.read_packet(0, 0).unwrap()), "音频恒关键帧");
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
            pkt(0, 0, 10, true, &[0xA0, 0xA1]),
            pkt(0, 10, 10, false, &[0xA2, 0xA3]),
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
    assert_eq!(&*r.read_packet(0, 1).unwrap().data, &[0xA2, 0xA3]);
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
    cfg.timebase = Rational::new(30, 1001);
    let mut w = Mp4Writer::new(
        Cursor::new(Vec::new()),
        Mp4WriterOptions {
            ..Default::default()
        },
    );
    misuse(w.add_stream(cfg).unwrap_err(), "num==1");
}

#[test]
fn unsupported_other_media_type_and_codec() {
    let mut w = Mp4Writer::new(
        Cursor::new(Vec::new()),
        Mp4WriterOptions {
            ..Default::default()
        },
    );
    // media_type=Subtitle（非 vide/soun）→ Unsupported。
    let mut cfg = video_cfg(1000);
    cfg.media_type = MediaType::Subtitle;
    match w.add_stream(cfg).unwrap_err() {
        WriterError::Unsupported(what) => assert_eq!(what, "仅支持视频/音频轨（vide/soun）"),
        other => panic!("应为 Unsupported，实为 {other:?}"),
    }
    // Video + 非 H264（写不出 avc1）→ Unsupported。
    let mut cfg = video_cfg(1000);
    cfg.codec = CodecId::Hevc;
    match w.add_stream(cfg).unwrap_err() {
        WriterError::Unsupported(what) => assert_eq!(what, "仅支持 avc1（视频）与 mp4a（音频）"),
        other => panic!("应为 Unsupported，实为 {other:?}"),
    }
    // Audio + 非 Aac → Unsupported。
    let mut cfg = audio_cfg(48_000);
    cfg.codec = CodecId::Flac;
    match w.add_stream(cfg).unwrap_err() {
        WriterError::Unsupported(what) => assert_eq!(what, "仅支持 avc1（视频）与 mp4a（音频）"),
        other => panic!("应为 Unsupported，实为 {other:?}"),
    }
}

#[test]
fn misuse_media_type_info_mismatch() {
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
    cfg.audio = Some(AudioParams {
        sample_rate: 48_000,
        channel_layout: Some(ChannelLayout::STEREO),
        sample_format: None,
    });
    misuse(w.add_stream(cfg).unwrap_err(), "audio 信息");
    // Audio 缺 audio 信息。
    let mut cfg = audio_cfg(48_000);
    cfg.audio = None;
    misuse(w.add_stream(cfg).unwrap_err(), "audio 信息");
    // Audio 多带 video 信息。
    let mut cfg = audio_cfg(48_000);
    cfg.video = Some(VideoParams {
        width: 64,
        height: 96,
        pixel_format: None,
        frame_rate: None,
    });
    misuse(w.add_stream(cfg).unwrap_err(), "video 信息");
    // Audio 无声道布局（channel_count 无从取）。
    let mut cfg = audio_cfg(48_000);
    cfg.audio = Some(AudioParams {
        sample_rate: 48_000,
        channel_layout: None,
        sample_format: None,
    });
    misuse(w.add_stream(cfg).unwrap_err(), "声道数");
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
    w.write_packet(&pkt(0, 0, 10, true, &[0])).unwrap();
    misuse(w.add_stream(video_cfg(1000)).unwrap_err(), "首包已写");
}

#[test]
fn misuse_missing_dts_or_duration() {
    // 移植新增契约：Packet 缺 dts/duration ⇒ Misuse（M1 不推断时戳）。
    let mut w = Mp4Writer::new(
        Cursor::new(Vec::new()),
        Mp4WriterOptions {
            ..Default::default()
        },
    );
    w.add_stream(video_cfg(1000)).unwrap();
    let mut p = pkt(0, 0, 10, true, &[0]);
    p.dts = None;
    misuse(w.write_packet(&p).unwrap_err(), "缺 dts");
    let mut p = pkt(0, 0, 10, true, &[0]);
    p.duration = None;
    misuse(w.write_packet(&p).unwrap_err(), "缺 duration");
}

#[test]
fn misuse_dts_regression_gap_and_overlap() {
    // 回退：dts[2] < dts[1]（首包 dts 恒 0，回退只能出现在第 2 包起）。
    let mut w = Mp4Writer::new(
        Cursor::new(Vec::new()),
        Mp4WriterOptions {
            ..Default::default()
        },
    );
    w.add_stream(video_cfg(1000)).unwrap();
    w.write_packet(&pkt(0, 0, 10, true, &[0])).unwrap();
    w.write_packet(&pkt(0, 10, 10, true, &[1])).unwrap();
    misuse(
        w.write_packet(&pkt(0, 5, 10, true, &[2])).unwrap_err(),
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
    w.write_packet(&pkt(0, 0, 10, true, &[0])).unwrap();
    misuse(
        w.write_packet(&pkt(0, 20, 10, true, &[1])).unwrap_err(),
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
    w.write_packet(&pkt(0, 0, 10, true, &[0])).unwrap();
    misuse(
        w.write_packet(&pkt(0, 5, 10, true, &[1])).unwrap_err(),
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
    let err = w.write_packet(&pkt(0, -1, 10, true, &[0])).unwrap_err();
    match err {
        WriterError::Misuse(msg) => assert_eq!(
            msg,
            "负 dts 不可表达（stts 0 起/tfdt u64），请调用方整体平移"
        ),
        other => panic!("应为 Misuse，实为 {other:?}"),
    }
    // 负 duration。
    misuse(
        w.write_packet(&pkt(0, 0, -1, true, &[0])).unwrap_err(),
        "负",
    );
}

#[test]
fn misuse_first_dts_nonzero() {
    // stts 隐式从 0 起算，非零首偏移无 elst 不可表达（契约补充规则）。
    // 轨道时基域判定：10 单位 ≠ 0。
    let mut w = Mp4Writer::new(
        Cursor::new(Vec::new()),
        Mp4WriterOptions {
            ..Default::default()
        },
    );
    w.add_stream(video_cfg(1000)).unwrap();
    misuse(
        w.write_packet(&pkt(0, 10, 10, true, &[0])).unwrap_err(),
        "首包 dts 须为 0",
    );
    // 跨时基：包时基 1/90000 的 900 单位（=10ms）重标度后非 0 ⇒ 同样拒绝。
    misuse(
        w.write_packet(&pkt_tb(0, 900, 900, true, &[0], 90_000))
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
    match w.write_packet(&pkt(5, 0, 10, true, &[0])).unwrap_err() {
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

// —— ⑩ 跨时基重标度（合流裁决 #4：封装器向目标流时基重标度，NearInf）——

/// 包时基 ≠ 轨道时基：1/90000 的包写进 1/1000 轨。
/// - 精确可表示值（90000 的倍数 ⇔ 1000 的倍数）：逐包恒等；
/// - 非表示值按 NearInf 舍入：89999 @1/90000 = 0.99998…s → 1000 单位；
///   44999 @1/90000 → 500 单位（44999/90 = 499.98… ⇒ 500）。
#[test]
fn cross_timebase_rescale_near_inf() {
    let data = mux(
        Mp4WriterOptions {
            ..Default::default()
        },
        vec![video_cfg(1000)],
        vec![
            pkt_tb(0, 0, 90_000, true, &[0xA0], 90_000), // dur → 1000 单位
            pkt_tb(0, 90_000, 89_999, false, &[0xA1], 90_000), // dur → NearInf 1000 单位
        ],
    )
    .unwrap();
    let mut r = Mp4Reader::from_vec(data).unwrap();
    let tb = Rational::new(1, 1000);
    assert_eq!(r.stream(0).unwrap().time_base, tb);
    let p0 = r.read_packet(0, 0).unwrap();
    assert_eq!(p0.dts, Some(t(0)));
    assert_eq!(
        p0.duration,
        Some(t(1000)),
        "90_000/90_000 s = 1s = 1000 单位"
    );
    let p1 = r.read_packet(0, 1).unwrap();
    assert_eq!(
        p1.dts,
        Some(t(1000)),
        "衔接校验在单位域：重标度后 90_000→1000"
    );
    assert_eq!(
        p1.duration,
        Some(t(1000)),
        "89_999 @1/90000 → NearInf(999.98…) = 1000 单位"
    );
    // 重标度语义断言（ye-time 同款 NearInf）：
    assert_eq!(
        Timestamp::new(89_999, Rational::new(1, 90_000))
            .rescale(tb)
            .unwrap()
            .ticks(),
        1000
    );
    assert_eq!(
        Timestamp::new(44_999, Rational::new(1, 90_000))
            .rescale(tb)
            .unwrap()
            .ticks(),
        500
    );
    // pts 跨时基同样重标度：cts = rescale(pts) − rescale(dts)。
    let mut p = pkt_tb(0, 90_000, 90_000, true, &[0xA2], 90_000);
    p.pts = Some(Timestamp::new(180_000, Rational::new(1, 90_000)));
    let data2 = mux(
        Mp4WriterOptions {
            ..Default::default()
        },
        vec![video_cfg(1000)],
        vec![pkt_tb(0, 0, 90_000, true, &[0xA0], 90_000), p],
    )
    .unwrap();
    let mut r2 = Mp4Reader::from_vec(data2).unwrap();
    let p1 = r2.read_packet(0, 1).unwrap();
    assert_eq!(p1.dts, Some(t(1000)));
    assert_eq!(p1.pts, Some(t(2000)), "pts 180_000@1/90000 → 2000 单位");
}

/// 间隙判定在重标度后的单位域进行：包时基的微小差距重标度后被抹平
/// （90000 与 89999 同落 1000 单位 ⇒ 按重标度域衔接通过）。
#[test]
fn cross_timebase_gap_check_in_unit_domain() {
    let mut w = Mp4Writer::new(
        Cursor::new(Vec::new()),
        Mp4WriterOptions {
            ..Default::default()
        },
    );
    w.add_stream(video_cfg(1000)).unwrap();
    w.write_packet(&pkt_tb(0, 0, 89_999, true, &[0], 90_000))
        .unwrap();
    // 89_999 → 1000 单位（NearInf 入）；下一包 dts 90_000 → 1000 单位，
    // 与「上一包尾 = 0 + 1000」衔接成立 ⇒ 不报间隙。
    w.write_packet(&pkt_tb(0, 90_000, 90_000, false, &[1], 90_000))
        .unwrap();
    // 真间隙（包时基域也缺一段）：dts 跳到 270_000（=3000 单位）而
    // 期望 2000 ⇒ Misuse。
    misuse(
        w.write_packet(&pkt_tb(0, 270_000, 90_000, false, &[2], 90_000))
            .unwrap_err(),
        "间隙/重叠",
    );
}

/// duration==0 合法：写 delta 0，dts 停留（同 dts 连包）。
#[test]
fn zero_duration_packet_is_legal() {
    let data = mux(
        Mp4WriterOptions {
            ..Default::default()
        },
        vec![video_cfg(1000)],
        vec![pkt(0, 0, 0, true, &[0xA0]), pkt(0, 0, 10, false, &[0xA1])],
    )
    .unwrap();
    let mut r = Mp4Reader::from_vec(data.clone()).unwrap();
    let stbl = stbl_of(&r, 0);
    let stts = find(&stbl.children, "stts").unwrap();
    assert_eq!(stts_entries(payload(&data, stts)), vec![(1, 0), (1, 10)]);
    let p0 = r.read_packet(0, 0).unwrap();
    assert_eq!((p0.dts, p0.duration), (Some(t(0)), Some(t(0))));
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
        vec![pkt(0, 0, 10, true, &[]), pkt(0, 10, 10, false, &[])],
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
    assert!(p0.data.is_empty());
    assert_eq!(p0.duration, Some(t(10)));
    let p1 = r.read_packet(0, 1).unwrap();
    assert!(p1.data.is_empty());
    assert_eq!(p1.dts, Some(t(10)));
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
    match w.write_packet(&pkt(0, 0, 10, true, &[0xA0])) {
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

// —— 2026-10-24 评审回归组 ——

/// tkhd 时长按 movie 时基（1/1000）表达、mdhd 按轨道时基表达
/// （回归：此前两域同值直写轨道单位——timescale≠1000 的轨错报时长，
/// 48kHz 音频 1 秒写成 48000 即 480 秒；fMP4 回填路径一直是对的，
/// 两布局行为不一致；现有 golden 全用 1/1000，两域恰好同值测不出）。
#[test]
fn progressive_tkhd_duration_in_movie_timescale() {
    // 音频轨 timescale=48000，写满 1 秒（48000 单位）。
    let bytes = mux(
        Mp4WriterOptions {
            layout: Layout::Progressive,
            ..Default::default()
        },
        vec![audio_cfg(48_000)],
        vec![
            pkt_tb(0, 0, 24_000, true, &[0xA0, 0xA1], 48_000),
            pkt_tb(0, 24_000, 24_000, true, &[0xA2, 0xA3], 48_000),
        ],
    )
    .unwrap();
    let r = Mp4Reader::from_vec(bytes.clone()).unwrap();
    let moov = find(r.box_tree(), "moov").unwrap();
    let trak = find(&moov.children, "trak").unwrap();
    let tkhd = find(&trak.children, "tkhd").unwrap();
    let mdhd = find(&find(&trak.children, "mdia").unwrap().children, "mdhd").unwrap();
    // tkhd v0：vf(4)+creation(4)+modification(4)+track_id(4)+reserved(4)
    // +duration(4) ⇒ duration 在载荷 +20。
    let tkhd_p = payload(&bytes, tkhd);
    assert_eq!(u32b(tkhd_p, 0) >> 24, 0, "1 秒装得进 v0");
    assert_eq!(u32b(tkhd_p, 20), 1000, "tkhd 时长应为 movie 单位 1000");
    // mdhd v0：vf(4)+creation(4)+modification(4)+timescale(4)+duration(4)
    // ⇒ timescale +12、duration +16。
    let mdhd_p = payload(&bytes, mdhd);
    assert_eq!(u32b(mdhd_p, 12), 48_000, "mdhd timescale 应为轨道时基");
    assert_eq!(u32b(mdhd_p, 16), 48_000, "mdhd 时长应为轨道单位 48000");
    // mvhd 同为 movie 时基：各轨 1 秒 ⇒ 1000。
    let mvhd = find(&moov.children, "mvhd").unwrap();
    let mvhd_p = payload(&bytes, mvhd);
    assert_eq!(u32b(mvhd_p, 12), 1000);
    assert_eq!(u32b(mvhd_p, 16), 1000);
}

/// 首个 I/O 错误后封装器封闭（poisoned）：后续 write_packet/
/// flush_fragment/finalize 一律返回该错误，不再落任何字节（回归：
/// 2026-10-24 评审——写一半失败后复用会静默产出损坏文件：重试把
/// ftyp 再写一遍 / 从文件中段再写一份完整 moof+mdat）。
#[test]
fn writer_is_poisoned_after_first_io_error() {
    /// 预算写：总写入量超 budget 即失败（模拟底层 I/O 半途故障）。
    struct BudgetW {
        buf: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
        pos: usize,
        budget: usize,
        failed: std::cell::Cell<bool>,
    }
    impl std::io::Write for BudgetW {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            if self.failed.get() || self.pos + b.len() > self.budget {
                self.failed.set(true);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "budget exhausted",
                ));
            }
            let mut buf = self.buf.borrow_mut();
            if self.pos + b.len() > buf.len() {
                buf.resize(self.pos + b.len(), 0);
            }
            buf[self.pos..self.pos + b.len()].copy_from_slice(b);
            self.pos += b.len();
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl std::io::Seek for BudgetW {
        fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
            let len = self.buf.borrow().len() as i64;
            let new = match pos {
                std::io::SeekFrom::Start(s) => s as i64,
                std::io::SeekFrom::End(e) => len + e,
                std::io::SeekFrom::Current(c) => self.pos as i64 + c,
            };
            self.pos = new as usize;
            Ok(new as u64)
        }
    }

    // Fragmented：前置 moov（ftyp 32 + moov ~1KB）超出预算 ⇒ 首个
    // flush_fragment 写 moov 时失败。
    let buf = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let mk_w = || BudgetW {
        buf: buf.clone(),
        pos: 0,
        budget: 64,
        failed: std::cell::Cell::new(false),
    };
    let mut w = Mp4Writer::new(
        mk_w(),
        Mp4WriterOptions {
            layout: Layout::Fragmented,
            ..Default::default()
        },
    );
    w.add_stream(audio_cfg(1000)).unwrap();
    for i in 0..4i64 {
        w.write_packet(&pkt(0, i * 10, 10, true, &[0xA0, 0xA1]))
            .unwrap();
    }
    // 首次失败：I/O 错误。
    match w.flush_fragment() {
        Err(WriterError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::WriteZero),
        other => panic!("应为 Io，实为 {other:?}"),
    }
    let len_at_failure = buf.borrow().len();
    // 复用一律返回 Io（不再写、不再推进状态）。
    for _ in 0..2 {
        match w.flush_fragment() {
            Err(WriterError::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::WriteZero),
            other => panic!("复用应返回锁存的 Io，实为 {other:?}"),
        }
    }
    assert!(
        w.write_packet(&pkt(0, 40, 10, true, &[0xA2])).is_err(),
        "poisoned 后写包应拒"
    );
    assert!(w.finalize().is_err(), "poisoned 后收尾应拒");
    assert_eq!(buf.borrow().len(), len_at_failure, "失败后不得再落字节");

    // Progressive 同语义：mdat 数据写失败后复用立即拒、零字节增长。
    let buf2 = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let mk_w2 = || BudgetW {
        buf: buf2.clone(),
        pos: 0,
        budget: 48, // ftyp(32)+mdat 头(16) 刚好放行，包数据必失败
        failed: std::cell::Cell::new(false),
    };
    let mut w2 = Mp4Writer::new(
        mk_w2(),
        Mp4WriterOptions {
            layout: Layout::Progressive,
            ..Default::default()
        },
    );
    w2.add_stream(audio_cfg(1000)).unwrap();
    assert!(w2.write_packet(&pkt(0, 0, 10, true, &[0xB0])).is_err());
    let len2 = buf2.borrow().len();
    assert!(w2.write_packet(&pkt(0, 10, 10, true, &[0xB1])).is_err());
    assert!(w2.finalize().is_err());
    assert_eq!(buf2.borrow().len(), len2, "失败后不得再落字节");
}

/// Fragmented 的字段宽度校验先于 fragment_duration 自动切：注定被拒
/// 的包（dur_units 超 u32）不得先触发 flush 改变写入器状态（回归：
/// 2026-10-24 评审——此前被拒包若恰是到达阈值的关键帧，会先把攒的
/// 样例切成 fragment 落盘再返回 Misuse）。
#[test]
fn fragmented_width_validation_precedes_auto_flush() {
    let bytes = {
        let mut w = Mp4Writer::new(
            Cursor::new(Vec::new()),
            Mp4WriterOptions {
                layout: Layout::Fragmented,
                fragment_duration: Some(t(30)),
                ..Default::default()
            },
        );
        w.add_stream(audio_cfg(1000)).unwrap();
        // 攒出 ≥30 单位的待切跨度（30 单位后到达的关键帧会触发自动切）。
        w.write_packet(&pkt(0, 0, 100, true, &[0xA0])).unwrap();
        w.write_packet(&pkt(0, 100, 100, false, &[0xA1])).unwrap();
        // 坏包：关键帧 + 时长超 u32（trun duration 宽度）⇒ Misuse。
        let bad = pkt(0, 200, u32::MAX as i64 + 1, true, &[0xA2]);
        match w.write_packet(&bad) {
            Err(WriterError::Misuse(msg)) => {
                assert!(msg.contains("时长超出 u32"), "msg={msg}");
            }
            other => panic!("应为 Misuse，实为 {other:?}"),
        }
        // 坏包不得触发自动切：后续包用非关键帧（不再触发正常自动切），
        // 与之前的待切样例合成同一 fragment 由 finalize 冲出——若坏包
        // 先切了（旧行为），后续样例会切成第二个 fragment。
        w.write_packet(&pkt(0, 200, 100, false, &[0xA3])).unwrap();
        w.write_packet(&pkt(0, 300, 100, false, &[0xA4])).unwrap();
        let cur = w.finalize().unwrap();
        cur.into_inner()
    };
    // 整个文件只应有一个 moof（4 样例同一 fragment）。
    let mut r = Mp4Reader::from_vec(bytes).unwrap();
    let moofs: Vec<_> = r
        .box_tree()
        .iter()
        .filter(|n| n.info.kind.as_str() == "moof")
        .collect();
    assert_eq!(moofs.len(), 1, "坏包不得制造额外 fragment 边界");
    assert_eq!(r.sample_count(0).unwrap(), 4);
    // 末包数据可读回（0xA4）。
    let p = r.read_packet(0, 3).unwrap();
    assert_eq!(&*p.data, &[0xA4]);
}

/// 数据驱动的时戳重标度溢出 ⇒ WriterError::Time（非 Misuse；Muxer
/// trait 契约「重标度失败以 Err(ContainerError::Time(..)) 透出」的具体
/// 实现侧断言，回归：2026-10-24 评审——此前误归 Misuse）。
#[test]
fn rescale_overflow_is_time_error() {
    let mut w = Mp4Writer::new(
        Cursor::new(Vec::new()),
        Mp4WriterOptions {
            layout: Layout::Progressive,
            ..Default::default()
        },
    );
    w.add_stream(audio_cfg(1000)).unwrap();
    // 包时基 1/1、dts=i64::MAX：重标度到 1/1000 需 ×1000 ⇒ 溢出。
    let mut p = Packet::empty(0);
    p.dts = Some(Timestamp::new(i64::MAX, Rational::ONE));
    p.duration = Some(Timestamp::new(1, Rational::ONE));
    match w.write_packet(&p) {
        Err(WriterError::Time(msg)) => assert!(msg.contains("重标度"), "msg={msg}"),
        other => panic!("应为 Time，实为 {other:?}"),
    }
    // pts−dts 偏移的算术溢出仍是 Misuse（非重标度失败）：包时基与
    // 轨道时基一致（重标度恒等），pts=i64::MIN − dts=1000 落不回 i64。
    let mut w2 = Mp4Writer::new(
        Cursor::new(Vec::new()),
        Mp4WriterOptions {
            layout: Layout::Progressive,
            ..Default::default()
        },
    );
    w2.add_stream(audio_cfg(1000)).unwrap();
    let mut q = Packet::empty(0);
    let tb1k = Rational::new(1, 1000);
    q.dts = Some(Timestamp::new(1000, tb1k));
    q.pts = Some(Timestamp::new(i64::MIN, tb1k));
    q.duration = Some(Timestamp::new(1, tb1k));
    assert!(matches!(w2.write_packet(&q), Err(WriterError::Misuse(_))));
}
