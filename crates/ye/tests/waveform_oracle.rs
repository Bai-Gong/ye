//! 波形 oracle 对拍（m2-jian-plan §3.2，T3 ye 侧；jian 侧 T8 同口径）。
//!
//! 参考侧 ffmpeg `-ignore_editlist 1`（**mov 解封装器私有选项，必须在
//! -i 之前**，坑 §1.4-5）解到 packed f32le，喂**同一份分桶纯核**
//! [`peaks_by_bucket_count`]——口径自洽：两侧桶边界公式同源，差异只来自
//! 解码器。ye 轨按裸码流语义输出全部含 priming 帧（conformance
//! README:113-116），ffmpeg 侧不忽略 edit list 即含同样的 priming/padding
//! 样本，两侧样本序列对齐。
//!
//! **两条向量都要过**（正弦对 min/max 敏感、白噪声对错位敏感——能暴露
//! edit list 口径错误，评审问题 4）。向量配方与定位器复用
//! `tests/common/mod.rs`（T1 属主，只读不改）；skip 语义分级同处：
//! 定位器落空 → common 已打印原因，测试合法 skip 返回；命中后的任何
//! 失败（参考侧 ffmpeg 退出非零/输出未落盘）＝测试失败，不许 skip。

#![cfg(all(feature = "mp4", feature = "aac"))]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;

use ye::container::ReadSeek;
use ye::waveform::{PeakBucket, audio_peaks_with_count, peaks_by_bucket_count};

/// POLICY_NOTE（沿 conformance README:129-136 与 ye-filter mod.rs 先例）：
/// 下述容差常量**只准收紧**——向更严方向修改免评审；任何放宽必须回
/// 施工图（m2-jian-plan §3.2）重新定口径。初值 2e-4（依据：AAC 解码
/// 对拍口径 100% 样本 |Δ|≤4 LSB(s16)，conformance README:119，峰值差
/// 最坏即样本差上界，4/32768 ≈ 1.22e-4，留余量取 2e-4）。
///
/// **实测收紧（2026-10-27，duan binaries ffmpeg N-127696）**：正弦
/// （vec-audio，立体声 48k，确定性源）|Δmin|=1.840e-6、|Δmax|=5.022e-6、
/// |Δrms|=4.470e-8；白噪声（vec-noise，单声道 44.1k，anoisesrc 未钉
/// 种子 → 内容逐次随机，7 次采样）|Δmax| 分布 5.5e-6 ~ 8.810e-5、
/// |Δrms| ≤ 4.2e-7。全流最坏 8.810e-5 → 收紧至 1.5e-4（最坏值 1.7×
/// 余量；随机向量留抽样余量，进一步收紧须先钉 anoisesrc seed——配方
/// 属 T1 common/mod.rs，改动走属主）。
const PEAK_TOL: f32 = 1.5e-4;

/// 对拍桶数（§3.2：100——2 秒向量桶长 ~960 样本）。
const BUCKETS: usize = 100;

/// 打开向量文件为门面输入（与 tests/waveform.rs 同法）。
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
        "ye-waveform-oracle-{tag}-{}-{nanos}.{ext}",
        std::process::id()
    ))
}

