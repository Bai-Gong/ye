//! 用例 1（黄金路径）与用例 5（box_tree 结构）。

mod helpers;

use helpers::*;
use ye_container_mp4::{BoxNode, FourCc, MediaKind, Mp4Reader, Rational, Timestamp, VideoInfo};

fn golden() -> Vec<u8> {
    let (c1, c2) = golden_offsets(&golden_file(&[0, 0], false));
    golden_file(&[c1, c2], false)
}

#[test]
fn golden_path_stream_metadata() {
    let r = Mp4Reader::from_vec(golden()).unwrap();

    assert_eq!(r.streams().len(), 1);
    let s = r.stream(0).unwrap();
    assert_eq!(s.index, 0);
    assert_eq!(s.track_id, 7);
    assert_eq!(s.kind, MediaKind::Video);
    assert_eq!(s.codec, FourCc::new(*b"avc1"));
    assert_eq!(s.codec.as_str(), "avc1");
    assert_eq!(format!("{}", s.codec), "avc1");
    // 时基 = 1/timescale = 1/1000。
    assert_eq!(s.timebase, Rational::new(1, 1000).unwrap());
    // 时长：mdhd 30 @ 1/1000 s = 30 ms = 211680 tick。
    assert_eq!(s.duration, Timestamp::from_ticks(211_680));
    assert_eq!(
        s.video,
        Some(VideoInfo {
            width: 64,
            height: 96
        })
    );
    assert_eq!(s.audio, None);
    // extra_data = avcC 载荷（AVCDecoderConfigurationRecord 本体）。
    assert_eq!(s.extra_data, AVCC.to_vec());

    assert_eq!(r.sample_count(0).unwrap(), 3);
    assert!(r.stream(1).is_none());
}

#[test]
fn golden_path_read_packets_in_order() {
    let mut r = Mp4Reader::from_vec(golden()).unwrap();

    // 样例 0：dts=0；ctts v1 首段偏移 -5 ms ⇒ pts=-35280 tick；stss 仅样例 1。
    let p0 = r.read_packet(0, 0).unwrap();
    assert_eq!((p0.stream_index, p0.sample_index), (0, 0));
    assert_eq!(p0.dts, Timestamp::ZERO);
    assert_eq!(p0.pts, Some(Timestamp::from_ticks(-35_280)));
    assert_eq!(p0.duration, Timestamp::from_ticks(70_560)); // stts delta 10 ms
    assert!(p0.keyframe);
    assert_eq!(p0.data, vec![0xA0, 0xA1, 0xA2, 0xA3]);

    // 样例 1：dts=10 ms；ctts 第二段偏移 0 ⇒ pts=dts；非关键帧。
    let p1 = r.read_packet(0, 1).unwrap();
    assert_eq!(p1.dts, Timestamp::from_ticks(70_560));
    assert_eq!(p1.pts, Some(Timestamp::from_ticks(70_560)));
    assert_eq!(p1.duration, Timestamp::from_ticks(70_560));
    assert!(!p1.keyframe);
    assert_eq!(p1.data, (0xA4..=0xA8).collect::<Vec<u8>>());

    // 样例 2：chunk2 起点 = chunk1 + 4 + 5。
    let p2 = r.read_packet(0, 2).unwrap();
    assert_eq!(p2.dts, Timestamp::from_ticks(141_120));
    assert_eq!(p2.pts, Some(Timestamp::from_ticks(141_120)));
    assert_eq!(p2.duration, Timestamp::from_ticks(70_560));
    assert!(!p2.keyframe);
    assert_eq!(p2.data, (0xA9..=0xAE).collect::<Vec<u8>>());

    // 乱序随机访问也可（重读样例 0）。
    let again = r.read_packet(0, 0).unwrap();
    assert_eq!(again.data, p0.data);
    assert_eq!(again.pts, Some(Timestamp::from_ticks(-35_280)));
}

fn child_kinds(node: &BoxNode) -> Vec<String> {
    node.children.iter().map(|c| c.info.kind.as_str()).collect()
}

fn find_child<'a>(node: &'a BoxNode, kind: &str) -> Option<&'a BoxNode> {
    node.children.iter().find(|c| c.info.kind.as_str() == kind)
}

#[test]
fn golden_path_box_tree_structure() {
    let data = golden();
    let moov_size = moov_len(&data) as u64;
    let mdat_offset = data.len() as u64 - (8 + MDAT.len() as u64);
    let r = Mp4Reader::from_vec(data).unwrap();

    let tree = r.box_tree();
    assert_eq!(tree.len(), 3);
    let ftyp_len = ftyp().len() as u64;
    assert_eq!(tree[0].info, box_info("ftyp", 0, ftyp_len));
    // moov 紧随 ftyp。
    let moov = &tree[1];
    assert_eq!(moov.info.kind.as_str(), "moov");
    assert_eq!(moov.info.offset, ftyp_len);
    assert_eq!(moov.info.size, moov_size);
    // mdat：8 字节头 + 16 字节载荷。
    let mdat = &tree[2];
    assert_eq!(mdat.info.kind.as_str(), "mdat");
    assert_eq!(mdat.info.size, 8 + MDAT.len() as u64);
    assert_eq!(mdat.info.offset, mdat_offset);

    // 只递归 moov/trak/mdia/minf/stbl。
    assert_eq!(child_kinds(moov), vec!["mvhd", "trak"]);
    let trak = find_child(moov, "trak").unwrap();
    assert_eq!(child_kinds(trak), vec!["tkhd", "mdia"]);
    let mdia = find_child(trak, "mdia").unwrap();
    assert_eq!(child_kinds(mdia), vec!["mdhd", "hdlr", "minf"]);
    let minf = find_child(mdia, "minf").unwrap();
    assert_eq!(child_kinds(minf), vec!["stbl"]);
    let stbl = find_child(minf, "stbl").unwrap();
    assert_eq!(
        child_kinds(stbl),
        vec!["stsd", "stts", "ctts", "stss", "stsc", "stsz", "stco"]
    );

    // 叶子 box 无子节点；FourCc 显示为可读四字符。
    let stsz = find_child(stbl, "stsz").unwrap();
    assert!(stsz.children.is_empty());
    assert_eq!(format!("{}", stsz.info.kind), "stsz");
    assert_eq!(format!("{:?}", stsz.info.kind), "stsz");
    // 非全可打印 ⇒ 逐字节转义。
    assert_eq!(
        format!("{}", FourCc::new([0x00, 0xFF, 0x41, 0x30])),
        "\\x00\\xFF\\x41\\x30"
    );
}

/// 从构造数据算 moov 完整大小（顶层 mdat 之前的部分）。
fn moov_len(data: &[u8]) -> usize {
    let mdat_size = 8 + MDAT.len();
    data.len() - ftyp().len() - mdat_size
}

fn box_info(kind: &str, offset: u64, size: u64) -> ye_container_mp4::BoxInfo {
    ye_container_mp4::BoxInfo {
        kind: FourCc::from_bytes(kind.as_bytes()).unwrap(),
        offset,
        size,
    }
}
