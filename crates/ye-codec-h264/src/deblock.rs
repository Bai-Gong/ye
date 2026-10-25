//! H.264 去块滤波平面核（纯函数）：边界强度（bS）判定 + α/β/tc 阈值
//! 推导 + 各强度滤波核。只消费 [`Macroblock`] 的冻结字段
//! （`kind/qp/nnz/mv/ref_idx/slice_id`）与平面缓冲，不含解码逻辑、
//! 不依赖解码器其他模块的行为。
//!
//! ## 接口契约（②c 接线）
//!
//! - **边组织**（每宏块、光栅序，x264 `x264_frame_deblock_row` 同构）：
//!   - luma：垂直边 0..3（列 `mbx*16 + 4*e`，边两侧块列 `e−1`/`e`；
//!     水平边 0..3（行 `mby*16 + 4*e`）。每条边 16 样本、按 4 行一组
//!     取 bS。MB 边（e=0）用相邻两宏块 QP 的平均，内部边即本宏块 QP。
//!   - chroma（4:2:0）：只滤偶数边 e ∈ {0, 2}，位置
//!     `mbx*8 + 2*e`（垂直）/`mby*8 + 2*e`（水平），边 8 样本。
//!     **与 luma 边共用同一 bS/tc 数组**：`bs[i]` 作用于色度边的第
//!     `2i`、`2i+1` 行（x264 `deblock_chroma_c`/FFmpeg `loop_filter_chroma`
//!     的 4 组 × 2 行同构）。
//!   - `transform_8x8`（High Profile）时奇数 luma 边不滤——当前
//!     profile 边界（≤Main）无此形态。
//! - **bS=4 → 一律走 `*_intra` 强核**（luma 与 chroma 都不用 tc）；
//!   bS ∈ 1..=3 走 tc 核；bS=0 跳过。`tc0_of` 对 bS≥4 按 bS=3 取值
//!   （openh264 `& 3` 同防）。
//! - **平面几何前提**：调用方（MB 几何）保证边两侧各有 ≥4 个样本在
//!   平面内（luma：x/y ≥ 16 于 MB 边、≥ 20 于内部边；chroma 同比减半），
//!   函数内直接索引不再检查。
//! - idc 语义（②a 勘误已冻结）：idc=1 禁用、idc=0 全滤、idc=2 除片
//!   边界外滤——跨片门控用 [`boundary_strength_gated`]。
//!
//! ## 公式出处（三源互证：ISO/IEC 14496-10 §8.8、x264 `common/deblock.c`
//! ## （其标量核自注 "From ffmpeg"）、openh264 `deblocking.cpp`）
//!
//! - bS 规则（§8.8.2.3）：p/q 任一 intra → MB 边 4、内部边 3；
//!   否则**系数非零 → 2**，再否则**参考帧不同或 |ΔMV| ≥ 4（QPEL）→ 1**，
//!   否则 0。（x264 `deblock_strength_c` 与 openh264
//!   `DeblockingBsMarginalMBAvcbase` 逐字同序——注意与常见教科书的
//!   行序记忆相反，nnz 档高于 MV 档。）
//! - 阈值：luma `QP = (QPp+QPq+1)>>1`；chroma 先各自查 4:2:0 QPC 表再
//!   平均（x264 `chroma_qp_table` 同式）。`indexA/B = Clip3(0,51,
//!   QP+offset)`；α/β 查表（α 或 β 为 0 即不滤）；luma
//!   `tc = TC0[indexA][bS−1]`，**chroma tc 再 +1**（x264 `b_chroma`/
//!   openh264 `TC0_TBL_LOOKUP` 的 +bChroma）。
//! - 四个滤波核逐字取自 x264 `deblock_edge_luma_c` /
//!   `deblock_edge_luma_intra_c` / `deblock_edge_chroma_c` /
//!   `deblock_edge_chroma_intra_c`；关键行为：
//!   - bS 1..3 的 Δ 用**原始** p1/q1（p1/q1 的修改先落位但不回读）；
//!   - |p2−p0|<β / |q2−q0|<β 分别使 tc 增 1（Δ 的钳位范围放宽）；
//!     tc0=0 时 p1/q1 不修改（但 tc 增量照加）；
//!   - bS=4 luma：|p0−q0| < (α>>2)+2 走强支路（p0/p1/p2 三点），
//!     否则弱支路只改 p0/q0；色度 bS=4 无 tc。

// ②c 主链接线（edge 组织循环 + PPS 偏移接线）前，本模块的平面入口
// 未消费——dead_code 豁免到接线时移除。
#![allow(dead_code)]

use crate::macroblock::{Macroblock, MbKind};

// ---------------------------------------------------------------------------
// 常量表（ISO Table 8-16/8-17；x264 i_alpha_table/i_beta_table/i_tc0_table
// 的 52 项核心段逐字，openh264 g_kuiAlphaTable 等锚点交叉核对）
// ---------------------------------------------------------------------------

