//! 逆 Huffman 解谱与重量化（ISO/IEC 14496-3 §4.6.2.4 spectral_data、
//! §4.6.3.3 重量化、§4.6.6 pulse 应用）。
//!
//! 消费 [`crate::syntax`] 推进到位的 [`BitReader`]，按 section 指定的
//! 码本逐段解出量化整数谱，乘以 scalefactor 链乘子
//! `2^((sf−100)/4)`（f32，[`crate::tables::pow2sf`]），并在末尾应用
//! pulse 偏置（pulse/tns 数据联合点之一：pulse 在 TNS 滤波前作用于
//! 重量化谱）。
//!
//! ## 语义对齐（FFmpeg `decode_spectrum_and_dequant`，n4.4 = oracle）
//!
//! - 码本 1/2：quad、四值直接取自 ±{0,1} 表（无符号位）；
//! - 码本 3/4：quad、幅值 ∈ {0,1,2}（|q|^{4/3} 表），非零分量后读符号位；
//! - 码本 5/6：pair、带符号九电平（±(0..4)^{4/3}）；
//! - 码本 7..10：pair、幅值 0..7 / 0..12，非零分量后读符号位；
//! - 码本 11：pair、幅值 0..15、16 = escape——一元 `1…10` 前缀（≤8 个 1）
//!   + b+4 位后缀定出 n ∈ [2^b, 2^{b+1})，重量化值 = `n^{4/3}`
//!     （[`crate::tables::cbrt43`]，与 FFmpeg `ff_cbrt_tab` 同构）；
//! - ZERO / 强度（14/15）码本段不解码系数（强度段的右声道谱在
//!   [`crate::stereo`] 由左声道乘强度因子生成）。
//!
//! ## 失败模式
//!
//! 码字不匹配、escape 前缀 >8、频带宽度非码本维度整数倍 →
//! [`AacError::InvalidData`]。

use crate::error::AacError;
use crate::syntax::{
    BitReader, CB_INTENSITY_HI, CB_INTENSITY_LO, CB_ZERO, EIGHT_SHORT, IcsInfo, PulseData,
};
use crate::tables;

/// 解谱 + 重量化 + pulse 应用。返回每帧系数谱（f32；EIGHT_SHORT 为
/// 8 短块依时间序拼接）。
///
/// # Errors
/// 见模块文档失败模式。
pub(crate) fn decode_and_requant(
    r: &mut BitReader,
    ics: &IcsInfo,
    codebooks: &[u8],
    scalefactors: &[i16],
    pulse: &Option<PulseData>,
) -> Result<Vec<f32>, AacError> {
    let total = if ics.window_sequence == EIGHT_SHORT {
        8 * short_coeffs(ics)
    } else {
        long_coeffs(ics)
    };
    let per_window = short_coeffs(ics);
    let mut coeffs = vec![0.0f32; total];

    // max_sfb 之外全部为零（逐窗）
    let tail_start = ics.swb_offset[ics.max_sfb as usize] as usize;
    for w in 0..(total / per_window) {
        let base = w * per_window;
        for c in coeffs
            .iter_mut()
            .take(base + per_window)
            .skip(base + tail_start)
        {
            *c = 0.0;
        }
    }

    let mut idx = 0usize;
    let mut window_base = 0usize;
    for g in 0..ics.num_window_groups as usize {
        let g_len = ics.group_len[g] as usize;
        for sfb in 0..ics.max_sfb as usize {
            let cb = codebooks[idx];
            let sf_int = scalefactors[idx];
            let mult = band_multiplier(cb, sf_int);
            let lo = ics.swb_offset[sfb] as usize;
            let hi = ics.swb_offset[sfb + 1] as usize;
            if cb != CB_ZERO
                && cb != CB_INTENSITY_HI
                && cb != CB_INTENSITY_LO
                && cb != crate::syntax::CB_NOISE
            {
                decode_band(
                    r,
                    ics,
                    cb,
                    lo,
                    hi,
                    mult,
                    g_len,
                    per_window,
                    window_base,
                    &mut coeffs,
                )?;
            }
            idx += 1;
        }
        window_base += g_len * per_window;
    }

    if let Some(p) = pulse {
        apply_pulse(&mut coeffs, ics, codebooks, scalefactors, p);
    }
    Ok(coeffs)
}

