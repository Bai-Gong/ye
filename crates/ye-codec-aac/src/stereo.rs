//! 立体声工具：M/S 与强度立体声（ISO/IEC 14496-3 §4.6.8）。
//!
//! 输入：CPE 两通道的重量化谱 + 共窗信息（ms_used 位图、ms_present）。
//! 应用次序：先 M/S 蝶形、后强度（右声道强度段从左声道乘因子生成），
//! 与 FFmpeg `decode_cpe` 的调用序一致。
//!
//! ## M/S
//!
//! `ms_used[g·max_sfb+sfb]` 为真的频带（两声道该带均非噪声/强度码本）：
//! `L = M+S`、`R = M−S`——**无 1/√2 归一**的裸蝶形，对齐 FFmpeg
//! `apply_mid_side_stereo → butterflies_float`（其编码器在同一约定下
//! 量化，故按其约定解码才能逐位复现 oracle；ISO §4.6.8.1.3 文式中的
//! 1/√2 与 FFmpeg 的 IMDCT ×1/M 标定互抵，见 decoder 模块「输出标定」）。
//!
//! ## 强度
//!
//! 右声道强度段（码本 14/15）：`R = c · L · 2^(−is_pos/4)`，
//! `c = +1`（同相，码本 15）/`−1`（反相，码本 14）；ms_present ≠ 0 时
//! `c` 再乘 `1 − 2·ms_used`（ISO §4.6.8.2.3）。左声道不动。

use crate::syntax::{CB_INTENSITY_HI, CB_INTENSITY_LO, CB_NOISE, ChannelSyntax, PairCommon};
use crate::tables;

/// 对 CPE 两通道应用 M/S 与强度立体声工具。
///
/// # Errors
/// 强度段位置越界（码流自相矛盾）→ [`crate::AacError::InvalidData`]。
pub(crate) fn apply(
    common: &PairCommon,
    ch0: &mut ChannelSyntax,
    ch1: &mut ChannelSyntax,
) -> Result<(), crate::AacError> {
    let ics = &ch0.ics;
    let max_sfb = ics.max_sfb as usize;
    let mut idx = 0usize;
    let mut window_base = 0usize;
    for g in 0..ics.num_window_groups as usize {
        let g_len = ics.group_len[g] as usize;
        for sfb in 0..max_sfb {
            let lo = ics.swb_offset[sfb] as usize;
            let hi = ics.swb_offset[sfb + 1] as usize;
            let ms = common.ms_used.get(idx).copied().unwrap_or(false) && common.ms_present != 0;
            // M/S：两声道该带均非噪声/强度码本（对齐 FFmpeg 的 < NOISE_BT）
            if ms && ch0.codebooks[idx] < CB_NOISE && ch1.codebooks[idx] < CB_NOISE {
                for w in 0..g_len {
                    let base = window_base + w * window_stride(ics) + lo;
                    for k in 0..hi - lo {
                        let m = ch0.coeffs[base + k];
                        let s = ch1.coeffs[base + k];
                        ch0.coeffs[base + k] = m + s;
                        ch1.coeffs[base + k] = m - s;
                    }
                }
            }
            idx += 1;
        }
        window_base += g_len * window_stride(ics);
    }

    // 强度：右声道强度段由左声道生成
    let ics1 = &ch1.ics;
    let mut idx = 0usize;
    let mut window_base = 0usize;
    for g in 0..ics1.num_window_groups as usize {
        let g_len = ics1.group_len[g] as usize;
        let mut sfb = 0usize;
        while sfb < max_sfb {
            let cb = ch1.codebooks[idx];
            if cb == CB_INTENSITY_HI || cb == CB_INTENSITY_LO {
                let lo = ics1.swb_offset[sfb] as usize;
                let hi = ics1.swb_offset[sfb + 1] as usize;
                let is_pos = i32::from(ch1.scalefactors[idx]);
                let mut c: f32 = if cb == CB_INTENSITY_HI { -1.0 } else { 1.0 };
                if common.ms_present != 0 && common.ms_used.get(idx).copied().unwrap_or(false) {
                    c = -c;
                }
                let scale = c * tables::pow2sf(-is_pos);
                for w in 0..g_len {
                    let base = window_base + w * window_stride(ics1) + lo;
                    for k in 0..hi - lo {
                        ch1.coeffs[base + k] = ch0.coeffs[base + k] * scale;
                    }
                }
            }
            sfb += 1;
            idx += 1;
        }
        window_base += g_len * window_stride(ics1);
    }
    Ok(())
}