/// α 表（QP 0..51）。QP ≤ 15 恒 0（低码率不滤）。
const ALPHA_TABLE: [i32; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    4, 4, 5, 6, 7, 8, 9, 10, 12, 13, 15, 17, 20, 22, //
    25, 28, 32, 36, 40, 45, 50, 56, 63, 71, //
    80, 90, 101, 113, 127, 144, 162, 182, 203, 226, //
    255, 255,
];

/// β 表（QP 0..51）。
const BETA_TABLE: [i32; 52] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 6, 6, 7, 7, //
    8, 8, 9, 9, 10, 10, 11, 11, 12, 12, //
    13, 13, 14, 14, 15, 15, 16, 16, 17, 17, //
    18, 18,
];

/// tc0 表（QP 0..51 × bS 1..3）。
const TC0_TABLE: [[i32; 3]; 52] = [
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0], // QP 0..16
    [0, 0, 1],
    [0, 0, 1],
    [0, 0, 1],
    [0, 0, 1], // 17..20
    [0, 1, 1],
    [0, 1, 1], // 21..22
    [1, 1, 1],
    [1, 1, 1],
    [1, 1, 1],
    [1, 1, 1], // 23..26
    [1, 1, 2],
    [1, 1, 2],
    [1, 1, 2],
    [1, 1, 2], // 27..30
    [1, 2, 3],
    [1, 2, 3], // 31..32
    [2, 2, 3], // 33
    [2, 2, 4], // 34
    [2, 3, 4],
    [2, 3, 4], // 35..36
    [3, 3, 5], // 37
    [3, 4, 6],
    [3, 4, 6],    // 38..39
    [4, 5, 7],    // 40
    [4, 5, 8],    // 41
    [4, 6, 9],    // 42
    [5, 7, 10],   // 43
    [6, 8, 11],   // 44
    [6, 8, 13],   // 45
    [7, 10, 14],  // 46
    [8, 11, 16],  // 47
    [9, 12, 18],  // 48
    [10, 13, 20], // 49
    [11, 15, 23], // 50
    [13, 17, 25], // 51
];

/// chroma QP 映射（4:2:0；FFmpeg `ff_h264_chroma_qp` 8bit 行）。
/// 与 macroblock.rs 的私有 `CHROMA_QP` 同值——冻结面私有表，此处本地
/// 复制以免改动主链文件。
const CHROMA_QP: [u8; 52] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26, 27, 28, 29, // 0..29 恒等
    29, 30, 31, 32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38, 39, 39, 39, 39,
];

// ---------------------------------------------------------------------------
// 边界强度（ISO §8.8.2.3）
// ---------------------------------------------------------------------------

fn mb_is_intra(mb: &Macroblock) -> bool {
    matches!(mb.kind, MbKind::Intra4x4 | MbKind::Intra16x16 | MbKind::Pcm)
}

/// 边界强度判定（完整规则，消费冻结面字段）。
///
/// `p_blk`/`q_blk` 为边两侧 4x4 块的 luma 光栅块号（0..16）；chroma 边
/// 复用同一函数取对应 luma 块（4 组 × 2 行的粒度即 luma 块粒度）。
/// `mb_boundary`：边是否为宏块边界。
pub(crate) fn boundary_strength(
    p_mb: &Macroblock,
    p_blk: usize,
    q_mb: &Macroblock,
    q_blk: usize,
    mb_boundary: bool,
) -> u8 {
    if mb_is_intra(p_mb) || mb_is_intra(q_mb) {
        return if mb_boundary { 4 } else { 3 };
    }
    // inter：系数非零 → 2（先判）；参考/MV 差异 → 1；否则 0。
    if p_mb.nnz[p_blk] != 0 || q_mb.nnz[q_blk] != 0 {
        return 2;
    }
    let dmvx = i32::from(p_mb.mv[p_blk][0]) - i32::from(q_mb.mv[q_blk][0]);
    let dmvy = i32::from(p_mb.mv[p_blk][1]) - i32::from(q_mb.mv[q_blk][1]);
    if p_mb.ref_idx[p_blk] != q_mb.ref_idx[q_blk] || dmvx.abs() >= 4 || dmvy.abs() >= 4 {
        return 1;
    }
    0
}

/// 片边界门控后的边界强度：`disable_deblocking_filter_idc == 2`（除片
/// 边界外滤）时跨片边返回 0。`filter_across_slices` = idc ≠ 2。
pub(crate) fn boundary_strength_gated(
    p_mb: &Macroblock,
    p_blk: usize,
    q_mb: &Macroblock,
    q_blk: usize,
    mb_boundary: bool,
    filter_across_slices: bool,
) -> u8 {
    if !filter_across_slices && p_mb.slice_id != q_mb.slice_id {
        return 0;
    }
    boundary_strength(p_mb, p_blk, q_mb, q_blk, mb_boundary)
}

// ---------------------------------------------------------------------------
// 阈值推导（ISO §8.8.3.2；x264 deblock_edge 同式）
// ---------------------------------------------------------------------------

fn clip3_0_51(v: i32) -> usize {
    v.clamp(0, 51) as usize
}

