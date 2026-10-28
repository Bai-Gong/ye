//! 量化与谱码本编码几何：正向量化、码本反向符号表（量化整数组 →
//! Huffman 符号）、escape 逃逸序列。
//!
//! ## 量化口径（与解码侧重量化互逆）
//!
//! 解码侧重建值 `x̂ = sign(q)·|q|^{4/3}·2^{(sf−100)/4}`（谱值表
//! `|q|^{4/3}` 来自 [`crate::tables`]，escape 走 `cbrt43`）。编码侧
//! 正向量化（f64）：
//!
//! ```text
//! q = sign(x)·round((|x|·2^{(100−sf)/4})^{3/4})，钳 |q| ≤ 8191
//! ```
//!
//! ## 码本几何（写侧）
//!
//! 解码侧由符号取 `idx` 打包半字节/双位 → 值表索引；写侧建**反向表**
//! `量化整数组 → 符号`（静态缓存，逐码本一次构建）：
//!
//! - 码本 1/2（quad）：分量 ∈ {−1,0,1} 直接带符号（无符号位），
//!   反向键 = 各分量的值表索引（−1→0、0→1、+1→2）；
//! - 码本 3/4（quad）：幅值 0..=2 + 非零分量后随符号位（`CB_IDX_02`
//!   双位域只用 0..=2，3 从不出现——3⁴ = 81 满网格）；
//! - 码本 5/6（pair）：带符号九电平 ±(0..=4)，反向键 = `q+4`；
//! - 码本 7/8（pair）：幅值 0..=7 + 符号位（共用 8×8 索引网格
//!   `CB_IDX_6`，键 = `|q|`）；
//! - 码本 9/10（pair）：幅值 0..=12 + 符号位（13×13 网格 `CB_IDX_8`）；
//! - 码本 11（pair）：幅值 0..=15、16 = escape（`idx` 位 8:9 为逃逸
//!   掩码），逃逸分量的幅值半字节无意义 → 反向键以哨兵 16 归一；
//!   `|q|` ≥ 16 时按 ISO/IEC 14496-3 写逃逸序列：`b` 个 1 + 0 的
//!   一元前缀（`b = floor(log2|q|) − 4` ∈ 0..=8）+ `b+4` 位后缀
//!   （`|q| − 2^{b+4}`），重建值 `n^{4/3}`。
//!
//! 各码本 Huffman 表是满网格（3⁴/9²/8²/13²/17²），合法量化组必有
//! 符号；越界（幅值超码本上限）返回 `None` 交由码本选择层排除。

use std::collections::HashMap;
use std::sync::OnceLock;

use super::bitwriter::BitWriter;
use crate::tables::{codebook_bits, codebook_codes, codebook_shape};

/// escape 量化幅值上限（cbrt43 表长）。
pub(crate) const Q_ESCAPE_MAX: i32 = 8191;

/// 一个谱码本的写侧几何：反向符号表 + 幅值上限 + 形态位。
struct CbEncode {
    /// 量化整数组（quad 4 元 / pair 2 元，逃逸分量以哨兵归一）→ 符号。
    lookup: HashMap<Vec<i16>, u16>,
    /// 可编码的最大 |q|（不含 escape：码本 11 为 15）。
    max_mag: i32,
    /// 每符号分量数（4 = quad、2 = pair）。
    dim: usize,
    /// 分量后随符号位（码本 3/4/7..=11；escape 分量也耗符号位；
    /// 1/2/5/6 符号内嵌于值表，不耗位）。
    sign_bits: bool,
}

/// 逃逸分量在反向键中的哨兵（表内逃逸半字节无意义，统一归一）。
const ESC_KEY: i16 = 16;

/// 码本 `cb`（1..=11）的写侧几何（静态缓存）。
fn cb_encode(cb: u8) -> Option<&'static CbEncode> {
    static TABLES: OnceLock<Vec<Option<CbEncode>>> = OnceLock::new();
    let all = TABLES.get_or_init(|| {
        let mut arr: Vec<Option<CbEncode>> = (0..12).map(|_| None).collect();
        for cb in 1u8..=11 {
            arr[cb as usize] = build_cb_encode(cb);
        }
        arr
    });
    all.get(cb as usize).and_then(|s| s.as_ref())
}

