//! 门面 [`remux`] 助手的回归测试：注册表 + 门面 re-export 全链路
//! （内存造双轨 MP4 → `ye::container::remux` 直通 → 注册表读回对拍）。
//!
//! trait 面的 `write_trailer` 按值消费输出句柄，故输出落在共享缓冲上
//! （`Arc<Mutex<Vec<u8>>>` 的 Write+Seek 适配，标准覆写语义——
//! progressive MP4 finalize 会后向寻址回填 mdat largesize）。

#![forbid(unsafe_code)]

use std::io::{Cursor, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex};

use ye::container::mp4::{
    CodecId, Layout, MediaType, Mp4Writer, Mp4WriterOptions, Packet, PacketFlags, TrackConfig,
    register_mp4,
};
use ye::container::{ContainerRegistry, ReadSeek, RemuxStats, remux};
use ye::time::{Rational, Timestamp};

// —— 共享输出缓冲 ——

#[derive(Clone)]
struct SharedBuf {
    data: Arc<Mutex<Vec<u8>>>,
    pos: usize,
}

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut d = self.data.lock().unwrap();
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

impl Seek for SharedBuf {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let len = self.data.lock().unwrap().len() as i64;
        let new = match pos {
            SeekFrom::Start(s) => s as i64,
            SeekFrom::End(e) => len + e,
            SeekFrom::Current(c) => self.pos as i64 + c,
        };
        assert!(new >= 0, "负 seek 不应出现：{new}");
        self.pos = new as usize;
        Ok(new as u64)
    }
}

// —— 造源 ——

fn pkt(si: u32, tb: Rational, dts: i64, dur: i64, key: bool, byte: u8) -> Packet {
    let mut p = Packet::empty(si);
    p.pts = Some(Timestamp::new(dts + 1, tb));
    p.dts = Some(Timestamp::new(dts, tb));
    p.duration = Some(Timestamp::new(dur, tb));
    p.data = Arc::from(vec![byte; 4]);
    if key {
        p.flags.insert(PacketFlags::KEYFRAME);
    }
    p
}

/// 双轨源（fMP4）：视频 1/25 时基 4 帧（0/2 号关键帧）、音频 1/100
/// 时基 5 样例；视频首包 dts = 50（2s）、音频首包 dts = 20——fMP4 的
/// tfdt 可承载非零首 dts，验证 remux 平移到首包 dts=0（progressive
/// 输出的 stts 从 0 起算）。
fn build_source() -> Vec<u8> {
    let v_tb = Rational::new(1, 25);
    let a_tb = Rational::new(1, 100);
    let mut w = Mp4Writer::new(
        Cursor::new(Vec::<u8>::new()),
        Mp4WriterOptions {
            layout: Layout::Fragmented,
            ..Default::default()
        },
    );
    w.add_stream(TrackConfig {
        media_type: MediaType::Video,
        codec: CodecId::H264,
        timebase: v_tb,
        video: Some(ye::container::mp4::VideoParams {
            width: 320,
            height: 240,
            pixel_format: None,
            frame_rate: None,
        }),
        audio: None,
        extra_data: vec![1, 2, 3],
    })
    .unwrap();
    w.add_stream(TrackConfig {
        media_type: MediaType::Audio,
        codec: CodecId::Aac,
        timebase: a_tb,
        video: None,
        audio: Some(ye::container::mp4::AudioParams {
            sample_rate: 44_100,
            channel_layout: Some(ye::pixfmt::ChannelLayout::STEREO),
            sample_format: None,
        }),
        extra_data: vec![4, 5],
    })
    .unwrap();
    // 视频首包 dts=50（2s）→ remux 应平移回 0；音频 dts 也整体 >0。
    for i in 0..4i64 {
        w.write_packet(&pkt(0, v_tb, 50 + i, 1, i % 2 == 0, 0xA0 + i as u8))
            .unwrap();
    }
    for i in 0..5i64 {
        w.write_packet(&pkt(1, a_tb, 20 + i * 10, 10, true, 0xB0 + i as u8))
            .unwrap();
    }
    w.finalize().unwrap().into_inner()
}

