//! CAVLC 熵编码（H.264 §9.2 残差语法写侧）——`crate::cavlc` 读侧的镜像。
//!
//! 码表与读侧同源（FFmpeg `h264_cavlc.c` / x264 `common/tables.c`）。
//! 读侧码表是 `crate::cavlc` 的私有 static 且该模块不在本任务文件域内，
//! 此处为写侧副本；两份表的一致性由位级回环测试保证——写侧产出的
//! 每个码字必须被读侧（`read_coeff_token` / `read_residual` 等
//! `pub(crate)` 入口）原符号解回并恰好消费同一码长。
//!
//! level 写侧是读侧算法的精确逆，含其三处「非显然」口径（见
//! [`write_level`]）：
//! - 首层（first）后缀位长 `min(s,1)`、`prefix==14 && s==0` 读 4 位；
//! - 首 level 的 ±2 修正（t1<3 时 `level += signum(level)`，等价于
//!   levelCode +2），转义路径（prefix ≥ 14）后缀长度恒置 2；
//! - 转义基数：`!first && s ≥ 2` 用 `(15<<s) + …`，其余（含首层
//!   s∈{0,1}）用 `30 + …`（30 == 15<<1，与读侧逐位一致）。
//!
//! 零第三方依赖、无 unsafe、无 panic：系数数据越出可编码域
//! （level 超大、cbp 等组合无码字）一律 `Result`。

use crate::cavlc::BlockKind;
use crate::encoder::bitwriter::BitWriter;
use crate::error::H264Error;

// ---------------------------------------------------------------------------
// 码表（读侧 crate::cavlc 同源副本；一致性由回环测试保证）
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
/// 每行只列到可达零数（越界项为 0 = 无码字）。
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

/// nC（0..=16）→ coeff_token 表索引（读侧 COEFF_TOKEN_TABLE_INDEX 同源）。
static COEFF_TOKEN_TABLE_INDEX: [u8; 17] = [0, 0, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 3, 3, 3, 3, 3];

/// level 的 suffixLength 升级阈值：`|level| > SUFFIX_LIMIT[s]` 则 +1
/// （末项 ∞ = 恒不升级，s 封顶 6；读侧同源）。
static SUFFIX_LIMIT: [u32; 7] = [0, 3, 6, 12, 24, 48, u32::MAX];

// ---------------------------------------------------------------------------
// VLC 写出：表内符号 → (len, bits) 码字
// ---------------------------------------------------------------------------

/// 查表写一个 (len, bits) 前缀码。符号无码字（len==0）或越界 →
/// `InvalidData`（系数组合超出码表可达域，数据驱动失败）。
fn put_vlc(
    w: &mut BitWriter,
    lens: &[u8],
    bits: &[u8],
    sym: usize,
    what: &str,
) -> Result<(), H264Error> {
    let Some(&len) = lens.get(sym) else {
        return Err(H264Error::InvalidData(format!(
            "cavlc write: {what} symbol {sym} out of table"
        )));
    };
    if len == 0 {
        return Err(H264Error::InvalidData(format!(
            "cavlc write: {what} symbol {sym} has no codeword"
        )));
    }
    w.u(u32::from(len), u64::from(bits[sym]))
}

/// coeff_token 写：`(total_coeff, trailing_ones)` → 码字。
/// `nc` 为 nC（0..=16，超界钳 16——读侧同式）。符号 =
/// `(total_coeff << 2) | trailing_ones`。
pub(crate) fn write_coeff_token(
    w: &mut BitWriter,
    nc: u8,
    total_coeff: u8,
    trailing_ones: u8,
) -> Result<(), H264Error> {
    let idx = usize::from(COEFF_TOKEN_TABLE_INDEX[usize::from(nc.min(16))]);
    let sym = usize::from(total_coeff) * 4 + usize::from(trailing_ones);
    put_vlc(
        w,
        &COEFF_TOKEN_LEN[idx],
        &COEFF_TOKEN_BITS[idx],
        sym,
        "coeff_token",
    )
}

/// chroma DC 的 coeff_token 写（4:2:0 专用表）。
pub(crate) fn write_coeff_token_chroma_dc(
    w: &mut BitWriter,
    total_coeff: u8,
    trailing_ones: u8,
) -> Result<(), H264Error> {
    let sym = usize::from(total_coeff) * 4 + usize::from(trailing_ones);
    put_vlc(
        w,
        &CHROMA_DC_COEFF_TOKEN_LEN,
        &CHROMA_DC_COEFF_TOKEN_BITS,
        sym,
        "chroma_dc_coeff_token",
    )
}

