//! 抽帧 oracle 对拍（m2-jian-plan §3.3，T3 ye 侧；jian 侧 T8 同口径）。
//!
//! 参考侧 `ffmpeg -ss {t} -i vec.mp4 -frames:v 1 -f rawvideo -pix_fmt
//! rgba`（`-ss` 在 -i 前：输入侧定位，解码到目标帧即止——与 ye 门面
//! 「取首个 pts ≥ 时点的帧即置 cancel」同语义，两侧取到同一物理帧）。
//! 点位 §3.1：`t = f × 33333 + 5000`（µs），f ∈ {0, 45, 88}。
//!
//! 向量 lavfi 写死（§3.1）：vec.mp4 = testsrc2 Baseline（ye 只收
//! Baseline/CAVLC）；vec-video-multi.mp4 = 同视频 + `-ac 6` AAC 音轨——
//! 该向量**不做像素对拍**，只断言 ye 侧整体 Err 与 ffmpeg 参考侧可正常
//! 出帧：差异即「该类素材走 ffmpeg 回退」的依据（行为锁定）。定位器与
//! skip 语义分级复用 `tests/common/mod.rs`（T1 属主，只读不改）：定位器
//! 落空 → common 已打印原因，测试合法 skip；命中后的任何失败（参考侧
//! ffmpeg 退出非零/输出未落盘）＝测试失败，不许 skip。
//!
//! ## 交付状态（2026-10-27 更新：解码器分歧已收口，对拍过门）
//!
//! 2026-10-27 早班以 `#[ignore]` 交付两处超差（首差帧 0 luma / 81
//! chroma，90 帧 max_abs=74~76，rgba 域三点位 21/35/130）；同日解码器
//! 修复收口（ye-codec-h264：去块延后到图级 + pred_8x16 C→D 回退 +
//! chroma QP 偏移接线）后 `#[ignore]` 翻除，两条用例过门。实测留档见
//! 各用例注释。vec-video-multi 行为锁定照常生效。
//!
//! 像素转换口径的前提（已实测锁定，2026-10-27）：两侧转换口径同源
//! （swscale 逐像素表路径，ye-filter convert.rs 头注），像素对拍的全部
//! 余量预算留给解码侧。

#![cfg(all(feature = "mp4", feature = "h264", feature = "png"))]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use ye::container::ReadSeek;
use ye::frame_grab::decode_frame_rgba;

/// POLICY_NOTE（沿 conformance README:129-136 先例）：下述容差常量
/// **只准收紧**——向更严方向修改免评审；任何放宽必须回施工图
/// （m2-jian-plan §3.3）重新定口径。初值依据：H.264 YUV 口径 100%
/// |Δ|≤1 且 99.9% Δ=0（conformance README:91-94），叠加 yuv420p→rgba8
/// BT.601 转换两侧舍入路径差异放一档（100% 通道 |Δ|≤2 且 Δ=0 通道
/// 占比 ≥99%）。实测数字与收紧记录见各用例注释。
const CHANNEL_TOL: u32 = 2;

/// Δ=0 通道占比下限（同上 POLICY_NOTE 管辖：只准上调）。
const ZERO_RATIO_MIN: f64 = 0.99;

/// 解码 YUV 级对拍容差（M2 一致性 YUV 口径同数：逐帧 100% 样本 |Δ|≤1
/// 且该帧 Δ=0 占比 ≥999‰；POLICY_NOTE 同上只准收紧）。
const YUV_SAMPLE_TOL: u32 = 1;

/// 解码 YUV 级对拍逐帧 Δ=0 占比下限（999‰；POLICY_NOTE 只准上调）。
const YUV_ZERO_RATIO_MIN: f64 = 0.999;

/// 向量宽高（testsrc2 320x240）。
const W: u32 = 320;
const H: u32 = 240;

/// vec.mp4 配方（§3.1：testsrc2 3s 320x240 30fps，libx264 Baseline
/// yuv420p——ye H.264 只收 Baseline/CAVLC）。
const VEC_VIDEO_ARGS: &[&str] = &[
    "-v",
    "error",
    "-f",
    "lavfi",
    "-i",
    "testsrc2=duration=3:size=320x240:rate=30",
    "-c:v",
    "libx264",
    "-profile:v",
    "baseline",
    "-pix_fmt",
    "yuv420p",
    "-y",
];

