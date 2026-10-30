//! 临时诊断（提交前剥离）：all-i no-deblock/8x8dct 流帧 1 的 (2,6) 宏块
//! bins 注入枚举——在指定位点用指定槽多读 1 bin 后重放全帧，寻找使
//! 帧 1 (2,6) 区域像素与 ffmpeg 真值一致的单点修正。用后删除本文件。
//!
//! 运行：cargo test -p ye-codec-h264 --release --test cabac_inject -- --nocapture inject_scan
//! 前置：$TEMP/aind.mp4（no-deblock 向量）、$TEMP/aind.yuv（ffmpeg 全解）。

use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::PathBuf;
use std::process::Command;

use ye_codec::{Decoded, Decoder, Frame};
use ye_codec_h264::H264Decoder;
use ye_container::Demuxer;
use ye_container_mp4::Mp4Demuxer;

const W: usize = 176;
const H: usize = 144;
const FL: usize = W * H * 3 / 2;

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(name)
}

fn decode_luma_frames(path: &PathBuf) -> Vec<Vec<u8>> {
    let file = File::open(path).expect("open mp4");
    let mut demux = Mp4Demuxer::new(Box::new(BufReader::new(file))).expect("demux");
    let header = demux.read_header().expect("header");
    let video = header
        .streams
        .iter()
        .find(|s| s.media_type == ye_codec::MediaType::Video)
        .expect("video stream")
        .clone();
    let mut dec = H264Decoder::new(&video.codec).expect("decoder");
    let mut frames = Vec::new();
    let mut push = |dec: &mut H264Decoder, frames: &mut Vec<Vec<u8>>| loop {
        match dec.pull().expect("pull") {
            Decoded::Frame(Frame::Video(f)) => {
                let stride = f.layout().plane(0).stride;
                let luma: Vec<u8> = f.plane(0)[..H * stride]
                    .chunks_exact(stride)
                    .flat_map(|row| row[..W].to_vec())
                    .collect();
                frames.push(luma);
            }
            Decoded::Frame(Frame::Audio(_)) => {}
            Decoded::NeedInput | Decoded::Finished => break,
        }
    };
    while let Some(pkt) = demux.read_packet().expect("packet") {
        let mut packet = ye_codec::Packet::empty(0);
        packet.data = pkt.data;
        if dec.feed(&packet).is_ok() {
            push(&mut dec, &mut frames);
        } else {
            break;
        }
    }
    dec.flush();
    push(&mut dec, &mut frames);
    frames
}

#[test]
fn ye_inject_child() {
    // 由父枚举测试驱动：读 YE_INJECT_ABS/YE_INJECT_S，解码 aind.mp4，
    // 落盘帧 1 luma 到 ye-dump-f1.y（YE_DUMP_Y 由 cabac.rs 钩子处理）。
    let frames = decode_luma_frames(&tmp("aind.mp4"));
    let _ = std::fs::write(tmp("ye-child-nframes.txt"), format!("{}", frames.len()));
}