/// total_zeros 写（非 chroma DC）。`total_coeff` 1..=15 选表。
fn write_total_zeros(w: &mut BitWriter, total_coeff: u8, zeros: u8) -> Result<(), H264Error> {
    let Some(idx) = usize::from(total_coeff).checked_sub(1) else {
        return Err(H264Error::InvalidData(format!(
            "cavlc write: total_zeros needs total_coeff >= 1 (got {total_coeff})"
        )));
    };
    if idx >= TOTAL_ZEROS_LEN.len() {
        return Err(H264Error::InvalidData(format!(
            "cavlc write: total_zeros total_coeff {total_coeff} out of range"
        )));
    }
    put_vlc(
        w,
        &TOTAL_ZEROS_LEN[idx],
        &TOTAL_ZEROS_BITS[idx],
        usize::from(zeros),
        "total_zeros",
    )
}

/// chroma DC 的 total_zeros 写。`total_coeff` 1..=3 选表（tc==4 满块
/// 不写 total_zeros，见 [`write_residual`]）。
fn write_total_zeros_chroma_dc(
    w: &mut BitWriter,
    total_coeff: u8,
    zeros: u8,
) -> Result<(), H264Error> {
    let Some(idx) = usize::from(total_coeff).checked_sub(1) else {
        return Err(H264Error::InvalidData(format!(
            "cavlc write: chroma_dc total_zeros needs total_coeff >= 1 (got {total_coeff})"
        )));
    };
    if idx >= CHROMA_DC_TOTAL_ZEROS_LEN.len() {
        return Err(H264Error::InvalidData(format!(
            "cavlc write: chroma_dc total_zeros total_coeff {total_coeff} out of range"
        )));
    }
    put_vlc(
        w,
        &CHROMA_DC_TOTAL_ZEROS_LEN[idx],
        &CHROMA_DC_TOTAL_ZEROS_BITS[idx],
        usize::from(zeros),
        "chroma_dc_total_zeros",
    )
}

/// run_before 写：zeros_left 1..=7+ 选表（≥7 用表 6）。`zeros_left == 0`
/// 是调用序错误（读侧此态不读）→ `InvalidArguments`。
fn write_run_before(w: &mut BitWriter, zeros_left: u8, run: u8) -> Result<(), H264Error> {
    let Some(idx) = usize::from(zeros_left).checked_sub(1) else {
        return Err(H264Error::InvalidArguments(
            "cavlc write: run_before needs zeros_left >= 1",
        ));
    };
    let idx = idx.min(6);
    put_vlc(
        w,
        &RUN_LEN[idx],
        &RUN_BITS[idx],
        usize::from(run),
        "run_before",
    )
}

// ---------------------------------------------------------------------------
// level 写出（读侧 read_level 的精确逆）
// ---------------------------------------------------------------------------

/// map_level 的逆：L>0 → levelCode 2L−2，L<0 → −2L−1（0 无对应码——
/// CAVLC level 恒非零）。
fn inv_map_level(level: i32) -> i64 {
    if level > 0 {
        2 * i64::from(level) - 2
    } else {
        -2 * i64::from(level) - 1
    }
}

/// 写 level 的 (prefix, suffix) 位型：prefix 个 0、终止 1、suffix_bits
/// 位后缀（MSB first）。
fn put_level_bits(
    w: &mut BitWriter,
    prefix: u32,
    suffix_bits: u32,
    suffix: u64,
) -> Result<(), H264Error> {
    for _ in 0..prefix {
        w.u1(false);
    }
    w.u1(true);
    if suffix_bits > 0 {
        w.u(suffix_bits, suffix)?;
    }
    Ok(())
}

/// 后续层 suffixLength 升级（读侧同式）：`|level| > SUFFIX_LIMIT[s]` +1。
fn next_suffix(suffix_length: u8, level: i32) -> u8 {
    suffix_length + u8::from(level.unsigned_abs() > SUFFIX_LIMIT[usize::from(suffix_length)])
}