/// vec-video-multi.mp4 配方（§3.1：上述视频 + 440Hz 正弦 AAC **6 声道**
/// ——ye AAC 拒多声道，作「不可解音轨」源）。与消费它的测试同挂
/// `aac` feature 门（未开 aac 时该向量经 `NotRegistered` 打回的行为已由
/// frame_grab 模块文档锁定，无需重复配方）。
#[cfg(feature = "aac")]
const VEC_MULTI_ARGS: &[&str] = &[
    "-v",
    "error",
    "-f",
    "lavfi",
    "-i",
    "testsrc2=duration=3:size=320x240:rate=30",
    "-f",
    "lavfi",
    "-i",
    "sine=frequency=440:sample_rate=48000:duration=3",
    "-map",
    "0:v",
    "-map",
    "1:a",
    "-c:v",
    "libx264",
    "-profile:v",
    "baseline",
    "-c:a",
    "aac",
    "-ac",
    "6",
    "-aac_pns",
    "0",
    "-y",
];

/// 对拍点位（µs）：`t = f × 33333 + 5000`（§3.1 修正案）。
fn point_us(frame: i64) -> i64 {
    frame * 33_333 + 5_000
}

/// 打开向量文件为门面输入。
fn open_vector(path: &Path) -> Box<dyn ReadSeek> {
    Box::new(std::fs::File::open(path).expect("打开向量"))
}

/// 参考侧临时文件路径（CARGO_TARGET_TMPDIR 唯一名，用后即删归调用方）。
fn ref_path(tag: &str, ext: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "ye-frame-grab-oracle-{tag}-{}-{nanos}.{ext}",
        std::process::id()
    ))
}

/// 参考侧 ffmpeg 单帧 rgba（`-ss` 在 -i 前；输出恰 `W×H×4` 字节）。
///
/// 调用前置：定位器已命中（`ensure_vector` 已返回路径）。本函数内任何
/// 失败（启动失败/退出非零/输出尺寸不符）＝ panic（测试失败），不许
/// skip。
fn reference_frame(exe: &Path, vec: &Path, t_us: i64, tag: &str) -> Vec<u8> {
    let out = ref_path(tag, "rgba");
    let seconds = format!(
        "{:.6}",
        f64::from(i32::try_from(t_us).expect("时点在 i32 域")) / 1e6
    );
    let status = Command::new(exe)
        .args(["-v", "error", "-ss"])
        .arg(&seconds)
        .arg("-i")
        .arg(vec)
        .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgba"])
        .arg(&out)
        .status()
        .unwrap_or_else(|err| panic!("启动 ffmpeg 失败（{}）：{err}", exe.display()));
    assert!(
        status.success(),
        "ffmpeg 参考侧抽帧失败（退出码 {status:?}）——定位器已命中，不许 skip"
    );
    let rgba = std::fs::read(&out)
        .unwrap_or_else(|err| panic!("读参考侧输出失败（{}）：{err}", out.display()));
    let _ = std::fs::remove_file(&out); // 用后即删（仓库约定）。
    assert_eq!(
        rgba.len(),
        (W * H * 4) as usize,
        "{tag}：参考侧应恰出一帧 {W}x{H} rgba"
    );
    rgba
}

/// 参考侧 ffmpeg 全量解码 yuv420p（`-pix_fmt yuv420p -f rawvideo`，M2
/// 一致性 YUV 口径同命令；失败语义同 [`reference_frame`]）。
fn reference_yuv(exe: &Path, vec: &Path, tag: &str) -> Vec<u8> {
    let out = ref_path(tag, "yuv");
    let status = Command::new(exe)
        .args(["-v", "error", "-i"])
        .arg(vec)
        .args(["-map", "0:v:0", "-pix_fmt", "yuv420p", "-f", "rawvideo"])
        .arg(&out)
        .status()
        .unwrap_or_else(|err| panic!("启动 ffmpeg 失败（{}）：{err}", exe.display()));
    assert!(
        status.success(),
        "ffmpeg 参考侧解 yuv 失败（退出码 {status:?}）——定位器已命中，不许 skip"
    );
    let yuv = std::fs::read(&out)
        .unwrap_or_else(|err| panic!("读参考侧输出失败（{}）：{err}", out.display()));
    let _ = std::fs::remove_file(&out); // 用后即删（仓库约定）。
    yuv
}

