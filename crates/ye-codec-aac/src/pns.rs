//! AAC-LC PNS：每带单位能量噪声按 noise scalefactor 缩放。
//! 随机状态属于解码器；重建噪声不要求与其他实现逐样本一致。
use crate::syntax::{CB_NOISE, ChannelSyntax, EIGHT_SHORT, PairCommon};

fn normalize(band: &mut [f32], amplitude: f32) {
    let energy: f64 = band.iter().map(|&x| f64::from(x).powi(2)).sum();
    if energy > 0.0 {
        let scale = f64::from(amplitude) / energy.sqrt();
        for x in band {
            *x = (f64::from(*x) * scale) as f32;
        }
    }
}

pub(crate) fn generate(ch: &mut ChannelSyntax, state: &mut u32) {
    let ics = &ch.ics;
    let stride = if ics.window_sequence == EIGHT_SHORT {
        128
    } else {
        1024
    };
    let mut window = 0;
    for g in 0..ics.num_window_groups as usize {
        for b in 0..ics.max_sfb as usize {
            let i = g * ics.max_sfb as usize + b;
            if ch.codebooks[i] != CB_NOISE {
                continue;
            }
            for w in 0..ics.group_len[g] as usize {
                let start = (window + w) * stride + ics.swb_offset[b] as usize;
                let end = (window + w) * stride + ics.swb_offset[b + 1] as usize;
                let band = &mut ch.coeffs[start..end];
                for x in band.iter_mut() {
                    *state ^= *state << 13;
                    *state ^= *state >> 17;
                    *state ^= *state << 5;
                    *x = (*state as i32) as f32 / i32::MAX as f32;
                }
                normalize(band, crate::tables::pow2sf(ch.scalefactors[i] as i32));
            }
        }
        window += ics.group_len[g] as usize;
    }
}

pub(crate) fn correlate(common: &PairCommon, left: &ChannelSyntax, right: &mut ChannelSyntax) {
    // No shared M/S mask means no correlation. In particular, independently
    // windowed channels need not have the same number of bands or groups.
    if common.ms_present == 0 {
        return;
    }
    let ics = &left.ics;
    let stride = if ics.window_sequence == EIGHT_SHORT {
        128
    } else {
        1024
    };
    let mut window = 0;
    for g in 0..ics.num_window_groups as usize {
        for b in 0..ics.max_sfb as usize {
            let i = g * ics.max_sfb as usize + b;
            if left.codebooks[i] != CB_NOISE
                || right.codebooks[i] != CB_NOISE
                || common.ms_present == 0
                || !common.ms_used.get(i).copied().unwrap_or(false)
            {
                continue;
            }
            for w in 0..ics.group_len[g] as usize {
                let start = (window + w) * stride + ics.swb_offset[b] as usize;
                let end = (window + w) * stride + ics.swb_offset[b + 1] as usize;
                right.coeffs[start..end].copy_from_slice(&left.coeffs[start..end]);
                normalize(
                    &mut right.coeffs[start..end],
                    crate::tables::pow2sf(right.scalefactors[i] as i32),
                );
            }
        }
        window += ics.group_len[g] as usize;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn independent_noise_bands_do_not_read_the_other_channel() {
        use crate::syntax::{CB_NOISE, ChannelSyntax, IcsInfo, ONLY_LONG, PairCommon};
        let left = ChannelSyntax {
            ics: IcsInfo {
                window_sequence: ONLY_LONG,
                window_shape: false,
                max_sfb: 2,
                num_window_groups: 1,
                group_len: [1, 0, 0, 0, 0, 0, 0, 0],
                swb_offset: &[0, 4, 8],
                num_swb: 2,
                tns_max_bands: 2,
            },
            codebooks: vec![0, CB_NOISE],
            scalefactors: vec![0, 0],
            tns: None,
            coeffs: vec![1.0; 1024],
        };
        let mut right = left.clone();
        right.ics.max_sfb = 1;
        right.codebooks.truncate(1);
        right.scalefactors.truncate(1);
        right.coeffs.fill(2.0);
        super::correlate(
            &PairCommon {
                ms_present: 0,
                ms_used: Vec::new(),
            },
            &left,
            &mut right,
        );
        assert!(right.coeffs.iter().all(|v| *v == 2.0));
    }
    #[test]
    fn requested_noise_energy() {
        let mut band = [1.0, -2.0, 3.0, -4.0];
        super::normalize(&mut band, 8.0);
        let energy: f32 = band.iter().map(|x| x * x).sum();
        assert!((energy - 64.0).abs() < 0.0001);
    }
    #[test]
    fn noise_bands_preserve_energy_and_stereo_correlation() {
        use crate::syntax::{ChannelSyntax, EIGHT_SHORT, IcsInfo, ONLY_LONG, PairCommon};
        for sequence in [ONLY_LONG, EIGHT_SHORT] {
            let windows = if sequence == EIGHT_SHORT { 8 } else { 1 };
            let channel = ChannelSyntax {
                ics: IcsInfo {
                    window_sequence: sequence,
                    window_shape: false,
                    max_sfb: 1,
                    num_window_groups: 1,
                    group_len: [windows, 0, 0, 0, 0, 0, 0, 0],
                    swb_offset: &[0, 64],
                    num_swb: 1,
                    tns_max_bands: 1,
                },
                codebooks: vec![super::CB_NOISE],
                scalefactors: vec![12],
                tns: None,
                coeffs: vec![0.0; 1024],
            };
            let mut left = channel.clone();
            let mut right = channel;
            let mut seed = 739;
            super::generate(&mut left, &mut seed);
            super::generate(&mut right, &mut seed);
            assert_ne!(
                left.coeffs, right.coeffs,
                "uncorrelated bands use independent noise"
            );
            right.scalefactors[0] = 16;
            super::correlate(
                &PairCommon {
                    ms_present: 1,
                    ms_used: vec![true],
                },
                &left,
                &mut right,
            );
            for w in 0..windows as usize {
                let start = w * 128;
                let energy: f32 = left.coeffs[start..start + 64].iter().map(|x| x * x).sum();
                assert!((energy - 64.0).abs() < 0.001);
                for i in start..start + 64 {
                    assert!((right.coeffs[i] - 2.0 * left.coeffs[i]).abs() < 0.00001);
                }
            }
        }
    }
}