#[test]
fn inject_scan_frame1_mb2_6() {
    if std::env::var_os("YE_CHILD").is_some() {
        return; // 子进程不枚举
    }
    let mut raw = Vec::new();
    File::open(tmp("aind.yuv"))
        .and_then(|mut f| f.read_to_end(&mut raw))
        .expect("读 aind.yuv");
    let f1_ref = &raw[FL..2 * FL][..W * H];

    // (2,6) 的绝对 D-bin 范围（binlog 实测 55671..56014，注入点在区间内）
    let lo = 55_671usize;
    let hi = 56_014usize;
    let slots: Vec<usize> = {
        let mut v: Vec<usize> = (85..=105).collect();
        v.extend([120, 134, 149, 152, 166, 181, 195, 210, 213, 227, 237]);
        v.extend(248..=276);
        v
    };

    let out_path = tmp("ye-inject-hits.txt");
    let mut hits: Vec<String> = Vec::new();
    let exe = std::env::current_exe().unwrap();
    let nslots = slots.len();
    let total = (hi - lo) * nslots;
    eprintln!("scan {total} combos ({nslots} slots x {} pos)", hi - lo);
    for k in lo..hi {
        for &s in &slots {
            let _ = std::fs::remove_file(tmp("ye-dump-f1.y"));
            let out = Command::new(&exe)
                .args(["ye_inject_child", "--exact", "--nocapture", "--test-threads=1"])
                .env("YE_CHILD", "1")
                .env("YE_INJECT_ABS", k.to_string())
                .env("YE_INJECT_S", s.to_string())
                .env("YE_DUMP_Y", "2")
                .output()
                .expect("child");
            if !out.status.success() {
                continue;
            }
            if let Ok(ours) = std::fs::read(tmp("ye-dump-f1.y")) {
                if ours.len() == W * H {
                    let mut bad = 0usize;
                    for y in 0..16usize {
                        for x in 0..16usize {
                            if ours[(96 + y) * W + 32 + x] != f1_ref[(96 + y) * W + 32 + x] {
                                bad += 1;
                            }
                        }
                    }
                    if bad == 0 {
                        let line = format!("k={k} s={s} mb26_clean");
                        eprintln!("HIT {line}");
                        hits.push(line);
                    }
                }
            }
        }
        if k % 20 == 0 {
            eprintln!("...pos {k} ({} done, hits {})", (k - lo) * nslots, hits.len());
            let mut f = File::create(&out_path).unwrap();
            for l in &hits {
                let _ = writeln!(f, "{l}");
            }
        }
    }
    let mut f = File::create(&out_path).unwrap();
    for l in &hits {
        let _ = writeln!(f, "{l}");
    }
    eprintln!("total hits: {}", hits.len());
}

#[test]
fn subst_scan_frame1() {
    if std::env::var_os("YE_CHILD").is_some() {
        return;
    }
    let mut raw = Vec::new();
    File::open(tmp("aind.yuv"))
        .and_then(|mut f| f.read_to_end(&mut raw))
        .expect("读 aind.yuv");
    let f1_ref = &raw[FL..2 * FL][..W * H];
    let lo = 55_671usize;
    let hi = 56_014usize;
    let slots: Vec<usize> = {
        let mut v: Vec<usize> = (85..=105).collect();
        v.extend([120, 134, 149, 152, 166, 181, 195, 210, 213, 227, 237]);
        v.extend(248..=276);
        v.extend([399, 400, 60, 61, 62, 63]);
        v
    };
    let out_path = tmp("ye-subst-hits.txt");
    let mut hits: Vec<String> = Vec::new();
    let exe = std::env::current_exe().unwrap();
    let nslots = slots.len();
    eprintln!("subst scan {} combos", (hi - lo) * nslots);
    for k in lo..hi {
        for &s in &slots {
            let _ = std::fs::remove_file(tmp("ye-dump-f1.y"));
            let out = Command::new(&exe)
                .args(["ye_inject_child", "--exact", "--nocapture", "--test-threads=1"])
                .env("YE_CHILD", "1")
                .env("YE_SUBST_ABS", k.to_string())
                .env("YE_SUBST_S", s.to_string())
                .env("YE_DUMP_Y", "2")
                .output()
                .expect("child");
            if !out.status.success() {
                continue;
            }
            if let Ok(ours) = std::fs::read(tmp("ye-dump-f1.y")) {
                if ours.len() == W * H {
                    let mut bad = 0usize;
                    for y in 0..16usize {
                        for x in 0..16usize {
                            if ours[(96 + y) * W + 32 + x] != f1_ref[(96 + y) * W + 32 + x] {
                                bad += 1;
                            }
                        }
                    }
                    if bad == 0 {
                        let line = format!("k={k} s={s} mb26_clean");
                        eprintln!("HIT {line}");
                        hits.push(line);
                    }
                }
            }
        }
        if k % 20 == 0 {
            eprintln!("...pos {k} hits {}", hits.len());
            let mut f = File::create(&out_path).unwrap();
            for l in &hits {
                let _ = writeln!(f, "{l}");
            }
        }
    }
    let mut f = File::create(&out_path).unwrap();
    for l in &hits {
        let _ = writeln!(f, "{l}");
    }
    eprintln!("total hits: {}", hits.len());
}