/// 参考侧 ffmpeg 解码全量样本（f32le 落盘 → 读回 f32）。
///
/// 调用前置：定位器已命中（`ensure_vector` 已返回路径）。本函数内任何
/// 失败（启动失败/退出非零/输出未落盘）＝ panic（测试失败），不许 skip。
fn reference_samples(exe: &Path, vec: &Path, tag: &str) -> Vec<f32> {
    let out = ref_path(tag, "f32");
    let status = Command::new(exe)
        .args(["-v", "error", "-ignore_editlist", "1", "-i"])
        .arg(vec)
        .args(["-map", "0:a:0", "-f", "f32le", "-c:a", "pcm_f32le"])
        .arg(&out)
        .status()
        .unwrap_or_else(|err| panic!("启动 ffmpeg 失败（{}）：{err}", exe.display()));
    assert!(
        status.success(),
        "ffmpeg 参考侧解 f32le 失败（退出码 {status:?}）——定位器已命中，不许 skip"
    );
    let bytes = std::fs::read(&out)
        .unwrap_or_else(|err| panic!("读参考侧输出失败（{}）：{err}", out.display()));
    let _ = std::fs::remove_file(&out); // 用后即删（仓库约定）。
    assert!(!bytes.is_empty(), "参考侧输出为空——2 秒向量应有实样本");
    assert!(
        bytes.len().is_multiple_of(4),
        "f32le 输出字节数 {} 非 4 整倍数",
        bytes.len()
    );
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

/// 三元组差的最坏值（诊断与收紧依据）。
#[derive(Debug, Clone, Copy)]
struct WorstDiff {
    /// max |Δmin|。
    min: f32,
    /// max |Δmax|。
    max: f32,
    /// max |Δrms|。
    rms: f32,
}

/// 逐声道逐桶对拍：结构（声道数/桶数）精确等，三元组 |Δ| 全在容差内。
///
/// 返回实测最坏差（调用方打印留档——收紧容差的依据）。
fn assert_peaks_close(got: &[Vec<PeakBucket>], want: &[Vec<PeakBucket>], ctx: &str) -> WorstDiff {
    assert_eq!(
        got.len(),
        want.len(),
        "{ctx}：声道数应一致（ye {} vs 参考 {}）",
        got.len(),
        want.len()
    );
    let mut worst = WorstDiff {
        min: 0.0,
        max: 0.0,
        rms: 0.0,
    };
    for (channel, (ye_row, ref_row)) in got.iter().zip(want).enumerate() {
        assert_eq!(
            ye_row.len(),
            ref_row.len(),
            "{ctx}：声道 {channel} 桶数应一致（ye {} vs 参考 {}）",
            ye_row.len(),
            ref_row.len()
        );
        for (bucket, (ye_b, ref_b)) in ye_row.iter().zip(ref_row).enumerate() {
            let (dmin, dmax, drms) = (
                (ye_b.min - ref_b.min).abs(),
                (ye_b.max - ref_b.max).abs(),
                (ye_b.rms - ref_b.rms).abs(),
            );
            worst.min = worst.min.max(dmin);
            worst.max = worst.max.max(dmax);
            worst.rms = worst.rms.max(drms);
            assert!(
                dmin <= PEAK_TOL && dmax <= PEAK_TOL && drms <= PEAK_TOL,
                "{ctx}：声道 {channel} 桶 {bucket} 超容差 {PEAK_TOL}——\
                 ye {ye_b:?} vs 参考 {ref_b:?}（|Δmin|={dmin} |Δmax|={dmax} \
                 |Δrms|={drms}）"
            );
        }
    }
    worst
}

/// 一条向量的完整对拍：参考侧纯核 vs `audio_peaks_with_count`。
///
/// 打印实测最坏差（收紧容差的留档数字）。
fn run_oracle(vec_name: &str, recipe: &[&str], channels: usize) {
    let Some(path) = common::ensure_vector(vec_name, recipe) else {
        return; // 定位器落空：common 已打印原因，合法 skip（分级语义）。
    };
    let exe = common::ffmpeg_exe().expect("定位器已命中（向量已生成）");
    let tag = Path::new(vec_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(vec_name);
    let reference = reference_samples(&exe, &path, tag);
    let ref_peaks =
        peaks_by_bucket_count(&reference, channels, BUCKETS).expect("参考侧纯核分桶应成功");
    let ye_peaks = audio_peaks_with_count(open_vector(&path), BUCKETS, &AtomicBool::new(false))
        .expect("ye 轨真解码应成功");
    let _ = std::fs::remove_file(&path); // 向量用后即删（仓库约定）。
    assert_eq!(ye_peaks.channels, channels, "{vec_name}：声道数");
    assert_eq!(
        ye_peaks.buckets, BUCKETS,
        "{vec_name}：桶数入口返回桶数 = 请求值"
    );
    let worst = assert_peaks_close(&ye_peaks.peaks, &ref_peaks, vec_name);
    eprintln!(
        "waveform_oracle[{vec_name}]：实测最坏差 |Δmin|={:.3e} |Δmax|={:.3e} \
         |Δrms|={:.3e}（容差 {PEAK_TOL:e}，全过）",
        worst.min, worst.max, worst.rms
    );
}

/// 正弦向量（vec-audio：立体声 440Hz 48k、2s）——对 min/max 敏感。
#[test]
fn 正弦向量逐桶峰值与ffmpeg参考一致() {
    run_oracle("vec-audio.m4a", common::VEC_AUDIO_ARGS, 2);
}

/// 白噪声向量（vec-noise：单声道 44.1k、2s）——对样本错位敏感（错一帧
/// 即 rms 大幅漂移，能暴露 edit list 口径错误）。
#[test]
fn 白噪声向量逐桶峰值与ffmpeg参考一致() {
    run_oracle("vec-noise.m4a", common::VEC_NOISE_ARGS, 1);
}