/// 写一个 level，返回写后的新 suffixLength（读侧 `read_level` 的逆）。
///
/// `first` = 是否首个非 trailing level；`trailing_ones` 供首层的 +2
/// 修正。三个分支与读侧逐位对偶（code 域相接无重叠，见读侧
/// `crate::cavlc` doc）：
/// - 小码支（prefix < 14）：code = `(prefix << sbits) | suffix`，
///   `sbits = first ? min(s,1) : s`；
/// - prefix==14 支：s==0 读 4 位（code ∈ [14,30)），s≥1 读 s 位
///   （code ∈ [14<<s, 15<<s)）；
/// - 转义支（prefix ≥ 15）：基数 `!first && s≥2 ? 15<<s : 30`，
///   prefix==15 配 12 位后缀，p≥16 配 p−3 位后缀（p ≤ 28，
///   读侧同界）。
///
/// 首 layer 的 s_new：小码支 `1 + (level+3 无符号重释 > 6)`（读侧
/// 逐字），prefix ≥ 14 支恒 2；后续层一律 [`next_suffix`]。
///
/// # Errors
/// - `InvalidData`：level == 0、或 first 且 t1<3 下 |level| < 2（读侧
///   无对应码字）、或 level 大到 level_prefix > 28；
/// - `InvalidArguments`：`!first && s == 0` 或 `first && s > 1`
///   （读侧不可达态的调用序错误）。
fn write_level(
    w: &mut BitWriter,
    level: i32,
    suffix_length: u8,
    first: bool,
    trailing_ones: u8,
) -> Result<u8, H264Error> {
    if level == 0 {
        return Err(H264Error::InvalidData(
            "cavlc write: level must be nonzero".into(),
        ));
    }
    if !first && suffix_length == 0 {
        return Err(H264Error::InvalidArguments(
            "cavlc write: non-first level needs suffix_length >= 1",
        ));
    }
    if first && suffix_length > 1 {
        return Err(H264Error::InvalidArguments(
            "cavlc write: first level needs suffix_length 0 or 1",
        ));
    }

    // 读侧最终 map 的 levelCode：first 且 t1<3 时读侧 +2（等价于
    // level += signum(level)），写侧先扣回。
    let mut code = inv_map_level(level);
    if first && trailing_ones < 3 {
        code -= 2;
    }
    if code < 0 {
        return Err(H264Error::InvalidData(format!(
            "cavlc write: level {level} not encodable as first level (t1={trailing_ones})"
        )));
    }
    let code = code as u64;
    let s = u32::from(suffix_length);
    let sbits = if first { s.min(1) } else { s };

    if code < 14u64 << sbits {
        // 小码支
        let prefix = code >> sbits;
        let suffix = code & ((1u64 << sbits) - 1);
        put_level_bits(w, prefix as u32, sbits, suffix)?;
        if first {
            Ok(1 + u8::from(level.wrapping_add(3) as u32 > 6))
        } else {
            Ok(next_suffix(suffix_length, level))
        }
    } else if code < if s == 0 { 30 } else { 15u64 << s } {
        // prefix==14 支（读侧：s==0 配 4 位后缀 → code ∈ [14,30)；
        // s≥1 配 s 位 → [14<<s, 15<<s)）
        let suffix = code - (14u64 << s);
        let suffix_bits = if s == 0 { 4 } else { s };
        put_level_bits(w, 14, suffix_bits, suffix)?;
        if first {
            Ok(2)
        } else {
            Ok(next_suffix(suffix_length, level))
        }
    } else {
        // 转义支（prefix ≥ 15）
        let base = if !first && s >= 2 { 15u64 << s } else { 30 };
        let Some(rel) = code.checked_sub(base) else {
            return Err(H264Error::InvalidData(format!(
                "cavlc write: level {level} falls in the prefix-14 gap (code {code}, base {base})"
            )));
        };
        if rel < 4096 {
            put_level_bits(w, 15, 12, rel)?;
        } else {
            // p ≥ 16：q = p−3 位后缀，rel ∈ [2^q − 4096, 2^(q+1) − 4096)
            let x = rel + 4096;
            let q = 63 - x.leading_zeros();
            let p = q + 3;
            if p > 28 {
                return Err(H264Error::InvalidData(format!(
                    "cavlc write: level {level} needs level_prefix {p} > 28"
                )));
            }
            let suffix = x - (1u64 << q);
            put_level_bits(w, p, q, suffix)?;
        }
        if first {
            Ok(2)
        } else {
            Ok(next_suffix(suffix_length, level))
        }
    }
}

// ---------------------------------------------------------------------------
// 残差块编码：系数块（光栅槽位）→ 位流
// ---------------------------------------------------------------------------

