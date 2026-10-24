//! 用例 4：错误路径。
//!
//! 移植后语义：时戳为流时基原生单位（tfdf/trun 的轨道单位直读）。

mod helpers;

use helpers::*;
use ye_container_mp4::{Mp4Error, Mp4Reader, Rational, Timestamp};

fn golden() -> Vec<u8> {
    let (c1, c2) = golden_offsets(&golden_file(&[0, 0], false));
    golden_file(&[c1, c2], false)
}

#[test]
fn truncated_stsz_is_malformed_with_box_path() {
    let mut stbl = golden_stbl(&[0, 0], false);
    // 声明 3 个样例但只给 2 个大小。
    stbl[5] = stsz_truncated(&[4, 5], 3);
    let data = file_with(&[video_trak(&stbl, 1000, 30)]);

    let err = Mp4Reader::from_vec(data).unwrap_err();
    match err {
        Mp4Error::Malformed {
            ref path,
            ref reason,
        } => {
            assert_eq!(path, "moov.trak[0].mdia.minf.stbl.stsz");
            assert!(reason.contains("截断"), "reason 应说明截断：{reason}");
        }
        other => panic!("应为 Malformed，实为 {other:?}"),
    }
}

#[test]
fn moof_fragment_appends_to_track_samples() {
    // 新契约：moof 不再 Unsupported——空 stbl 轨道 + moof 的 fragment 样例
    // 并入统一样例序（fmp4_read.rs 有全字段深测，此处验证行为翻转本身）。
    let stbl = vec![
        stsd_video(64, 96, &AVCC),
        stts(&[]),
        stsc(&[]),
        stsz_var(&[]),
        stco(&[]),
    ];
    let moov = wrap(
        "moov",
        &[mvhd(), video_trak(&stbl, 1000, 0), mvex_trex(7, 0, 0, 0)],
    );
    // moof 的 data_offset 相对 moof 起点（default-base-is-moof）：两遍构造回填。
    let build_moof = |data_offset: i32| {
        wrap(
            "moof",
            &[wrap(
                "traf",
                &[
                    tfhd_box(7, 0x0002_0000, None),
                    tfdt_box(0, 30),
                    trun_box(
                        &[(Some(10), Some(4), None), (Some(10), Some(5), None)],
                        Some(data_offset),
                        None,
                    ),
                ],
            )],
        )
    };
    let moof = build_moof(0);
    let offset = moof.len() as i32 + 8; // mdat 头 8 字节，样例 0 在载荷起点
    let moof = build_moof(offset);
    let mut f = Vec::new();
    push(&mut f, &ftyp());
    push(&mut f, &moov);
    push(&mut f, &moof);
    push(&mut f, &r#box("mdat", &MDAT));

    let tb = Rational::new(1, 1000);
    let mut r = Mp4Reader::from_vec(f).unwrap();
    assert_eq!(r.sample_count(0).unwrap(), 2);
    let p0 = r.read_packet(0, 0).unwrap();
    assert_eq!(p0.dts, Some(Timestamp::new(30, tb))); // tfdt 30 @ 1/1000
    assert_eq!(p0.duration, Some(Timestamp::new(10, tb)));
    assert_eq!(&*p0.data, &[0xA0, 0xA1, 0xA2, 0xA3]);
    let p1 = r.read_packet(0, 1).unwrap();
    assert_eq!(p1.dts, Some(Timestamp::new(40, tb)));
    assert_eq!(&*p1.data, &(0xA4..=0xA8).collect::<Vec<u8>>());
}

#[test]
fn mvex_without_moof_reads_progressive() {
    // 新契约：mvex 出现不再 Unsupported。无 moof ⇒ 纯 progressive 语义，
    // mvex/trex 仅记录不参与——非空 stbl 照常读样例。
    // 占位探测须含 mvex（它使 moov 变长、mdat 后移）。
    let build = |c1: u64, c2: u64| {
        let mut f = Vec::new();
        push(&mut f, &ftyp());
        push(
            &mut f,
            &wrap(
                "moov",
                &[
                    mvhd(),
                    video_trak(&golden_stbl(&[c1, c2], false), 1000, 30),
                    r#box("mvex", &r#box("trex", &[0u8; 24])),
                ],
            ),
        );
        push(&mut f, &r#box("mdat", &MDAT));
        f
    };
    let (c1, c2) = golden_offsets(&build(0, 0));
    let mut r = Mp4Reader::from_vec(build(c1, c2)).unwrap();
    assert_eq!(r.streams().len(), 1);
    assert_eq!(r.sample_count(0).unwrap(), 3);
    let p0 = r.read_packet(0, 0).unwrap();
    assert_eq!(p0.dts, Some(Timestamp::new(0, Rational::new(1, 1000))));
    assert_eq!(&*p0.data, &[0xA0, 0xA1, 0xA2, 0xA3]);
    let p2 = r.read_packet(0, 2).unwrap();
    assert_eq!(&*p2.data, &(0xA9..=0xAE).collect::<Vec<u8>>());
}

#[test]
fn multiple_sample_descriptions_rejected() {
    let stbl = vec![
        stsd_two_entries(64, 96, &AVCC),
        stts(&[(3, 10)]),
        stsc(&[(1, 3, 1)]),
        stsz_var(&[4, 5, 6]),
        stco(&[0]),
    ];
    let data = file_with(&[video_trak(&stbl, 1000, 30)]);
    let err = Mp4Reader::from_vec(data).unwrap_err();
    match err {
        Mp4Error::Unsupported(what) => assert_eq!(what, "多 sample description"),
        other => panic!("应为 Unsupported，实为 {other:?}"),
    }
}

#[test]
fn sample_index_out_of_range() {
    let mut r = Mp4Reader::from_vec(golden()).unwrap();
    let err = r.read_packet(0, 5).unwrap_err();
    match err {
        Mp4Error::SampleIndexOutOfRange {
            stream,
            sample,
            count,
        } => {
            assert_eq!((stream, sample, count), (0, 5, 3));
        }
        other => panic!("应为 SampleIndexOutOfRange，实为 {other:?}"),
    }
    // 顺读越界同样报错（合法序号 0..2 之外）。
    assert!(matches!(
        r.read_packet(0, 3),
        Err(Mp4Error::SampleIndexOutOfRange { count: 3, .. })
    ));
}

#[test]
fn invalid_stream_index() {
    let mut r = Mp4Reader::from_vec(golden()).unwrap();
    match r.read_packet(3, 0) {
        Err(Mp4Error::InvalidStreamIndex { index, count }) => assert_eq!((index, count), (3, 1)),
        other => panic!("应为 InvalidStreamIndex，实为 {other:?}"),
    }
    match r.sample_count(1) {
        Err(Mp4Error::InvalidStreamIndex { index, count }) => assert_eq!((index, count), (1, 1)),
        other => panic!("应为 InvalidStreamIndex，实为 {other:?}"),
    }
}

#[test]
fn empty_moov_gives_no_streams() {
    let mut f = Vec::new();
    push(&mut f, &ftyp());
    push(&mut f, &wrap("moov", &[mvhd()]));
    push(&mut f, &r#box("mdat", &MDAT));

    let mut r = Mp4Reader::from_vec(f).unwrap();
    assert!(r.streams().is_empty());
    assert!(r.stream(0).is_none());
    match r.sample_count(0) {
        Err(Mp4Error::InvalidStreamIndex { index, count }) => assert_eq!((index, count), (0, 0)),
        other => panic!("应为 InvalidStreamIndex，实为 {other:?}"),
    }
    assert!(matches!(
        r.read_packet(0, 0),
        Err(Mp4Error::InvalidStreamIndex { .. })
    ));
    // 树结构照常可用。
    assert_eq!(r.box_tree().len(), 3);
    assert_eq!(r.box_tree()[1].info.kind.as_str(), "moov");
    assert_eq!(r.box_tree()[1].children.len(), 1);
}

#[test]
fn missing_stbl_table_is_malformed() {
    // 缺 stco/co64：chunk 偏移无从谈起。
    let stbl = vec![
        stsd_video(64, 96, &AVCC),
        stts(&[(3, 10)]),
        stsc(&[(1, 3, 1)]),
        stsz_var(&[4, 5, 6]),
    ];
    let data = file_with(&[video_trak(&stbl, 1000, 30)]);
    match Mp4Reader::from_vec(data).unwrap_err() {
        Mp4Error::Malformed { path, .. } => {
            assert_eq!(path, "moov.trak[0].mdia.minf.stbl.stco");
        }
        other => panic!("应为 Malformed，实为 {other:?}"),
    }
}

#[test]
fn io_error_is_wrapped() {
    let err = Mp4Reader::open("不存在的文件.mp4").unwrap_err();
    assert!(matches!(err, Mp4Error::Io(_)));
    assert!(err.to_string().starts_with("I/O 错误"));
    // Io 变体的 source 是底层 io::Error。
    assert!(std::error::Error::source(&err).is_some());
}

// —— fMP4 box 构造（moof 翻写用例内联使用；完整矩阵在 fmp4_read.rs）——

/// mvex 包 trex：version/flags + track_id + 4 个缺省字段。
fn mvex_trex(track_id: u32, dur: u32, size: u32, flags: u32) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be_u32(0));
    push(&mut p, &be_u32(track_id));
    push(&mut p, &be_u32(1)); // default_sample_description_index
    push(&mut p, &be_u32(dur));
    push(&mut p, &be_u32(size));
    push(&mut p, &be_u32(flags));
    wrap("mvex", &[r#box("trex", &p)])
}

/// tfhd：flags + track_id + 可选 base_data_offset。
fn tfhd_box(track_id: u32, flags: u32, base: Option<u64>) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be_u32(flags));
    push(&mut p, &be_u32(track_id));
    if let Some(b) = base {
        push(&mut p, &be_u64(b));
    }
    r#box("tfhd", &p)
}

/// tfdt：v0 用 u32、v1 用 u64。
fn tfdt_box(version: u32, base: u64) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be_u32(version << 24));
    if version == 1 {
        push(&mut p, &be_u64(base));
    } else {
        push(&mut p, &be_u32(base as u32));
    }
    r#box("tfdt", &p)
}

