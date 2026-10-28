//! 轻量心理声学模型：MDCT 谱 → 逐 scalefactor 频带掩蔽阈 → 逐带
//! 比例因子（sf）初值。
//!
//! ## 设计裁决（轻量 = 频域能量模型，不做 FFT/双域展开）
//!
//! 解码主链的量化噪声与信号同处 MDCT 域，直接在 MDCT 系数上做
//! 带能量统计即可获得可用的掩蔽估计（免去 ISO 心理声学模型的
//! 1024 点 FFT 与 tonality 判别）：
//!
//! 1. **带能量**：`e[j] = Σ X_k²`（频带 j 内系数平方和）；
//! 2. **简化扩散**：相邻带能量按 [`SPREAD`] 系数双向泄漏
//!    `spr[j] = e[j] + SPREAD·(e[j−1] + e[j+1])`（约 −4 dB/带，
//!    掩蔽的保守下界近似——真实扩散函数随频带间距指数衰减，
//!    单带泄漏已覆盖最主要的邻近掩蔽）；
//! 3. **掩蔽阈**：`thr[j] = max(spr[j]·MARGIN, width[j]·abs_line_e())`
//!    ——可容噪声 = 掩蔽能量降 [`MARGIN`]=30 dB（tonal 掩蔽
//!    的保守安全量），下限为绝对听阈（[`abs_line_e`] ≈ −96 dBFS
//!    谱线在系数域的能量）；
//! 4. **sf 推导**（量化噪声贴住掩蔽阈）：本量化器是幂律压扩
//!    `x̂ = q^{4/3}·G`（`G = 2^{(sf−100)/4}`），大系数处的局部步长
//!    `≈ (4/3)·q^{1/3}·G`，单系数量化噪声标准差
//!    `≈ (1/2)·(4/3)·q^{1/3}·G/√3 = 0.385·q^{1/3}·G`。令带内总噪声
//!    `Σ (0.385·q_k^{1/3}·G)² = 0.148·G^{3/2}·Σ√|X_k|`（代入
//!    `q^{2/3} = (X/G)^{1/2}`）等于 `thr[j]`，解出
//!    `sf[j] = 100 + (8/3)·log2(thr[j] / (0.148·S[j]))`，
//!    `S[j] = Σ_k √|X_k|`。
//!
//! ## 值域护栏
//!
//! - 每带另算 sf 下限：量化值 |q| ≤ 8191（码本 11 escape 的 cbrt 表
//!   上限）→ `sf ≥ 100 + 4·log2(peak/8191^{4/3})`，避免大系数被钳顶；
//! - 全链钳制到 `0..=255`（句法：global_gain u8、sf 链 0..=255）。
//!
//! 该模型只产 sf **初值**：超预算时 [`super::element`] 的码率环整帧
//! 上移 sf（量化变粗、位数下降），模型不必自身闭环。

use std::sync::OnceLock;

/// 相邻带扩散系数（能量域，≈ −4 dB）。
const SPREAD: f64 = 0.4;

/// 掩蔽安全余量：可容噪声 = 掩蔽能量 / 1024（能量比，= 30 dB）。
const MARGIN: f64 = 1.0 / 1024.0;

/// 绝对听阈：单条谱线可容噪声能量（系数域）。推导：−96 dBFS 幅度
/// （`2^16·10^{-4.8}`）× 1024 点正弦窗 MDCT 相干增益（×512）。
fn abs_line_e() -> f64 {
    static V: OnceLock<f64> = OnceLock::new();
    *V.get_or_init(|| {
        let amp = 65_536.0 * 10f64.powf(-96.0 / 20.0);
        (amp * 512.0) * (amp * 512.0)
    })
}

/// 幂律量化噪声系数：`(1/2·(4/3)/√3)² ≈ 0.148`（见模块文档推导）。
const NOISE_K: f64 = 0.148_477_672_5;

/// `8191^{4/3}`：escape 量化值上限（sf 下限推导用）。
fn qmax43() -> f64 {
    static V: OnceLock<f64> = OnceLock::new();
    *V.get_or_init(|| 8_191.0f64.powf(4.0 / 3.0))
}

