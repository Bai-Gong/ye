//! ②a 过界门禁：纯 I 向量集（清单 `set=intra`）逐帧对拍全过。
//!
//! 流程（每条向量）：
//! 1. `ye-container-mp4` 解封装取 AVCC 包 + avcC extradata；
//! 2. [`ye_codec_h264::H264Decoder`]（本 crate）解码为 yuv420p 帧；
//! 3. 写出显示尺寸、行紧致的 `<cache>/<name>.our.yuv`
//!    （runner 主口径的接线文件，写完即被 `cargo run -p ye-conformance`
//!    自动纳入对拍）；
//! 4. 本测试内再以 ffmpeg rawvideo 为 oracle、用 ye-conformance 的
//!    比较器与施工图②容差（100% |Δ|≤1 且 ≥99.9% Δ=0）独立裁决。
//!
//! 向量缺席（未跑 `bash tests/conformance/fetch.sh`）按**失败**处理——
//! 门禁不许假绿。

use std::io::Read;
use std::path::PathBuf;
use std::process::Command;

use ye_codec::{Decoded, Decoder};
use ye_codec_h264::H264Decoder;
use ye_conformance::compare::{self, ChromaSampling};
use ye_conformance::manifest::Manifest;
use ye_conformance::tolerance::{self, Tolerance};
use ye_container::Demuxer;
use ye_container_mp4::Mp4Demuxer;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn cache_dir() -> PathBuf {
    std::env::var_os("YE_CONFORMANCE_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("tests/conformance/.cache"))
}

/// 解码一条向量 → 行紧致 yuv420p 字节流（帧序列）。
fn decode_vector(mp4: &PathBuf) -> (Vec<u8>, usize, usize) {
    let file = std::fs::File::open(mp4).expect("打开向量文件");
    let mut demux = Mp4Demuxer::new(Box::new(file)).expect("mp4 解封装");
    let header = demux.read_header().expect("读 mp4 头");
    let video = header
        .streams
        .iter()
        .find(|s| s.media_type == ye_codec::MediaType::Video)
        .expect("向量含视频流");
    let mut dec = H264Decoder::new(&video.codec).expect("构造 H264Decoder");

    let mut yuv = Vec::new();
    let (mut w, mut h) = (0usize, 0usize);
    let push_frame =
        |yuv: &mut Vec<u8>, w: &mut usize, h: &mut usize, frame: ye_codec::VideoFrame| {
            let layout = *frame.layout();
            if *w == 0 {
                *w = layout.width() as usize;
                *h = layout.height() as usize;
            }
            let cw = (*w).div_ceil(2);
            for p in 0..3usize {
                let pl = layout.plane(p);
                let (rows, cols) = if p == 0 {
                    (*h, *w)
                } else {
                    ((*h).div_ceil(2), cw)
                };
                for y in 0..rows {
                    let s = y * pl.stride;
                    yuv.extend_from_slice(&frame.plane(p)[s..s + cols]);
                }
            }
        };
    while let Some(pkt) = demux.read_packet().expect("读包") {
        dec.feed(&pkt).expect("feed");
        loop {
            match dec.pull().expect("pull") {
                Decoded::Frame(f) => {
                    let ye_codec::Frame::Video(vf) = f else {
                        panic!("视频流解出非视频帧");
                    };
                    push_frame(&mut yuv, &mut w, &mut h, vf);
                }
                Decoded::NeedInput => break,
                Decoded::Finished => break,
            }
        }
    }
    dec.flush();
    loop {
        match dec.pull().expect("pull after flush") {
            Decoded::Frame(f) => {
                let ye_codec::Frame::Video(vf) = f else {
                    panic!("视频流解出非视频帧");
                };
                push_frame(&mut yuv, &mut w, &mut h, vf);
            }
            Decoded::Finished => break,
            Decoded::NeedInput => panic!("flush 后不应 NeedInput"),
        }
    }
    (yuv, w, h)
}

/// ffmpeg oracle：`-pix_fmt yuv420p -f rawvideo`。
fn ffmpeg_oracle_yuv(mp4: &PathBuf) -> Vec<u8> {
    let out = std::env::temp_dir().join(format!(
        "ye-h264-conformance-{}-{}.yuv",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let status = Command::new("ffmpeg")
        .arg("-y")
        .arg("-v")
        .arg("error")
        .arg("-i")
        .arg(mp4)
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-f")
        .arg("rawvideo")
        .arg(&out)
        .status()
        .expect("启动 ffmpeg（PATH 需有 ffmpeg）");
    assert!(status.success(), "ffmpeg oracle 解码失败");
    let mut buf = Vec::new();
    std::fs::File::open(&out)
        .and_then(|mut f| f.read_to_end(&mut buf))
        .expect("读 oracle yuv");
    let _ = std::fs::remove_file(&out);
    buf
}

fn run_set(manifest_set: &str) {
    let root = workspace_root();
    let manifest_text =
        std::fs::read_to_string(root.join("tests/conformance/manifest.toml")).expect("读清单");
    let manifest = Manifest::parse(&manifest_text).expect("清单解析");
    let intra: Vec<_> = manifest
        .vectors
        .iter()
        .filter(|v| v.set.as_deref() == Some(manifest_set))
        .collect();
    assert!(
        !intra.is_empty(),
        "清单缺 {manifest_set} 集（对拍器维护员负责）"
    );

    let cache = cache_dir();
    if let Ok(path) = std::env::var("YE_H264_SCRATCH_VEC") {
        // 诊断通道：单向量临时路径（不走清单/缓存）
        let mp4 = PathBuf::from(path);
        let (our_yuv, w, h) = decode_vector(&mp4);
        assert!(w > 0 && h > 0, "scratch：无帧输出");
        let ref_yuv = ffmpeg_oracle_yuv(&mp4);
        let report = compare::diff_yuv_frames(&our_yuv, &ref_yuv, w, h, ChromaSampling::Yuv420)
            .unwrap_or_else(|e| panic!("scratch：布局不符：{e}"));
        println!(
            "SCRATCH: frames={} max_abs={} first_diff={:?}",
            report.frames, report.max_abs, report.first_diff
        );
        if std::env::var_os("YE_H264_SCRATCH_DUMP").is_some() {
            std::fs::write(
                "C:/Users/hi/AppData/Local/Temp/our_f0.yuv",
                &our_yuv[..w * h],
            )
            .unwrap();
            println!("dumped our frame0 ({w}x{h})");
        }
        return;
    }
    for v in &intra {
        let mp4 = cache.join(format!("{}.mp4", v.name));
        assert!(
            mp4.exists(),
            "向量缺席：{}（先跑 `bash tests/conformance/fetch.sh`）",
            mp4.display()
        );

        let (our_yuv, w, h) = decode_vector(&mp4);
        assert!(w > 0 && h > 0, "{}：无帧输出", v.name);
        // 接线文件（runner 主口径消费；写齐后 cargo run -p ye-conformance 自动纳入）
        let our_path = cache.join(format!("{}.our.yuv", v.name));
        std::fs::write(&our_path, &our_yuv).expect("写 our.yuv");

        let ref_yuv = ffmpeg_oracle_yuv(&mp4);
        let report = compare::diff_yuv_frames(&our_yuv, &ref_yuv, w, h, ChromaSampling::Yuv420)
            .unwrap_or_else(|e| panic!("{}：布局不符：{e}", v.name));
        let tol = tolerance::tolerances("h264")
            .iter()
            .find_map(|t| match t {
                Tolerance::MaxAbs { .. } => Some(*t),
                _ => None,
            })
            .expect("h264 容差表含逐帧口径");
        if let Err(msg) = tol.check_yuv_frames(&report) {
            let mb = report
                .first_diff_mb(ChromaSampling::Yuv420)
                .unwrap_or((0, 0));
            panic!(
                "{}：逐帧对拍不过：{msg}（帧数 {}，max_abs={}，首差宏块 {:?}）",
                v.name, report.frames, report.max_abs, mb
            );
        }
    }
}

#[test]
fn intra_set_frame_by_frame() {
    run_set("intra");
}

#[test]
fn ip_set_frame_by_frame() {
    run_set("ip");
}