/// 构建单个码本的反向表：遍历全部符号，按其 `idx` 打包解出量化组，
/// 塞进 `量化组 → 符号` 哈希。逃逸键归一后可能多符号同键（逃逸
/// 半字节无意义），取码字最短者；非逃逸键由满网格保证唯一。
fn build_cb_encode(cb: u8) -> Option<CbEncode> {
    let shape = codebook_shape(cb)?;
    let bits = codebook_bits(cb)?;
    let mut lookup: HashMap<Vec<i16>, u16> = HashMap::with_capacity(shape.size);
    let mut best_len: HashMap<Vec<i16>, u32> = HashMap::with_capacity(shape.size);
    for (sym, &idx) in shape.idx.iter().enumerate() {
        let packed = usize::from(idx);
        // 组键：quad → 4 个值表索引；pair → 两个半字节（逃逸位并入）
        let key: Vec<i16> = if cb <= 4 {
            (0..4).map(|i| ((packed >> (2 * i)) & 0x3) as i16).collect()
        } else if cb == 11 {
            let mask = (packed >> 8) & 0x3;
            let n0 = (packed & 0xF) as i16;
            let n1 = ((packed >> 4) & 0xF) as i16;
            vec![
                if mask & 1 != 0 { ESC_KEY } else { n0 },
                if mask & 2 != 0 { ESC_KEY } else { n1 },
            ]
        } else {
            vec![(packed & 0xF) as i16, ((packed >> 4) & 0xF) as i16]
        };
        let len = u32::from(bits[sym]);
        let _ = sym;
        if best_len.get(&key).is_none_or(|&l| len < l) {
            best_len.insert(key.clone(), len);
            lookup.insert(key, sym as u16);
        }
    }
    let max_mag: i32 = match cb {
        1 | 2 => 1,
        3 | 4 => 2,
        5 | 6 => 4,
        7 | 8 => 7,   // 码本 7/8 共用 8×8 索引网格（CB_IDX_6）
        9 | 10 => 12, // 13×13（CB_IDX_8）
        _ => 15,      // 11：16×16 + escape（CB_IDX_10）
    };
    Some(CbEncode {
        lookup,
        max_mag,
        dim: shape.dim,
        sign_bits: matches!(cb, 3 | 4 | 7..=11),
    })
}

/// 正向量化一个频带：系数切片 + 该带 sf → 量化整数（写入 `out`）。
///
/// f64 幂律口径见模块文档；输入非有限值按 0 处理（不 panic）。
pub(crate) fn quantize_band(x: &[f32], sf: i16, out: &mut [i32]) {
    let g = 2f64.powf((100.0 - f64::from(sf)) / 4.0);
    for (o, &v) in out.iter_mut().zip(x.iter()) {
        let a = f64::from(v.abs());
        if !a.is_finite() || a <= 0.0 {
            *o = 0;
            continue;
        }
        let q = (a * g).powf(0.75).round();
        let q = q.min(f64::from(Q_ESCAPE_MAX));
        *o = if v < 0.0 { -q as i32 } else { q as i32 };
    }
}

/// 码本 `cb` 可编码的最大幅值（选码本候选用）。
pub(crate) fn cb_max_mag(cb: u8) -> i32 {
    cb_encode(cb).map_or(0, |e| e.max_mag)
}

/// 码本 `cb` 每符号分量数。
pub(crate) fn cb_dim(cb: u8) -> usize {
    cb_encode(cb).map_or(2, |e| e.dim)
}

/// 量化组 → 符号查找（组内值须在码本幅值域内，否则 `None`）。
fn lookup_symbol(cb: u8, group: &[i32]) -> Option<u16> {
    let enc = cb_encode(cb)?;
    let mut key = Vec::with_capacity(group.len());
    for &q in group {
        let a = q.abs();
        if a > enc.max_mag {
            if cb == 11 && a <= Q_ESCAPE_MAX {
                key.push(ESC_KEY);
                continue;
            }
            return None; // 幅值超码本域
        }
        let k = match cb {
            1 | 2 => match q {
                -1 => 0,
                0 => 1,
                _ => 2, // +1（幅值域 ≤ 1 保证）
            },
            5 | 6 => (q + 4) as i16,
            _ => a as i16,
        };
        key.push(k);
    }
    enc.lookup.get(&key).copied()
}

/// escape 序列位数（`|q|` ∈ 16..=8191；其余 0）。
fn escape_bits(q: i32) -> usize {
    let a = q.unsigned_abs();
    if a <= 15 {
        return 0;
    }
    let b = (32 - a.leading_zeros() - 1) - 4; // floor(log2|q|) − 4
    b as usize + 1 + (b as usize + 4)
}

/// 写 escape 序列：`b` 个 1 + 0 前缀 + `b+4` 位后缀（n = |q|）。
fn write_escape(w: &mut BitWriter, q: i32) {
    let a = q.unsigned_abs();
    debug_assert!((16..=Q_ESCAPE_MAX as u32).contains(&a));
    let b = (32 - a.leading_zeros() - 1) - 4;
    for _ in 0..b {
        w.put(1, 1);
    }
    w.put(0, 1);
    w.put(a - (1 << (b + 4)), b + 4);
}

