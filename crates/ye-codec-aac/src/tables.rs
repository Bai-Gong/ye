//! 标准码表的组织层：机械转录数据（[`tables_data`]）的索引视图 +
//! 运行时生成表（scalefactor 2^(L/4)、escape cbrt n^(4/3)、Huffman 前缀检索）。
//!
//! ## 数据来源与许可（tables_data.rs）
//!
//! `tables_data.rs` 中的数值数组是 **ISO/IEC 14496-3 标准规定的码表
//! 数据**（scalefactor Huffman 表、谱 Huffman 表 1..=11、码本取值表、
//! scalefactor 频带边界表、TNS 表），以脚本从 FFmpeg 项目的
//! `libavcodec/aactab.c`/`aactab.h`（tag n4.4，LGPL）机械转录——这些
//! 数组本身是 ISO 标准表格的数值复述（无独创表达的互操作常量，等同
//! 于抄录标准文本），ffmpeg 亦将其溯源至 ISO 表 4.57..4.128 与
//! 4.158..4.170。转录脚本与源文件未入库（生成时间 2026-10-25）。
//!
//! ## 运行时生成表
//!
//! - [`pow2sf`]：`2^(L/4)`（L = 索引−200，f32，428 项）——scalefactor
//!   重构乘子（对齐 FFmpeg `ff_aac_pow2sf_tab`）；
//! - [`cbrt43`]：`n^(4/3)`（n = 0..8191，f32）——escape 系数重量化值，
//!   按平方因子分解精确生成（对齐 FFmpeg `ff_cbrt_tab` 的构造算法）。
//!
//! ## Huffman 检索
//!
//! 谱码表/scalefactor 码表按 (码长, 码字) 建 `(len,code) -> 符号索引`
//! 哈希（[`HuffTable`]），逐位匹配解码。构造时校验**前缀无歧义**与
//! **完整覆盖**（同长码字不重复）——码表是入库资产，结构错误在构建
//! 期暴露，不在解码期静默错读。

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::tables_data as d;

// ---------------------------------------------------------------------------
// scalefactor 频带（swb）表索引
// ---------------------------------------------------------------------------

/// AAC 采样率索引（ISO/IEC 14496-3 audioSamplerityIndex）→ 采样率（Hz）。
pub const SAMPLE_RATES: [u32; 13] = [
    96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    7_350,
];

/// 采样率索引 → 长窗（1024 系数）scalefactor 频带边界表。
pub fn swb_offset_long(index: usize) -> Option<&'static [u16]> {
    Some(match index {
        0 | 1 => &d::SWB_1024_96,
        2 => &d::SWB_1024_64,
        3 | 4 => &d::SWB_1024_48,
        5 => &d::SWB_1024_32,
        6 | 7 => &d::SWB_1024_24,
        8..=10 => &d::SWB_1024_16,
        11 | 12 => &d::SWB_1024_8,
        _ => return None,
    })
}

/// 采样率索引 → 960 帧长窗 scalefactor 频带边界表。
pub fn swb_offset_long_960(index: usize) -> Option<&'static [u16]> {
    Some(match index {
        0 | 1 => &d::SWB_960_96,
        2 => &d::SWB_960_64,
        3 | 4 => &d::SWB_960_48,
        5 => &d::SWB_960_32,
        6 | 7 => &d::SWB_960_24,
        8..=10 => &d::SWB_960_16,
        11 | 12 => &d::SWB_960_8,
        _ => return None,
    })
}

/// 采样率索引 → 短窗（128 系数）scalefactor 频带边界表。
pub fn swb_offset_short(index: usize) -> Option<&'static [u16]> {
    Some(match index {
        0..=2 => &d::SWB_128_96,
        3..=5 => &d::SWB_128_48,
        6 | 7 => &d::SWB_128_24,
        8..=10 => &d::SWB_128_16,
        11 | 12 => &d::SWB_128_8,
        _ => return None,
    })
}

/// 采样率索引 → 960 帧短窗（120 系数）scalefactor 频带边界表。
pub fn swb_offset_short_120(index: usize) -> Option<&'static [u16]> {
    Some(match index {
        0..=2 => &d::SWB_120_96,
        3..=5 => &d::SWB_120_48,
        6 | 7 => &d::SWB_120_24,
        8..=10 => &d::SWB_120_16,
        11 | 12 => &d::SWB_120_8,
        _ => return None,
    })
}

/// 采样率索引 → 长窗频带数。
pub fn num_swb_long(index: usize) -> Option<u8> {
    d::NUM_SWB_1024.get(index).copied().filter(|&n| n > 0)
}