/// 逐通道像素对拍（容差见 POLICY_NOTE；返回 (max |Δ|, Δ=0 占比) 供打印）。
///
/// 先全量统计（超差不提前 panic——留全量实测数字，收紧容差的依据），
/// 再按容差断言。
fn assert_pixels_close(got: &[u8], want: &[u8], ctx: &str) -> (u32, f64) {
    assert_eq!(got.len(), want.len(), "{ctx}：字节数");
    let mut max_diff = 0u32;
    let mut zero = 0usize;
    let mut worst_at = 0usize;
    for (index, (a, b)) in got.iter().zip(want).enumerate() {
        let diff = u32::from(a.abs_diff(*b));
        if diff > max_diff {
            max_diff = diff;
            worst_at = index;
        }
        if diff == 0 {
            zero += 1;
        }
    }
    let ratio = zero as f64 / got.len() as f64;
    eprintln!(
        "{ctx}：实测 max |Δ|={max_diff}（最坏字节 {worst_at}：ye {} vs 参考 {}），\
         Δ=0 通道占比 {ratio:.5}",
        got[worst_at], want[worst_at]
    );
    assert!(
        max_diff <= CHANNEL_TOL,
        "{ctx}：max |Δ|={max_diff} > {CHANNEL_TOL}"
    );
    assert!(
        ratio >= ZERO_RATIO_MIN,
        "{ctx}：Δ=0 通道占比 {ratio:.5} < {ZERO_RATIO_MIN}"
    );
    (max_diff, ratio)
}

/// vec.mp4 三点位像素对拍：ye `decode_frame_rgba` vs ffmpeg 同点位
/// rawvideo（宽高精确等后逐像素比）。
///
/// 实测留档（2026-10-27 修复收口后）：三点位 max |Δ|=1、Δ=0 通道占比
/// ≥0.9998（解码 YUV 与 ffmpeg 逐位一致——余量全部来自 yuv420p→rgba
/// 两侧舍入路径，见下方 YUV 级用例 max |Δ|=0）。
#[test]
fn 基线向量三时点抽帧与ffmpeg参考一致() {
    let Some(path) = common::ensure_vector("vec.mp4", VEC_VIDEO_ARGS) else {
        return; // 定位器落空：common 已打印原因，合法 skip（分级语义）。
    };
    let exe = common::ffmpeg_exe().expect("定位器已命中（向量已生成）");
    for frame in [0i64, 45, 88] {
        let t = point_us(frame);
        let got = decode_frame_rgba(open_vector(&path), t)
            .unwrap_or_else(|err| panic!("帧 {frame}（t={t}µs）ye 侧取帧应成功：{err}"));
        assert_eq!((got.width, got.height), (W, H), "帧 {frame}：宽高");
        let want = reference_frame(&exe, &path, t, &format!("vec-f{frame}"));
        assert_pixels_close(
            &got.rgba,
            &want,
            &format!("frame_grab_oracle[vec.mp4 f={frame}]"),
        );
    }
    let _ = std::fs::remove_file(&path); // 向量用后即删（仓库约定）。
}