/// 边的 luma QP：`(QPp + QPq + 1) >> 1`。
pub(crate) fn edge_qp_luma(qp_p: u8, qp_q: u8) -> i32 {
    (i32::from(qp_p) + i32::from(qp_q) + 1) >> 1
}

/// luma QP → 4:2:0 chroma QP。
pub(crate) fn chroma_qp_of(qp: u8) -> u8 {
    CHROMA_QP[usize::from(qp)]
}

/// 边的 chroma QP：两宏块的 chroma QP（各自查表后）平均。
pub(crate) fn edge_qp_chroma(qp_p: u8, qp_q: u8) -> i32 {
    (i32::from(chroma_qp_of(qp_p)) + i32::from(chroma_qp_of(qp_q)) + 1) >> 1
}

/// α：`ALPHA_TABLE[Clip3(0,51, qp + alpha_c0_offset)]`。
pub(crate) fn alpha_of(qp: i32, alpha_offset: i32) -> i32 {
    ALPHA_TABLE[clip3_0_51(qp + alpha_offset)]
}

/// β：`BETA_TABLE[Clip3(0,51, qp + beta_offset)]`。
pub(crate) fn beta_of(qp: i32, beta_offset: i32) -> i32 {
    BETA_TABLE[clip3_0_51(qp + beta_offset)]
}

/// luma/chroma 的 tc：`TC0_TABLE[Clip3(0,51, qp + alpha_offset)][bS−1]`，
/// chroma 再 +1。bS ≥ 4 按 bS=3 取（bS=4 应走 `*_intra` 核，此处仅
/// 防御；openh264 `& 3` 同）。bS=0 返回 0（调用方本就跳过）。
pub(crate) fn tc0_of(qp: i32, alpha_offset: i32, bs: u8, chroma: bool) -> i32 {
    if bs == 0 {
        return 0;
    }
    let col = bs.clamp(1, 3) as usize - 1;
    TC0_TABLE[clip3_0_51(qp + alpha_offset)][col] + i32::from(chroma)
}

// ---------------------------------------------------------------------------
// 滤波核（样本数组级，逐字 x264）
// ---------------------------------------------------------------------------