/// trun v0：每样例 (duration, size, flags) 三元组按 Some 与否决定字段存在。
fn trun_box(
    samples: &[(Option<u32>, Option<u32>, Option<u32>)],
    data_offset: Option<i32>,
    first_flags: Option<u32>,
) -> Vec<u8> {
    let mut p = Vec::new();
    let mut flags = 0u32;
    if data_offset.is_some() {
        flags |= 0x0000_0001;
    }
    if first_flags.is_some() {
        flags |= 0x0000_0004;
    }
    if samples.iter().any(|s| s.0.is_some()) {
        flags |= 0x0000_0100;
    }
    if samples.iter().any(|s| s.1.is_some()) {
        flags |= 0x0000_0200;
    }
    if samples.iter().any(|s| s.2.is_some()) {
        flags |= 0x0000_0400;
    }
    push(&mut p, &be_u32(flags));
    push(&mut p, &be_u32(samples.len() as u32));
    if let Some(d) = data_offset {
        push(&mut p, &be_u32(d as u32));
    }
    if let Some(f) = first_flags {
        push(&mut p, &be_u32(f));
    }
    for &(dur, size, fl) in samples {
        if let Some(d) = dur {
            push(&mut p, &be_u32(d));
        }
        if let Some(s) = size {
            push(&mut p, &be_u32(s));
        }
        if let Some(f) = fl {
            push(&mut p, &be_u32(f));
        }
    }
    r#box("trun", &p)
}