/// vec.mp4 解码 YUV 级对拍：ye `H264Decoder` 逐帧 yuv420p vs ffmpeg
/// rawvideo（M2 一致性 YUV 口径：逐帧 100% 样本 |Δ|≤1 且该帧 Δ=0 占比
/// ≥999‰）。像素对拍的差分定位依据（隔离转换核后仍超差即解码侧问题）。
///
/// 实测留档（2026-10-27 修复收口后，YE_H264_SCRATCH_VEC 诊断通道
/// 同口径复核）：去块启用配方 90 帧 max_abs=0、逐位一致；同配方
/// `-x264-params no-deblock=1` 对照向量 max_abs=1（帧 81 起 Cr 平面
/// 孤立 ±1 样本，300 帧扩展实测 24 帧 × 每帧 ≤2 样本，M2 YUV 容差
/// 内；余留登记见 ye-codec-h264 tests/conformance.rs ip30 注释）。
/// 两条新向量已入清单 set=ip30（manifest.toml，配方重放防退化）。
#[test]
fn 基线向量解码yuv逐帧与ffmpeg参考一致() {
    use ye::codec::{Decoded, Decoder, MediaType};
    use ye::codec_h264::H264Decoder;
    use ye::container::Demuxer;
    use ye::container::mp4::Mp4Demuxer;

    let Some(path) = common::ensure_vector("vec.mp4", VEC_VIDEO_ARGS) else {
        return; // 定位器落空：合法 skip（分级语义）。
    };
    let exe = common::ffmpeg_exe().expect("定位器已命中（向量已生成）");
    let reference = reference_yuv(&exe, &path, "vec-yuv");

    // ye 侧：Mp4Demuxer 逐包喂 H264Decoder，帧收为行紧致 yuv420p 拼接
    // （与 ye-codec-h264 一致性测试同法）。
    let file = std::fs::File::open(&path).expect("打开向量");
    let mut demux = Mp4Demuxer::new(Box::new(file)).expect("mp4 解封装");
    let header = demux.read_header().expect("读 mp4 头");
    let video = header
        .streams
        .iter()
        .find(|s| s.media_type == MediaType::Video)
        .expect("向量含视频流");
    let mut dec = H264Decoder::new(&video.codec).expect("构造 H264Decoder");
    let _ = std::fs::remove_file(&path); // 向量用后即删（仓库约定）。
    let mut our: Vec<u8> = Vec::new();
    let push = |frame: &ye::codec::VideoFrame, our: &mut Vec<u8>| {
        let layout = *frame.layout();
        let (w, h) = (layout.width() as usize, layout.height() as usize);
        let cw = w.div_ceil(2);
        for p in 0..3usize {
            let pl = layout.plane(p);
            let (rows, cols) = if p == 0 { (h, w) } else { (h.div_ceil(2), cw) };
            for y in 0..rows {
                let s = y * pl.stride;
                our.extend_from_slice(&frame.plane(p)[s..s + cols]);
            }
        }
    };
    let drain = |dec: &mut H264Decoder,
                 our: &mut Vec<u8>,
                 push: &dyn Fn(&ye::codec::VideoFrame, &mut Vec<u8>)| {
        loop {
            match dec.pull().expect("pull") {
                Decoded::Frame(f) => {
                    let ye::codec::Frame::Video(vf) = f else {
                        panic!("视频流解出非视频帧");
                    };
                    push(&vf, our);
                }
                Decoded::NeedInput => break,
                Decoded::Finished => break,
            }
        }
    };
    while let Some(pkt) = demux.read_packet().expect("读包") {
        dec.feed(&pkt).expect("feed");
        drain(&mut dec, &mut our, &push);
    }
    dec.flush();
    loop {
        match dec.pull().expect("pull after flush") {
            Decoded::Frame(f) => {
                let ye::codec::Frame::Video(vf) = f else {
                    panic!("flush 后非视频帧");
                };
                push(&vf, &mut our);
            }
            Decoded::Finished => break,
            Decoded::NeedInput => panic!("flush 后不应 NeedInput"),
        }
    }

    // 逐帧对拍（M2 YUV 口径；单帧劣化不被其他帧摊薄）。
    let frame_len = (W as usize) * (H as usize) * 3 / 2;
    assert_eq!(
        our.len() % frame_len,
        0,
        "ye 输出应为整数帧 yuv420p（{} 字节）",
        our.len()
    );
    assert_eq!(
        our.len(),
        reference.len(),
        "两侧总样本数应一致（ye {} vs 参考 {}）",
        our.len(),
        reference.len()
    );
    let frames = our.len() / frame_len;
    let mut global_max = 0u32;
    for f in 0..frames {
        let (mut max_diff, mut zero, mut worst) = (0u32, 0usize, 0usize);
        for (i, (a, b)) in our[f * frame_len..][..frame_len]
            .iter()
            .zip(&reference[f * frame_len..][..frame_len])
            .enumerate()
        {
            let diff = u32::from(a.abs_diff(*b));
            if diff > max_diff {
                max_diff = diff;
                worst = i;
            }
            zero += usize::from(diff == 0);
        }
        global_max = global_max.max(max_diff);
        let ratio = zero as f64 / frame_len as f64;
        assert!(
            max_diff <= YUV_SAMPLE_TOL && ratio >= YUV_ZERO_RATIO_MIN,
            "帧 {f}：max |Δ|={max_diff}（样本 {worst}：ye {} vs 参考 {}），\
             Δ=0 占比 {ratio:.5}——超 M2 YUV 口径（|Δ|≤{YUV_SAMPLE_TOL} 且 \
             ≥{YUV_ZERO_RATIO_MIN}）",
            our[f * frame_len + worst],
            reference[f * frame_len + worst]
        );
    }
    eprintln!(
        "frame_grab_oracle[vec.mp4 yuv]：{frames} 帧逐帧对拍全过（全流 max |Δ|={global_max}）"
    );
}