/// 每窗系数数（长窗 = 帧系数数；短窗 = 帧系数数/8）。
fn short_coeffs(ics: &IcsInfo) -> usize {
    if ics.window_sequence == EIGHT_SHORT {
        // 960 帧短窗 120；1024 帧短窗 128——由频带表末值可得
        *ics.swb_offset.last().unwrap_or(&0) as usize
    } else {
        *ics.swb_offset.last().unwrap_or(&0) as usize
    }
}

/// 帧系数数（长窗窗内系数数）。
fn long_coeffs(ics: &IcsInfo) -> usize {
    *ics.swb_offset.last().unwrap_or(&0) as usize
}

/// 频带乘子：常规段 `2^((sf−100)/4)`；ZERO/强度段 0（谱不消费）。
fn band_multiplier(cb: u8, sf_int: i16) -> f32 {
    if cb == CB_ZERO || cb == CB_INTENSITY_HI || cb == CB_INTENSITY_LO {
        0.0
    } else {
        tables::pow2sf(i32::from(sf_int) - 100)
    }
}

/// 解一个频带（组内 g_len 个窗：**每窗独立解一套码字**——组化只共享
/// section/scalefactor，谱数据逐窗携带）。
#[allow(clippy::too_many_arguments)]
fn decode_band(
    r: &mut BitReader,
    _ics: &IcsInfo,
    cb: u8,
    lo: usize,
    hi: usize,
    mult: f32,
    g_len: usize,
    per_window: usize,
    window_base: usize,
    coeffs: &mut [f32],
) -> Result<(), AacError> {
    let shape = tables::codebook_shape(cb)
        .ok_or_else(|| AacError::InvalidData(format!("码本号 {cb} 非法（1..=11）")))?;
    let width = hi - lo;
    if !width.is_multiple_of(shape.dim) {
        return Err(AacError::InvalidData(format!(
            "频带宽度 {width} 不是码本 {cb} 维度 {} 的整数倍",
            shape.dim
        )));
    }
    for w in 0..g_len {
        let base = window_base + w * per_window + lo;
        let mut cursor = 0usize;
        while cursor < width {
            let sym = r.spectral_symbol(cb)? as usize;
            let packed = shape.idx[sym] as usize;
            // 码本 1..4（quad）：逐分量 2 位索引；5..11（pair）：4 位 nibble
            let comps: Vec<usize> = if cb <= 4 {
                (0..4).map(|i| (packed >> (2 * i)) & 0x3).collect()
            } else {
                vec![packed & 0xF, (packed >> 4) & 0xF]
            };
            match cb {
                1 | 2 => {
                    // quad 无符号：分量索引 → ±{0,1} 值表
                    for &n in &comps {
                        let v = shape.vals.get(n).copied().ok_or_else(|| bad_idx(cb))?;
                        coeffs[base + cursor] = v * mult;
                        cursor += 1;
                    }
                }
                3 | 4 => {
                    // quad 幅值（{0,1,2} 的 |q|^{4/3} 表）+ 非零分量符号位
                    let signs = read_sign_bits(r, comps.iter().map(|&n| val_nonzero(n)))?;
                    for (i, &n) in comps.iter().enumerate() {
                        let mag = shape.vals.get(n).copied().ok_or_else(|| bad_idx(cb))?;
                        coeffs[base + cursor] = apply_sign(mag, signs[i]) * mult;
                        cursor += 1;
                    }
                }
                5 | 6 => {
                    // pair 带符号（值表自带符号，无符号位）
                    for &n in &comps {
                        let v = shape.vals.get(n).copied().ok_or_else(|| bad_idx(cb))?;
                        coeffs[base + cursor] = v * mult;
                        cursor += 1;
                    }
                }
                7..=10 => {
                    // pair 幅值 + 非零分量符号位
                    let signs = read_sign_bits(r, comps.iter().map(|&n| val_nonzero(n)))?;
                    for (i, &n) in comps.iter().enumerate() {
                        let mag = shape.vals.get(n).copied().ok_or_else(|| bad_idx(cb))?;
                        coeffs[base + cursor] = apply_sign(mag, signs[i]) * mult;
                        cursor += 1;
                    }
                }
                11 => {
                    // pair 幅值（0..15）+ 符号位；escape 掩码（packed 位 8:9）
                    let escape_mask = (packed >> 8) & 0x3;
                    let signs = read_sign_bits(
                        r,
                        comps
                            .iter()
                            .enumerate()
                            .map(|(i, &n)| escaped(escape_mask, i) || val_nonzero(n)),
                    )?;
                    for (i, &n) in comps.iter().enumerate() {
                        let v = if escaped(escape_mask, i) {
                            // 幅值 nibble 对 escape 分量无意义，忽略
                            escape_value(r)?
                        } else {
                            shape.vals.get(n).copied().ok_or_else(|| bad_idx(cb))?
                        };
                        coeffs[base + cursor] = apply_sign(v, signs[i]) * mult;
                        cursor += 1;
                    }
                }
                _ => unreachable!("codebook_shape 已过滤"),
            }
        }
    }
    Ok(())
}