/// 一个量化组在码本 `cb` 下的编码位数（符号码 + 符号位 + escape）。
/// 组值超域 → `None`（候选排除）。
pub(crate) fn group_cost(cb: u8, group: &[i32]) -> Option<usize> {
    let enc = cb_encode(cb)?;
    let sym = lookup_symbol(cb, group)?;
    let mut bits = usize::from(codebook_bits(cb)?[sym as usize]);
    if enc.sign_bits {
        bits += group.iter().filter(|&&q| q != 0).count();
    }
    if cb == 11 {
        for &q in group {
            bits += escape_bits(q);
        }
    }
    Some(bits)
}

/// 写一个量化组：符号码字 +（依序）非零/逃逸分量符号位 + 逃逸序列。
///
/// 调用方保证组值在码本域内（经 [`group_cost`] 筛过的候选）。
pub(crate) fn write_group(w: &mut BitWriter, cb: u8, group: &[i32]) {
    let enc = cb_encode(cb).expect("合法码本");
    let sym = lookup_symbol(cb, group).expect("组值在码本域内");
    w.put(
        u32::from(codebook_codes(cb).unwrap()[sym as usize]),
        u32::from(codebook_bits(cb).unwrap()[sym as usize]),
    );
    if enc.sign_bits {
        for &q in group {
            if q != 0 {
                w.put_bit(q < 0);
            }
        }
    }
    if cb == 11 {
        for &q in group {
            if q.abs() > enc.max_mag {
                write_escape(w, q);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::BitReader;
    use crate::tables::codebook_shape;

    /// 反向表规模：满网格码本键数 = 符号数（码本 8 与 7 共用 8×8
    /// 索引网格）；11 号 = 17² 归一键（16×16 常规 + 逃逸行/列/角）。
    #[test]
    fn reverse_tables_cover_grids() {
        let expect = [
            (1u8, 81usize),
            (2, 81),
            (3, 81),
            (4, 81),
            (5, 81),
            (6, 81),
            (7, 64),
            (8, 64),
            (9, 169),
            (10, 169),
            (11, 289),
        ];
        for (cb, n) in expect {
            assert_eq!(cb_encode(cb).unwrap().lookup.len(), n, "cb={cb}");
        }
        // 码本 11 逃逸键抽查：单逃逸/双逃逸/边界幅值
        for g in [
            vec![16i32, 0],
            vec![0, 16],
            vec![16, 16],
            vec![8191, 15],
            vec![-16, 1],
            vec![255, 8191],
        ] {
            assert!(lookup_symbol(11, &g).is_some(), "cb11 {g:?}");
        }
    }

    /// 组级写 → 解码侧语义重建（含符号位次序与 escape）：逐码本取
    /// 值域内代表组，写出的位流按解码器口径（符号 → 值表 + 符号位
    /// + 逃逸序列）读回应逐值相等。
    #[test]
    fn write_group_roundtrip_via_decoder_semantics() {
        let decode_group = |cb: u8, r: &mut BitReader| -> Vec<i32> {
            let shape = codebook_shape(cb).unwrap();
            let sym = r.spectral_symbol(cb).unwrap() as usize;
            let packed = usize::from(shape.idx[sym]);
            let nibs: Vec<usize> = if cb <= 4 {
                (0..4).map(|i| (packed >> (2 * i)) & 0x3).collect()
            } else {
                vec![packed & 0xF, (packed >> 4) & 0xF]
            };
            let esc_mask = if cb == 11 { (packed >> 8) & 0x3 } else { 0 };
            let mags: Vec<i32> = nibs
                .iter()
                .map(|&n| match cb {
                    1 | 2 => [-1, 0, 1][n.min(2)],
                    3 | 4 | 7..=11 => n as i32,
                    _ => n as i32 - 4, // 5 | 6
                })
                .collect();
            let escaped: Vec<bool> = (0..mags.len()).map(|i| esc_mask & (1 << i) != 0).collect();
            // 符号位：非零或逃逸分量依序（1/2/5/6 无）
            let signs: Vec<bool> = if matches!(cb, 3 | 4 | 7..=11) {
                mags.iter()
                    .zip(&escaped)
                    .map(|(&m, &e)| m != 0 || e)
                    .map(|need| need && r.bit().unwrap() == 1)
                    .collect()
            } else {
                vec![false; mags.len()]
            };
            // 逃逸序列：逃逸分量依序
            let mut vals = mags;
            for (i, &e) in escaped.iter().enumerate() {
                if e {
                    let mut b = 0u32;
                    while r.bit().unwrap() == 1 {
                        b += 1;
                    }
                    let suffix = r.bits(b + 4).unwrap();
                    vals[i] = ((1u32 << (b + 4)) + suffix) as i32;
                }
            }
            for (i, &s) in signs.iter().enumerate() {
                if s {
                    vals[i] = -vals[i];
                }
            }
            vals
        };

        let cases: Vec<(u8, Vec<Vec<i32>>)> = vec![
            (
                1,
                vec![vec![0, 0, 0, 0], vec![-1, 1, 0, -1], vec![1, 1, 1, 1]],
            ),
            (2, vec![vec![0, 0, 0, 0], vec![1, -1, -1, 0]]),
            (
                3,
                vec![vec![0, 0, 0, 0], vec![2, -2, 1, 0], vec![-2, 2, -1, 1]],
            ),
            (4, vec![vec![0, 0, 0, 0], vec![1, 2, 2, -2]]),
            (5, vec![vec![0, 0], vec![4, -4], vec![-1, 2], vec![-3, 3]]),
            (6, vec![vec![0, 0], vec![2, -2], vec![4, 0]]),
            (7, vec![vec![0, 0], vec![7, -7], vec![-5, 3]]),
            (8, vec![vec![0, 0], vec![7, -7], vec![6, 3]]),
            (9, vec![vec![0, 0], vec![-9, 11], vec![12, 1]]),
            (10, vec![vec![0, 0], vec![8, -8]]),
            (
                11,
                vec![
                    vec![0, 0],
                    vec![15, -15],
                    vec![16, 0],
                    vec![-16, 1],
                    vec![255, 17],
                    vec![8191, -8191],
                ],
            ),
        ];
        for (cb, groups) in cases {
            let mut w = BitWriter::new();
            for g in &groups {
                write_group(&mut w, cb, g);
            }
            w.align_zero();
            let bytes = w.into_bytes();
            let mut r = BitReader::new(&bytes);
            for g in &groups {
                let got = decode_group(cb, &mut r);
                assert_eq!(got, *g, "cb={cb} group={g:?}");
            }
            r.byte_align(); // 尾部补零位跳过（写侧 align_zero）
            assert_eq!(r.remaining(), 0, "cb={cb} 恰好耗尽");
        }
    }

    /// 组成本/越界：幅值超域 → None；escape 位数口径抽查。
    #[test]
    fn group_cost_guards() {
        assert!(group_cost(7, &[8, 0]).is_none(), "cb7 幅值 8 超域");
        assert!(group_cost(8, &[8, 0]).is_none(), "cb8 幅值域同为 0..=7");
        assert!(group_cost(9, &[13, 0]).is_none());
        assert!(group_cost(4, &[3, 0, 0, 0]).is_none(), "quad 幅值域 0..=2");
        assert!(group_cost(5, &[5, 0]).is_none());
        assert!(group_cost(2, &[2, 0]).is_none());
        assert!(group_cost(11, &[8192, 0]).is_none(), "超 escape 上限");
        assert!(group_cost(11, &[8191, 0]).is_some());
        assert_eq!(escape_bits(16), 5);
        assert_eq!(escape_bits(8191), 21);
        assert_eq!(escape_bits(15), 0);
        // 成本随分量非零数增长（符号位）
        let z = group_cost(7, &[0, 0]).unwrap();
        let nz = group_cost(7, &[1, 0]).unwrap();
        assert!(nz > z, "符号位应计入成本");
    }

    /// 正向量化口径：重建值落在半个局部步长内、钳顶、符号。
    #[test]
    fn quantize_band_inverse() {
        let sf = 180i16;
        let g = 2f64.powf((100.0 - f64::from(sf)) / 4.0);
        let x: Vec<f32> = vec![3.0e7, -1.2e6, 0.0, 4.5e3, -20.0];
        let mut q = vec![0i32; x.len()];
        quantize_band(&x, sf, &mut q);
        for (i, &qi) in q.iter().enumerate() {
            let rebuilt = (qi.abs() as f64).powf(4.0 / 3.0) / g;
            let orig = f64::from(x[i].abs());
            assert!(qi.abs() <= Q_ESCAPE_MAX);
            if qi == 0 {
                // 死区：q=0 ⇔ |x| < 0.5^{4/3}·2^{(sf−100)/4}
                let dead_edge = 0.5f64.powf(4.0 / 3.0) / g;
                assert!(
                    orig <= dead_edge,
                    "i={i}: q=0 但 |x|={orig} 超死区 {dead_edge}"
                );
            } else {
                // 局部步长 ~ (4/3)·q^{1/3}·G
                let step = (4.0 / 3.0) * (qi.abs() as f64).powf(1.0 / 3.0) / g;
                assert!(
                    (rebuilt - orig).abs() <= step,
                    "i={i}: {rebuilt} vs {orig} (step {step})"
                );
            }
        }
        assert_eq!(q[2], 0);
        assert!(q[0] > 0 && q[1] < 0, "符号保持");
        // 钳顶：极大系数 + 极细 sf → ±8191
        let mut q2 = vec![0i32; 2];
        quantize_band(&[1.0e30f32, -1.0e30], 0, &mut q2);
        assert_eq!(q2, vec![Q_ESCAPE_MAX, -Q_ESCAPE_MAX]);
    }
}
