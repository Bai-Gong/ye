//! 时域噪声整形（TNS，ISO/IEC 14496-3 §4.6.9）：频域全极点滤波。
//!
//! 滤波器作用在重量化谱上（次序：spectral → M/S+强度 → **TNS** →
//! IMDCT；pulse/tns 数据联合点之二）。每窗最多 `n_filt` 个滤波器，
//! 自窗末频带向上按 `length` 个频带划界，作用区为
//! `[swb_offset[bottom], swb_offset[top])` 的系数区间。
//!
//! ## 系数换算
//!
//! 位流格点索引在语法层已折算为格点值（[`crate::tables::tns_coef_map`]，
//! 与 FFmpeg `tns_tmp2_map` 逐值一致）。此处换算对齐 FFmpeg
//! `apply_tns → compute_lpc_coefs(coef, order, lpc, 0, 0, 0)` 的
//! `normalize=0` 路径：先**取负** `r[m] = −coef[m]`，再按 Levinson 步进
//! 升阶：`a[0]=r[0]`，每升一阶 `a[i] += r[m]·a[m−1−i]`（i<m）、`a[m]=r[m]`。
//!（ISO/IEC 14496-3 §4.6.9 的反射→预测系数换算；负号来自其反射系数
//! 符号约定。）
//!
//! ## 滤波形状
//!
//! 全极点反馈（对齐 FFmpeg `apply_tns` 的 ar 分支）：
//! `y[m] = x[m] − Σ_{i=1..min(m,order)} lpc[i−1]·y[m−i]`；
//! `direction` 决定作用区推进方向（false = 低频→高频，真 = 反向）。

use crate::syntax::{EIGHT_SHORT, IcsInfo, TnsData};

/// 应用 TNS（原地修改 `coeffs`；系数即格点值，见语法层）。
pub(crate) fn apply(coeffs: &mut [f32], ics: &IcsInfo, tns: &TnsData) {
    let mmm = ics.tns_max_bands.min(ics.max_sfb) as usize;
    if mmm == 0 {
        return;
    }
    let per_window = *ics.swb_offset.last().unwrap_or(&0) as usize;
    let num_windows = if ics.window_sequence == EIGHT_SHORT {
        8
    } else {
        1
    };

    for w in 0..num_windows {
        let Some(filts) = tns.filters.get(w) else {
            continue;
        };
        let mut bottom = ics.num_swb as usize;
        for filt in filts {
            let top = bottom;
            bottom = top.saturating_sub(filt.length as usize);
            if filt.order == 0 {
                continue;
            }
            let negated: Vec<f32> = filt.coefs.iter().map(|&c| -c).collect();
            // FFmpeg compute_lpc_coefs normalize=0：先取负再步进升阶
            let lpc = reflection_to_lpc(&negated);
            let start_band = bottom.min(mmm);
            let end_band = top.min(mmm);
            let start = ics.swb_offset[start_band] as usize;
            let end = ics.swb_offset[end_band] as usize;
            let size = end.saturating_sub(start);
            if size == 0 {
                continue;
            }
            let order = filt.order as usize;
            if filt.direction {
                // 反向：自高频端向低频端
                let base = w * per_window + end - 1;
                ar_filter_rev(coeffs, base, size, order, &lpc);
            } else {
                let base = w * per_window + start;
                ar_filter_fwd(coeffs, base, size, order, &lpc);
            }
        }
    }
}

/// 反射（格点）系数 → 直接型 LPC（Levinson 步进）。
fn reflection_to_lpc(k: &[f32]) -> Vec<f32> {
    let order = k.len();
    let mut a = vec![0.0f32; order];
    if order == 0 {
        return a;
    }
    a[0] = k[0];
    for m in 1..order {
        let prev = a[..m].to_vec();
        for i in 0..m {
            a[i] = prev[i] + k[m] * prev[m - 1 - i];
        }
        a[m] = k[m];
    }
    a
}

/// 正向全极点滤波（升下标），窗口 [base, base+size)。
fn ar_filter_fwd(coeffs: &mut [f32], base: usize, size: usize, order: usize, lpc: &[f32]) {
    for m in 0..size {
        let idx = base + m;
        let mut acc = 0.0f32;
        for i in 1..=order.min(m) {
            acc += coeffs[idx - i] * lpc[i - 1];
        }
        coeffs[idx] -= acc;
    }
}

