//! 用例 2（变体矩阵）与用例 3（音频轨）。
//!
//! 移植后语义：时戳/时长为流时基原生单位（黄金轨 1/1000、音频轨
//! 1/48000），pts 恒 Some（无 ctts ⇒ pts==dts）。

mod helpers;

use helpers::*;
use ye_container_mp4::{
    AudioParams, CodecId, MediaType, Mp4Reader, PacketFlags, Rational, Timestamp,
};
use ye_pixfmt::ChannelLayout;

fn find_top<'a>(
    r: &'a Mp4Reader<std::io::Cursor<Vec<u8>>>,
    kind: &str,
) -> &'a ye_container_mp4::BoxNode {
    r.box_tree()
        .iter()
        .find(|n| n.info.kind.as_str() == kind)
        .unwrap()
}

fn find_child<'a>(
    node: &'a ye_container_mp4::BoxNode,
    kind: &str,
) -> Option<&'a ye_container_mp4::BoxNode> {
    node.children.iter().find(|c| c.info.kind.as_str() == kind)
}

#[test]
fn co64_instead_of_stco() {
    let probe = golden_file(&[0, 0], true);
    let (c1, c2) = golden_offsets(&probe);
    let data = golden_file(&[c1, c2], true);
    let mut r = Mp4Reader::from_vec(data).unwrap();

    assert_eq!(r.streams().len(), 1);
    assert_eq!(r.sample_count(0).unwrap(), 3);
    let s = r.stream(0).unwrap();
    assert_eq!(s.time_base, Rational::new(1, 1000));
    assert_eq!(s.duration, Some(Timestamp::new(30, Rational::new(1, 1000))));

    // 数据读取不受 co64 影响。
    let p0 = r.read_packet(0, 0).unwrap();
    assert_eq!(&*p0.data, &[0xA0, 0xA1, 0xA2, 0xA3]);
    let p2 = r.read_packet(0, 2).unwrap();
    assert_eq!(&*p2.data, &(0xA9..=0xAE).collect::<Vec<u8>>());

    // 树里是 co64 而非 stco。
    let moov = find_top(&r, "moov");
    let stbl = find_child(
        find_child(
            find_child(find_child(moov, "trak").unwrap(), "mdia").unwrap(),
            "minf",
        )
        .unwrap(),
        "stbl",
    )
    .unwrap();
    assert!(find_child(stbl, "co64").is_some());
    assert!(find_child(stbl, "stco").is_none());
}

/// 定长 stsz + 无 ctts/stss（顺带验证缺省语义：视频全关键帧、pts==dts）。
#[test]
fn stsz_fixed_sample_size() {
    let stbl = vec![
        stsd_video(64, 96, &AVCC),
        stts(&[(3, 10)]),
        stsc(&[(1, 2, 1), (2, 1, 1)]),
        stsz_fixed(4, 3),
        stco(&[0, 0]), // 占位，下面回填
    ];
    let probe = file_with(&[video_trak(&stbl, 1000, 30)]);
    let c1 = (probe.len() - MDAT.len()) as u64;
    let c2 = c1 + 8; // chunk1 两个样例各 4 字节
    let stbl = vec![
        stsd_video(64, 96, &AVCC),
        stts(&[(3, 10)]),
        stsc(&[(1, 2, 1), (2, 1, 1)]),
        stsz_fixed(4, 3),
        stco(&[c1 as u32, c2 as u32]),
    ];
    let mut r = Mp4Reader::from_vec(file_with(&[video_trak(&stbl, 1000, 30)])).unwrap();

    let tb = Rational::new(1, 1000);
    assert_eq!(r.sample_count(0).unwrap(), 3);
    for (i, expect) in [
        vec![0xA0, 0xA1, 0xA2, 0xA3],
        vec![0xA4, 0xA5, 0xA6, 0xA7],
        vec![0xA8, 0xA9, 0xAA, 0xAB],
    ]
    .into_iter()
    .enumerate()
    {
        let p = r.read_packet(0, i).unwrap();
        assert_eq!(&*p.data, &expect, "样例 {i}");
        assert_eq!(p.duration, Some(Timestamp::new(10, tb)));
        // 无 ctts ⇒ pts==dts（打戳恒 Some）；视频轨无 stss ⇒ 恒关键帧。
        assert_eq!(p.pts, Some(Timestamp::new(10 * i as i64, tb)));
        assert_eq!(p.dts, p.pts);
        assert!(
            p.flags.contains(PacketFlags::KEYFRAME),
            "样例 {i} 应为关键帧"
        );
    }
}