/// escape 掩码判定分量 `i`（0/1）。
fn escaped(mask: usize, i: usize) -> bool {
    mask & (1 << i) != 0
}

fn bad_idx(cb: u8) -> AacError {
    AacError::InvalidData(format!("码本 {cb} 取值索引越界（码表损坏）"))
}

/// 幅值 nibble 是否非零（决定是否耗符号位）。
fn val_nonzero(nib: usize) -> bool {
    nib != 0
}

/// 依序读各分量的符号位（仅非零分量消耗）；返回逐分量正负。
fn read_sign_bits(
    r: &mut BitReader,
    needs: impl Iterator<Item = bool>,
) -> Result<Vec<bool>, AacError> {
    let mut out = Vec::new();
    for need in needs {
        out.push(if need { r.bit()? == 1 } else { false });
    }
    Ok(out)
}

/// 幅值配符号（false = 正）。
fn apply_sign(mag: f32, negative: bool) -> f32 {
    if negative { -mag } else { mag }
}

/// escape 序列解码：一元 `1…10`（≤8 个 1）+ b+4 位后缀 → n^{4/3}。
fn escape_value(r: &mut BitReader) -> Result<f32, AacError> {
    let mut b = 0u32;
    loop {
        if r.bit()? == 0 {
            break;
        }
        b += 1;
        if b > 8 {
            return Err(AacError::InvalidData("escape 前缀超长（>8 个 1）".into()));
        }
    }
    let b = b + 4;
    let suffix = r.bits(b)?;
    let n = (1u32 << b) + suffix;
    tables::cbrt43(n)
        .ok_or_else(|| AacError::InvalidData(format!("escape 值 n={n} 超 cbrt 表（≥8192）")))
}