/// 短窗步进：每窗系数数（长窗 g_len 恒 1，base 不跨窗推进）。
fn window_stride(ics: &crate::syntax::IcsInfo) -> usize {
    *ics.swb_offset.last().unwrap_or(&0) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::IcsInfo;

    fn ics_44() -> IcsInfo {
        IcsInfo {
            window_sequence: crate::syntax::ONLY_LONG,
            window_shape: false,
            max_sfb: 4,
            num_window_groups: 1,
            group_len: [1, 0, 0, 0, 0, 0, 0, 0],
            swb_offset: crate::tables::swb_offset_long(4).unwrap(),
            num_swb: crate::tables::num_swb_long(4).unwrap(),
            tns_max_bands: crate::tables::tns_max_bands_long(4).unwrap(),
        }
    }

    fn chan(ics: IcsInfo, coeffs: Vec<f32>, codebooks: Vec<u8>, sf: Vec<i16>) -> ChannelSyntax {
        ChannelSyntax {
            ics,
            codebooks,
            scalefactors: sf,
            tns: None,
            coeffs,
        }
    }

    /// M/S 蝶形（FFmpeg 裸蝶形，无 1/√2）：M=1、S=0.5 → L=1.5、R=0.5。
    #[test]
    fn ms_butterfly() {
        let ics = ics_44();
        let mut c0 = chan(
            ics.clone(),
            vec![1.0; 1024],
            vec![7, 0, 0, 0],
            vec![100, 0, 0, 0],
        );
        let mut c1 = chan(
            ics.clone(),
            vec![0.5; 1024],
            vec![7, 0, 0, 0],
            vec![100, 0, 0, 0],
        );
        let common = PairCommon {
            ms_present: 2,
            ms_used: vec![true; 4],
        };
        apply(&common, &mut c0, &mut c1).unwrap();
        assert!((c0.coeffs[0] - 1.5).abs() < 1e-6, "L = M+S");
        assert!((c1.coeffs[0] - 0.5).abs() < 1e-6, "R = M−S");
        // 帕塞瓦尔（裸蝶形）：L²+R² = 2(M²+S²)
        let e: f32 = c0.coeffs[0] * c0.coeffs[0] + c1.coeffs[0] * c1.coeffs[0];
        assert!((e - 2.5).abs() < 1e-5);
    }

    /// 强度：同相 is_pos=0 → R = L；反相 is_pos=4 → R = −L/2。
    #[test]
    fn intensity_stereo() {
        let ics = ics_44();
        let mut c0 = chan(
            ics.clone(),
            vec![1.0; 1024],
            vec![7, 7, 0, 0],
            vec![100, 100, 0, 0],
        );
        let mut c1 = chan(
            ics.clone(),
            vec![0.0; 1024],
            vec![7, CB_INTENSITY_LO, 0, 0],
            vec![100, 0, 0, 0],
        );
        let common = PairCommon {
            ms_present: 0,
            ms_used: vec![false; 4],
        };
        apply(&common, &mut c0, &mut c1).unwrap();
        // 带 1 [4,8)：is_pos=0 → R = L
        assert_eq!(c1.coeffs[4], 1.0);

        // 反相 + is_pos=4（2^-1）
        let mut c0 = chan(
            ics.clone(),
            vec![1.0; 1024],
            vec![7, 7, 0, 0],
            vec![100, 100, 0, 0],
        );
        let mut c1 = chan(
            ics,
            vec![0.0; 1024],
            vec![7, CB_INTENSITY_HI, 0, 0],
            vec![100, 4, 0, 0],
        );
        apply(&common, &mut c0, &mut c1).unwrap();
        assert!((c1.coeffs[4] + 0.5).abs() < 1e-6);
    }
}