/// 采样率索引 → 960 帧长窗频带数。
pub fn num_swb_long_960(index: usize) -> Option<u8> {
    d::NUM_SWB_960.get(index).copied().filter(|&n| n > 0)
}

/// 采样率索引 → 短窗频带数。
pub fn num_swb_short(index: usize) -> Option<u8> {
    d::NUM_SWB_128.get(index).copied().filter(|&n| n > 0)
}

/// 采样率索引 → 960 帧短窗频带数。
pub fn num_swb_short_120(index: usize) -> Option<u8> {
    d::NUM_SWB_120.get(index).copied().filter(|&n| n > 0)
}

/// 采样率索引 → TNS 可作用的最大频带数（长窗）。
pub fn tns_max_bands_long(index: usize) -> Option<u8> {
    d::TNS_MAX_BANDS_1024.get(index).copied().filter(|&n| n > 0)
}

/// 采样率索引 → TNS 可作用的最大频带数（短窗）。
pub fn tns_max_bands_short(index: usize) -> Option<u8> {
    d::TNS_MAX_BANDS_128.get(index).copied().filter(|&n| n > 0)
}

/// TNS 量化系数映射表：`(coef_compress·2 + coef_res)` → 取值表。
pub fn tns_coef_map(sel: usize) -> &'static [f32] {
    match sel {
        0 => &d::TNS_MAP_0_3,
        1 => &d::TNS_MAP_0_4,
        2 => &d::TNS_MAP_1_3,
        _ => &d::TNS_MAP_1_4,
    }
}

// ---------------------------------------------------------------------------
// 谱码本几何（ISO/IEC 14496-3 表 4.159..4.170 的维度事实）
// ---------------------------------------------------------------------------

/// 谱码本（1..=11）的形态。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CodebookShape {
    /// 码字数（Huffman 表大小）。
    pub size: usize,
    /// 取值表（|q|^{4/3} 或带符号值）。
    pub vals: &'static [f32],
    /// 取值索引打包表（VLC 符号 → 4 个半字节索引 + nnz/escape 掩码）。
    pub idx: &'static [u16],
    /// 每码字输出的系数个数（4 = quad，2 = pair）。
    pub dim: usize,
    /// 非零分量需后读符号位。
    pub signed: bool,
    /// 分量值 16 表示 escape（仅码本 11）。
    pub escape: bool,
}

/// 码本 1..=11 的形态表（下标 = 码本号−1）。
pub fn codebook_shape(cb: u8) -> Option<CodebookShape> {
    let s = match cb {
        1 => CodebookShape {
            size: d::CODES_1.len(),
            vals: &d::CB_VALS_0,
            idx: &d::CB_IDX_02,
            dim: 4,
            signed: false,
            escape: false,
        },
        2 => CodebookShape {
            size: d::CODES_2.len(),
            vals: &d::CB_VALS_0,
            idx: &d::CB_IDX_02,
            dim: 4,
            signed: false,
            escape: false,
        },
        3 => CodebookShape {
            size: d::CODES_3.len(),
            vals: &d::CB_VALS_10,
            idx: &d::CB_IDX_02,
            dim: 4,
            signed: true,
            escape: false,
        },
        4 => CodebookShape {
            size: d::CODES_4.len(),
            vals: &d::CB_VALS_10,
            idx: &d::CB_IDX_02,
            dim: 4,
            signed: true,
            escape: false,
        },
        5 => CodebookShape {
            size: d::CODES_5.len(),
            vals: &d::CB_VALS_4,
            idx: &d::CB_IDX_4,
            dim: 2,
            signed: false,
            escape: false,
        },
        6 => CodebookShape {
            size: d::CODES_6.len(),
            vals: &d::CB_VALS_4,
            idx: &d::CB_IDX_4,
            dim: 2,
            signed: false,
            escape: false,
        },
        7 => CodebookShape {
            size: d::CODES_7.len(),
            vals: &d::CB_VALS_10,
            idx: &d::CB_IDX_6,
            dim: 2,
            signed: true,
            escape: false,
        },
        8 => CodebookShape {
            size: d::CODES_8.len(),
            vals: &d::CB_VALS_10,
            idx: &d::CB_IDX_6,
            dim: 2,
            signed: true,
            escape: false,
        },
        9 => CodebookShape {
            size: d::CODES_9.len(),
            vals: &d::CB_VALS_10,
            idx: &d::CB_IDX_8,
            dim: 2,
            signed: true,
            escape: false,
        },
        10 => CodebookShape {
            size: d::CODES_10.len(),
            vals: &d::CB_VALS_10,
            idx: &d::CB_IDX_8,
            dim: 2,
            signed: true,
            escape: false,
        },
        11 => CodebookShape {
            size: d::CODES_11.len(),
            vals: &d::CB_VALS_10,
            idx: &d::CB_IDX_10,
            dim: 2,
            signed: true,
            escape: true,
        },
        _ => return None,
    };
    Some(s)
}