/// 残差块编码（读侧 `crate::cavlc::read_residual` 的镜像）：
/// 光栅槽位系数块 `levels` + `scan`（zigzag 序 → 槽位，与读侧同契约）
/// → 位流。返回 `total_coeff`（供编码器侧 nnz 缓存）。
///
/// - `nc`：nC（0..=16，超界钳 16；chroma DC 忽略）；
/// - `kind`：决定 token 表（chroma DC 用专用表）与 total_zeros 表；
/// - `scan.len()` 为块系数容量（16/15/4），越界 → `InvalidArguments`。
///
/// 编码顺序：coeff_token → trailing ones 符号（解码序：高频在前）→
/// 非 trailing levels（suffixLength 状态机与读侧同式）→ total_zeros
/// （tc < scan.len() 时；值 = 最高频非零系数前的总零数）→ run_before
/// （相邻非零间零游程，解码序）。全零块只写 token。
///
/// # Errors
/// 系数组合无码字、level 超出可编码域 → [`H264Error::InvalidData`]；
/// `scan` 超 16 槽 → [`H264Error::InvalidArguments`]。
pub(crate) fn write_residual(
    w: &mut BitWriter,
    kind: BlockKind,
    nc: u8,
    scan: &[u8],
    levels: &[i16; 16],
) -> Result<u8, H264Error> {
    if scan.len() > 16 || scan.is_empty() {
        return Err(H264Error::InvalidArguments(
            "cavlc write: scan needs 1..=16 slots",
        ));
    }
    let max_coeff = scan.len() as u8;

    // scan 序收集非零（低频在前，位置递增）；逆序即解码序（高频在前）
    let mut coeffs: [(u8, i32); 16] = [(0, 0); 16];
    let mut tc: usize = 0;
    for (pos, &slot) in scan.iter().enumerate() {
        let lv = i32::from(levels[usize::from(slot)]);
        if lv != 0 {
            coeffs[tc] = (pos as u8, lv);
            tc += 1;
        }
    }

    // trailing ones：解码序起连续 ±1，封顶 3（t1 < 3 时首个真 level
    // 恒 |L| ≥ 2——计数停止即保证，读侧可解性同款约束）
    let mut t1: usize = 0;
    while t1 < tc && t1 < 3 && coeffs[tc - 1 - t1].1.abs() == 1 {
        t1 += 1;
    }

    match kind {
        BlockKind::ChromaDc => write_coeff_token_chroma_dc(w, tc as u8, t1 as u8)?,
        _ => write_coeff_token(w, nc, tc as u8, t1 as u8)?,
    }
    if tc == 0 {
        return Ok(0);
    }

    // trailing ones 符号（解码序 level[0] 起；位=1 → −1）
    for i in 0..t1 {
        w.u1(coeffs[tc - 1 - i].1 < 0);
    }

    // 非 trailing levels（解码序）；初值 s 与读侧同式
    if t1 < tc {
        let mut s: u8 = u8::from(tc > 10 && t1 < 3);
        for i in t1..tc {
            let (_, lv) = coeffs[tc - 1 - i];
            s = write_level(w, lv, s, i == t1, t1 as u8)?;
        }
    }

    // total_zeros = 最高频非零系数（scan 位置 p_top）之前的总零数
    // = p_top − (tc − 1)；tc == scan 容量时必为 0，读侧同款跳写
    if (tc as u8) < max_coeff {
        let zeros = coeffs[tc - 1].0 - (tc as u8 - 1);
        match kind {
            BlockKind::ChromaDc => write_total_zeros_chroma_dc(w, tc as u8, zeros)?,
            _ => write_total_zeros(w, tc as u8, zeros)?,
        }
    }

    // run_before：解码序 level[1..tc]，相邻非零间零游程；
    // zeros_left 初 = total_zeros，读侧 zeros_left==0 后不再读
    let mut zeros_left = usize::from(coeffs[tc - 1].0) - (tc - 1);
    for j in 1..tc {
        let gap = usize::from(coeffs[tc - j].0) - usize::from(coeffs[tc - 1 - j].0) - 1;
        if zeros_left > 0 {
            if gap > zeros_left {
                return Err(H264Error::InvalidData(format!(
                    "cavlc write: run accounting mismatch (gap {gap} > zeros_left {zeros_left})"
                )));
            }
            write_run_before(w, zeros_left as u8, gap as u8)?;
            zeros_left -= gap;
        } else if gap > 0 {
            return Err(H264Error::InvalidData(
                "cavlc write: run remains after zeros exhausted".into(),
            ));
        }
    }
    // 剩余 zeros_left = 最低频系数前的零（不编码；读侧同语义）
    Ok(tc as u8)
}