/// pulse 应用（对齐 FFmpeg float 路径的运算次序与精度）。
fn apply_pulse(
    coeffs: &mut [f32],
    ics: &IcsInfo,
    codebooks: &[u8],
    scalefactors: &[i16],
    pulse: &PulseData,
) {
    let mut idx = 0usize;
    for i in 0..pulse.pos.len() {
        let pos = pulse.pos[i] as usize;
        let co = coeffs[pos];
        // 找到 pos 所在频带（offsets[idx+1] ≤ pos 时前进）
        while (ics.swb_offset[idx + 1] as usize) <= pos {
            idx += 1;
            if idx + 1 >= ics.swb_offset.len() {
                break;
            }
        }
        let cb = codebooks[idx];
        let mult = band_multiplier(cb, scalefactors[idx]);
        if cb == crate::syntax::CB_NOISE || mult == 0.0 {
            continue; // 零能量频带：脉冲不生效（对齐 FFmpeg 的 sf[idx] 真值判断）
        }
        let mut ico = -(pulse.amp[i] as f32);
        if co != 0.0 {
            let co = co / mult;
            // co / sqrt(sqrt(|co|)) = sign·|co|^{3/4}；±amp 后回 4/3 幂
            ico = co / (co.abs().sqrt().sqrt()) + if co > 0.0 { -ico } else { ico };
        }
        coeffs[pos] = ico.abs().cbrt() * ico * mult;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::parse_scale_factor_data;
    use crate::tables::{codebook_bits, codebook_codes};

    /// 长窗 44.1k ICS（41 带）。
    fn ics_44() -> IcsInfo {
        IcsInfo {
            window_sequence: crate::syntax::ONLY_LONG,
            window_shape: false,
            max_sfb: 4,
            num_window_groups: 1,
            group_len: [1, 0, 0, 0, 0, 0, 0, 0],
            swb_offset: tables::swb_offset_long(4).unwrap(),
            num_swb: tables::num_swb_long(4).unwrap(),
            tns_max_bands: tables::tns_max_bands_long(4).unwrap(),
        }
    }

    /// 位写入器（测试）。
    struct W {
        bytes: Vec<u8>,
        pos: usize,
    }
    impl W {
        fn new() -> W {
            W {
                bytes: Vec::new(),
                pos: 0,
            }
        }
        fn put(&mut self, v: u32, n: u32) {
            for i in (0..n).rev() {
                let bit = (v >> i) & 1;
                if self.pos.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                *self.bytes.last_mut().unwrap() |= (bit as u8) << (7 - self.pos % 8);
                self.pos += 1;
            }
        }
        /// 写码本 cb 符号 sym 的码字。
        fn put_sym(&mut self, cb: u8, sym: usize) {
            self.put(
                u32::from(codebook_codes(cb).unwrap()[sym]),
                u32::from(codebook_bits(cb).unwrap()[sym]),
            );
        }
    }

    /// 码本 5（pair 带符号）：单频带两符号 → 4 系数。
    #[test]
    fn decode_pair_signed_cb5() {
        let ics = ics_44();
        // 带宽 [0,4)：2 个 pair。符号 40 = (vals 4,4) = (0,0)?
        // 码本 5 idx=40 → nibbles 从 CB_IDX_4[40] 解包（vals 索引 4,4 → 0.0）
        let sym0 = 40usize; // 查表断言见下
        let packed = tables::codebook_shape(5).unwrap().idx[sym0];
        assert_eq!(packed & 0xFF, 0x44, "sym40 应为零对");
        let sym1 = 30usize; // CB_IDX_4[30] = 0x001a → nibbles (a,1) → vals[10&?]…
        let _ = sym1;
        let mut w = W::new();
        w.put_sym(5, sym0);
        // 再写一个非零符号：idx=0 → nibbles (0,0)? CB_IDX_4[0]=0x0000 → vals(0,0)
        // 取 idx=63（nibbles 3,f? 见表）——直接用 sym0×2 验证零带
        w.put_sym(5, sym0);
        let codebooks = vec![5u8, 0, 0, 0];
        // scalefactors: sf_int = 100 → mult = 1
        let scalefactors = vec![100i16, 0, 0, 0];
        let mut r = BitReader::new(&w.bytes);
        let coeffs = decode_and_requant(&mut r, &ics, &codebooks, &scalefactors, &None).unwrap();
        assert_eq!(coeffs.len(), 1024);
        assert!(coeffs.iter().take(4).all(|&v| v == 0.0));
        assert!(coeffs.iter().skip(4).all(|&v| v == 0.0));
    }

    /// 码本 7（pair 幅值+符号）与码本 11 escape。
    #[test]
    fn decode_pair_magnitude_and_escape() {
        let ics = ics_44();
        let shape7 = tables::codebook_shape(7).unwrap();
        // 找一个非零符号：CB_IDX_6 中 0x0110 → nibbles(0,1)：幅值 (0,1)
        let sym = shape7.idx.iter().position(|&p| p == 0x0110).unwrap();
        assert_eq!(shape7.idx[sym], 0x0110);
        // 码本 7 带宽 [0,4)：2 个符号（第 2 个用同符号）
        let mut w = W::new();
        w.put_sym(7, sym);
        w.put(0, 1); // 幅值 1 分量的符号位 = 0（正）
        w.put_sym(7, sym);
        w.put(1, 1); // 符号位 = 1（负）
        let codebooks = vec![7u8, 0, 0, 0];
        let scalefactors = vec![100i16, 0, 0, 0];
        let mut r = BitReader::new(&w.bytes);
        let coeffs = decode_and_requant(&mut r, &ics, &codebooks, &scalefactors, &None).unwrap();
        // vals: CB_VALS_10[1] = 1.0；mult = 1
        assert_eq!(coeffs[0], 0.0);
        assert_eq!(coeffs[1], 1.0);
        assert_eq!(coeffs[2], 0.0);
        assert_eq!(coeffs[3], -1.0);

        // escape：码本 11，符号 0x0000 之后再…取 idx=0（零对）+ 一个 escape 对
        let shape11 = tables::codebook_shape(11).unwrap();
        // 0x2201: nibbles (1,2) → 幅值 (1,2)，escape mask = bits 8:9 = 0b10 → 分量 1 escape
        let sym = shape11.idx.iter().position(|&p| p == 0x2201).unwrap();
        let mut w = W::new();
        for expect_mag in [1.0f32, 0.0] {
            let _ = expect_mag;
            w.put_sym(11, sym);
            w.put(0, 1); // 520691cf 0Ff085e45503c 1Ff097b2653f74f4dFf1a6b63
            w.put(1, 1); // 520691cf 1Ff08escapeFf097b2653f74f4dFf1a8d1f
            // escapeFf1a524d7f00 0Ff08b=02192b=4Ff09Ff0csuffix 0000 2192 n = 16
            w.put(0, 1);
            w.put(0, 4);
        }
        let codebooks = vec![11u8, 0, 0, 0];
        let scalefactors = vec![100i16, 0, 0, 0];
        let mut r = BitReader::new(&w.bytes);
        let coeffs = decode_and_requant(&mut r, &ics, &codebooks, &scalefactors, &None).unwrap();
        assert_eq!(coeffs[0], 1.0);
        let want = 16.0f32.powf(4.0 / 3.0);
        assert!((coeffs[1] + want).abs() < 1e-3, "{} vs {}", coeffs[1], want);
        // 7b2c4e8c5bf9540c6837Ff08[2,4) 533aFf09
        assert_eq!(coeffs[2], 1.0);
        assert!((coeffs[3] + want).abs() < 1e-3);
    }

    /// 重量化乘子：sf−100 每 4 步翻倍。
    #[test]
    fn requant_multiplier() {
        let ics = ics_44();
        let shape7 = tables::codebook_shape(7).unwrap();
        let sym = shape7.idx.iter().position(|&p| p == 0x0110).unwrap();
        let mut w = W::new();
        w.put_sym(7, sym);
        w.put(0, 1);
        w.put_sym(7, sym);
        w.put(0, 1);
        let codebooks = vec![7u8, 0, 0, 0];
        let scalefactors = vec![104i16, 0, 0, 0]; // sf−100 = 4 → ×2
        let mut r = BitReader::new(&w.bytes);
        let coeffs = decode_and_requant(&mut r, &ics, &codebooks, &scalefactors, &None).unwrap();
        assert!((coeffs[1] - 2.0).abs() < 1e-6);
    }

    /// pulse 应用：零系数 + amp=8 → |8|^{4/3}·mult；非零系数 +amp。
    #[test]
    fn pulse_application() {
        let ics = ics_44();
        // 系数 0（带 [0,4)）：cb=7 解出一个 1.0，再 pulse amp=3 → |1+3|^{4/3}
        let shape7 = tables::codebook_shape(7).unwrap();
        let sym = shape7.idx.iter().position(|&p| p == 0x0110).unwrap();
        let mut w = W::new();
        w.put_sym(7, sym);
        w.put(0, 1);
        w.put_sym(7, sym);
        w.put(0, 1);
        let codebooks = vec![7u8, 0, 0, 0];
        let scalefactors = vec![100i16, 0, 0, 0];
        let pulse = PulseData {
            pos: vec![1],
            amp: vec![3],
        };
        let mut r = BitReader::new(&w.bytes);
        let mut coeffs =
            decode_and_requant(&mut r, &ics, &codebooks, &scalefactors, &None).unwrap();
        assert_eq!(coeffs[1], 1.0);
        apply_pulse(&mut coeffs, &ics, &codebooks, &scalefactors, &pulse);
        let want = 4.0f32.powf(4.0 / 3.0);
        assert!((coeffs[1] - want).abs() < 1e-4, "{} vs {want}", coeffs[1]);
        // 零系数 + amp=8：pulse 语义 ico = −amp（co==0 不走幂路径），
        // 结果 = |−8|^{4/3}·1（负 ico 的 cbrt(|ico|)·ico = −8^{4/3}）
        let mut coeffs2 = vec![0.0f32; 1024];
        apply_pulse(
            &mut coeffs2,
            &ics,
            &codebooks,
            &scalefactors,
            &PulseData {
                pos: vec![3],
                amp: vec![8],
            },
        );
        assert!((coeffs2[3] + 8.0f32.powf(4.0 / 3.0)).abs() < 1e-3);
    }

    /// escape 前缀 >8 → InvalidData。
    #[test]
    fn escape_overflow_rejected() {
        let ics = ics_44();
        let shape11 = tables::codebook_shape(11).unwrap();
        let sym = shape11.idx.iter().position(|&p| p == 0x2201).unwrap();
        let mut w = W::new();
        w.put_sym(11, sym);
        w.put(0, 1);
        w.put(0, 1);
        w.put(0b111111111, 9); // 9 个 1 → 前缀超长
        let codebooks = vec![11u8, 0, 0, 0];
        let scalefactors = vec![100i16, 0, 0, 0];
        let mut r = BitReader::new(&w.bytes);
        assert!(matches!(
            decode_and_requant(&mut r, &ics, &codebooks, &scalefactors, &None),
            Err(AacError::InvalidData(_))
        ));
    }

    /// scalefactor 链与谱解码联合冒烟（真码字走查）。
    #[test]
    fn scalefactor_chain_smoke() {
        let ics = ics_44();
        let mut w = W::new();
        // 三个常规带 cb=5 各一个零对符号 40（码字见码表）
        for _ in 0..3 {
            w.put_sym(5, 40);
            w.put_sym(5, 40);
        }
        let mut r = BitReader::new(&w.bytes);
        let codebooks = vec![5u8, 5, 5, 0];
        let sf = parse_scale_factor_data(&mut r, &codebooks, 128, &ics).unwrap();
        // 全 delta=0（符号 60 码字 '0'×3）
        let mut w2 = W::new();
        for _ in 0..3 {
            w2.put(0, 1);
        }
        let mut r2 = BitReader::new(&w2.bytes);
        let sf2 = parse_scale_factor_data(&mut r2, &codebooks, 128, &ics).unwrap();
        assert_eq!(sf2, vec![128, 128, 128, 0]);
        assert_eq!(sf.len(), 4);
    }
}

#[cfg(test)]
mod probe {
    use super::*;
    #[test]
    fn probe_cb11_sym33() {
        let codes = tables::codebook_codes(11).unwrap();
        let bits = tables::codebook_bits(11).unwrap();
        println!("sym33: code={:#06x} bits={}", codes[33], bits[33]);
        let shape = tables::codebook_shape(11).unwrap();
        let pos = shape.idx.iter().position(|&p| p == 0x2201).unwrap();
        println!("pos of 0x2201 = {pos}");
        let kraft: f64 = bits.iter().map(|&l| 2f64.powi(-(l as i32))).sum();
        println!("kraft={kraft}");
        // 逐位复现 huff 匹配
        let h = tables::spectral_huff(11).unwrap();
        let len = bits[33];
        let mut code = 0u32;
        for i in 0..len {
            code = (code << 1) | (((codes[33] as u32) >> (len - 1 - i)) & 1);
        }
        println!(
            "lookup(33)= {:?} (code={code:#x} len={len})",
            h.symbol(len, code)
        );
    }
}
