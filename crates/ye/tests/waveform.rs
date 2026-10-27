//! 波形消费助手真解码路径测试（分级 skip 语义）。
//!
//! 定位器（`tests/common/mod.rs`，§2.5 统一口径）命中 → 按 §3.1 AAC
//! 配方生成向量、`audio_peaks`/`audio_peaks_with_count` 真解码实跑；
//! 落空 → common 已打印原因，测试合法 skip 返回（CI 无 duan 检出恒
//! skip，门禁不红；本地收口必须实跑全绿）。
//!
//! 本文件只做结构/布局断言（桶数=请求值、声道/采样率、非静默）——
//! **真向量逐桶数值对拍收口归 T3**（ffmpeg 参考侧喂同一份纯核），
//! 此处不重复其口径。

#![cfg(all(feature = "mp4", feature = "aac"))]

mod common;

use std::path::Path;
use std::sync::atomic::AtomicBool;

use ye::container::ReadSeek;
use ye::waveform::{audio_peaks, audio_peaks_with_count};

/// 打开向量文件为泵输入。
fn open_vector(path: &Path) -> Box<dyn ReadSeek> {
    Box::new(std::fs::File::open(path).expect("打开向量"))
}

/// 定位器命中时桶数入口真解码：返回桶数等于请求值、声道/采样率正确、
/// 正弦桶非静默（vec-audio：立体声 440Hz 正弦 48k、2s）。
#[test]
fn 定位器命中时桶数入口真解码返回请求桶数() {
    let Some(path) = common::ensure_vector("vec-audio.m4a", common::VEC_AUDIO_ARGS) else {
        return; // 定位器落空：common 已打印原因，合法 skip（分级语义）。
    };
    let result = audio_peaks_with_count(open_vector(&path), 100, &AtomicBool::new(false));
    let _ = std::fs::remove_file(&path); // 用后即删（仓库约定）。
    let peaks = result.expect("真解码应成功");
    assert_eq!(peaks.buckets, 100, "桶数入口返回桶数 = 请求值");
    assert_eq!(peaks.channels, 2, "vec-audio 为立体声");
    assert_eq!(peaks.sample_rate, 48_000, "vec-audio 采样率 48k");
    assert_eq!(peaks.peaks.len(), 2);
    assert_eq!(peaks.peaks[0].len(), 100);
    // lavfi sine 源固有幅度 1/8（实测解码峰值 ≈0.092）；头部 AAC
    // priming（约 1029 帧 < 2 桶）使首桶近静默——非静默桶 ≥98 即结构
    // 正常（数值对拍归 T3，只锁结构级下界）。
    let loud = peaks.peaks[0]
        .iter()
        .filter(|bucket| bucket.max > 0.05)
        .count();
    assert!(loud >= 98, "非静默桶 {loud}/100");
    assert!(peaks.peaks[0][2].max > 0.05, "中段桶应有幅度");
}

/// 定位器命中时桶长入口流式真解码：声道/采样率/桶长正确、桶数覆盖
/// 2s 素材（vec-noise：单声道白噪声 44.1k、2s）。
#[test]
fn 定位器命中时桶长入口流式真解码分桶() {
    let Some(path) = common::ensure_vector("vec-noise.m4a", common::VEC_NOISE_ARGS) else {
        return; // 定位器落空：合法 skip。
    };
    // 0.1s/桶（4410 帧）：2s ≈ 88200 帧 + AAC priming → 20~21 桶。
    let result = audio_peaks(open_vector(&path), 4410, &AtomicBool::new(false));
    let _ = std::fs::remove_file(&path);
    let peaks = result.expect("真解码应成功");
    assert_eq!(peaks.channels, 1, "vec-noise 为单声道");
    assert_eq!(peaks.sample_rate, 44_100, "vec-noise 采样率 44.1k");
    assert_eq!(peaks.bucket_samples, 4410, "桶长 = 请求值");
    // 下界防漏解码、上界防重复混桶（priming 口径不锁死，收口归 T3）。
    assert!(
        (20..=21).contains(&peaks.buckets),
        "2s @44.1k 桶长 4410 → 20~21 桶，实得 {}",
        peaks.buckets
    );
    assert_eq!(peaks.peaks[0].len(), peaks.buckets);
    // 白噪声非静默（中段桶避开头部 priming 静音）。
    assert!(
        peaks.peaks[0][10].max > 0.05,
        "中段桶应有幅度：{:?}",
        peaks.peaks[0][10]
    );
}