/// size==1 的 64 位大 mdat。
#[test]
fn largesize_mdat() {
    let probe = golden_file(&[0, 0], false);
    let (c1, c2) = golden_offsets(&probe);
    // box64 头 16 字节比普通头多 8，所有 chunk 偏移随之 +8。
    let (c1, c2) = (c1 + 8, c2 + 8);
    let mut f = Vec::new();
    push(&mut f, &ftyp());
    push(
        &mut f,
        &moov(&[video_trak(&golden_stbl(&[c1, c2], false), 1000, 30)]),
    );
    push(&mut f, &box64("mdat", &MDAT));
    // 布局与推算一致：mdat 载荷起点即 chunk1。
    assert_eq!((f.len() - MDAT.len()) as u64, c1);

    let mut r = Mp4Reader::from_vec(f).unwrap();
    let p0 = r.read_packet(0, 0).unwrap();
    assert_eq!(&*p0.data, &[0xA0, 0xA1, 0xA2, 0xA3]);
    let mdat = find_top(&r, "mdat");
    assert_eq!(mdat.info.size, 16 + MDAT.len() as u64); // largesize 头 16 字节
    let p2 = r.read_packet(0, 2).unwrap();
    assert_eq!(&*p2.data, &(0xA9..=0xAE).collect::<Vec<u8>>());
}

/// size==0 的顶层末 box：延伸到 EOF。
#[test]
fn last_box_extends_to_eof() {
    let probe = golden_file(&[0, 0], false);
    let (c1, c2) = golden_offsets(&probe);
    let mut f = Vec::new();
    push(&mut f, &ftyp());
    push(
        &mut f,
        &moov(&[video_trak(&golden_stbl(&[c1, c2], false), 1000, 30)]),
    );
    let mdat_offset = f.len() as u64;
    push(&mut f, &box_to_eof("mdat", &MDAT));
    assert_eq!(f.len() as u64 - MDAT.len() as u64, c1);

    let mut r = Mp4Reader::from_vec(f).unwrap();
    let p1 = r.read_packet(0, 1).unwrap();
    assert_eq!(&*p1.data, &(0xA4..=0xA8).collect::<Vec<u8>>());
    let mdat = find_top(&r, "mdat");
    assert_eq!(mdat.info.offset, mdat_offset);
    assert_eq!(mdat.info.size, 8 + MDAT.len() as u64); // 到 EOF 即 24
}