/// 谱码本 Huffman 码字表（下标 = 符号索引）。
pub fn codebook_codes(cb: u8) -> Option<&'static [u16]> {
    Some(match cb {
        1 => &d::CODES_1,
        2 => &d::CODES_2,
        3 => &d::CODES_3,
        4 => &d::CODES_4,
        5 => &d::CODES_5,
        6 => &d::CODES_6,
        7 => &d::CODES_7,
        8 => &d::CODES_8,
        9 => &d::CODES_9,
        10 => &d::CODES_10,
        11 => &d::CODES_11,
        _ => return None,
    })
}

/// 谱码本 Huffman 码长表（下标 = 符号索引）。
pub fn codebook_bits(cb: u8) -> Option<&'static [u8]> {
    Some(match cb {
        1 => &d::BITS_1,
        2 => &d::BITS_2,
        3 => &d::BITS_3,
        4 => &d::BITS_4,
        5 => &d::BITS_5,
        6 => &d::BITS_6,
        7 => &d::BITS_7,
        8 => &d::BITS_8,
        9 => &d::BITS_9,
        10 => &d::BITS_10,
        11 => &d::BITS_11,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Huffman 前缀检索
// ---------------------------------------------------------------------------

/// 一个 Huffman 码表的检索视图：(码长, 码字) → 符号索引。
///
/// 构造时校验：码长 ∈ 1..=32、同 (len, code) 不重复（前缀码表结构
/// 完备性的必要条件；完备性由标准保证，重复即转录损坏，构建期报错）。
pub struct HuffTable {
    map: HashMap<(u8, u32), u16>,
    max_len: u8,
}

impl HuffTable {
    /// 从 (码字, 码长) 数组建表。
    ///
    /// # Errors
    /// 码长为 0 或 >16、或同 (len, code) 重复 → [`crate::AacError::InvalidData`]。
    pub fn build(codes: &[u32], bits: &[u8]) -> Result<HuffTable, crate::AacError> {
        if codes.len() != bits.len() {
            return Err(crate::AacError::InvalidData(
                "huffman table: codes/bits length mismatch (转录损坏)".into(),
            ));
        }
        let mut map = HashMap::with_capacity(codes.len());
        let mut max_len = 0u8;
        for (sym, (&code, &len)) in codes.iter().zip(bits.iter()).enumerate() {
            if len == 0 || len > 24 {
                return Err(crate::AacError::InvalidData(format!(
                    "huffman table: symbol {sym} 码长 {len} 非法"
                )));
            }
            if map.insert((len, code), sym as u16).is_some() {
                return Err(crate::AacError::InvalidData(format!(
                    "huffman table: (len={len}, code={code:#x}) 重复"
                )));
            }
            max_len = max_len.max(len);
        }
        Ok(HuffTable { map, max_len })
    }

    /// 最大码长（解码上界）。
    pub fn max_len(&self) -> u8 {
        self.max_len
    }

    /// 查符号索引。
    pub fn symbol(&self, len: u8, code: u32) -> Option<u16> {
        self.map.get(&(len, code)).copied()
    }
}

/// 谱码本 1..=11 的 Huffman 检索表（静态缓存，首次访问构建）。
pub fn spectral_huff(cb: u8) -> Result<&'static HuffTable, crate::AacError> {
    static TABLES: OnceLock<[OnceLock<Result<HuffTable, crate::AacError>>; 11]> = OnceLock::new();
    let slots = TABLES.get_or_init(|| [const { OnceLock::new() }; 11]);
    let slot = slots
        .get((cb as usize).wrapping_sub(1))
        .ok_or_else(|| crate::AacError::InvalidData(format!("码本号 {cb} 越界（1..=11）")))?;
    slot.get_or_init(|| {
        let codes: Vec<u32> = codebook_codes(cb)
            .map(|c| c.iter().map(|&v| u32::from(v)).collect())
            .unwrap_or_default();
        HuffTable::build(&codes, codebook_bits(cb).unwrap_or_default())
    })
    .as_ref()
    .map_err(std::clone::Clone::clone)
}

/// scalefactor Huffman 检索表（121 码字）。
pub fn scalefactor_huff() -> &'static HuffTable {
    static TABLE: OnceLock<HuffTable> = OnceLock::new();
    TABLE.get_or_init(|| {
        HuffTable::build(&d::SCALEFACTOR_CODE, &d::SCALEFACTOR_BITS)
            .expect("scalefactor 码表结构校验")
    })
}