fn clip3(v: i32, lo: i32, hi: i32) -> i32 {
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

/// luma bS 1..3 单段核。`s = [p3,p2,p1,p0,q0,q1,q2,q3]`，返回滤波后
/// 同布局（p3/q3 恒不变；本核至多改 p1..q1）。
fn luma_subedge(s: [i32; 8], alpha: i32, beta: i32, tc0: i32) -> [i32; 8] {
    let [_, p2, p1, p0, q0, q1, q2, _] = s;
    let mut out = s;
    if (p0 - q0).abs() < alpha && (p1 - p0).abs() < beta && (q1 - q0).abs() < beta {
        let mut tc = tc0;
        if (p2 - p0).abs() < beta {
            if tc0 != 0 {
                let m = ((p2 + ((p0 + q0 + 1) >> 1)) >> 1) - p1;
                out[2] = p1 + clip3(m, -tc0, tc0);
            }
            tc += 1;
        }
        if (q2 - q0).abs() < beta {
            if tc0 != 0 {
                let m = ((q2 + ((p0 + q0 + 1) >> 1)) >> 1) - q1;
                out[5] = q1 + clip3(m, -tc0, tc0);
            }
            tc += 1;
        }
        // Δ 用原始 p1/q1（上面的修改不回读）。
        let d = clip3((4 * (q0 - p0) + (p1 - q1) + 4) >> 3, -tc, tc);
        out[3] = (p0 + d).clamp(0, 255);
        out[4] = (q0 - d).clamp(0, 255);
    }
    out
}

/// luma bS=4 单段核（无 tc）。
fn luma_subedge_intra(s: [i32; 8], alpha: i32, beta: i32) -> [i32; 8] {
    let [p3, p2, p1, p0, q0, q1, q2, q3] = s;
    let mut out = s;
    if (p0 - q0).abs() < alpha && (p1 - p0).abs() < beta && (q1 - q0).abs() < beta {
        if (p0 - q0).abs() < ((alpha >> 2) + 2) {
            if (p2 - p0).abs() < beta {
                out[3] = (p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3;
                out[2] = (p2 + p1 + p0 + q0 + 2) >> 2;
                out[1] = (2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3;
            } else {
                out[3] = (2 * p1 + p0 + q1 + 2) >> 2;
            }
            if (q2 - q0).abs() < beta {
                out[4] = (p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3;
                out[5] = (p0 + q0 + q1 + q2 + 2) >> 2;
                out[6] = (2 * q3 + 3 * q2 + q1 + q0 + p0 + 4) >> 3;
            } else {
                out[4] = (2 * q1 + q0 + p1 + 2) >> 2;
            }
        } else {
            out[3] = (2 * p1 + p0 + q1 + 2) >> 2;
            out[4] = (2 * q1 + q0 + p1 + 2) >> 2;
        }
    }
    out
}

/// chroma bS 1..3 单段核（只用 p1,p0,q0,q1；Δ 公式与 luma 同）。
fn chroma_subedge(s: [i32; 4], alpha: i32, beta: i32, tc: i32) -> [i32; 4] {
    let [p1, p0, q0, q1] = s;
    let mut out = s;
    if (p0 - q0).abs() < alpha && (p1 - p0).abs() < beta && (q1 - q0).abs() < beta {
        let d = clip3((4 * (q0 - p0) + (p1 - q1) + 4) >> 3, -tc, tc);
        out[1] = (p0 + d).clamp(0, 255);
        out[2] = (q0 - d).clamp(0, 255);
    }
    out
}

/// chroma bS=4 单段核（无 tc）。
fn chroma_subedge_intra(s: [i32; 4], alpha: i32, beta: i32) -> [i32; 4] {
    let [p1, p0, q0, q1] = s;
    let mut out = s;
    if (p0 - q0).abs() < alpha && (p1 - p0).abs() < beta && (q1 - q0).abs() < beta {
        out[1] = (2 * p1 + p0 + q1 + 2) >> 2;
        out[2] = (2 * q1 + q0 + p1 + 2) >> 2;
    }
    out
}

// ---------------------------------------------------------------------------
// 平面级入口
// ---------------------------------------------------------------------------

/// luma 垂直边（列 x−1|x 之间，行 y..y+16）。`bs[i]`/`tc0[i]` 作用于第
/// i 个 4 行段；bS=0 的段跳过。α 或 β 为 0 整边不滤（x264 同）。
#[allow(clippy::too_many_arguments)] // 平面核参数组，聚类反而伤可读性
pub(crate) fn filter_luma_v(
    plane: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    alpha: i32,
    beta: i32,
    bs: &[u8; 4],
    tc0: &[i32; 4],
) {
    if alpha == 0 || beta == 0 {
        return;
    }
    for (i, (&b, &t)) in bs.iter().zip(tc0.iter()).enumerate() {
        if b == 0 {
            continue;
        }
        for d in 0..4 {
            let row = y + 4 * i + d;
            let base = row * stride + x;
            let s: [i32; 8] = std::array::from_fn(|k| i32::from(plane[base - 4 + k]));
            let out = luma_subedge(s, alpha, beta, t);
            for k in 2..=5 {
                plane[base - 4 + k] = out[k] as u8;
            }
        }
    }
}

/// luma 水平边（行 y−1|y 之间，列 x..x+16）。
#[allow(clippy::too_many_arguments)] // 同上
pub(crate) fn filter_luma_h(
    plane: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    alpha: i32,
    beta: i32,
    bs: &[u8; 4],
    tc0: &[i32; 4],
) {
    if alpha == 0 || beta == 0 {
        return;
    }
    for (i, (&b, &t)) in bs.iter().zip(tc0.iter()).enumerate() {
        if b == 0 {
            continue;
        }
        for d in 0..4 {
            let col = x + 4 * i + d;
            let s: [i32; 8] = std::array::from_fn(|k| i32::from(plane[(y - 4 + k) * stride + col]));
            let out = luma_subedge(s, alpha, beta, t);
            for k in 2..=5 {
                plane[(y - 4 + k) * stride + col] = out[k] as u8;
            }
        }
    }
}

/// luma 垂直边 bS=4 强核（整边 16 行，无 tc）。
pub(crate) fn filter_luma_v_intra(
    plane: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    alpha: i32,
    beta: i32,
) {
    if alpha == 0 || beta == 0 {
        return;
    }
    for row in y..y + 16 {
        let base = row * stride + x;
        let s: [i32; 8] = std::array::from_fn(|k| i32::from(plane[base - 4 + k]));
        let out = luma_subedge_intra(s, alpha, beta);
        for k in 1..=6 {
            plane[base - 4 + k] = out[k] as u8;
        }
    }
}

/// luma 水平边 bS=4 强核。
pub(crate) fn filter_luma_h_intra(
    plane: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    alpha: i32,
    beta: i32,
) {
    if alpha == 0 || beta == 0 {
        return;
    }
    for col in x..x + 16 {
        let s: [i32; 8] = std::array::from_fn(|k| i32::from(plane[(y - 4 + k) * stride + col]));
        let out = luma_subedge_intra(s, alpha, beta);
        for k in 1..=6 {
            plane[(y - 4 + k) * stride + col] = out[k] as u8;
        }
    }
}

/// chroma 垂直边（列 x−1|x，行 y..y+8）。`bs[i]`/`tc0[i]` 作用于第
/// `2i`、`2i+1` 行（4 组 × 2 行，与 luma 边共用 bS 数组）。
#[allow(clippy::too_many_arguments)] // 同上
pub(crate) fn filter_chroma_v(
    plane: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    alpha: i32,
    beta: i32,
    bs: &[u8; 4],
    tc0: &[i32; 4],
) {
    if alpha == 0 || beta == 0 {
        return;
    }
    for (i, (&b, &t)) in bs.iter().zip(tc0.iter()).enumerate() {
        if b == 0 {
            continue;
        }
        for d in 0..2 {
            let row = y + 2 * i + d;
            let base = row * stride + x;
            let s: [i32; 4] = std::array::from_fn(|k| i32::from(plane[base - 2 + k]));
            let out = chroma_subedge(s, alpha, beta, t);
            for k in 1..=2 {
                plane[base - 2 + k] = out[k] as u8;
            }
        }
    }
}

/// chroma 水平边（行 y−1|y，列 x..x+8）。
#[allow(clippy::too_many_arguments)] // 同上
pub(crate) fn filter_chroma_h(
    plane: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    alpha: i32,
    beta: i32,
    bs: &[u8; 4],
    tc0: &[i32; 4],
) {
    if alpha == 0 || beta == 0 {
        return;
    }
    for (i, (&b, &t)) in bs.iter().zip(tc0.iter()).enumerate() {
        if b == 0 {
            continue;
        }
        for d in 0..2 {
            let col = x + 2 * i + d;
            let s: [i32; 4] = std::array::from_fn(|k| i32::from(plane[(y - 2 + k) * stride + col]));
            let out = chroma_subedge(s, alpha, beta, t);
            for k in 1..=2 {
                plane[(y - 2 + k) * stride + col] = out[k] as u8;
            }
        }
    }
}

/// chroma 垂直边 bS=4 强核（无 tc）。
pub(crate) fn filter_chroma_v_intra(
    plane: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    alpha: i32,
    beta: i32,
) {
    if alpha == 0 || beta == 0 {
        return;
    }
    for row in y..y + 8 {
        let base = row * stride + x;
        let s: [i32; 4] = std::array::from_fn(|k| i32::from(plane[base - 2 + k]));
        let out = chroma_subedge_intra(s, alpha, beta);
        for k in 1..=2 {
            plane[base - 2 + k] = out[k] as u8;
        }
    }
}

/// chroma 水平边 bS=4 强核。
pub(crate) fn filter_chroma_h_intra(
    plane: &mut [u8],
    stride: usize,
    x: usize,
    y: usize,
    alpha: i32,
    beta: i32,
) {
    if alpha == 0 || beta == 0 {
        return;
    }
    for col in x..x + 8 {
        let s: [i32; 4] = std::array::from_fn(|k| i32::from(plane[(y - 2 + k) * stride + col]));
        let out = chroma_subedge_intra(s, alpha, beta);
        for k in 1..=2 {
            plane[(y - 2 + k) * stride + col] = out[k] as u8;
        }
    }
}

// ---------------------------------------------------------------------------
// 单测：边界强度表逐档 + 滤波核逐档（构造边界差值手算用例）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 表锚点（x264 表逐字抽取）----

    #[test]
    fn table_anchors() {
        assert_eq!(ALPHA_TABLE[..16].iter().sum::<i32>(), 0); // QP≤15 不滤
        assert_eq!(ALPHA_TABLE[16], 4);
        assert_eq!(ALPHA_TABLE[30], 25);
        assert_eq!(ALPHA_TABLE[44], 127);
        assert_eq!(ALPHA_TABLE[51], 255);
        assert_eq!(BETA_TABLE[15], 0);
        assert_eq!(BETA_TABLE[16], 2);
        assert_eq!(BETA_TABLE[19], 3);
        assert_eq!(BETA_TABLE[51], 18);
        assert_eq!(TC0_TABLE[16], [0, 0, 0]);
        assert_eq!(TC0_TABLE[17], [0, 0, 1]);
        assert_eq!(TC0_TABLE[21], [0, 1, 1]);
        assert_eq!(TC0_TABLE[23], [1, 1, 1]);
        assert_eq!(TC0_TABLE[27], [1, 1, 2]);
        assert_eq!(TC0_TABLE[33], [2, 2, 3]);
        assert_eq!(TC0_TABLE[51], [13, 17, 25]);
        // 单调性：各列随 QP 非降（相邻行逐列比较）
        for (i, pair) in TC0_TABLE.windows(2).enumerate() {
            let qp = i + 1;
            assert!(
                pair[1][0] >= pair[0][0] && pair[1][1] >= pair[0][1] && pair[1][2] >= pair[0][2],
                "tc 非单调 @qp={qp}"
            );
        }
    }

    // ---- 阈值推导 ----

    #[test]
    fn threshold_derivation() {
        assert_eq!(edge_qp_luma(10, 11), 11);
        assert_eq!(edge_qp_luma(0, 0), 0);
        assert_eq!(edge_qp_luma(51, 50), 51);
        assert_eq!(chroma_qp_of(29), 29);
        assert_eq!(chroma_qp_of(30), 29);
        assert_eq!(chroma_qp_of(40), 36);
        assert_eq!(chroma_qp_of(51), 39);
        assert_eq!(edge_qp_chroma(40, 40), 36);
        assert_eq!(edge_qp_chroma(51, 51), 39);
        assert_eq!(alpha_of(40, 0), 80);
        assert_eq!(beta_of(40, 0), 13);
        assert_eq!(alpha_of(5, -10), 0); // Clip3(0,51) → QP0 → 0
        assert_eq!(alpha_of(45, 10), ALPHA_TABLE[51]);
        assert_eq!(tc0_of(40, 0, 1, false), 4);
        assert_eq!(tc0_of(40, 0, 2, false), 5);
        assert_eq!(tc0_of(40, 0, 3, false), 7);
        assert_eq!(tc0_of(36, 0, 1, true), 3); // 2 + 1（chroma）
        assert_eq!(tc0_of(36, 0, 4, false), 4); // bS=4 防御性按 3 取
        assert_eq!(tc0_of(36, 0, 0, false), 0);
    }

    // ---- bS 规则逐档 ----

    fn intra_mb(slice_id: u16) -> Macroblock {
        Macroblock {
            decoded: true,
            slice_id,
            ..Macroblock::default()
        }
    }

    /// 均匀 inter 宏块：块 3（p 侧）与块 4（q 侧）同值，其余块全零/−1。
    fn inter_mb(nnz: u8, mv: [i16; 2], ref_idx: i8, slice_id: u16) -> Macroblock {
        let mut mb = Macroblock {
            decoded: true,
            kind: MbKind::InterP,
            slice_id,
            ..Macroblock::default()
        };
        for b in [3usize, 4] {
            mb.nnz[b] = nnz;
            mb.mv[b] = mv;
            mb.ref_idx[b] = ref_idx;
        }
        mb
    }

    #[test]
    fn boundary_strength_all_tiers() {
        // intra：MB 边 4、内部边 3
        let p = intra_mb(0);
        let q = intra_mb(0);
        assert_eq!(boundary_strength(&p, 3, &q, 4, true), 4);
        assert_eq!(boundary_strength(&p, 3, &q, 4, false), 3);
        // 一侧 intra 即生效
        let qi = inter_mb(0, [0, 0], 0, 0);
        assert_eq!(boundary_strength(&p, 3, &qi, 4, true), 4);
        // PCM 也是 intra
        let mut pcm = intra_mb(0);
        pcm.kind = MbKind::Pcm;
        pcm.nnz = [16; 24];
        assert_eq!(boundary_strength(&pcm, 3, &qi, 4, false), 3);
        // inter：nnz → 2（先于 MV 档）
        let a = inter_mb(1, [0, 0], 0, 0);
        let b = inter_mb(0, [0, 0], 0, 0);
        assert_eq!(boundary_strength(&a, 3, &b, 4, false), 2);
        assert_eq!(boundary_strength(&b, 3, &a, 4, false), 2);
        // MV 差 ≥ 4 QPEL → 1（任一分量）
        let c = inter_mb(0, [4, 0], 0, 0);
        assert_eq!(boundary_strength(&b, 3, &c, 4, false), 1);
        let d = inter_mb(0, [0, -4], 0, 0);
        assert_eq!(boundary_strength(&b, 3, &d, 4, false), 1);
        // 差 3 不触发
        let e = inter_mb(0, [3, 0], 0, 0);
        assert_eq!(boundary_strength(&b, 3, &e, 4, false), 0);
        // 参考帧不同 → 1（MV 相同）
        let f = inter_mb(0, [0, 0], 1, 0);
        assert_eq!(boundary_strength(&b, 3, &f, 4, false), 1);
        // nnz 与 MV 差同时存在 → 2（nnz 档更高）
        let g = inter_mb(2, [100, 100], 3, 0);
        assert_eq!(boundary_strength(&g, 3, &f, 4, false), 2);
    }

    #[test]
    fn boundary_strength_slice_gating() {
        let p = intra_mb(0);
        let q = intra_mb(1);
        // idc=2：跨片边不滤
        assert_eq!(boundary_strength_gated(&p, 3, &q, 4, true, false), 0);
        // idc≠2：照常 4
        assert_eq!(boundary_strength_gated(&p, 3, &q, 4, true, true), 4);
        // 同片不受门控影响
        let q2 = intra_mb(0);
        assert_eq!(boundary_strength_gated(&p, 3, &q2, 4, true, false), 4);
    }

    // ---- luma 滤波核逐档（手算）----
    // 构造：16×16 平面全 100，第 4..11 列放 [p3,p2,p1,p0,q0,q1,q2,q3]，
    // 边在列 8（x=8）。全部 16 行同值 ⇒ 每段结果一致。

    fn luma_plane(pq: [u8; 8]) -> Vec<u8> {
        let mut v = vec![100u8; 16 * 16];
        for row in 0..16 {
            for (k, &s) in pq.iter().enumerate() {
                v[row * 16 + 4 + k] = s;
            }
        }
        v
    }

    fn cols(v: &[u8], row: usize) -> [u8; 8] {
        std::array::from_fn(|k| v[row * 16 + 4 + k])
    }

    #[test]
    fn luma_bs123_each_strength() {
        // QP40：α=80、β=13、tc(bS1..3) = 4/5/7
        let pq = [100, 100, 100, 110, 130, 122, 100, 100];
        // 手算（见模块文档公式）：
        // p 侧 |p2−p0|=10<13 → p1 修：m = ((100+120)>>1) − 100 = 10
        //   bs1: clip±4 → p1'=104；q 侧 |q2−q0|=30≥13 不修；tc=5
        //   Δ = clip±5, (80 + (100−122) + 4)>>3 = 62>>3 = 7 → 5
        //   ⇒ p0'=115、q0'=125
        let mut v = luma_plane(pq);
        filter_luma_v(&mut v, 16, 8, 0, 80, 13, &[1, 1, 1, 1], &[4, 4, 4, 4]);
        assert_eq!(cols(&v, 0), [100, 100, 104, 115, 125, 122, 100, 100]);
        assert_eq!(cols(&v, 15), cols(&v, 0)); // 全行一致
        // bs2（tc=5）：p1'=105、tc=6、Δ=clip±6(7)=6 → 116/124
        let mut v = luma_plane(pq);
        filter_luma_v(&mut v, 16, 8, 0, 80, 13, &[2, 2, 2, 2], &[5, 5, 5, 5]);
        assert_eq!(cols(&v, 3), [100, 100, 105, 116, 124, 122, 100, 100]);
        // bs3（tc=7）：p1'=107、tc=8、Δ=clip±8(7)=7 → 117/123
        let mut v = luma_plane(pq);
        filter_luma_v(&mut v, 16, 8, 0, 80, 13, &[3, 3, 3, 3], &[7, 7, 7, 7]);
        assert_eq!(cols(&v, 7), [100, 100, 107, 117, 123, 122, 100, 100]);
        // bS=0 段跳过：只有第 0/1 段滤
        let mut v = luma_plane(pq);
        filter_luma_v(&mut v, 16, 8, 0, 80, 13, &[1, 0, 0, 0], &[4, 0, 0, 0]);
        assert_eq!(cols(&v, 0), [100, 100, 104, 115, 125, 122, 100, 100]);
        assert_eq!(cols(&v, 4), pq); // 第 1 段（行 4..7）未动
    }

    #[test]
    fn luma_bs123_tc0_zero_still_filters_p0q0() {
        // QP22 bs1：tc0=0。p1/q1 不修改（tc0==0 守卫）但 tc 增量照加，
        // p0/q0 以 clip±1 滤。α(22)=9、β(22)=3。
        let pq = [100, 102, 102, 104, 112, 110, 100, 100];
        // 门：|104−112|=8<9、|102−104|=2<3、|110−112|=2<3 过；
        // p 侧 |102−104|=2<3 → tc=1（p1 不改）；q 侧 |100−112|=12≥3 跳过；
        // Δ = clip±1, (32 + (102−110) + 4)>>3 = 28>>3 = 3 → 1
        // ⇒ p0'=105、q0'=111，p1 保持 102
        let mut v = luma_plane(pq);
        filter_luma_v(&mut v, 16, 8, 0, 9, 3, &[1, 1, 1, 1], &[0, 0, 0, 0]);
        assert_eq!(cols(&v, 0), [100, 102, 102, 105, 111, 110, 100, 100]);
    }

    #[test]
    fn luma_no_filter_when_beta_exceeded() {
        // |q1−q0| = 20 ≥ β=13 → 整段不动
        let pq = [100, 100, 100, 110, 130, 150, 100, 100];
        let mut v = luma_plane(pq);
        filter_luma_v(&mut v, 16, 8, 0, 80, 13, &[2, 2, 2, 2], &[5, 5, 5, 5]);
        assert_eq!(cols(&v, 5), pq);
        // α=0（QP≤15）→ 整边直接返回
        let mut v2 = luma_plane([100, 100, 100, 110, 130, 122, 100, 100]);
        filter_luma_v(&mut v2, 16, 8, 0, 0, 0, &[3, 3, 3, 3], &[7, 7, 7, 7]);
        assert_eq!(cols(&v2, 0), [100, 100, 100, 110, 130, 122, 100, 100]);
    }

    #[test]
    fn luma_bs4_strong_and_weak_branches() {
        // QP40：α=80、β=13、强支路阈值 (α>>2)+2 = 22
        // 强支路：|p0−q0|=20 < 22 且 |p2−p0|=10<13（p 三点）、
        // |q2−q0|=30≥13（q 走弱式）
        let pq = [100, 100, 100, 110, 130, 120, 100, 100];
        // p0'=(100+200+220+260+120+4)>>3=113；p1'=(100+100+110+130+2)>>2=110；
        // p2'=(200+300+100+110+130+4)>>3=105；q0'=(240+130+100+2)>>2=118
        //（弱式用原始 p1=100）
        let mut v = luma_plane(pq);
        filter_luma_v_intra(&mut v, 16, 8, 0, 80, 13);
        assert_eq!(cols(&v, 0), [100, 105, 110, 113, 118, 120, 100, 100]);
        // 弱支路：|p0−q0|=35 ≥ 22 → 只改 p0/q0
        let pq2 = [100, 100, 100, 110, 145, 135, 100, 100];
        // p0'=(200+110+135+2)>>2=111；q0'=(270+145+100+2)>>2=129
        let mut v2 = luma_plane(pq2);
        filter_luma_v_intra(&mut v2, 16, 8, 0, 80, 13);
        assert_eq!(cols(&v2, 9), [100, 100, 100, 111, 129, 135, 100, 100]);
    }

    #[test]
    fn luma_horizontal_edge_matches_vertical() {
        // 同一组样本转置成行：边在行 8，每列同值
        let pq = [100, 100, 100, 110, 130, 122, 100, 100];
        let mut v = vec![100u8; 16 * 16];
        for (k, &s) in pq.iter().enumerate() {
            for col in 0..16 {
                v[(4 + k) * 16 + col] = s;
            }
        }
        filter_luma_h(&mut v, 16, 0, 8, 80, 13, &[1, 1, 1, 1], &[4, 4, 4, 4]);
        let row = |r: usize| -> [u8; 8] { std::array::from_fn(|k| v[(4 + k) * 16 + r]) };
        // 与垂直边 bs1 用例逐位一致
        assert_eq!(row(0), [100, 100, 104, 115, 125, 122, 100, 100]);
        assert_eq!(row(15), row(0));
        // intra 水平边同样对齐
        let mut v2 = vec![100u8; 16 * 16];
        for (k, &s) in [100u8, 100, 100, 110, 130, 120, 100, 100]
            .iter()
            .enumerate()
        {
            for col in 0..16 {
                v2[(4 + k) * 16 + col] = s;
            }
        }
        filter_luma_h_intra(&mut v2, 16, 0, 8, 80, 13);
        let row2 = |r: usize| -> [u8; 8] { std::array::from_fn(|k| v2[(4 + k) * 16 + r]) };
        assert_eq!(row2(0), [100, 105, 110, 113, 118, 120, 100, 100]);
    }

    // ---- chroma 滤波核 ----

    fn chroma_plane(pq: [u8; 4]) -> Vec<u8> {
        let mut v = vec![100u8; 8 * 8];
        for row in 0..8 {
            for (k, &s) in pq.iter().enumerate() {
                v[row * 8 + 2 + k] = s;
            }
        }
        v
    }

    fn ccols(v: &[u8], row: usize) -> [u8; 4] {
        std::array::from_fn(|k| v[row * 8 + 2 + k])
    }

    #[test]
    fn chroma_bs123_and_two_row_groups() {
        // QP40 chroma：qpc=36 → α=50、β=11、tc(bs1)=TC0[36][0]+1 = 3
        let pq = [101, 110, 130, 121];
        // 门：20<50、9<11、9<11 过；Δ = clip±3, (80 + (101−121) + 4)>>3 = 8 → 3
        // ⇒ p0'=113、q0'=127
        // 分组：bs=[1,1,0,0] → 第 0/1 组（行 0..3）滤、行 4..7 不动
        let mut v = chroma_plane(pq);
        filter_chroma_v(&mut v, 8, 4, 0, 50, 11, &[1, 1, 0, 0], &[3, 3, 0, 0]);
        assert_eq!(ccols(&v, 0), [101, 113, 127, 121]);
        assert_eq!(ccols(&v, 2), [101, 113, 127, 121]); // 第 1 组（行 2/3）
        assert_eq!(ccols(&v, 4), pq); // 第 2 组 bS=0 不动
        assert_eq!(ccols(&v, 7), pq);
    }

    #[test]
    fn chroma_intra() {
        // qpc=36（α=50、β=11）：门全过（35/10/10）
        let pq = [100, 110, 145, 135];
        // p0'=(200+110+135+2)>>2=111；q0'=(270+145+100+2)>>2=129
        let mut v = chroma_plane(pq);
        filter_chroma_v_intra(&mut v, 8, 4, 0, 50, 11);
        assert_eq!(ccols(&v, 0), [100, 111, 129, 135]);
        assert_eq!(ccols(&v, 7), [100, 111, 129, 135]);
        // 门不过（β 内梯度超）：|q1−q0| = 15 ≥ 11 → 不动
        let mut v2 = chroma_plane([100, 110, 145, 130]);
        filter_chroma_v_intra(&mut v2, 8, 4, 0, 50, 11);
        assert_eq!(ccols(&v2, 0), [100, 110, 145, 130]);
    }

    #[test]
    fn chroma_horizontal_edge() {
        let pq = [101, 110, 130, 121];
        let mut v = vec![100u8; 8 * 8];
        for (k, &s) in pq.iter().enumerate() {
            for col in 0..8 {
                v[(2 + k) * 8 + col] = s;
            }
        }
        filter_chroma_h(&mut v, 8, 0, 4, 50, 11, &[1, 1, 1, 1], &[3, 3, 3, 3]);
        let col = |c: usize| -> [u8; 4] { std::array::from_fn(|k| v[(2 + k) * 8 + c]) };
        assert_eq!(col(0), [101, 113, 127, 121]);
        assert_eq!(col(7), [101, 113, 127, 121]);
    }
}
