//! CAVLC 熵解码（H.264 §9.2 残差语法）。
//!
//! 施工图②a：码表与 level 解析算法逐字对齐 FFmpeg `h264_cavlc.c`
//! （master 分支逐字转写，并与 x264 `common/tables.c` 逐值交叉核对），
//! 包括三处「非显然」语义：
//! - level 转义（prefix ≥ 15）：FFmpeg 首层（s∈{0,1}）为 `30 + …`，
//!   与规范的 `(15<<s) + read(12)`（s==0 时 +15）在 s≤1 时取值相同；
//!   s≥2（只出现在后续 level）取规范式 `(15<<s) + …`（h264j/x264 同）；
//! - 首个 level 的 suffixLength 更新：快路径（prefix+1+s ≤ 8 位）
//!   `s = 1 + (|level| >= 4)`，慢路径恒 `s = 2`；
//! - 后续 level 的升级阈值 `suffix_limit = {0,3,6,12,24,48,∞}`，
//!   判据为符号幅值 `|level| > suffix_limit[s]`。
//!
//! 本模块只做「位流 → 系数」：反量化在 AC 块存储时按 FFmpeg 折叠式
//! `(level·qmul+32)>>6` 原地完成（DC 块存原始 level，反量化并入
//! Hadamard 逆变换，见 `macroblock.rs`）。
//!
//! 零第三方依赖、无 unsafe、无 panic：畸形位流一律
//! [`H264Error::InvalidData`]。

use crate::bitreader::BitReader;
use crate::error::H264Error;

// ---------------------------------------------------------------------------
// 码表（FFmpeg libavcodec/h264_cavlc.c master 逐字转写；
// 与 x264 common/tables.c 的 x264_coeff_token/x264_total_zeros 逐值核对一致）
// ---------------------------------------------------------------------------

/// coeff_token VLC 的 4 张表（按 nC 档位选择），每张 68 项：
/// 索引 = (total_coeff << 2) | trailing_ones，值 = 码长。
/// len==0 的项为不可达组合（无码字）。
static COEFF_TOKEN_LEN: [[u8; 68]; 4] = [
    [
        1, 0, 0, 0, // tc=0
        6, 2, 0, 0, 8, 6, 3, 0, 9, 8, 7, 5, 10, 9, 8, 6, // tc=1..4
        11, 10, 9, 7, 13, 11, 10, 8, 13, 13, 11, 9, 13, 13, 13, 10, // tc=5..8
        14, 14, 13, 11, 14, 14, 14, 13, 15, 15, 14, 14, 15, 15, 15, 14, // tc=9..12
        16, 15, 15, 15, 16, 16, 16, 15, 16, 16, 16, 16, 16, 16, 16, 16, // tc=13..16
    ],
    [
        2, 0, 0, 0, //
        6, 2, 0, 0, 6, 5, 3, 0, 7, 6, 6, 4, 8, 6, 6, 4, //
        8, 7, 7, 5, 9, 8, 8, 6, 11, 9, 9, 6, 11, 11, 11, 7, //
        12, 11, 11, 9, 12, 12, 12, 11, 12, 12, 12, 11, 13, 13, 13, 12, //
        13, 13, 13, 13, 13, 14, 13, 13, 14, 14, 14, 13, 14, 14, 14, 14, //
    ],
    [
        4, 0, 0, 0, //
        6, 4, 0, 0, 6, 5, 4, 0, 6, 5, 5, 4, 7, 5, 5, 4, //
        7, 5, 5, 4, 7, 6, 6, 4, 7, 6, 6, 4, 8, 7, 7, 5, //
        8, 8, 7, 6, 9, 8, 8, 7, 9, 9, 8, 8, 9, 9, 9, 8, //
        10, 9, 9, 9, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, //
    ],
    [
        6, 0, 0, 0, //
        6, 6, 0, 0, 6, 6, 6, 0, 6, 6, 6, 6, 6, 6, 6, 6, //
        6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, //
        6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, //
        6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, //
    ],
];

/// coeff_token 码字（与 [`COEFF_TOKEN_LEN`] 同布局；值为码字的整数值）。
static COEFF_TOKEN_BITS: [[u8; 68]; 4] = [
    [
        1, 0, 0, 0, //
        5, 1, 0, 0, 7, 4, 1, 0, 7, 6, 5, 3, 7, 6, 5, 3, //
        7, 6, 5, 4, 15, 6, 5, 4, 11, 14, 5, 4, 8, 10, 13, 4, //
        15, 14, 9, 4, 11, 10, 13, 12, 15, 14, 9, 12, 11, 10, 13, 8, //
        15, 1, 9, 12, 11, 14, 13, 8, 7, 10, 9, 12, 4, 6, 5, 8, //
    ],
    [
        3, 0, 0, 0, //
        11, 2, 0, 0, 7, 7, 3, 0, 7, 10, 9, 5, 7, 6, 5, 4, //
        4, 6, 5, 6, 7, 6, 5, 8, 15, 6, 5, 4, 11, 14, 13, 4, //
        15, 10, 9, 4, 11, 14, 13, 12, 8, 10, 9, 8, 15, 14, 13, 12, //
        11, 10, 9, 12, 7, 11, 6, 8, 9, 8, 10, 1, 7, 6, 5, 4, //
    ],
    [
        15, 0, 0, 0, //
        15, 14, 0, 0, 11, 15, 13, 0, 8, 12, 14, 12, 15, 10, 11, 11, //
        11, 8, 9, 10, 9, 14, 13, 9, 8, 10, 9, 8, 15, 14, 13, 13, //
        11, 14, 10, 12, 15, 10, 13, 12, 11, 14, 9, 12, 8, 10, 13, 8, //
        13, 7, 9, 12, 9, 12, 11, 10, 5, 8, 7, 6, 1, 4, 3, 2, //
    ],
    [
        3, 0, 0, 0, //
        0, 1, 0, 0, 4, 5, 6, 0, 8, 9, 10, 11, 12, 13, 14, 15, //
        16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, //
        32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, //
        48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, //
    ],
];