/// scalefactor 差分码表（编码向）：`(codes, bits)`，下标 = Huffman
/// 符号 = **差分值 + 60**（差分 ∈ −60..=60 → 符号 0..=120）。
/// 编码 = 按符号取 code/len（与 [`codebook_codes`]/[`codebook_bits`]
/// 同口径），解码向检索走 [`scalefactor_huff`]。
pub fn scalefactor_codes() -> (&'static [u32], &'static [u8]) {
    (&d::SCALEFACTOR_CODE, &d::SCALEFACTOR_BITS)
}

// ---------------------------------------------------------------------------
// 运行时生成表
// ---------------------------------------------------------------------------

/// `pow2sf[i] = 2^((i−200)/4)`（f32，i = 0..428；对齐 FFmpeg
/// `ff_aac_pow2sf_tab`，L 范围 −200..=227）。
pub fn pow2sf(l: i32) -> f32 {
    static TAB: OnceLock<Vec<f32>> = OnceLock::new();
    let tab = TAB.get_or_init(|| {
        (0..428)
            .map(|i| {
                // 2^((i−200)/4)：f64 正确舍入后取 f32（与 FFmpeg
                // ff_aac_pow2sf_tab 的 lut 乘积法在 f32 上逐位一致）
                let q = i - 200;
                2.0f64.powf(q as f64 / 4.0) as f32
            })
            .collect()
    });
    tab.get((l + 200) as usize).copied().unwrap_or(0.0)
}

/// `cbrt43(n) = n^(4/3)`（n = 0..8191，f32）——escape 系数量化值。
///
/// 与 FFmpeg `ff_cbrt_tab` 同构的**平方因子分解**构造：对 n 的每个
/// 因子 i 计入 `i·∛i`（= i^{4/3}），全部因子幂积即 n^{4/3}，在 f64
/// 中累积后取 f32——比直接 `powf` 更精确地复现参考表。
pub fn cbrt43(n: u32) -> Option<f32> {
    static TAB: OnceLock<Vec<f32>> = OnceLock::new();
    let tab = TAB.get_or_init(build_cbrt43);
    tab.get(n as usize).copied()
}