/// vec-video-multi.mp4（视频可解 + 6 声道不可解音轨）：ye 侧**整体 Err**
/// （泵给每条流建解码器，AAC 多声道解码 Err 打回整个抽帧——§2.2 行为
/// 锁定，`aac` feature 开启时锁的是「多声道拒收」本义路径；未开 aac 时
/// 同一向量经 `NotRegistered` 同样整体打回，frame_grab 模块文档已述）；
/// ffmpeg 参考侧可正常出帧——差异即「该类素材走 ffmpeg 回退」的依据，
/// 不做像素对拍。
#[cfg(feature = "aac")]
#[test]
fn 不可解音轨向量ye侧整体打回() {
    let Some(path) = common::ensure_vector("vec-video-multi.mp4", VEC_MULTI_ARGS) else {
        return; // 定位器落空：合法 skip。
    };
    let exe = common::ffmpeg_exe().expect("定位器已命中（向量已生成）");
    let t = point_us(45);
    let err = decode_frame_rgba(open_vector(&path), t)
        .expect_err("含不可解音轨的素材，ye 侧应整体 Err（不静默丢帧）");
    assert!(
        err.to_string().contains("channelConfiguration=6"),
        "打回原因应述多声道能力面外：{err}"
    );
    eprintln!("frame_grab_oracle[vec-video-multi]：ye 侧整体 Err（{err}）");
    // 参考侧对照：ffmpeg 同点位可正常出帧（行为差异即回退依据；仅断言
    // 非空尺寸，不做像素对拍——§3.3）。
    let reference = reference_frame(&exe, &path, t, "multi-f45");
    assert_eq!(reference.len(), (W * H * 4) as usize, "参考侧可正常出帧");
    let _ = std::fs::remove_file(&path); // 用后即删。
}

#[test]
fn bt709_vui_limited_and_full_range_reach_rgba_output() {
    let Some(exe) = common::ffmpeg_exe() else {
        return;
    };
    for range in ["tv", "pc"] {
        let path = std::env::temp_dir().join(format!("ye-vui-{}-{range}.mp4", std::process::id()));
        let generated = Command::new(&exe)
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "smptebars=size=320x240:rate=1",
                "-frames:v",
                "1",
                "-c:v",
                "libx264",
                "-profile:v",
                "high",
                "-colorspace",
                "bt709",
                "-color_primaries",
                "bt709",
                "-color_trc",
                "bt709",
                "-color_range",
                range,
            ])
            .arg(&path)
            .output()
            .unwrap();
        assert!(
            generated.status.success(),
            "{}",
            String::from_utf8_lossy(&generated.stderr)
        );
        let ours =
            ye::frame_grab::decode_frame_rgba(Box::new(std::fs::File::open(&path).unwrap()), 0)
                .unwrap();
        let reference = Command::new(&exe)
            .args(["-v", "error", "-i"])
            .arg(&path)
            .args(["-frames:v", "1", "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
            .output()
            .unwrap();
        assert!(reference.status.success());
        assert_eq!(ours.rgba.len(), reference.stdout.len());
        let worst = ours
            .rgba
            .iter()
            .zip(&reference.stdout)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(worst <= 2, "{range} BT709 maxAbs={worst}");
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn high_b_frames_random_timestamps_match_oracle() {
    let Some(exe) = common::ffmpeg_exe() else {
        return;
    };
    let path = std::env::temp_dir().join(format!("ye-random-b-{}.mp4", std::process::id()));
    let out = Command::new(&exe)
        .args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=320x240:rate=30000/1001:duration=3",
            "-c:v",
            "libx264",
            "-profile:v",
            "high",
            "-g",
            "30",
            "-bf",
            "3",
        ])
        .arg(&path)
        .output()
        .unwrap();
    assert!(out.status.success());
    for time in [0, 155000, 850000, 1550000, 2850000] {
        let ours =
            ye::frame_grab::decode_frame_rgba(Box::new(std::fs::File::open(&path).unwrap()), time)
                .unwrap();
        let reference = reference_frame(&exe, &path, time, &format!("b-{time}"));
        let worst = ours
            .rgba
            .iter()
            .zip(&reference)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(worst <= 2, "B frame timestamp {time}: maxAbs {worst}");
    }
    std::fs::remove_file(path).unwrap();
}