/// chroma DC 的 coeff_token（20 项：total_coeff 0..4 × trailing_ones 0..3）。
static CHROMA_DC_COEFF_TOKEN_LEN: [u8; 20] = [
    2, 0, 0, 0, // tc=0
    6, 1, 0, 0, // tc=1
    6, 6, 3, 0, // tc=2
    6, 7, 7, 6, // tc=3
    6, 8, 8, 7, // tc=4
];

/// chroma DC coeff_token 码字。
static CHROMA_DC_COEFF_TOKEN_BITS: [u8; 20] = [
    1, 0, 0, 0, //
    7, 1, 0, 0, //
    4, 6, 1, 0, //
    3, 3, 2, 5, //
    2, 3, 2, 0, //
];

/// total_zeros VLC（15 张表：total_coeff−1 索引；表内按 zeros 0..15）。
/// FFmpeg 源表每行只列到可达零数（越界项为 0 = 无码字）。
static TOTAL_ZEROS_LEN: [[u8; 16]; 15] = [
    [1, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 9],
    [3, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 6, 6, 6, 6, 0],
    [4, 3, 3, 3, 4, 4, 3, 3, 4, 5, 5, 6, 5, 6, 0, 0],
    [5, 3, 4, 4, 3, 3, 3, 4, 3, 4, 5, 5, 5, 0, 0, 0],
    [4, 4, 4, 3, 3, 3, 3, 3, 4, 5, 4, 5, 0, 0, 0, 0],
    [6, 5, 3, 3, 3, 3, 3, 3, 4, 3, 6, 0, 0, 0, 0, 0],
    [6, 5, 3, 3, 3, 2, 3, 4, 3, 6, 0, 0, 0, 0, 0, 0],
    [6, 4, 5, 3, 2, 2, 3, 3, 6, 0, 0, 0, 0, 0, 0, 0],
    [6, 6, 4, 2, 2, 3, 2, 5, 0, 0, 0, 0, 0, 0, 0, 0],
    [5, 5, 3, 2, 2, 2, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [4, 4, 3, 3, 1, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [4, 4, 2, 1, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 3, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [2, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
];

/// total_zeros 码字。
static TOTAL_ZEROS_BITS: [[u8; 16]; 15] = [
    [1, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2, 1],
    [7, 6, 5, 4, 3, 5, 4, 3, 2, 3, 2, 3, 2, 1, 0, 0],
    [5, 7, 6, 5, 4, 3, 4, 3, 2, 3, 2, 1, 1, 0, 0, 0],
    [3, 7, 5, 4, 6, 5, 4, 3, 3, 2, 2, 1, 0, 0, 0, 0],
    [5, 4, 3, 7, 6, 5, 4, 3, 2, 1, 1, 0, 0, 0, 0, 0],
    [1, 1, 7, 6, 5, 4, 3, 2, 1, 1, 0, 0, 0, 0, 0, 0],
    [1, 1, 5, 4, 3, 3, 2, 1, 1, 0, 0, 0, 0, 0, 0, 0],
    [1, 1, 1, 3, 3, 2, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 0, 1, 3, 2, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 0, 1, 3, 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 1, 2, 1, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
];

/// chroma DC 的 total_zeros（3 张表：total_coeff−1 ∈ 0..3）。
static CHROMA_DC_TOTAL_ZEROS_LEN: [[u8; 4]; 3] = [
    [1, 2, 3, 3], //
    [1, 2, 2, 0], //
    [1, 1, 0, 0], //
];

/// chroma DC total_zeros 码字。
static CHROMA_DC_TOTAL_ZEROS_BITS: [[u8; 4]; 3] = [
    [1, 1, 1, 0], //
    [1, 1, 0, 0], //
    [1, 0, 0, 0], //
];

/// run_before VLC（7 张表：zeros_left−1 索引，zeros_left ≥ 7 用表 6）。
static RUN_LEN: [[u8; 16]; 7] = [
    [1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 2, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [2, 2, 2, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [2, 2, 2, 3, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [2, 2, 3, 3, 3, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [2, 3, 3, 3, 3, 3, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 3, 3, 3, 3, 3, 3, 4, 5, 6, 7, 8, 9, 10, 11, 0],
];

/// run_before 码字。
static RUN_BITS: [[u8; 16]; 7] = [
    [1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 2, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 2, 3, 2, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [3, 0, 1, 3, 2, 5, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [7, 6, 5, 4, 3, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0],
];

/// nC（0..=16）→ coeff_token 表索引（FFmpeg coeff_token_table_index）。
static COEFF_TOKEN_TABLE_INDEX: [u8; 17] = [0, 0, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 3, 3, 3, 3, 3];

/// level 解析的 suffixLength 升级阈值：`|level| > SUFFIX_LIMIT[s]` 则 +1
/// （末项 ∞ = 恒不升级，s 封顶 6）。
static SUFFIX_LIMIT: [u32; 7] = [0, 3, 6, 12, 24, 48, u32::MAX];

// ---------------------------------------------------------------------------
// VLC 读取：逐位累积 + 前缀码匹配（码表为前缀码，首个全匹配即解）
// ---------------------------------------------------------------------------

/// 读一个 (len, bits) 前缀码：返回符号（表内索引）。
/// 位流耗尽或 17 位内无匹配 → `InvalidData`。
fn read_vlc(r: &mut BitReader<'_>, lens: &[u8], bits: &[u8]) -> Result<u8, H264Error> {
    let mut acc: u16 = 0;
    let mut len: u16 = 0;
    loop {
        if len >= 17 {
            return Err(H264Error::InvalidData(format!(
                "cavlc vlc: no codeword matched within 17 bits at bit {}",
                r.bit_position()
            )));
        }
        acc = (acc << 1) | u16::from(r.u1()?);
        len += 1;
        for (i, &l) in lens.iter().enumerate() {
            if u16::from(l) == len && u16::from(bits[i]) == acc {
                return Ok(i as u8);
            }
        }
    }
}

/// coeff_token：`(total_coeff, trailing_ones)`。
/// `nc` 为 nC（0..=16，超界钳 16）；表由 [`COEFF_TOKEN_TABLE_INDEX`] 选出。
pub(crate) fn read_coeff_token(r: &mut BitReader<'_>, nc: u8) -> Result<(u8, u8), H264Error> {
    let idx = usize::from(COEFF_TOKEN_TABLE_INDEX[usize::from(nc.min(16))]);
    let sym = read_vlc(r, &COEFF_TOKEN_LEN[idx], &COEFF_TOKEN_BITS[idx])?;
    // sym 最大 67（tc=16, t1s=3）：不得掩码 4 位——tc=16 会被截成 0
    Ok(((sym >> 2), sym & 3))
}

/// chroma DC 的 coeff_token（4:2:0 专用表）。
pub(crate) fn read_coeff_token_chroma_dc(r: &mut BitReader<'_>) -> Result<(u8, u8), H264Error> {
    let sym = read_vlc(r, &CHROMA_DC_COEFF_TOKEN_LEN, &CHROMA_DC_COEFF_TOKEN_BITS)?;
    Ok(((sym >> 2) & 0x07, sym & 3))
}

/// total_zeros（非 chroma DC）。`total_coeff` 1..=15 选表。
fn read_total_zeros(r: &mut BitReader<'_>, total_coeff: u8) -> Result<u8, H264Error> {
    let idx = usize::from(total_coeff - 1);
    if idx >= TOTAL_ZEROS_LEN.len() {
        return Err(H264Error::InvalidData(format!(
            "cavlc total_zeros: total_coeff {total_coeff} out of range"
        )));
    }
    read_vlc(r, &TOTAL_ZEROS_LEN[idx], &TOTAL_ZEROS_BITS[idx])
}

/// chroma DC 的 total_zeros。`total_coeff` 1..=4 选表。
fn read_total_zeros_chroma_dc(r: &mut BitReader<'_>, total_coeff: u8) -> Result<u8, H264Error> {
    let idx = usize::from(total_coeff - 1);
    if idx >= CHROMA_DC_TOTAL_ZEROS_LEN.len() {
        return Err(H264Error::InvalidData(format!(
            "cavlc chroma_dc total_zeros: total_coeff {total_coeff} out of range"
        )));
    }
    read_vlc(
        r,
        &CHROMA_DC_TOTAL_ZEROS_LEN[idx],
        &CHROMA_DC_TOTAL_ZEROS_BITS[idx],
    )
}

/// run_before：zeros_left 1..=7+ 选表（≥7 用表 6），返回 run 值。
fn read_run_before(r: &mut BitReader<'_>, zeros_left: u8) -> Result<u8, H264Error> {
    let idx = usize::from(zeros_left.saturating_sub(1)).min(6);
    read_vlc(r, &RUN_LEN[idx], &RUN_BITS[idx])
}

// ---------------------------------------------------------------------------
// level 解析（FFmpeg h264_cavlc.c 的 level 算法，含其转义口径）
// ---------------------------------------------------------------------------

/// 读 level 前缀（前导零个数，终止于 1）。> 28 → `InvalidData`
/// （FFmpeg 同界：prefix > 25+3 拒收）。
fn read_level_prefix(r: &mut BitReader<'_>) -> Result<u32, H264Error> {
    let mut prefix = 0u32;
    loop {
        if r.u1()? {
            return Ok(prefix);
        }
        prefix += 1;
        if prefix > 28 {
            return Err(H264Error::InvalidData(format!(
                "cavlc level_prefix {} > 28 at bit {}",
                prefix,
                r.bit_position()
            )));
        }
    }
}

/// levelCode → 有符号 level：偶为正 `c/2+1`，奇为负 `−(c+1)/2`
/// （FFmpeg `mask=-(c&1); (((2+c)>>1)^mask)-mask` 的等价直写）。
fn map_level(code: u64) -> Result<i32, H264Error> {
    let v = if code & 1 == 0 {
        ((2 + code) >> 1) as i64
    } else {
        -(((2 + code) >> 1) as i64)
    };
    i32::try_from(v)
        .map_err(|_| H264Error::InvalidData(format!("cavlc level magnitude {} overflows i32", v)))
}

/// 读一个 level。`first` = 是否为首个非 trailing level；
/// `trailing_ones` 供首 level 的 +2 调整。返回 `(level, 新 suffixLength)`。
///
/// 首 level 的 suffixLength 更新分快慢路径（FFmpeg 一致）：
/// 快（prefix+1+s ≤ 8 位）：`s = 1 + (|level| ≥ 4)`；慢（更长码）：恒 `s = 2`。
fn read_level(
    r: &mut BitReader<'_>,
    suffix_length: u8,
    first: bool,
    trailing_ones: u8,
) -> Result<(i32, u8), H264Error> {
    let prefix = read_level_prefix(r)?;
    let s = u32::from(suffix_length);

    // 原始 level_code（无符号）：FFmpeg decode_residual 逐字
    // —— 首 level 的后缀位长是 min(s,1)，prefix==14 且 s==0 读 4 位，
    // prefix>=15 为转义（30/15<<s 起）
    let mut code: u64 = if prefix < 14 {
        let sbits = s.min(if first { 1 } else { s });
        let suffix = if sbits > 0 { r.u(sbits)? } else { 0 };
        (u64::from(prefix) << sbits) + suffix
    } else if prefix == 14 {
        if s == 0 && first {
            14 + r.u(4)?
        } else if s == 0 {
            // 非 first 且 s==0 不会出现（后续层 s>=1），防御走 4 位
            14 + r.u(4)?
        } else {
            (14u64 << s) + r.u(s)?
        }
    } else {
        // prefix >= 15（转义）
        let suffix_bits = if prefix == 15 {
            r.u(12)?
        } else {
            r.u(prefix - 3)?
        };
        let mut c = if !first && s >= 2 {
            (15u64 << s) + suffix_bits
        } else {
            30 + suffix_bits
        };
        if prefix >= 16 {
            c = c.wrapping_add((1u64 << (prefix - 3)).wrapping_sub(4096));
        }
        c
    };

    if first {
        // FFmpeg 逐字：小码路径（prefix<14）对带符号 level 做 ±1 修正，
        // 后缀长度 `1 + (level_code+3U > 6U)`（无符号回绕语义，±3 不对称）；
        // 转义路径（prefix>=14）level_code += 2、后缀长度置 2。
        if prefix < 14 {
            let mut level = map_level(code)?;
            if trailing_ones < 3 {
                level += level.signum();
            }
            let s_new = 1 + u8::from(level.wrapping_add(3) as u32 > 6);
            Ok((level, s_new))
        } else {
            if trailing_ones < 3 {
                code += 2;
            }
            let level = map_level(code)?;
            Ok((level, 2))
        }
    } else {
        let level = map_level(code)?;
        let mag = level.unsigned_abs();
        let bump = mag > SUFFIX_LIMIT[usize::from(suffix_length)];
        Ok((level, suffix_length + u8::from(bump)))
    }
}

// ---------------------------------------------------------------------------
// 残差块解码：位流 → （AC：已反量化的）系数块
// ---------------------------------------------------------------------------

/// 残差块种类（决定 token 表与反量化路径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockKind {
    /// Intra16x16 的 luma DC（16 系数，存原始 level）。
    LumaDc,
    /// luma 4x4 残差块（Intra4x4：16 系数全扫描；Intra16x16 AC：15 系数
    /// scan[1..]；存 `(level·qmul+32)>>6`）。
    LumaAc,
    /// chroma DC（4 系数，存原始 level）。
    ChromaDc,
    /// chroma 4x4 残差块（15 系数，反量化同 LumaAc）。
    ChromaAc,
}

/// 解码一个残差块到 `out`（长度 16；chroma DC 用其中 4 槽位）。
///
/// - `nc`：nC（0..=16，由调用方从相邻块 nnz 算出；chroma DC 忽略）；
/// - `scan`：zigzag 序 → 块内光栅槽位的映射（Intra4x4 luma 传全 16 项
///   zigzag，Intra16x16 AC 传 zigzag[1..]，chroma AC 传 chroma zigzag[1..]，
///   chroma DC 传 `&[0, 1, 4, 5]` 即 2x2 光栅）；
/// - `qmul`：16 槽位的反量化乘子（AC 用；DC 忽略）。
///
/// 返回 `total_coeff`（供 nnz 缓存）。系数解码顺序为**逆 zigzag**
/// （level[0] 在最高频），run_before 把零游程铺在其间——与
/// FFmpeg `decode_residual` 的 STORE_BLOCK 逐位一致。
///
/// # Errors
/// 位流畸形（token 越界、zeros_left/run 越界、反量化值超出 i16）
/// → [`H264Error::InvalidData`]。
pub(crate) fn read_residual(
    r: &mut BitReader<'_>,
    kind: BlockKind,
    nc: u8,
    scan: &[u8],
    qmul: &[u32; 16],
    out: &mut [i16; 16],
) -> Result<u8, H264Error> {
    let max_coeff = scan.len() as u8;

    let (total_coeff, trailing_ones) = match kind {
        BlockKind::ChromaDc => read_coeff_token_chroma_dc(r)?,
        _ => read_coeff_token(r, nc)?,
    };
    if total_coeff == 0 {
        return Ok(0);
    }
    if trailing_ones > total_coeff || total_coeff > max_coeff {
        return Err(H264Error::InvalidData(format!(
            "cavlc residual: total_coeff {total_coeff} / trailing_ones {trailing_ones} inconsistent (max {max_coeff})"
        )));
    }

    let mut level = [0i32; 16];
    // trailing ones 符号（位=1 → −1）
    for lv in level.iter_mut().take(usize::from(trailing_ones)) {
        let b = r.u1()?;
        *lv = if b { -1 } else { 1 };
    }

    // 非 trailing 的 level 序列
    if trailing_ones < total_coeff {
        let mut s: u8 = u8::from(total_coeff > 10 && trailing_ones < 3);
        let first = usize::from(trailing_ones);
        for (i, lv_slot) in level
            .iter_mut()
            .enumerate()
            .take(usize::from(total_coeff))
            .skip(first)
        {
            let (lv, s_new) = read_level(r, s, i == first, trailing_ones)?;
            *lv_slot = lv;
            s = s_new;
        }
    }

    // total_zeros
    let mut zeros_left = if total_coeff < max_coeff {
        match kind {
            BlockKind::ChromaDc => read_total_zeros_chroma_dc(r, total_coeff)?,
            _ => read_total_zeros(r, total_coeff)?,
        }
    } else {
        0
    };

    // 铺系数（逆 zigzag 游走，FFmpeg STORE_BLOCK 同构）
    let store = |out: &mut [i16; 16], scan_pos: usize, lv: i32| -> Result<(), H264Error> {
        let slot = usize::from(scan[scan_pos]);
        let value = match kind {
            BlockKind::LumaDc | BlockKind::ChromaDc => i64::from(lv),
            BlockKind::LumaAc | BlockKind::ChromaAc => {
                (i64::from(lv) * i64::from(qmul[slot]) + 32) >> 6
            }
        };
        let v = i16::try_from(value).map_err(|_| {
            H264Error::InvalidData(format!(
                "cavlc residual: dequantized value {value} exceeds i16 (slot {slot})"
            ))
        })?;
        out[slot] = v;
        Ok(())
    };

    let mut pos = usize::from(zeros_left) + usize::from(total_coeff) - 1;
    if pos >= scan.len() {
        return Err(H264Error::InvalidData(format!(
            "cavlc residual: scan position {pos} out of range ({} slots)",
            scan.len()
        )));
    }
    store(out, pos, level[0])?;
    for lv in level.iter().take(usize::from(total_coeff)).skip(1) {
        if zeros_left > 0 {
            let run = read_run_before(r, zeros_left)?;
            if run > zeros_left {
                return Err(H264Error::InvalidData(format!(
                    "cavlc residual: run_before {run} exceeds zeros_left {zeros_left} (bit {}, tc {total_coeff}, max {max_coeff})",
                    r.bit_position()
                )));
            }
            zeros_left -= run;
            pos = pos
                .checked_sub(usize::from(run) + 1)
                .ok_or_else(|| H264Error::InvalidData("cavlc residual: scan underflow".into()))?;
        } else {
            pos = pos
                .checked_sub(1)
                .ok_or_else(|| H264Error::InvalidData("cavlc residual: scan underflow".into()))?;
        }
        if pos >= scan.len() {
            // usize 下溢已在 checked_sub 拦截；此处防御 scan 越界
            return Err(H264Error::InvalidData(format!(
                "cavlc residual: scan position {pos} out of range"
            )));
        }
        store(out, pos, *lv)?;
    }
    // 剩余 zeros_left = 最低频系数前的直流侧前导零（块已零初始化，
    // 无需动作；FFmpeg decode_residual 同语义，不报错）
    Ok(total_coeff)
}

// ---------------------------------------------------------------------------
// 单测：码表黄金位流（用表自带码字构造位流，验证读回符号）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 位串（'0'/'1'）→ 字节（右补零）。
    fn bits(s: &str) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(s.len().div_ceil(8));
        let mut acc: u8 = 0;
        let mut n = 0u32;
        for c in s.chars() {
            acc = (acc << 1) | u8::from(c == '1');
            n += 1;
            if n == 8 {
                bytes.push(acc);
                acc = 0;
                n = 0;
            }
        }
        if n > 0 {
            bytes.push(acc << (8 - n));
        }
        bytes
    }

    /// 码字的位串形式（len 位）。
    fn codeword(len: u8, bits_val: u8) -> String {
        format!("{:0width$b}", bits_val, width = len as usize)
    }

    /// 检查一组 (len, bits) 码字两两前缀无关（码表结构自检）。
    fn assert_prefix_free(lens: &[u8], bits_tab: &[u8], what: &str) {
        let mut codes: Vec<(u16, u8)> = Vec::new();
        for (&l, &b) in lens.iter().zip(bits_tab.iter()) {
            if l > 0 {
                codes.push((u16::from(b), l));
            }
        }
        for (i, &(ci, li)) in codes.iter().enumerate() {
            for &(cj, lj) in codes.iter().skip(i + 1) {
                let (big, small, bl, sl) = if li >= lj {
                    (ci, cj, li, lj)
                } else {
                    (cj, ci, lj, li)
                };
                // big 的高 sl 位是否等于 small
                let shifted = big >> (bl - sl);
                assert_ne!(shifted, small, "{what}: 码字互为前缀");
            }
        }
    }

    #[test]
    fn vlc_tables_are_prefix_free() {
        for t in 0..4 {
            assert_prefix_free(
                &COEFF_TOKEN_LEN[t],
                &COEFF_TOKEN_BITS[t],
                &format!("coeff_token[{t}]"),
            );
        }
        assert_prefix_free(
            &CHROMA_DC_COEFF_TOKEN_LEN,
            &CHROMA_DC_COEFF_TOKEN_BITS,
            "chroma_dc_coeff_token",
        );
        for t in 0..15 {
            assert_prefix_free(
                &TOTAL_ZEROS_LEN[t],
                &TOTAL_ZEROS_BITS[t],
                &format!("total_zeros[{t}]"),
            );
        }
        for t in 0..3 {
            assert_prefix_free(
                &CHROMA_DC_TOTAL_ZEROS_LEN[t],
                &CHROMA_DC_TOTAL_ZEROS_BITS[t],
                &format!("chroma_dc_total_zeros[{t}]"),
            );
        }
        for t in 0..7 {
            assert_prefix_free(&RUN_LEN[t], &RUN_BITS[t], &format!("run[{t}]"));
        }
    }

    #[test]
    fn vlc_roundtrip_all_coeff_token_entries() {
        // 每张表每个可达 (tc, t1)：码字喂进去必须读回同一符号
        for (t, (lens, bits_tab)) in COEFF_TOKEN_LEN
            .iter()
            .zip(COEFF_TOKEN_BITS.iter())
            .enumerate()
        {
            for sym in 0..68u8 {
                let (l, b) = (lens[usize::from(sym)], bits_tab[usize::from(sym)]);
                if l == 0 {
                    continue;
                }
                let data = bits(&codeword(l, b));
                let mut r = BitReader::new(&data);
                let read = read_vlc(&mut r, lens, bits_tab).unwrap();
                assert_eq!(read, sym, "table {t} symbol {sym}");
                assert_eq!(
                    r.bit_position(),
                    usize::from(l),
                    "table {t} symbol {sym} 应恰好消费码长"
                );
            }
        }
    }

    #[test]
    fn coeff_token_golden_bitstreams() {
        // 表 0（nC 档 0）的黄金对（FFmpeg 码表）：
        // (0,0)="1"；(1,0)="000101"；(1,1)="01"；(2,2)="001"；
        // (3,2)=len7 bits5="0000101"；(3,3)=len5 bits3="00011"；(4,3)=len6 bits3="000011"
        let cases: &[(&str, u8, u8)] = &[
            ("1", 0, 0),
            ("000101", 1, 0),
            ("01", 1, 1),
            ("001", 2, 2),
            ("0000101", 3, 2),
            ("00011", 3, 3),
            ("000011", 4, 3),
        ];
        for (cw, tc, t1) in cases {
            let data = bits(cw);
            let mut r = BitReader::new(&data);
            assert_eq!(
                read_coeff_token(&mut r, 0).unwrap(),
                (*tc, *t1),
                "codeword {cw}"
            );
        }
        // nC 档位切换：nC=2 → 表 1；(0,0) = len2 bits3 = "11"
        let d = bits("11");
        let mut r = BitReader::new(&d);
        assert_eq!(read_coeff_token(&mut r, 2).unwrap(), (0, 0));
        // nC=4 → 表 2；(0,0) = len4 bits15 = "1111"
        let d = bits("1111");
        let mut r = BitReader::new(&d);
        assert_eq!(read_coeff_token(&mut r, 4).unwrap(), (0, 0));
        // nC=8 → 表 3；(0,0) = len6 bits3 = "000011"
        let d = bits("000011");
        let mut r = BitReader::new(&d);
        assert_eq!(read_coeff_token(&mut r, 8).unwrap(), (0, 0));
        // nC 超界钳到 16（表 3）
        let d = bits("000011");
        let mut r = BitReader::new(&d);
        assert_eq!(read_coeff_token(&mut r, 200).unwrap(), (0, 0));
    }

    #[test]
    fn chroma_dc_token_golden() {
        // (0,0)="01"；(1,1)="1"；(1,0)=len6 bits7="000111"；(4,3)="0000000"
        let cases: &[(&str, u8, u8)] = &[
            ("01", 0, 0),
            ("1", 1, 1),
            ("000111", 1, 0),
            ("0000000", 4, 3),
        ];
        for (cw, tc, t1) in cases {
            let data = bits(cw);
            let mut r = BitReader::new(&data);
            assert_eq!(
                read_coeff_token_chroma_dc(&mut r).unwrap(),
                (*tc, *t1),
                "codeword {cw}"
            );
        }
    }

    #[test]
    fn total_zeros_and_run_golden() {
        // total_coeff=1 表：zeros=0→"1"，3→len4 bits3="0011"
        let d = bits("1");
        let mut r = BitReader::new(&d);
        assert_eq!(read_total_zeros(&mut r, 1).unwrap(), 0);
        let d = bits("0011");
        let mut r = BitReader::new(&d);
        assert_eq!(read_total_zeros(&mut r, 1).unwrap(), 3);
        // total_coeff=2：zeros=0 → len3 bits7 = "111"
        let d = bits("111");
        let mut r = BitReader::new(&d);
        assert_eq!(read_total_zeros(&mut r, 2).unwrap(), 0);
        // total_coeff=15：zeros=0 → len1 bits0 = "0"
        let d = bits("0");
        let mut r = BitReader::new(&d);
        assert_eq!(read_total_zeros(&mut r, 15).unwrap(), 0);
        // run_before：zeros_left=1 表 run0→"1"、run1→"0"
        let d = bits("10");
        let mut r = BitReader::new(&d);
        assert_eq!(read_run_before(&mut r, 1).unwrap(), 0);
        assert_eq!(read_run_before(&mut r, 1).unwrap(), 1);
        // zeros_left=3 表：run0→"11"、run2→"01"
        let d = bits("1101");
        let mut r = BitReader::new(&d);
        assert_eq!(read_run_before(&mut r, 3).unwrap(), 0);
        assert_eq!(read_run_before(&mut r, 3).unwrap(), 2);
        // zeros_left=10（>7 用表 6）：run0→len3 bits7="111"；run7→len4 bits1="0001"
        let d = bits("1110001");
        let mut r = BitReader::new(&d);
        assert_eq!(read_run_before(&mut r, 10).unwrap(), 0);
        assert_eq!(read_run_before(&mut r, 10).unwrap(), 7);
    }

    #[test]
    fn level_mapping_and_prefix() {
        // map_level：code 0→+1、1→−1、2→+2、3→−2、4→+3、8→+5、9→−5
        assert_eq!(map_level(0).unwrap(), 1);
        assert_eq!(map_level(1).unwrap(), -1);
        assert_eq!(map_level(2).unwrap(), 2);
        assert_eq!(map_level(3).unwrap(), -2);
        assert_eq!(map_level(4).unwrap(), 3);
        assert_eq!(map_level(8).unwrap(), 5);
        assert_eq!(map_level(9).unwrap(), -5);
        // 前缀读取：'1'→0；'01'→1；'0001'→3
        let d = bits("1");
        let mut r = BitReader::new(&d);
        assert_eq!(read_level_prefix(&mut r).unwrap(), 0);
        let d = bits("01");
        let mut r = BitReader::new(&d);
        assert_eq!(read_level_prefix(&mut r).unwrap(), 1);
        let d = bits("0001");
        let mut r = BitReader::new(&d);
        assert_eq!(read_level_prefix(&mut r).unwrap(), 3);
        // 全零位流：耗尽 → Err
        let d = [0x00u8];
        let mut r = BitReader::new(&d);
        assert!(matches!(
            read_level_prefix(&mut r),
            Err(H264Error::InvalidData(_))
        ));
    }

    /// 首 level 快路径：prefix=0、s=0、t1=0 → code=0+2=2 → +2，s_new=1。
    #[test]
    fn level_first_fast_path() {
        let d = bits("1");
        let mut r = BitReader::new(&d);
        let (lv, s) = read_level(&mut r, 0, true, 0).unwrap();
        assert_eq!((lv, s), (2, 1));
    }

    /// prefix=14、s=0：code = 14 + 4 位后缀（15）= 29，t1=0 → +2 = 31
    /// → level=−16（奇）；慢路径 s_new=2。
    #[test]
    fn level_escape_prefix14_s0() {
        // 14 个 0 + "1"（prefix 14）+ "1111"（suffix 15）
        let d = bits("0000000000000011111");
        let mut r = BitReader::new(&d);
        let (lv, s) = read_level(&mut r, 0, true, 0).unwrap();
        assert_eq!((lv, s), (-16, 2));
    }

    /// prefix=15、s=1（FFmpeg 首层口径 30）：code = 30 + 12 位后缀（12）= 42，
    /// t1=0 首 level → +2 = 44 → +23；s_new=2（慢路径）。
    #[test]
    fn level_escape_prefix15() {
        // 15 个 0 + "1" + 12 位后缀 000000001100 (=12)
        let d = bits("0000000000000001000000001100");
        let mut r = BitReader::new(&d);
        let (lv, s) = read_level(&mut r, 1, true, 0).unwrap();
        assert_eq!((lv, s), (23, 2));
    }

    /// 黄金残差块：单系数 (tc=1,t1=1, sign −1)，nC=0（表 0）。
    /// 位流 = token("01") + sign("1") + total_zeros(0)("1")，扫描长 15（AC 块）。
    #[test]
    fn residual_single_trailing_one() {
        let scan: Vec<u8> = [1u8, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15].to_vec();
        let data = bits("0111"); // token (1,1)="01" + sign −1 "1" + tz(0)="1"
        let mut r = BitReader::new(&data);
        let mut out = [0i16; 16];
        let qmul = [64u32; 16];
        let tc = read_residual(&mut r, BlockKind::LumaAc, 0, &scan, &qmul, &mut out).unwrap();
        assert_eq!(tc, 1);
        // pos = 0 + 1 − 1 = 0 → 槽 scan[0] = 1；值 = (−1·64+32)>>6 = −1
        assert_eq!(out[1], -1);
        assert!(out.iter().enumerate().all(|(i, &v)| i == 1 || v == 0));
        assert_eq!(r.bit_position(), 4); // 恰好消费 4 位
    }

    /// 多系数 + 无 run 的黄金流（按码表手工推导）：
    /// tc=3、t1=2、level[2]=+2。
    #[test]
    fn residual_multi_coeff() {
        let scan: Vec<u8> = [1u8, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15].to_vec();
        // nC=0 表：(3,2)=len7 bits5 → "0000101"
        // 符号：t1=2 → "00"（+1,+1）
        // 首 level：prefix=0 → "1"（s 初 0：tc≤10 → code=0；t1=2<3 → +2 → +2）
        // total_zeros：tc=3 → 表 idx2：zeros=0 → len4 bits5 "0101"
        // 无 run_before（zeros_left=0）
        let data = bits(concat!("0000101", "00", "1", "0101"));
        let mut r = BitReader::new(&data);
        let mut out = [0i16; 16];
        let qmul = [64u32; 16];
        let tc = read_residual(&mut r, BlockKind::LumaAc, 0, &scan, &qmul, &mut out).unwrap();
        assert_eq!(tc, 3);
        // 逆 zigzag：pos 初 = 0+3−1 = 2：scan[2]=8 ← level[0]=+1（t1 第二个）
        // i=1: pos 1：scan[1]=4 ← level[1]=+1（t1 第一个）
        // i=2: pos 0：scan[0]=1 ← level[2]=+2（真 level）
        assert_eq!(out[8], 1);
        assert_eq!(out[4], 1);
        assert_eq!(out[1], 2);
        assert!(
            out.iter()
                .enumerate()
                .all(|(i, &v)| matches!(i, 1 | 4 | 8) || v == 0)
        );
        assert_eq!(r.bit_position(), 14); // 7+2+1+4 位恰好消费
    }

    /// 带一个 run_before 的黄金流：tc=2（一个 t1 + 一个真 level），
    /// zeros=1，run=1 → level 隔一个槽位。
    #[test]
    fn residual_with_run_before() {
        let scan: Vec<u8> = [1u8, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15].to_vec();
        // nC=0 表：(2,1)=len6 bits4 → "000100"
        // 符号："0"（+1）
        // 真 level：prefix=0 "1"，s=0，t1=1<3 → code=0+2=2 → +2，s=1
        // total_zeros：tc=2 → 表 idx1：zeros=1 → len3 bits6 = "110"
        // run_before：zeros_left=1 表 run1 = "0"
        let data = bits(concat!("000100", "0", "1", "110", "0"));
        let mut r = BitReader::new(&data);
        let mut out = [0i16; 16];
        let qmul = [64u32; 16];
        let tc = read_residual(&mut r, BlockKind::LumaAc, 0, &scan, &qmul, &mut out).unwrap();
        assert_eq!(tc, 2);
        // pos 初 = 1+2−1 = 2：scan[2]=8 ← level[0]=+1（t1）
        // i=1: run=1 → zeros 0，pos = 2−2 = 0：scan[0]=1 ← level[1]=+2
        assert_eq!(out[8], 1);
        assert_eq!(out[1], 2);
        assert!(
            out.iter()
                .enumerate()
                .all(|(i, &v)| matches!(i, 1 | 8) || v == 0)
        );
        assert_eq!(r.bit_position(), 12); // 6+1+1+3+1 位恰好消费
    }

    /// 恶意位流：token 后被截断 → Err；chroma DC 满块合法。
    #[test]
    fn residual_rejects_malformed() {
        let scan: Vec<u8> = [1u8, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15].to_vec();
        let qmul = [64u32; 16];
        // 位耗尽：token (1,1)="01" 但没有符号位
        let data = bits("01");
        let mut r = BitReader::new(&data);
        let mut out = [0i16; 16];
        assert!(matches!(
            read_residual(&mut r, BlockKind::LumaAc, 0, &scan, &qmul, &mut out),
            Err(H264Error::InvalidData(_))
        ));
        // chroma DC (4,3)：token len7 bits0="0000000" + 3 符号位 "000"（+1×3）
        // + 1 个真 level（prefix 0 → "1"，s=0，t1=3 不加 2 → code 0 → +1）；
        // tc=4 == max_coeff → 跳过 total_zeros。
        let data = bits("00000000001");
        let mut r = BitReader::new(&data);
        let mut out = [0i16; 16];
        let cd_scan: Vec<u8> = [0u8, 1, 4, 5].to_vec();
        let tc = read_residual(&mut r, BlockKind::ChromaDc, 0, &cd_scan, &qmul, &mut out).unwrap();
        assert_eq!(tc, 4);
        // 逆序铺位：pos3→槽5、pos2→槽4、pos1→槽1、pos0→槽0，全 +1
        assert_eq!(out, [1, 1, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(r.bit_position(), 11); // 7 token + 3 符号 + 1 level
    }

    /// 反量化折叠式逐值：level·qmul+32>>6（qmul = v·64·2^(qp/6)）。
    #[test]
    fn dequant_fold_matches_expected_scale() {
        // qp=0：qmul 槽 0 = 10·64 = 640
        assert_eq!((640 + 32) >> 6, 10);
        assert_eq!((-640 + 32) >> 6, -10); // (−640+32)>>6 = −608>>6 = −10（算术右移）
        assert_eq!((3 * 640 + 32) >> 6, 30); // 1952>>6 = 30
    }
}