/// 逐带心理声学分析：MDCT 系数（1024）→ 逐带 sf 初值（`num_swb` 项）。
///
/// 输入须与频带表同长约束（1024 系数；调用方保证）。
pub(crate) fn analyze(coeffs: &[f32], swb_offset: &[u16], num_swb: usize) -> Vec<i16> {
    let mut sf = Vec::with_capacity(num_swb);
    // 带能量/峰统计
    let mut energy = vec![0.0f64; num_swb];
    let mut sqrt_sum = vec![0.0f64; num_swb];
    let mut peak = vec![0.0f64; num_swb];
    for j in 0..num_swb {
        let (lo, hi) = (swb_offset[j] as usize, swb_offset[j + 1] as usize);
        for &x in &coeffs[lo..hi] {
            let a = f64::from(x.abs());
            if !a.is_finite() {
                continue; // 输入异常样本按 0 处理（不 panic）
            }
            energy[j] += a * a;
            sqrt_sum[j] += a.sqrt();
            peak[j] = peak[j].max(a);
        }
    }
    for j in 0..num_swb {
        let width = f64::from(swb_offset[j + 1] - swb_offset[j]);
        // 简化扩散（首末带单侧）
        let mut spr = energy[j];
        if j > 0 {
            spr += SPREAD * energy[j - 1];
        }
        if j + 1 < num_swb {
            spr += SPREAD * energy[j + 1];
        }
        let thr = (spr * MARGIN).max(width * abs_line_e());
        let sf_val = if sqrt_sum[j] > 0.0 {
            100.0 + (8.0 / 3.0) * (thr / (NOISE_K * sqrt_sum[j])).log2()
        } else {
            255.0 // 静带：sf 无意义，取句法上限（最终按 cb0 跳过）
        };
        // |q| ≤ 8191 护栏（peak=0 时下限 0）
        let floor_sf = if peak[j] > 0.0 {
            100.0 + 4.0 * (peak[j] / qmax43()).log2()
        } else {
            0.0
        };
        sf.push(sf_val.max(floor_sf).round().clamp(0.0, 255.0) as i16);
    }
    sf
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tables::{num_swb_long, swb_offset_long};

    fn bands_44() -> (&'static [u16], usize) {
        (
            swb_offset_long(4).unwrap(),
            num_swb_long(4).unwrap() as usize,
        )
    }

    /// 静谱：全部 255（句法上限）。
    #[test]
    fn silence_gives_max_sf() {
        let (swb, n) = bands_44();
        let sf = analyze(&[0.0f32; 1024], swb, n);
        assert_eq!(sf.len(), n);
        assert!(sf.iter().all(|&s| s == 255));
    }

    /// 单音谱：能量带 sf 显著低于远端静带（掩蔽方向正确），且全部
    /// 落在 0..=255；大系数带满足 |q| ≤ 8191 护栏。
    #[test]
    fn tonal_spectrum_shapes_sf() {
        let (swb, n) = bands_44();
        let mut coeffs = vec![0.0f32; 1024];
        // 满幅单音（65536 域）落在 ~440 Hz 对应系数（44.1k 长窗
        // bin ≈ 21.5 Hz → 系数 20 = 频带 5 [20,24)）
        coeffs[20] = 3.0e7;
        let sf = analyze(&coeffs, swb, n);
        assert!(sf.iter().all(|&s| (0..=255).contains(&s)));
        let active = sf[5];
        let far = sf[40];
        assert!(
            active < far - 20,
            "能量带 sf {active} 应显著低于远端静带 {far}"
        );
        // 护栏：active 带量化峰值不超 8191
        let g = 2f64.powf((100 - i32::from(active)) as f64 / 4.0);
        let q = (3.0e7 * g).powf(0.75);
        assert!(q <= 8191.5, "q 峰值 {q} 超 escape 上限");
    }

    /// 常数条件求值自检（abs_line_e / qmax43 合理域）。
    #[test]
    fn constants_sane() {
        assert!((abs_line_e() > 1.0e4) && (abs_line_e() < 1.0e7)); // ≈ 2.8e5
        assert!((qmax43() - 1.651e5).abs() < 1.0e2);
    }
}