/// 反向全极点滤波（降下标），窗口 (base−size, base]。
fn ar_filter_rev(coeffs: &mut [f32], base: usize, size: usize, order: usize, lpc: &[f32]) {
    for m in 0..size {
        let idx = base - m;
        let mut acc = 0.0f32;
        for i in 1..=order.min(m) {
            acc += coeffs[idx + i] * lpc[i - 1];
        }
        coeffs[idx] -= acc;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 683c70b9 2192 LPCFf1aorder=1 60527b49Ff1b9ad896366309 Levinson 6b658fdb901098799a8c8bc13002
    #[test]
    fn reflection_conversion() {
        assert_eq!(reflection_to_lpc(&[0.5]), vec![0.5]);
        // order=2Ff1aa = [k0 + k100b7k0, k1]
        let a = reflection_to_lpc(&[0.5, -0.25]);
        assert!((a[0] - 0.375).abs() < 1e-7);
        assert!((a[1] + 0.25).abs() < 1e-7);
        // order=3Ff1a6b658fdb540e a = [0.34375, 22120.203125, 0.125]
        let a = reflection_to_lpc(&[0.5, -0.25, 0.125]);
        assert!((a[0] - 0.34375).abs() < 1e-7);
        assert!((a[1] + 0.203125).abs() < 1e-7);
        assert!((a[2] - 0.125).abs() < 1e-7);
    }

    /// 正/反向滤波手算样例：y = x − lpc0·y_prev。
    #[test]
    fn ar_filter_directions() {
        let mut fwd = vec![1.0f32, 1.0, 1.0];
        ar_filter_fwd(&mut fwd, 0, 3, 1, &[0.5]);
        // y0 = 1; y1 = 1 − 0.5·1 = 0.5; y2 = 1 − 0.5·0.5 = 0.75
        assert_eq!(fwd, vec![1.0, 0.5, 0.75]);
        let mut rev = vec![1.0f32, 1.0, 1.0];
        ar_filter_rev(&mut rev, 2, 3, 1, &[0.5]);
        // 自 idx2 向下：y2 = 1; y1 = 1 − 0.5·1 = 0.5; y0 = 1 − 0.5·0.5 = 0.75
        assert_eq!(rev[2], 1.0);
        assert_eq!(rev[1], 0.5);
        assert_eq!(rev[0], 0.75);
    }

    /// apply 的频带定位：长窗 bottom/top 裁剪到 mmm。
    #[test]
    fn apply_band_limits() {
        use crate::syntax::TnsFilter;
        let num_swb = crate::tables::num_swb_long(4).unwrap();
        let tns_max = crate::tables::tns_max_bands_long(4).unwrap();
        let ics = IcsInfo {
            window_sequence: crate::syntax::ONLY_LONG,
            window_shape: false,
            // max_sfb 覆盖 TNS 作用带：取 mmm（40/44.1k 表）
            max_sfb: tns_max.min(num_swb),
            num_window_groups: 1,
            group_len: [1, 0, 0, 0, 0, 0, 0, 0],
            swb_offset: crate::tables::swb_offset_long(4).unwrap(),
            num_swb,
            tns_max_bands: tns_max,
        };
        let mmm = tns_max.min(ics.max_sfb) as usize;
        let tns = TnsData {
            filters: vec![vec![TnsFilter {
                // top = num_swb，bottom = num_swb − 12 → 区间与 [0, mmm)
                // 相交为带 [num_swb−12, mmm)
                length: 12,
                order: 1,
                direction: false,
                coefs: vec![0.5],
            }]],
        };
        let mut coeffs = vec![1.0f32; 1024];
        apply(&mut coeffs, &ics, &tns);
        let start = ics.swb_offset[ics.num_swb as usize - 12] as usize;
        let end = ics.swb_offset[mmm] as usize;
        assert_eq!(coeffs[start - 1], 1.0, "作用区外不动");
        assert_eq!(coeffs[start], 1.0, "m=0 无反馈，首样本不变");
        // 格点 0.5 → 取负 −0.5：y1 = 1 − (−0.5)·1 = 1.5
        assert_eq!(coeffs[start + 1], 1.5, "y1 = 1 + 0.5·1");
        assert_eq!(coeffs[start + 2], 1.75, "y2 = 1 + 0.5·1.5");
        // 作用区之后（裁剪边界外）不动
        assert_eq!(coeffs[end], 1.0);
    }
}