// ---------------------------------------------------------------------------
// 位级回环：写侧产出经读侧（crate::cavlc pub(crate) 入口）解回全等
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitreader::BitReader;
    use crate::cavlc::{self, BlockKind};
    use crate::macroblock::{CHROMA_DC_SCAN, ZIGZAG};

    /// 手工位串 → 字节（读侧测试同款）。
    fn bits(s: &str) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(s.len().div_ceil(8));
        let mut acc: u8 = 0;
        let mut n: u32 = 0;
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

    /// 全量位级回环：写 → 读（qmul=64 使读侧反量化恒等：
    /// `(level·64+32)>>6 == level`，读侧测试同款）。断言 tc 相等、
    /// 系数块全等、读侧恰好消费写出的位数。
    fn roundtrip(kind: BlockKind, nc: u8, scan: &[u8], levels: &[i16; 16]) -> u8 {
        let mut w = BitWriter::new();
        let tc = write_residual(&mut w, kind, nc, scan, levels)
            .unwrap_or_else(|e| panic!("write failed for {levels:?}: {e}"));
        let written = w.bit_position();
        let data = w.take_rbsp();
        let mut r = BitReader::new(&data);
        let mut out = [0i16; 16];
        let tc2 = cavlc::read_residual(&mut r, kind, nc, scan, &[64; 16], &mut out)
            .unwrap_or_else(|e| panic!("read failed for {levels:?}: {e}"));
        assert_eq!(tc, tc2, "total_coeff mismatch for {levels:?}");
        assert_eq!(out, *levels, "coefficient block mismatch");
        assert_eq!(
            r.bit_position(),
            written,
            "read must consume exactly the written bits"
        );
        tc
    }

    /// coeff_token：4 张表 × 全部 68 组合（有码者逐项回环），并断言
    /// 无码组合（t1 > min(tc,3) 等）被拒收。每张表用其 nC 桶的边界
    /// 值驱动：桶 0（nC 0,1）、桶 1（2,3）、桶 2（4..7）、桶 3（8..16）。
    #[test]
    fn coeff_token_roundtrip_all_tables_all_buckets() {
        let buckets: [[u8; 2]; 4] = [[0, 1], [2, 3], [4, 7], [8, 16]];
        for (table, nc_pair) in buckets.iter().enumerate() {
            for &nc in nc_pair {
                for tc in 0..=16u8 {
                    for t1 in 0..=3u8 {
                        let mut w = BitWriter::new();
                        let res = write_coeff_token(&mut w, nc, tc, t1);
                        let len = COEFF_TOKEN_LEN[table][usize::from(tc) * 4 + usize::from(t1)];
                        if len == 0 {
                            assert!(matches!(res, Err(H264Error::InvalidData(_))));
                            continue;
                        }
                        res.unwrap();
                        assert_eq!(w.bit_position(), usize::from(len));
                        let data = w.take_rbsp();
                        let mut r = BitReader::new(&data);
                        assert_eq!(
                            cavlc::read_coeff_token(&mut r, nc).unwrap(),
                            (tc, t1),
                            "table {table} nc {nc} (tc={tc}, t1={t1})"
                        );
                    }
                }
            }
        }
    }

    /// 全部 nC 值 0..=16（含超界钳位 200 → 16）在残差上下文回环，
    /// 覆盖每个 nC 桶的实际选表路径。
    #[test]
    fn residual_roundtrip_every_nc_value() {
        let mut levels = [0i16; 16];
        levels[1] = 3;
        levels[4] = -1;
        levels[8] = -2;
        for nc in 0u8..=16 {
            assert_eq!(roundtrip(BlockKind::LumaAc, nc, &ZIGZAG, &levels), 3);
        }
        // 超界钳位：nc > 16 与 nc=16 产出同码字
        let mut w1 = BitWriter::new();
        write_coeff_token(&mut w1, 200, 3, 1).unwrap();
        let mut w2 = BitWriter::new();
        write_coeff_token(&mut w2, 16, 3, 1).unwrap();
        assert_eq!(w1.take_rbsp(), w2.take_rbsp());
        // 桶边界换表：同一 (tc,t1) 在相邻桶产出不同码字
        for (lo, hi) in [(1u8, 2u8), (3, 4), (7, 8)] {
            let mut a = BitWriter::new();
            write_coeff_token(&mut a, lo, 0, 0).unwrap();
            let mut b = BitWriter::new();
            write_coeff_token(&mut b, hi, 0, 0).unwrap();
            assert_ne!(a.take_rbsp(), b.take_rbsp(), "nc {lo} vs {hi}");
        }
    }

    /// chroma DC token 表全部 20 项回环。
    #[test]
    fn chroma_dc_token_roundtrip_all_entries() {
        for tc in 0..=4u8 {
            for t1 in 0..=3u8 {
                let mut w = BitWriter::new();
                let res = write_coeff_token_chroma_dc(&mut w, tc, t1);
                let len = CHROMA_DC_COEFF_TOKEN_LEN[usize::from(tc) * 4 + usize::from(t1)];
                if len == 0 {
                    assert!(matches!(res, Err(H264Error::InvalidData(_))));
                    continue;
                }
                res.unwrap();
                let data = w.take_rbsp();
                let mut r = BitReader::new(&data);
                assert_eq!(
                    cavlc::read_coeff_token_chroma_dc(&mut r).unwrap(),
                    (tc, t1),
                    "(tc={tc}, t1={t1})"
                );
            }
        }
    }

    /// 读侧黄金位流的写侧重现：读侧 cavlc.rs 测试的四个黄金用例
    /// （同款 15 槽扫描 = ZIGZAG[1..]），同一系数布局必须写出逐位
    /// 相同的码流。
    #[test]
    fn golden_bitstreams_match_read_side() {
        let scan = &ZIGZAG[1..]; // 读侧黄金用例的扫描（Intra16 AC 形态）
        // 读侧 residual_single_trailing_one：token(1,1)="01" + sign "1"
        // + total_zeros(1,0)="1" → "0111"
        let mut levels = [0i16; 16];
        levels[1] = -1;
        let mut w = BitWriter::new();
        write_residual(&mut w, BlockKind::LumaAc, 0, scan, &levels).unwrap();
        assert_eq!(w.take_rbsp(), bits("0111"));

        // 读侧 residual_multi_coeff：tc=3 t1=2 + level +2 →
        // "0000101"+"00"+"1"+"0101"
        let mut levels = [0i16; 16];
        levels[8] = 1;
        levels[4] = 1;
        levels[1] = 2;
        let mut w = BitWriter::new();
        write_residual(&mut w, BlockKind::LumaAc, 0, scan, &levels).unwrap();
        assert_eq!(w.take_rbsp(), bits(concat!("0000101", "00", "1", "0101")));

        // 读侧 residual_with_run_before：tc=2（t1 + level）、zeros=1、
        // run=1 → "000100"+"0"+"1"+"110"+"0"
        let mut levels = [0i16; 16];
        levels[8] = 1;
        levels[1] = 2;
        let mut w = BitWriter::new();
        write_residual(&mut w, BlockKind::LumaAc, 0, scan, &levels).unwrap();
        assert_eq!(w.take_rbsp(), bits(concat!("000100", "0", "1", "110", "0")));

        // 读侧 chroma DC 满块：(4,3) token "0000000" + 3 符号 "000"
        // + 真 level +1（t1=3 不修正，prefix 0）→ "00000000001"
        let mut levels = [0i16; 16];
        levels[0] = 1;
        levels[1] = 1;
        levels[4] = 1;
        levels[5] = 1;
        let mut w = BitWriter::new();
        write_residual(&mut w, BlockKind::ChromaDc, 0, &CHROMA_DC_SCAN, &levels).unwrap();
        assert_eq!(w.take_rbsp(), bits("00000000001"));
    }

    /// 全零块：四种块型只写 token（tc=0），回环全等。
    #[test]
    fn all_zero_blocks_encode_token_only() {
        let zero = [0i16; 16];
        // nC=0 → (0,0) 码 "1"（1 位）；nC=2 → "11"；nC=4 → "1111"；
        // nC=8 → "000011"
        assert_eq!(roundtrip(BlockKind::LumaAc, 0, &ZIGZAG, &zero), 0);
        assert_eq!(roundtrip(BlockKind::LumaDc, 0, &ZIGZAG, &zero), 0);
        assert_eq!(roundtrip(BlockKind::ChromaAc, 0, &ZIGZAG[1..], &zero), 0);
        assert_eq!(roundtrip(BlockKind::ChromaDc, 0, &CHROMA_DC_SCAN, &zero), 0);
        let mut w = BitWriter::new();
        write_residual(&mut w, BlockKind::LumaAc, 0, &ZIGZAG, &zero).unwrap();
        assert_eq!(w.bit_position(), 1); // 表 0 的 (0,0)="1"
    }

    /// 饱和系数块：满块全 ±1（t1=3 + 13 个真 ±1 level，s 状态机全
    /// 程 0→1 后平走）、±i16 极值混布（触发转义支与大 levelPrefix）。
    #[test]
    fn saturated_blocks_roundtrip() {
        // 16/16 全 ±1 交替（LumaDc 全 zigzag 容量 16）
        let mut levels = [0i16; 16];
        for (i, lv) in levels.iter_mut().enumerate() {
            *lv = if i % 2 == 0 { 1 } else { -1 };
        }
        assert_eq!(roundtrip(BlockKind::LumaDc, 0, &ZIGZAG, &levels), 16);

        // 15/15 AC 满块（scan = ZIGZAG[1..]，不含槽 0），幅值覆盖
        // 2..=16 与负值（suffix 升级路径）
        let mut levels = [0i16; 16];
        for (i, &slot) in ZIGZAG[1..].iter().enumerate() {
            let mag = 2 + i as i16;
            levels[usize::from(slot)] = if i % 3 == 2 { -mag } else { mag };
        }
        assert_eq!(roundtrip(BlockKind::LumaAc, 0, &ZIGZAG[1..], &levels), 15);

        // chroma DC 4/4 满块
        let mut levels = [0i16; 16];
        for &slot in CHROMA_DC_SCAN.iter() {
            levels[usize::from(slot)] = -1;
        }
        assert_eq!(
            roundtrip(BlockKind::ChromaDc, 0, &CHROMA_DC_SCAN, &levels),
            4
        );

        // ±i16 极值（LumaDc 存原始 level）：走转义支；i16::MIN 取反
        // 溢出，用 saturating_neg（仍是极值）
        for value in [32767i16, -32767, i16::MIN, i16::MIN + 1] {
            let mut levels = [0i16; 16];
            levels[0] = value;
            levels[15] = value.saturating_neg();
            assert_eq!(roundtrip(BlockKind::LumaDc, 0, &ZIGZAG, &levels), 2);
        }
    }

    /// level 状态机全覆盖：首层 ±2 起（t1<3 的最小真 level）、suffix
    /// 升级链 0→1→…→6 与封顶、t1=3 下 ±1 真 level（无修正路径）。
    #[test]
    fn level_suffix_state_machine_roundtrip() {
        // t1=1 + 升级链：2(→s1) 4(→s2) 7(→s3) 13(→s4) 25(→s5) 49(→s6)
        // 60(s6 封顶) 3（s6 下小码）
        let seq: [i16; 8] = [1, 2, 4, 7, 13, 25, 49, 60];
        let mut levels = [0i16; 16];
        // 解码序：高频在前 → scan 末端放 t1，往前依次放链上 level
        for (i, &v) in seq.iter().enumerate() {
            levels[usize::from(ZIGZAG[15 - i])] = v;
        }
        assert_eq!(roundtrip(BlockKind::LumaDc, 0, &ZIGZAG, &levels), 8);

        // t1=3 下第 4 个 ±1（first 无修正：code 0/1）+ 负向链
        let seq: [i16; 6] = [-1, -1, -1, 1, -3, -30];
        let mut levels = [0i16; 16];
        for (i, &v) in seq.iter().enumerate() {
            levels[usize::from(ZIGZAG[15 - i])] = v;
        }
        assert_eq!(roundtrip(BlockKind::LumaDc, 0, &ZIGZAG, &levels), 6);

        // 首层即大值（t1=0）：转义支 first 路径（code -= 2 后仍 ≥ 30）
        let mut levels = [0i16; 16];
        levels[usize::from(ZIGZAG[15])] = 30000;
        levels[usize::from(ZIGZAG[14])] = -30000;
        assert_eq!(roundtrip(BlockKind::LumaDc, 0, &ZIGZAG, &levels), 2);

        // write_level 防御口径（read_residual 不可达态，由 t1 计数封顶
        // 保证；直接调用须拒收）：t1<3 下首层 |L|==1、level==0、
        // 非法 suffix 状态（!first 且 s==0 / first 且 s>1）
        let mut w = BitWriter::new();
        assert!(matches!(
            write_level(&mut w, 1, 0, true, 2),
            Err(H264Error::InvalidData(_))
        ));
        assert!(matches!(
            write_level(&mut w, 5, 0, false, 0),
            Err(H264Error::InvalidArguments(_))
        ));
        assert!(matches!(
            write_level(&mut w, 2, 2, true, 0),
            Err(H264Error::InvalidArguments(_))
        ));
        assert!(matches!(
            write_level(&mut w, 0, 0, true, 0),
            Err(H264Error::InvalidData(_))
        ));
    }

    /// total_zeros 全表扫：tc × zeros 的全部可达组合（LumaDc 容量 16
    /// 与 LumaAc 容量 15），系数紧凑排布（无 run_before 干扰）。
    #[test]
    fn total_zeros_full_table_sweep() {
        for tc in 1..=16usize {
            for zeros in 0..=(16 - tc) {
                let mut levels = [0i16; 16];
                for (i, slot) in ZIGZAG.iter().enumerate().skip(zeros).take(tc) {
                    // 低频端（scan 序末 3 位）给 ±1 供 t1，其余真 level
                    let decode_off = tc - 1 - (i - zeros); // 解码序偏移
                    levels[usize::from(*slot)] = if decode_off < 3 {
                        if decode_off % 2 == 0 { 1 } else { -1 }
                    } else {
                        let mag = 2 + (decode_off as i16 % 15);
                        if decode_off % 2 == 0 { mag } else { -mag }
                    };
                }
                assert_eq!(
                    roundtrip(BlockKind::LumaDc, 0, &ZIGZAG, &levels),
                    tc as u8,
                    "tc={tc} zeros={zeros}"
                );
            }
        }
        for tc in 1..=15usize {
            for zeros in 0..=(15 - tc) {
                let mut levels = [0i16; 16];
                for (i, slot) in ZIGZAG[1..].iter().enumerate().skip(zeros).take(tc) {
                    levels[usize::from(*slot)] = if i % 5 == 4 { -2 } else { 2 };
                }
                assert_eq!(
                    roundtrip(BlockKind::LumaAc, 0, &ZIGZAG[1..], &levels),
                    tc as u8,
                    "tc={tc} zeros={zeros}"
                );
            }
        }
    }

    /// run_before 全表扫：每个 (zeros_left, run) 可达对（tc=2：p_top =
    /// zl+1、低频系数在 zl−run），覆盖 7 张表全部码字（含表 6 的
    /// zeros_left ≥ 7 长 run 与 14 位极值）。
    #[test]
    fn run_before_full_table_sweep() {
        for zl in 1..=14usize {
            for run in 0..=zl.min(14) {
                let p_top = zl + 1; // ≤ 15
                let p_low = zl - run;
                let mut levels = [0i16; 16];
                levels[usize::from(ZIGZAG[p_top])] = -1; // t1=1
                levels[usize::from(ZIGZAG[p_low])] = 2;
                assert_eq!(
                    roundtrip(BlockKind::LumaDc, 0, &ZIGZAG, &levels),
                    2,
                    "zeros_left={zl} run={run}"
                );
            }
        }
    }

    /// 多游程组合：3+ 系数、非平凡零分布（zeros_left 跨 7 边界递减）。
    #[test]
    fn multi_run_patterns_roundtrip() {
        // 位置 [0, 5, 15]：total_zeros=13，run 序列 9、4
        let mut levels = [0i16; 16];
        levels[usize::from(ZIGZAG[15])] = -1;
        levels[usize::from(ZIGZAG[5])] = 3;
        levels[usize::from(ZIGZAG[0])] = -7;
        assert_eq!(roundtrip(BlockKind::LumaDc, 0, &ZIGZAG, &levels), 3);

        // 位置 [1, 2, 14, 15]：zeros=12，runs = 0、11、0
        let mut levels = [0i16; 16];
        levels[usize::from(ZIGZAG[15])] = 1;
        levels[usize::from(ZIGZAG[14])] = -1;
        levels[usize::from(ZIGZAG[2])] = -4;
        levels[usize::from(ZIGZAG[1])] = 9;
        assert_eq!(roundtrip(BlockKind::LumaDc, 0, &ZIGZAG, &levels), 4);

        // 全 16 槽交错零系数（无连续）：tc=8、zeros=8、run 全 1
        let mut levels = [0i16; 16];
        for i in (0..16).step_by(2) {
            levels[usize::from(ZIGZAG[i])] = if i % 4 == 0 { 2 } else { -3 };
        }
        assert_eq!(roundtrip(BlockKind::LumaDc, 0, &ZIGZAG, &levels), 8);
    }

    /// chroma DC 残差全扫：tc 1..=4 × zeros 0..=4−tc（chroma 专用
    /// total_zeros 表全部 3 行）。
    #[test]
    fn chroma_dc_residual_sweep() {
        for tc in 1..=4usize {
            let zeros_max = 4 - tc;
            if tc == 4 {
                // 满块：无 total_zeros
                let mut levels = [0i16; 16];
                for (i, &slot) in CHROMA_DC_SCAN.iter().enumerate() {
                    levels[usize::from(slot)] = if i < 3 {
                        if i % 2 == 0 { 1 } else { -1 }
                    } else {
                        4
                    };
                }
                assert_eq!(
                    roundtrip(BlockKind::ChromaDc, 0, &CHROMA_DC_SCAN, &levels),
                    4
                );
                continue;
            }
            for zeros in 0..=zeros_max {
                let mut levels = [0i16; 16];
                for (i, &slot) in CHROMA_DC_SCAN.iter().enumerate().skip(zeros).take(tc) {
                    levels[usize::from(slot)] = if i == 3 { 3 } else { -1 };
                }
                assert_eq!(
                    roundtrip(BlockKind::ChromaDc, 0, &CHROMA_DC_SCAN, &levels),
                    tc as u8,
                    "tc={tc} zeros={zeros}"
                );
            }
        }
    }

    /// 大 nC 桶的满块（表 3 全 6 位码）+ scan 契约违规拒收。
    #[test]
    fn high_nc_full_block_and_scan_contract() {
        let mut levels = [0i16; 16];
        for (i, lv) in levels.iter_mut().enumerate() {
            *lv = if i % 2 == 0 { 2 } else { -1 };
        }
        assert_eq!(roundtrip(BlockKind::LumaDc, 16, &ZIGZAG, &levels), 16);
        levels[0] = 0; // ZIGZAG[1..] 不含槽 0（Intra16 AC 契约）
        assert_eq!(roundtrip(BlockKind::LumaAc, 9, &ZIGZAG[1..], &levels), 15);

        let mut w = BitWriter::new();
        assert!(matches!(
            write_residual(&mut w, BlockKind::LumaAc, 0, &[0u8; 17], &levels),
            Err(H264Error::InvalidArguments(
                "cavlc write: scan needs 1..=16 slots"
            ))
        ));
        assert!(matches!(
            write_residual(&mut w, BlockKind::LumaAc, 0, &[], &levels),
            Err(H264Error::InvalidArguments(_))
        ));
    }
}
