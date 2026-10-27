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
//! 三分支门禁语义：PATH 无 ffmpeg → 整组打印跳过、不计失败（oracle
//! 缺位时对拍无从谈起，先于向量检查短路）；ffmpeg 在而向量缓存缺席
//! （未跑 `bash tests/conformance/fetch.sh`）→ 仍按**失败**处理——
//! 工具就绪却没拉向量是维护疏漏，跳过即是假绿；两者齐备 → 逐条对拍。

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

/// PATH 上是否有可用 ffmpeg（探测式启动 `-version`，不依赖 shell）。
fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
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
        // 守卫语义：入口已查 ffmpeg 在位（缺席即整组跳过），此处失联
        // 属环境竞态，按失败暴露。
        .expect("启动 ffmpeg（入口守卫已确认在位）");
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

    // 三分支守卫①：ffmpeg 缺位 → 整组跳过（先于向量缓存检查，不计失败）。
    if !ffmpeg_available() {
        eprintln!(
            "跳过 {manifest_set} 集对拍：PATH 无 ffmpeg（oracle 缺位；\
             装回 ffmpeg 并跑 `bash tests/conformance/fetch.sh` 后复测）"
        );
        return;
    }

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
            let dump = std::env::temp_dir().join("ye-h264-scratch-our-f0.yuv");
            std::fs::write(&dump, &our_yuv[..w * h]).expect("写 scratch dump");
            println!("dumped our frame0 ({w}x{h}) -> {}", dump.display());
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

/// 去块启用 + 30fps 长链回归集（set=ip30，2026-10-27 解码器分歧修复
/// 施工登记，配方与实测记录见 manifest.toml 同名节）。
///
/// 余留登记（如实，2026-10-27 二轮复核）：nodeblock 条自帧 81 起 Cr
/// 平面出现孤立 ±1 样本（300 帧扩展实测 24 帧 × 每帧 ≤2 样本）。二轮
/// 已排除：MC/参考选择/mv（逐位一致，MC 后叠加前的 dst 双端取证）、
/// 残差反量化（涉事块 L·qmul 均为 64 整倍数，无舍入空间）、idct 语义
/// （与 FFmpeg 公开 C 源逐行一致，doxygen 引文核对）。ffmpeg 二进制
/// 输出（含 `-cpuflags 0` 纯 C 路径）在涉事块上偏离其公开 C 源的数学
/// （-1>>6 应为 -1 而实测 0），且 300 帧内偏差双向出现（帧 84 ye 反高
/// 一档），单一「向零截断」模型不可解释——归档为 ffmpeg 内部舍入边
/// 角口径，无反汇编/自建 ffmpeg 工具无法进一步定位。施工图②容差
/// （100% |Δ|≤1 且 ≥99.9% Δ=0，本集实测 ≥99.998%）内过门；收紧容差
/// 前须先收口此余留。
#[test]
fn ip30_set_frame_by_frame() {
    run_set("ip30");
}