/// 未知 box（udta/edts/free/wide/skip/meta）穿插的文件；chunk 偏移由调用方给。
fn unknown_boxes_file(chunk_offsets: &[u64]) -> Vec<u8> {
    let trak = wrap(
        "trak",
        &[
            tkhd(7, 30),
            r#box("edts", &r#box("elst", &[0u8; 20])),
            wrap(
                "mdia",
                &[
                    mdhd(1000, 30),
                    hdlr("vide"),
                    r#box("skip", &[0xAA; 8]),
                    wrap("minf", &[wrap("stbl", &golden_stbl(chunk_offsets, false))]),
                ],
            ),
        ],
    );
    let mut f = Vec::new();
    push(&mut f, &ftyp());
    push(&mut f, &r#box("free", &[0; 12]));
    push(
        &mut f,
        &moov(&[trak, r#box("udta", &r#box("meta", &[0u8; 12]))]),
    );
    push(&mut f, &r#box("wide", &[0; 8]));
    push(&mut f, &r#box("mdat", &MDAT));
    f
}

/// 未知 box（udta/edts/free/wide）穿插不崩且流表正确。
#[test]
fn unknown_boxes_are_skipped() {
    // 偏移定长 ⇒ 两遍构造布局不变。
    let probe = unknown_boxes_file(&[0, 0]);
    let c1 = (probe.len() - MDAT.len()) as u64;
    let c2 = c1 + 9; // chunk1 含样例 0/1，大小 4+5
    let mut r = Mp4Reader::from_vec(unknown_boxes_file(&[c1, c2])).unwrap();
    assert_eq!(r.streams().len(), 1);
    assert_eq!(r.sample_count(0).unwrap(), 3);
    let s = r.stream(0).unwrap();
    assert_eq!(r.track_info(0).unwrap().codec.as_str(), "avc1");
    assert_eq!(s.codec.extradata, Some(AVCC.to_vec()));
    let p0 = r.read_packet(0, 0).unwrap();
    assert_eq!(&*p0.data, &[0xA0, 0xA1, 0xA2, 0xA3]);
    let p2 = r.read_packet(0, 2).unwrap();
    assert_eq!(&*p2.data, &(0xA9..=0xAE).collect::<Vec<u8>>());
}

/// 用例 3：音频轨——hdlr=soun、mp4a(ch=2, rate=48000)+esds 进 extra_data、
/// 无 stss ⇒ 全关键帧；无 ctts ⇒ pts==dts。
#[test]
fn audio_track_metadata_and_packets() {
    let audio_mdat: [u8; 6] = [0xC0, 0xC1, 0xC2, 0xC3, 0xC4, 0xC5];
    let stbl = vec![
        stsd_audio(2, 48_000, &ESDS),
        stts(&[(3, 1)]),
        stsc(&[(1, 3, 1)]),
        stsz_var(&[2, 2, 2]),
        stco(&[0]), // 占位
    ];
    let probe = {
        let mut f = Vec::new();
        push(&mut f, &ftyp());
        push(&mut f, &moov(&[audio_trak(&stbl, 48_000, 3)]));
        push(&mut f, &r#box("mdat", &audio_mdat));
        f
    };
    let chunk1 = (probe.len() - audio_mdat.len()) as u64;
    let stbl = vec![
        stsd_audio(2, 48_000, &ESDS),
        stts(&[(3, 1)]),
        stsc(&[(1, 3, 1)]),
        stsz_var(&[2, 2, 2]),
        stco(&[chunk1 as u32]),
    ];
    let mut f = Vec::new();
    push(&mut f, &ftyp());
    push(&mut f, &moov(&[audio_trak(&stbl, 48_000, 3)]));
    push(&mut f, &r#box("mdat", &audio_mdat));

    let mut r = Mp4Reader::from_vec(f).unwrap();
    assert_eq!(r.streams().len(), 1);
    let s = r.stream(0).unwrap();
    assert_eq!(r.track_info(0).unwrap().track_id, 3);
    assert_eq!(s.media_type, MediaType::Audio);
    assert_eq!(s.codec.codec, CodecId::Aac);
    assert_eq!(r.track_info(0).unwrap().codec.as_str(), "mp4a");
    let tb = Rational::new(1, 48_000);
    assert_eq!(s.time_base, tb);
    // 时长：mdhd 3 单位 @ 1/48000，流时基原生精确。
    assert_eq!(s.duration, Some(Timestamp::new(3, tb)));
    assert_eq!(s.codec.video, None);
    // 声道 2 → 默认布局 STEREO（声道数经 layout.channels() 保全）。
    assert_eq!(
        s.codec.audio,
        Some(AudioParams {
            sample_rate: 48_000,
            channel_layout: Some(ChannelLayout::STEREO),
            sample_format: None,
        })
    );
    assert_eq!(
        s.codec
            .audio
            .as_ref()
            .unwrap()
            .channel_layout
            .unwrap()
            .channels(),
        2
    );
    // esds 载荷（含 4 字节版本/标志）原样透传。
    assert_eq!(s.codec.extradata, Some(ESDS.to_vec()));

    for (i, bytes) in [&[0xC0, 0xC1][..], &[0xC2, 0xC3][..], &[0xC4, 0xC5][..]]
        .into_iter()
        .enumerate()
    {
        let p = r.read_packet(0, i).unwrap();
        assert_eq!(p.dts, Some(Timestamp::new(i as i64, tb)), "样例 {i}");
        assert_eq!(p.pts, p.dts, "无 ctts ⇒ pts==dts");
        assert_eq!(p.duration, Some(Timestamp::new(1, tb)));
        assert!(p.flags.contains(PacketFlags::KEYFRAME), "音频轨恒关键帧");
        assert_eq!(&*p.data, bytes);
    }
}