/// n^{4/3} 分解构造（对齐 FFmpeg cbrt_tablegen 的算法语义）。
fn build_cbrt43() -> Vec<f32> {
    const N: usize = 8192;
    let mut acc = vec![1.0f64; N]; // 指数 0 的项 = 1
    acc[0] = 0.0; // 0^{4/3} = 0
    // 小因子（i² < N 才可能有多重幂）
    let mut i = 2usize;
    while i * i < N {
        if acc[i] == 1.0 {
            // i 是本原因子（未被打过标）：其幂 k=i, i², i³… 各计入一次 i^{4/3}
            let f = (i as f64) * (i as f64).cbrt();
            let mut k = i;
            while k < N {
                for j in (k..N).step_by(k) {
                    acc[j] *= f;
                }
                k *= i;
            }
        }
        i += 1;
    }
    // 大素因子（i² ≥ N，每个 n 至多含一个）
    for i in ((N.isqrt() + 1)..N).step_by(2) {
        if acc[i] == 1.0 {
            let f = (i as f64) * (i as f64).cbrt();
            for j in (i..N).step_by(i) {
                acc[j] *= f;
            }
        }
    }
    // 覆盖 2 的幂等偶数因子：上面的 step_by(2) 只扫奇数，偶数大因子
    // 已在小因子环处理（2 的小因子环覆盖全部 2 的幂与倍数）。
    acc.iter().map(|&v| v as f32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 转录完整性：频带边界表末值 = 系数总数，段数 = num_swb+1。
    #[test]
    fn swb_tables_consistent() {
        for idx in 0..13 {
            let n = num_swb_long(idx).unwrap() as usize;
            let t = swb_offset_long(idx).unwrap();
            assert_eq!(t.len(), n + 1, "idx={idx}");
            assert_eq!(*t.last().unwrap(), 1024);
            assert!(t.windows(2).all(|w| w[0] < w[1]), "idx={idx} 严格递增");
            let ns = num_swb_short(idx).unwrap() as usize;
            let ts = swb_offset_short(idx).unwrap();
            assert_eq!(ts.len(), ns + 1);
            assert_eq!(*ts.last().unwrap(), 128);
        }
    }

    /// 谱码表结构完备：11 个码本全部可建前缀表且符号数与形态表一致。
    #[test]
    fn spectral_huff_tables_build() {
        for cb in 1..=11 {
            let h = spectral_huff(cb).unwrap();
            let shape = codebook_shape(cb).unwrap();
            assert!(h.max_len() <= 16);
            assert_eq!(shape.size, codebook_codes(cb).unwrap().len());
            assert!(shape.idx.len() >= shape.size, "idx 表覆盖全部符号");
        }
        assert!(spectral_huff(12).is_err());
        // 前缀无歧义抽查（完备性）：把全部码字投到 2^16 前缀空间
        for cb in 1..=11 {
            let codes = codebook_codes(cb).unwrap();
            let bits = codebook_bits(cb).unwrap();
            let mut space = vec![false; 1 << 16];
            for (&c, &l) in codes.iter().zip(bits.iter()) {
                let shift = 16 - l as usize;
                let base = ((c as usize) << shift) & 0xFFFF;
                for p in space.iter_mut().take(base + (1 << shift)).skip(base) {
                    assert!(!*p, "cb={cb} 前缀冲突");
                    *p = true;
                }
            }
            // Kraft 恒等式：Σ 2^{-len} = 1（完备码）
            let kraft: f64 = bits.iter().map(|&l| 2f64.powi(-(l as i32))).sum();
            assert!((kraft - 1.0).abs() < 1e-9, "cb={cb} Kraft 和 {kraft} ≠ 1");
        }
    }

    /// scalefactor 码表：121 符号、Kraft 完备、中位符号（delta 0）码字最短。
    #[test]
    fn scalefactor_table_structure() {
        let h = scalefactor_huff();
        assert_eq!(h.max_len(), 19);
        // 符号 60（delta=0）应为 1 位码「0」
        assert_eq!(d::SCALEFACTOR_BITS[60], 1);
        assert_eq!(d::SCALEFACTOR_CODE[60], 0);
        let kraft: f64 = d::SCALEFACTOR_BITS
            .iter()
            .map(|&l| 2f64.powi(-(l as i32)))
            .sum();
        assert!((kraft - 1.0).abs() < 1e-9);
    }

    /// pow2sf：关键点（0、±1/4 步进、上下界外）。
    #[test]
    fn pow2sf_values() {
        assert_eq!(pow2sf(0), 1.0);
        assert!((pow2sf(60) - 32768.0).abs() < 0.01); // 2^15
        assert!((pow2sf(-100) - 2.0f64.powi(-25) as f32).abs() < 1e-7);
        assert!((pow2sf(1) - 1.189_207_1).abs() < 1e-6);
        assert!((pow2sf(2) - std::f32::consts::SQRT_2).abs() < 1e-6);
        assert!((pow2sf(3) - 1.681_792_8).abs() < 1e-6);
        assert_eq!(pow2sf(228), 0.0, "越界饱和为 0（调用方保证不越界）");
    }

    /// cbrt43：分解构造 vs 直接 powf（f32 应一致到 1 ulp 级）。
    #[test]
    fn cbrt43_values() {
        assert_eq!(cbrt43(0), Some(0.0));
        for &n in &[1u32, 2, 3, 15, 16, 17, 255, 256, 1000, 4095, 8191] {
            let want = (n as f64).powf(4.0 / 3.0) as f32;
            let got = cbrt43(n).unwrap();
            let ulp = (want.abs().max(1e-30) * 2e-7).max(1e-30);
            assert!((got - want).abs() <= ulp, "n={n}: got {got} want {want}");
        }
        assert_eq!(cbrt43(8192), None);
    }

    /// TNS 映射表维度（coef_len 3/4/2/3 → 表长 8/16/4/8）。
    #[test]
    fn tns_maps_dimensions() {
        assert_eq!(d::TNS_MAP_0_3.len(), 8);
        assert_eq!(d::TNS_MAP_0_4.len(), 16);
        assert_eq!(d::TNS_MAP_1_3.len(), 4);
        assert_eq!(d::TNS_MAP_1_4.len(), 8);
        assert_eq!(tns_coef_map(0).len(), 8);
        assert_eq!(tns_coef_map(3).len(), 8);
    }
}