fn registry() -> ContainerRegistry {
    let mut reg = ContainerRegistry::new();
    register_mp4(&mut reg);
    reg
}

/// 经注册表把字节读成 (stream_index, dts, pts, data[0]) 序列。
fn drain(data: Vec<u8>) -> Vec<(u32, i64, i64, u8)> {
    let (_, mut demuxer) = registry()
        .open_demuxer(Box::new(Cursor::new(data)))
        .expect("auto-probe mp4");
    demuxer.read_header().expect("read_header");
    let mut out = Vec::new();
    while let Some(p) = demuxer.read_packet().expect("read_packet") {
        out.push((
            p.stream_index,
            p.dts.unwrap().ticks(),
            p.pts.unwrap().ticks(),
            p.data[0],
        ));
    }
    out
}

#[test]
fn remux_round_trip_shifts_first_dts_to_zero() {
    let source = build_source();
    let src_view = drain(source.clone());

    let handle = Arc::new(Mutex::new(Vec::<u8>::new()));
    let stats: RemuxStats = remux(
        &registry(),
        Box::new(Cursor::new(source)),
        "mp4",
        Box::new(SharedBuf {
            data: handle.clone(),
            pos: 0,
        }),
    )
    .expect("remux 应成功");
    assert_eq!(
        stats,
        RemuxStats {
            streams: 2,
            packets: 9
        }
    );

    let out_view = drain(handle.lock().unwrap().clone());
    assert_eq!(out_view.len(), src_view.len());
    // 逐包对拍：流序/数据字节配对不变；dts/pts 每轨整体平移（平移量 =
    // 该轨首包 dts），轨内相对时序不变。
    for &(si, dts, pts, byte) in &out_view {
        let src = src_view
            .iter()
            .find(|(s, _, _, b)| *s == si && *b == byte)
            .expect("输出包应在源中有对应（流序+数据字节不变）");
        let shift = first_dts_of(&src_view, si);
        assert_eq!(dts, src.1 - shift, "流 {si} 包 {byte:#x} dts 应只差平移量");
        assert_eq!(pts, src.2 - shift, "流 {si} 包 {byte:#x} pts 应只差平移量");
    }
    // 平移后每轨首包 dts == 0。
    for si in [0u32, 1] {
        let first = out_view.iter().find(|(s, _, _, _)| *s == si).unwrap();
        assert_eq!(first.1, 0, "流 {si} 平移后首包 dts 应为 0");
    }
}

fn first_dts_of(view: &[(u32, i64, i64, u8)], si: u32) -> i64 {
    view.iter().find(|(s, _, _, _)| *s == si).unwrap().1
}

/// 未知输出格式名 → UnknownFormat。
#[test]
fn remux_unknown_output_format() {
    let err = remux(
        &registry(),
        Box::new(Cursor::new(build_source())),
        "matroska",
        Box::new(Cursor::new(Vec::<u8>::new())),
    )
    .unwrap_err();
    assert!(matches!(err, ye::container::ContainerError::UnknownFormat));
}

/// 非 MP4 输入（auto-probe 不可识别）→ UnknownFormat。
#[test]
fn remux_unrecognizable_input() {
    let err = remux(
        &registry(),
        Box::new(Cursor::new(vec![0u8; 64])),
        "mp4",
        Box::new(Cursor::new(Vec::<u8>::new())),
    )
    .unwrap_err();
    assert!(matches!(err, ye::container::ContainerError::UnknownFormat));
}

// ReadSeek 需要的类型证明（Box<dyn ReadSeek> 组合 trait 用法回归）。
#[allow(dead_code)]
fn read_seek_object_composes(data: Vec<u8>) -> Box<dyn ReadSeek> {
    Box::new(Cursor::new(data))
}
