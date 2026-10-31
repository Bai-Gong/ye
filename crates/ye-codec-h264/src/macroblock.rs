//! 宏块层解码：CAVLC 残差 → 反量化 → 反变换 → 帧内预测 → 重建。
//!
//! ②a 冻结面（施工图②）：
//! - [`Macroblock`] / [`Picture`] 是跨模块数据结构，②b 的 deblock.rs 与
//!   ②c 的 mc.rs 在此之上消费，不修改本文件的解码语义；
//! - 平面存取约定：`luma/cb/cr` 为无 padding 行主序缓冲（stride 见
//!   [`Picture`]），邻样读取一律先经可用性判定，不做越界读；
//! - 反变换/反量化/预测公式全部逐字取自 FFmpeg
//!   （h264idct_template.c / h264_mvpred.h / h264_mb.c / h264_parse.c /
//!   h264data.c / h264_cavlc.c）与 x264（common/predict.c），出处标注在
//!   各函数处；
//! - 行为约定：解不对的形态一律 `Err`（InvalidData/Unsupported），
//!   绝不静默输出错帧；所有数组下标经边界检查，无 panic 路径。

use crate::bitreader::BitReader;
use crate::cavlc::{self, BlockKind};
use crate::error::H264Error;
use crate::inter::InterCtx;
use crate::mc;
use crate::sps::Pps;

// ---------------------------------------------------------------------------
// 常量表（FFmpeg h264data.c / x264 逐字）
// ---------------------------------------------------------------------------

/// 反量化基表 `V[qp%6][class]`（FFmpeg `ff_h264_dequant4_coeff_init`）。
/// class：0=DC 位、1=首行/首列、2=内部。
pub(crate) const DEQUANT_V: [[u32; 3]; 6] = [
    [10, 13, 16],
    [11, 14, 18],
    [13, 16, 20],
    [14, 18, 23],
    [16, 20, 25],
    [18, 23, 29],
];

/// chroma QP 映射表（FFmpeg `ff_h264_chroma_qp` 8bit 行，52 项）。
pub(crate) const CHROMA_QP: [u8; 52] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25,
    26, 27, 28, 29, // 0..29 恒等
    29, 30, 31, 32, 32, 33, 34, 34, 35, 35, 36, 36, 37, 37, 37, 38, 38, 38, 39, 39, 39, 39,
];

/// 4x4 zigzag 扫描（槽位 = 行*4+列）。
pub(crate) const ZIGZAG: [u8; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];

/// I 帧宏块残差解析/重建顺序：z 序（象限光栅 × 象限内光栅）→ 光栅块号。
/// 即 FFmpeg scan8 序；`Z_ORDER[i]` = 第 i 个解析的块的光栅下标。
pub(crate) const Z_ORDER: [usize; 16] = [0, 1, 4, 5, 2, 3, 6, 7, 8, 9, 12, 13, 10, 11, 14, 15];

/// 光栅 (cx,cy) → z 序号（用于 E..H 重建可用性判断）。
pub(crate) const fn z_of(cx: usize, cy: usize) -> usize {
    4 * (2 * (cy >> 1) + (cx >> 1)) + (cx & 1) + 2 * (cy & 1)
}

/// I_NxN 的 coded_block_pattern me(v) 48 项映射（FFmpeg
/// `ff_h264_golomb_to_intra4x4_cbp` 逐字）。位约定：bit0..3 = 四个 8x8
/// 象限（象限 q = 2·(cy/2) + cx/2）luma AC，bit4 = chroma DC，
/// bit5 = chroma AC。
pub(crate) const GOLOMB_TO_INTRA4X4_CBP: [u8; 48] = [
    47, 31, 15, 0, 23, 27, 29, 30, 7, 11, 13, 14, 39, 43, 45, 46, 16, 3, 5, 10, 12, 19, 21, 26, 28,
    35, 37, 42, 44, 1, 2, 4, 8, 17, 18, 20, 24, 6, 9, 22, 25, 32, 33, 34, 36, 40, 38, 41,
];

/// I16x16：mb_type−1 = k → (pred_mode, cbp)。pred_mode 用 spec 编号
/// （0=V, 1=H, 2=DC, 3=Plane）：mb_type 1=V、2=H、3=DC、4=Plane 起循环。
/// （FFmpeg `ff_h264_i_mb_type_info` 的 (2,1,0,3) 是其内部枚举
/// DC=0/H=1/V=2 的换算，勿混用。）
pub(crate) const I16_PRED: [u8; 4] = [0, 1, 2, 3];
pub(crate) const I16_CBP: [u8; 6] = [0, 0x10, 0x20, 0x0F, 0x1F, 0x2F];

/// chroma DC 的 2x2 扫描（槽位语义 = 2x2 光栅）。
pub(crate) const CHROMA_DC_SCAN: [u8; 4] = [0, 1, 4, 5];

/// 4x4 模式改写变体码（FFmpeg h264pred.h 编号：9=LEFT_DC、10=TOP_DC、
/// 11=DC_128，避开 spec 模式码 0..8）。
pub(crate) const P4_DC_LEFT: u8 = 9;
pub(crate) const P4_DC_TOP: u8 = 10;
pub(crate) const P4_DC_128: u8 = 11;

/// 16x16/chroma 模式改写变体码（FFmpeg LEFT_DC_PRED8x8=4、TOP_DC=5、
/// DC_128=6；这两族 spec 模式码只用 0..3，无冲突）。
pub(crate) const P8_DC_LEFT: u8 = 4;
pub(crate) const P8_DC_TOP: u8 = 5;
pub(crate) const P8_DC_128: u8 = 6;

// ---------------------------------------------------------------------------
// 反量化 / 反变换
// ---------------------------------------------------------------------------

/// 4x4 反量化乘子表（逐槽位）：`qmul[slot] = V[qp%6][class] · 64 · 2^(qp/6)`，
/// `class = min(row,1) + min(col,1)`（FFmpeg `dequant4_coeff_init` +
/// `ff_h264_dequant4_coeff_init_scan` 交叉验证）。
pub(crate) fn dequant4_coeff(qp: u8) -> [u32; 16] {
    let v = DEQUANT_V[usize::from(qp % 6)];
    let shift = u32::from(qp / 6) + 6;
    let mut qmul = [0u32; 16];
    for r in 0..4usize {
        for c in 0..4usize {
            // FFmpeg h264_ps.c init_dequant4_coeff_table 逐字：
            // class(x) = (x&1) + ((x>>2)&1)，存取经 (x>>2)|((x<<2)&0xF)
            // 转置（平坦 scaling 下转置对称）⟹ class(槽) = (行&1)+(列&1)
            let class = (r & 1) + (c & 1);
            qmul[r * 4 + c] = v[class] << shift;
        }
    }
    qmul
}

/// DC 乘子：`V[qp%6][0] · 64 · 2^(qp/6)`。
pub(crate) fn dc_qmul(qp: u8) -> u32 {
    DEQUANT_V[usize::from(qp % 6)][0] << (u32::from(qp / 6) + 6)
}

/// 4x4 整数反变换 + 叠加（FFmpeg h264idct_template.c `ff_h264_idct_add`
/// 逐字：DC 预加 1<<5 折入舍入，先列（垂直）后行（水平），输出裸 `>>6`）。
/// `block` 为光栅 [row*4+col]；`dst` 指向平面内 4x4 区域左上角起的长切片。
pub(crate) fn idct4x4_add(dst: &mut [u8], stride: usize, block: &mut [i16; 16]) {
    block[0] += 1 << 5;
    // 垂直 pass：处理列 i（block[i + 4*j]，行主序即列 i）
    for i in 0..4usize {
        let b0 = i32::from(block[i]);
        let b4 = i32::from(block[i + 4]);
        let b8 = i32::from(block[i + 8]);
        let b12 = i32::from(block[i + 12]);
        let z0 = b0 + b8;
        let z1 = b0 - b8;
        let z2 = (b4 >> 1) - b12;
        let z3 = b4 + (b12 >> 1);
        block[i] = (z0 + z3) as i16;
        block[i + 4] = (z1 + z2) as i16;
        block[i + 8] = (z1 - z2) as i16;
        block[i + 12] = (z0 - z3) as i16;
    }
    // 水平 pass：处理行 i，直接叠加并裁剪
    for i in 0..4usize {
        let b0 = i32::from(block[i * 4]);
        let b1 = i32::from(block[i * 4 + 1]);
        let b2 = i32::from(block[i * 4 + 2]);
        let b3 = i32::from(block[i * 4 + 3]);
        let z0 = b0 + b2;
        let z1 = b0 - b2;
        let z2 = (b1 >> 1) - b3;
        let z3 = b1 + (b3 >> 1);
        for (x, v) in [(0usize, z0 + z3), (1, z1 + z2), (2, z1 - z2), (3, z0 - z3)] {
            let d = i32::from(dst[i * stride + x]) + (v >> 6);
            dst[i * stride + x] = d.clamp(0, 255) as u8;
        }
    }
}

/// DC-only 反变换叠加（FFmpeg `ff_h264_idct_dc_add` 逐字）。
pub(crate) fn idct4x4_dc_add(dst: &mut [u8], stride: usize, block: &[i16; 16]) {
    let dc = (i32::from(block[0]) + 32) >> 6;
    for y in 0..4usize {
        for x in 0..4usize {
            let d = i32::from(dst[y * stride + x]) + dc;
            dst[y * stride + x] = d.clamp(0, 255) as u8;
        }
    }
}

/// Intra16x16 luma DC 反量化（Hadamard `H·X·H` 自然序，蝶形两遍实现
/// 与 FFmpeg `ff_h264_luma_dc_dequant_idct` 同数学；落位映射经实测
/// 对拍锁定为自然光栅序，回归见单测 `luma_dc_dequant_matches_ffmpeg_pipeline`）。
/// 输入/输出均为 16 项光栅；`qmul` = `V[qp%6][0]·64·2^(qp/6)`。
pub(crate) fn luma_dc_dequant(x: &[i16; 16], qmul: u32) -> Result<[i16; 16], H264Error> {
    let mut temp = [0i64; 16];
    for i in 0..4usize {
        let (a, b, c, d) = (
            i64::from(x[4 * i]),
            i64::from(x[4 * i + 1]),
            i64::from(x[4 * i + 2]),
            i64::from(x[4 * i + 3]),
        );
        let z0 = a + b;
        let z1 = a - b;
        let z2 = c - d;
        let z3 = c + d;
        temp[4 * i] = z0 + z3;
        temp[4 * i + 1] = z0 - z3;
        temp[4 * i + 2] = z1 - z2;
        temp[4 * i + 3] = z1 + z2;
    }
    // 输出自然光栅序：蝶形逐列的四输出 [(z0+z3),(z1+z2),(z1−z2),(z0−z3)]
    // 恰为 Y[0..3][i]（H·X·H 的第 i 列），落位光栅块 (r, i) = 4r+i
    let mut out = [0i16; 16];
    for i in 0..4usize {
        let t0 = temp[i];
        let t4 = temp[4 + i];
        let t8 = temp[8 + i];
        let t12 = temp[12 + i];
        let z0 = t0 + t8;
        let z1 = t0 - t8;
        let z2 = t4 - t12;
        let z3 = t4 + t12;
        let scale = |v: i64| -> Result<i16, H264Error> {
            let r = (v * i64::from(qmul) + 128) >> 8;
            i16::try_from(r)
                .map_err(|_| H264Error::InvalidData(format!("luma dc dequant exceeds i16: {r}")))
        };
        out[i] = scale(z0 + z3)?;
        out[4 + i] = scale(z1 + z2)?;
        out[8 + i] = scale(z1 - z2)?;
        out[12 + i] = scale(z0 - z3)?;
    }
    Ok(out)
}

/// chroma DC 反量化（2x2 Hadamard，FFmpeg
/// `ff_h264_chroma_dc_dequant_idct` 逐字：截断 `>>7`，无舍入加项）。
/// `qmul` = `V[qpc%6][0]·64·2^(qpc/6)`；输入/输出均为 2x2 光栅序
/// （[UL, UR, LL, LR]）。
pub(crate) fn chroma_dc_dequant(x: &[i16; 4], qmul: u32) -> Result<[i16; 4], H264Error> {
    let mut a = x[0];
    let mut b = x[1];
    let e = a - b;
    a += b;
    b = x[2] - x[3];
    let mut c = x[2];
    c += x[3];
    let scale = |v: i64| -> Result<i16, H264Error> {
        let r = (v * i64::from(qmul)) >> 7;
        i16::try_from(r)
            .map_err(|_| H264Error::InvalidData(format!("chroma dc dequant exceeds i16: {r}")))
    };
    Ok([
        scale(i64::from(a + c))?,
        scale(i64::from(e + b))?,
        scale(i64::from(a - c))?,
        scale(i64::from(e - b))?,
    ])
}

// ---------------------------------------------------------------------------
// 帧内预测（公式逐字：x264 common/predict.c）
// ---------------------------------------------------------------------------

const fn f1(a: i32, b: i32) -> i32 {
    (a + b + 1) >> 1
}
const fn f2(a: i32, b: i32, c: i32) -> i32 {
    (a + 2 * b + c + 2) >> 2
}

/// 4x4 预测边沿：top[0..4]=A..D、top[4..8]=E..H（可能为 D 复制）、
/// left[0..4]=I..L、tl=M。不可用侧填 0（模式改写保证不读）。
pub(crate) struct Edges4 {
    top: [u8; 8],
    left: [u8; 4],
    tl: u8,
}

/// 4x4 帧内预测（mode：spec 编号 0=V..8=HU；9/10/11 为 DC 变体改写产物）。
/// 写入 `out`（4x4 光栅）。
pub(crate) fn pred_4x4(mode: u8, e: &Edges4, out: &mut [u8; 16]) {
    let t: [i32; 8] = std::array::from_fn(|i| i32::from(e.top[i]));
    let l: [i32; 4] = std::array::from_fn(|i| i32::from(e.left[i]));
    let tl = i32::from(e.tl);
    let mut px = |x: usize, y: usize, v: i32| out[y * 4 + x] = v.clamp(0, 255) as u8;
    match mode {
        0 => {
            // V
            for (x, &tv) in t.iter().enumerate().take(4) {
                for y in 0..4 {
                    px(x, y, tv);
                }
            }
        }
        1 => {
            // H
            for (y, &lv) in l.iter().enumerate().take(4) {
                for x in 0..4 {
                    px(x, y, lv);
                }
            }
        }
        2 | P4_DC_LEFT | P4_DC_TOP | P4_DC_128 => {
            // DC 及其变体（x264 predict_4x4_dc* 逐字）
            let dc = match mode {
                2 => (t[0] + t[1] + t[2] + t[3] + l[0] + l[1] + l[2] + l[3] + 4) >> 3,
                P4_DC_LEFT => (l[0] + l[1] + l[2] + l[3] + 2) >> 2,
                P4_DC_TOP => (t[0] + t[1] + t[2] + t[3] + 2) >> 2,
                _ => 128,
            };
            for y in 0..4 {
                for x in 0..4 {
                    px(x, y, dc);
                }
            }
        }
        3 => {
            // DDL（x264 predict_4x4_ddl_c 逐字；t7 复制）
            let (t0, t1, t2, t3) = (t[0], t[1], t[2], t[3]);
            let (t4, t5, t6, t7) = (t[4], t[5], t[6], t[7]);
            px(0, 0, f2(t0, t1, t2));
            let v = f2(t1, t2, t3);
            px(1, 0, v);
            px(0, 1, v);
            let v = f2(t2, t3, t4);
            px(2, 0, v);
            px(1, 1, v);
            px(0, 2, v);
            let v = f2(t3, t4, t5);
            px(3, 0, v);
            px(2, 1, v);
            px(1, 2, v);
            px(0, 3, v);
            let v = f2(t4, t5, t6);
            px(3, 1, v);
            px(2, 2, v);
            px(1, 3, v);
            let v = f2(t5, t6, t7);
            px(3, 2, v);
            px(2, 3, v);
            px(3, 3, f2(t6, t7, t7));
        }
        4 => {
            // DDR
            let (t0, t1, t2, t3) = (t[0], t[1], t[2], t[3]);
            let (l0, l1, l2, l3) = (l[0], l[1], l[2], l[3]);
            px(3, 0, f2(t3, t2, t1));
            let v = f2(t2, t1, t0);
            px(2, 0, v);
            px(3, 1, v);
            let v = f2(t1, t0, tl);
            px(1, 0, v);
            px(2, 1, v);
            px(3, 2, v);
            let v = f2(t0, tl, l0);
            px(0, 0, v);
            px(1, 1, v);
            px(2, 2, v);
            px(3, 3, v);
            let v = f2(tl, l0, l1);
            px(0, 1, v);
            px(1, 2, v);
            px(2, 3, v);
            let v = f2(l0, l1, l2);
            px(0, 2, v);
            px(1, 3, v);
            px(0, 3, f2(l1, l2, l3));
        }
        5 => {
            // VR
            let (t0, t1, t2, t3) = (t[0], t[1], t[2], t[3]);
            let (l0, l1, l2) = (l[0], l[1], l[2]);
            px(0, 3, f2(l2, l1, l0));
            px(0, 2, f2(l1, l0, tl));
            let v = f2(l0, tl, t0);
            px(0, 1, v);
            px(1, 3, v);
            let v = f1(tl, t0);
            px(0, 0, v);
            px(1, 2, v);
            let v = f2(tl, t0, t1);
            px(1, 1, v);
            px(2, 3, v);
            let v = f1(t0, t1);
            px(1, 0, v);
            px(2, 2, v);
            let v = f2(t0, t1, t2);
            px(2, 1, v);
            px(3, 3, v);
            let v = f1(t1, t2);
            px(2, 0, v);
            px(3, 2, v);
            px(3, 1, f2(t1, t2, t3));
            px(3, 0, f1(t2, t3));
        }
        6 => {
            // HD
            let (t0, t1, t2) = (t[0], t[1], t[2]);
            let (l0, l1, l2, l3) = (l[0], l[1], l[2], l[3]);
            px(0, 3, f1(l2, l3));
            px(1, 3, f2(l1, l2, l3));
            let v = f1(l1, l2);
            px(0, 2, v);
            px(2, 3, v);
            let v = f2(l0, l1, l2);
            px(1, 2, v);
            px(3, 3, v);
            let v = f1(l0, l1);
            px(0, 1, v);
            px(2, 2, v);
            let v = f2(tl, l0, l1);
            px(1, 1, v);
            px(3, 2, v);
            let v = f1(tl, l0);
            px(0, 0, v);
            px(2, 1, v);
            let v = f2(t0, tl, l0);
            px(1, 0, v);
            px(3, 1, v);
            px(2, 0, f2(t1, t0, tl));
            px(3, 0, f2(t2, t1, t0));
        }
        7 => {
            // VL
            let (t0, t1, t2, t3) = (t[0], t[1], t[2], t[3]);
            let (t4, t5, t6) = (t[4], t[5], t[6]);
            px(0, 0, f1(t0, t1));
            px(0, 1, f2(t0, t1, t2));
            let v = f1(t1, t2);
            px(1, 0, v);
            px(0, 2, v);
            let v = f2(t1, t2, t3);
            px(1, 1, v);
            px(0, 3, v);
            let v = f1(t2, t3);
            px(2, 0, v);
            px(1, 2, v);
            let v = f2(t2, t3, t4);
            px(2, 1, v);
            px(1, 3, v);
            let v = f1(t3, t4);
            px(3, 0, v);
            px(2, 2, v);
            let v = f2(t3, t4, t5);
            px(3, 1, v);
            px(2, 3, v);
            px(3, 2, f1(t4, t5));
            px(3, 3, f2(t4, t5, t6));
        }
        8 => {
            // HU（l3 复制）
            let (l0, l1, l2, l3) = (l[0], l[1], l[2], l[3]);
            px(0, 0, f1(l0, l1));
            px(1, 0, f2(l0, l1, l2));
            let v = f1(l1, l2);
            px(2, 0, v);
            px(0, 1, v);
            let v = f2(l1, l2, l3);
            px(3, 0, v);
            px(1, 1, v);
            let v = f1(l2, l3);
            px(2, 1, v);
            px(0, 2, v);
            let v = f2(l2, l3, l3);
            px(3, 1, v);
            px(1, 2, v);
            for (x, y) in [(3, 2), (1, 3), (0, 3), (2, 2), (2, 3), (3, 3)] {
                px(x, y, l3);
            }
        }
        // 9..=11 已并入 DC 分支；调用方保证 mode ∈ 0..=11
        _ => {}
    }
}

/// Intra16x16 预测（mode：spec 编号 0=V,1=H,2=DC,3=Plane + 4/5/6 变体）。
/// `top`/`left` 不可用侧由调用方填 0（模式改写保证不读）；`tl` 为左上角
/// 样本（Plane 需要：H/V 求和的 i=7 项读 p[6−i]=p[−1] 即角点）。
pub(crate) fn pred_16x16(
    mode: u8,
    top: &[u8; 16],
    left: &[u8; 16],
    tl: u8,
    out: &mut [u8],
    stride: usize,
) {
    match mode {
        0 => {
            for y in 0..16 {
                for x in 0..16 {
                    out[y * stride + x] = top[x];
                }
            }
        }
        1 => {
            for y in 0..16 {
                for x in 0..16 {
                    out[y * stride + x] = left[y];
                }
            }
        }
        2 | P8_DC_LEFT | P8_DC_TOP | P8_DC_128 => {
            let dc: i32 = match mode {
                2 => {
                    let s: i32 = top.iter().chain(left.iter()).copied().map(i32::from).sum();
                    (s + 16) >> 5
                }
                P8_DC_LEFT => {
                    let s: i32 = left.iter().copied().map(i32::from).sum();
                    (s + 8) >> 4
                }
                P8_DC_TOP => {
                    let s: i32 = top.iter().copied().map(i32::from).sum();
                    (s + 8) >> 4
                }
                _ => 128,
            };
            let d = dc.clamp(0, 255) as u8;
            for y in 0..16 {
                for x in 0..16 {
                    out[y * stride + x] = d;
                }
            }
        }
        3 => {
            // Plane（x264 predict_16x16_p_c 逐字）
            let mut h = 0i32;
            let mut v = 0i32;
            let tlc = i32::from(tl);
            for i in 0..8usize {
                let b = if i == 7 { tlc } else { i32::from(top[6 - i]) };
                let d = if i == 7 { tlc } else { i32::from(left[6 - i]) };
                h += (i as i32 + 1) * (i32::from(top[8 + i]) - b);
                v += (i as i32 + 1) * (i32::from(left[8 + i]) - d);
            }
            let a = 16 * (i32::from(left[15]) + i32::from(top[15]));
            let b = (5 * h + 32) >> 6;
            let c = (5 * v + 32) >> 6;
            let i00 = a - b * 7 - c * 7 + 16;
            for y in 0..16usize {
                for x in 0..16usize {
                    let pix = i00 + b * x as i32 + c * y as i32;
                    out[y * stride + x] = (pix >> 5).clamp(0, 255) as u8;
                }
            }
        }
        // 4..=6 已并入 DC 分支；调用方保证 mode ∈ 0..=6
        _ => {}
    }
}

/// chroma 8x8 预测（mode：spec 编号 0=DC,1=H,2=V,3=Plane + 4/5/6 变体）。
/// `tl` 为左上角样本（Plane 的 i=3 项读 p[2−i]=p[−1] 即角点）。
pub(crate) fn pred_8x8c(
    mode: u8,
    top: &[u8; 8],
    left: &[u8; 8],
    tl: u8,
    out: &mut [u8],
    stride: usize,
) {
    match mode {
        0 | P8_DC_LEFT | P8_DC_TOP | P8_DC_128 => {
            // 四象限 DC（x264 predict_8x8c_dc* 逐字）及变体
            let dcs: [[i32; 2]; 2] = match mode {
                0 => {
                    let s0: i32 = top[0..4].iter().copied().map(i32::from).sum();
                    let s1: i32 = top[4..8].iter().copied().map(i32::from).sum();
                    let s2: i32 = left[0..4].iter().copied().map(i32::from).sum();
                    let s3: i32 = left[4..8].iter().copied().map(i32::from).sum();
                    [
                        [(s0 + s2 + 4) >> 3, (s1 + 2) >> 2],
                        [(s3 + 2) >> 2, (s1 + s3 + 4) >> 3],
                    ]
                }
                P8_DC_LEFT => {
                    let s2: i32 = left[0..4].iter().copied().map(i32::from).sum();
                    let s3: i32 = left[4..8].iter().copied().map(i32::from).sum();
                    [
                        [(s2 + 2) >> 2, (s2 + 2) >> 2],
                        [(s3 + 2) >> 2, (s3 + 2) >> 2],
                    ]
                }
                P8_DC_TOP => {
                    let s0: i32 = top[0..4].iter().copied().map(i32::from).sum();
                    let s1: i32 = top[4..8].iter().copied().map(i32::from).sum();
                    [
                        [(s0 + 2) >> 2, (s1 + 2) >> 2],
                        [(s0 + 2) >> 2, (s1 + 2) >> 2],
                    ]
                }
                _ => [[128, 128], [128, 128]],
            };
            for y in 0..8usize {
                for x in 0..8usize {
                    let d = dcs[y / 4][x / 4].clamp(0, 255) as u8;
                    out[y * stride + x] = d;
                }
            }
        }
        1 => {
            for y in 0..8 {
                for x in 0..8 {
                    out[y * stride + x] = left[y];
                }
            }
        }
        2 => {
            for y in 0..8 {
                for x in 0..8 {
                    out[y * stride + x] = top[x];
                }
            }
        }
        3 => {
            // Plane（x264 predict_8x8c_p_c 逐字）
            let mut h = 0i32;
            let mut v = 0i32;
            let tlc = i32::from(tl);
            for i in 0..4usize {
                let b = if i == 3 { tlc } else { i32::from(top[2 - i]) };
                let d = if i == 3 { tlc } else { i32::from(left[2 - i]) };
                h += (i as i32 + 1) * (i32::from(top[4 + i]) - b);
                v += (i as i32 + 1) * (i32::from(left[4 + i]) - d);
            }
            let a = 16 * (i32::from(left[7]) + i32::from(top[7]));
            let b = (17 * h + 16) >> 5;
            let c = (17 * v + 16) >> 5;
            let i00 = a - 3 * b - 3 * c + 16;
            for y in 0..8usize {
                for x in 0..8usize {
                    let pix = i00 + b * x as i32 + c * y as i32;
                    out[y * stride + x] = (pix >> 5).clamp(0, 255) as u8;
                }
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// 冻结数据结构
// ---------------------------------------------------------------------------

/// 宏块类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MbKind {
    Intra4x4,
    Intra16x16,
    Pcm,
    /// P 片帧间宏块（含 P_Skip；残差与重建同 intra 路径叠加）。
    InterP,
    /// B 片帧间宏块（含 B_Skip/B_Direct；双向预测经 mv_l1/ref_l1）。
    InterB,
}

/// 帧间宏块的分区形状（直接预测的共置形状推断与 col_zero 判定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InterShape {
    /// 非 inter（intra/PCM）。
    None,
    P16x16,
    P16x8,
    P8x16,
    /// 8x8 四象限（含 B 的混合象限；细粒度见 [`Macroblock::sub_shape`]）。
    P8x8,
}

/// 单宏块解码结果（冻结面：②b/②c deblock.rs 消费 qp/nnz/slice_id/
/// skipped/平面）。
#[derive(Debug, Clone)]
pub(crate) struct Macroblock {
    /// 本帧内是否已完成解码（邻块可用性判定基准）。
    pub decoded: bool,
    pub kind: MbKind,
    /// 该宏块 QP（0..=51；PCM 按惯例记 0，供 ②b 去块滤波查表——
    /// FFmpeg qscale_table 同值）。
    pub qp: u8,
    /// cbp：bit0..3 luma 象限、bit4 chroma DC、bit5 chroma AC。
    pub cbp: u8,
    /// Intra16x16 预测模式（spec 编号 0=V,1=H,2=DC,3=Plane + 变体 4..6）。
    pub i16_mode: u8,
    /// Intra4x4 预测模式（光栅 16 块；spec 编号 0..8 + 变体 9..11）。
    pub i4_modes: [u8; 16],
    /// chroma 预测模式（spec 编号 0=DC,1=H,2=V,3=Plane + 变体 4..6）。
    pub chroma_mode: u8,
    /// chroma 预测模式**语法原值**（改写前 0..3；FFmpeg
    /// `chroma_pred_mode_table` 语义，供 CABAC 邻块 ctx 查询；
    /// inter/skip/未解码 = 0 与 FFmpeg 同）。
    pub chroma_syntax: u8,
    /// Intra4x4 预测模式**语法原值**（光栅 16 块、改写前 0..8；FFmpeg
    /// `write_back_intra_pred_mode` 在 `check` 改写**之前**落库——
    /// 邻块 prev 查询取改写前值。非 I4x4 宏块无意义（查询走 2）。
    pub i4_syntax: [u8; 16],
    /// nnz：0..15 luma（光栅，I16 为 AC 计数）、16..19 Cb AC、20..23 Cr AC。
    /// PCM 时全部 = 16。
    pub nnz: [u8; 24],
    /// chroma DC 块的 nnz（Cb、Cr；PCM 时 = 16）。
    pub nnz_chroma_dc: [u8; 2],
    /// luma DC（I16x16）是否编码（CABAC 下一 MB 的 DC cbf 上下文）。
    pub luma_dc_coded: bool,
    /// 帧内片编号。
    pub slice_id: u16,
    // ---- ②b 增补（P 帧冻结面；②c deblock 消费）----
    /// P_Skip 宏块标记（deblock bS 判定用；②c 接线前暂无读者）。
    #[allow(dead_code)]
    pub skipped: bool,
    /// 4x4 光栅网格的运动矢量（QPEL；intra 宏块全 0）。
    pub mv: [[i16; 2]; 16],
    /// 4x4 光栅网格的参考帧索引（P：0..；intra/未用块 = −1）。
    /// 参考索引物理粒度为 8x8（同 8x8 内四块共享），此处按 4x4 复制存储。
    pub ref_idx: [i8; 16],
    // ---- B 片增补（M2；直接预测/deblock/CABAC 上下文消费）----
    /// list1 运动矢量（B 片；InterP/intra 全 0）。
    pub mv_l1: [[i16; 2]; 16],
    /// list1 参考索引（B 片未用块 = −1）。
    pub ref_idx_l1: [i8; 16],
    /// 去块比较实际参考图身份，允许 B 双列表交换；缺席时保留索引比较。
    pub ref_poc: [[Option<i32>; 16]; 2],
    /// list0 mvd（mv−mvp；CABAC 邻块上下文；无邻语义 = 0）。
    pub mvd: [[i16; 2]; 16],
    /// list1 mvd。
    pub mvd_l1: [[i16; 2]; 16],
    /// luma 8x8 变换（High 档；deblock 内边与 8x8 残差路径消费）。
    pub transform_8x8: bool,
    /// 帧间分区形状（intra = None）。
    pub shape: InterShape,
    /// B 片直接预测宏块（B_Skip/B_Direct_16x16；CABAC 邻块 ctx 与
    /// ref ctx 的 direct 排除判定用）。
    pub direct: bool,
    /// 8x8 象限的子分区粒度（shape==P8x8 时有效）：
    /// 0=8x8、1=8x4、2=4x8、3=4x4、255=direct 象限。
    pub sub_shape: [u8; 4],
}

impl Default for Macroblock {
    fn default() -> Self {
        Self {
            decoded: false,
            kind: MbKind::Intra16x16,
            qp: 0,
            cbp: 0,
            i16_mode: 2,
            i4_modes: [2; 16],
            chroma_mode: 0,
            chroma_syntax: 0,
            i4_syntax: [2; 16],
            nnz: [0; 24],
            nnz_chroma_dc: [0; 2],
            luma_dc_coded: false,
            slice_id: 0,
            skipped: false,
            mv: [[0; 2]; 16],
            ref_idx: [-1; 16],
            mv_l1: [[0; 2]; 16],
            ref_idx_l1: [-1; 16],
            ref_poc: [[None; 16]; 2],
            mvd: [[0; 2]; 16],
            mvd_l1: [[0; 2]; 16],
            transform_8x8: false,
            shape: InterShape::None,
            direct: false,
            sub_shape: [0; 4],
        }
    }
}

/// 解码中的图像（冻结面）。平面为行主序、无 padding：
/// `luma` stride = `mb_width*16`、行数 `mb_height*16`；
/// `cb`/`cr` stride = `mb_width*8`、行数 `mb_height*8`。
#[derive(Debug, Clone)]
pub(crate) struct Picture {
    pub mb_width: usize,
    pub mb_height: usize,
    /// 裁剪后输出尺寸（SPS width()/height()）。
    pub width: usize,
    pub height: usize,
    pub luma: Vec<u8>,
    pub cb: Vec<u8>,
    pub cr: Vec<u8>,
    /// 宏块光栅网格。
    pub mbs: Vec<Macroblock>,
    /// 各片的去块参数（下标 = slice_id；解码期登记，图完成时统一滤波）。
    pub deblock_params: Vec<crate::slice::DeblockParams>,
    // ---- B 片增补（M2）----
    /// 本图 POC（直接预测缩放与 DPB 重排）。
    pub poc: i32,
    /// 长期参考标记（col_zero / dist_scale 门控）。
    pub is_long_term: bool,
    /// 解码本图时的两列参考快照（map_col 匹配用；缺项 None）。
    pub used_l0: Vec<Option<std::sync::Arc<Picture>>>,
    pub used_l1: Vec<Option<std::sync::Arc<Picture>>>,
}

impl Picture {
    pub(crate) fn new(mb_width: usize, mb_height: usize, width: usize, height: usize) -> Self {
        Self {
            mb_width,
            mb_height,
            width,
            height,
            luma: vec![0; mb_width * 16 * mb_height * 16],
            cb: vec![0; mb_width * 8 * mb_height * 8],
            cr: vec![0; mb_width * 8 * mb_height * 8],
            mbs: vec![Macroblock::default(); mb_width * mb_height],
            deblock_params: Vec::new(),
            poc: 0,
            is_long_term: false,
            used_l0: Vec::new(),
            used_l1: Vec::new(),
        }
    }

    pub(crate) fn luma_stride(&self) -> usize {
        self.mb_width * 16
    }
    pub(crate) fn chroma_stride(&self) -> usize {
        self.mb_width * 8
    }
}

// ---------------------------------------------------------------------------
// 邻居可用性（FFmpeg h264_mvpred.h fill_decode_neighbors 逐字语义）
// ---------------------------------------------------------------------------

/// MB 级邻居可用性（progressive、无 FMO；片判定含 FFmpeg 的 topleft
/// 短路怪癖：仅当 topleft 跨片/未解码时才连带检查 top/left 的片归属）。
pub(crate) struct NeighAvail {
    pub(crate) top: bool,
    pub(crate) left: bool,
    pub(crate) topleft: bool,
    /// 右上 E 宏块（仅光栅块 (3,0) 的 E..H 用）。
    pub(crate) er: bool,
}

pub(crate) fn neighbor_avail(
    pic: &Picture,
    mbx: usize,
    mby: usize,
    slice_id: u16,
    constrained: bool,
) -> NeighAvail {
    // constrained_intra_pred=1：intra 预测样本不得取自 inter 邻块
    // （FFmpeg fill_decode_caches 的 type_mask 语义；I 片内全 intra 无影响）
    let get = |x: usize, y: usize| -> Option<&Macroblock> {
        if x >= pic.mb_width || y >= pic.mb_height {
            return None;
        }
        let mb = &pic.mbs[y * pic.mb_width + x];
        if !mb.decoded {
            return None;
        }
        if constrained && mb.kind == MbKind::InterP {
            return None;
        }
        Some(mb)
    };
    let tl = if mbx > 0 && mby > 0 {
        get(mbx - 1, mby - 1)
    } else {
        None
    };
    let tp = if mby > 0 { get(mbx, mby - 1) } else { None };
    let lf = if mbx > 0 { get(mbx - 1, mby) } else { None };
    let er = if mby > 0 && mbx + 1 < pic.mb_width {
        get(mbx + 1, mby - 1)
    } else {
        None
    };

    let tl_diff = tl.map(|m| m.slice_id != slice_id).unwrap_or(true);
    let top = tp.is_some() && (!tl_diff || tp.is_some_and(|m| m.slice_id == slice_id));
    let left = lf.is_some() && (!tl_diff || lf.is_some_and(|m| m.slice_id == slice_id));
    let topleft = tl.is_some_and(|m| m.slice_id == slice_id);
    let er_avail = er.is_some_and(|m| m.slice_id == slice_id);
    // 说明：constrained_intra_pred 只排除 inter 邻块；②a 全 I 帧，
    // 已解码邻块必为 intra，该标志在此无损。
    NeighAvail {
        top,
        left,
        topleft,
        er: er_avail,
    }
}

/// nC：`(nA+nB+1)>>1`（两侧可用）；单侧 → 该值；全无 → 0。
/// 以 64 作不可用哨兵（FFmpeg h264_mvpred.h 的 0x40 技巧逐字：
/// `sum<64 ? (sum+1)>>1 : sum&31`）。
fn nc_of(a: u32, b: u32) -> u8 {
    let s = a + b;
    if s < 64 {
        ((s + 1) >> 1) as u8
    } else {
        (s & 31) as u8
    }
}

/// 16x16 / chroma 模式改写（FFmpeg h264_parse.c
/// `ff_h264_check_intra_pred_mode` 逐字；`dc_code` 传各自族的 DC 编号——
/// 16x16 为 2，chroma 为 0。H 需左、V 需顶、Plane 需两侧、DC 按可用面
/// 变体；先按顶改写再按左改写）。
pub(crate) fn rewrite_dc_mode(
    mode: u8,
    top: bool,
    left: bool,
    topleft: bool,
    dc_code: u8,
) -> Result<u8, H264Error> {
    if mode > 3 {
        return Err(H264Error::InvalidData(format!(
            "intra pred mode {mode} out of range"
        )));
    }
    if mode == 3 && !topleft {
        // Plane 的 H/V 求和读左上角样本（x264/FFmpeg 同）；多片 L 形边界下
        // 角点不可用时如实拒绝（FFmpeg 读宿主缓冲陈旧值，无法对拍）。
        return Err(H264Error::Unsupported(
            "plane pred mode needs unavailable topleft sample",
        ));
    }
    let v_code = if dc_code == 0 { 2 } else { 0 }; // 本族的 V 编号
    let mut m = mode;
    if !top {
        m = if m == dc_code {
            P8_DC_LEFT
        } else if m == 1 {
            1 // H 不需要顶
        } else {
            return Err(H264Error::InvalidData(
                "top samples unavailable for requested intra mode".into(),
            ));
        };
    }
    if !left {
        m = if m == dc_code {
            P8_DC_TOP
        } else if m == v_code {
            v_code // V 不需要左
        } else if m == P8_DC_LEFT {
            P8_DC_128
        } else {
            return Err(H264Error::InvalidData(
                "left samples unavailable for requested intra mode".into(),
            ));
        };
    }
    Ok(m)
}

/// 4x4 模式改写（FFmpeg `ff_h264_check_intra4x4_pred_mode` 逐字）：
/// 行 0 块按顶可用改写、列 0 块按左可用改写；需要侧缺失的模式 → Err。
pub(crate) fn rewrite_4x4_modes(
    modes: &mut [u8; 16],
    top: bool,
    left: bool,
    topleft: bool,
) -> Result<(), H264Error> {
    if !top {
        for mode in modes.iter_mut().take(4) {
            *mode = match *mode {
                1 | 8 => *mode,  // H、HU 不需要顶
                2 => P4_DC_LEFT, // DC → DC_LEFT
                _ => {
                    return Err(H264Error::InvalidData(
                        "top samples unavailable for intra4x4 mode".into(),
                    ));
                }
            };
        }
    }
    if !left {
        for y in 0..4usize {
            let i = y * 4;
            modes[i] = match modes[i] {
                0 | 3 | 7 => modes[i], // V、DDL、VL 不需要左
                2 => P4_DC_TOP,        // DC → DC_TOP
                P4_DC_LEFT => P4_DC_128,
                _ => {
                    return Err(H264Error::InvalidData(
                        "left samples unavailable for intra4x4 mode".into(),
                    ));
                }
            };
        }
    }
    // 块 (0,0) 的对角模式（DDR=4/VR=5/HD=6）需要 topleft 角样本；
    // 多片 L 形边界下不可用时如实拒绝（FFmpeg 读宿主缓冲陈旧值，
    // 无法对拍，按铁律收紧为 Unsupported）。
    if matches!(modes[0], 4..=6) && !topleft {
        return Err(H264Error::Unsupported(
            "intra4x4 diagonal mode at (0,0) needs unavailable topleft sample",
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 残差 nC（用本宏块解析中的 nnz + 已解码邻块）
// ---------------------------------------------------------------------------

/// luma 块 b（光栅）的 nC。`cur` = 本宏块已解析的 nnz（luma 槽），
/// `left`/`top` = 邻块对应块的 nnz（不可用 → None）。
pub(crate) fn nc_luma(cur: &[u8; 24], left: Option<u8>, top: Option<u8>, b: usize) -> u8 {
    let (cx, cy) = (b % 4, b / 4);
    let a = if cx > 0 {
        u32::from(cur[b - 1])
    } else {
        left.map(u32::from).unwrap_or(64)
    };
    let c = if cy > 0 {
        u32::from(cur[b - 4])
    } else {
        top.map(u32::from).unwrap_or(64)
    };
    nc_of(a, c)
}

/// chroma AC 块 i（2x2 光栅）的 nC；plane：0=Cb（nnz 16..19）、1=Cr。
pub(crate) fn nc_chroma_ac(
    cur: &[u8; 24],
    left: Option<u8>,
    top: Option<u8>,
    plane: usize,
    i: usize,
) -> u8 {
    let base = 16 + plane * 4;
    let (cx, cy) = (i % 2, i / 2);
    let a = if cx > 0 {
        u32::from(cur[base + i - 1])
    } else {
        left.map(u32::from).unwrap_or(64)
    };
    let c = if cy > 0 {
        u32::from(cur[base + i - 2])
    } else {
        top.map(u32::from).unwrap_or(64)
    };
    nc_of(a, c)
}

/// chroma DC 块的 nC（无块内邻居，只用邻块 DC nnz）。
pub(crate) fn nc_chroma_dc(left: Option<u8>, top: Option<u8>) -> u8 {
    nc_of(
        left.map(u32::from).unwrap_or(64),
        top.map(u32::from).unwrap_or(64),
    )
}

/// 邻块 Intra4x4 模式（非 I4x4 邻块贡献 2=DC——FFmpeg
/// `fill_decode_caches` 的 `2 - 3*!avail` 语义；可用性由调用方判定）。
/// 取**语法原值**（改写前）——FFmpeg `write_back_intra_pred_mode`
/// 先落库后由 `check` 改写缓存，邻块读到的是改写前值。
/// （缺陷修复登记 2026-10-29：曾取改写后值——边角块被改写成 DC
/// 变体后 prev 随之漂移，预测错位。）
pub(crate) fn neighbor_i4_mode(mb: &Macroblock, slot: usize) -> u8 {
    if mb.kind == MbKind::Intra4x4 {
        let index = if mb.transform_8x8 {
            ((slot / 8) * 2 + (slot % 4) / 2) * 4
        } else {
            slot
        };
        mb.i4_syntax[index]
    } else {
        2
    }
}

/// 最可能模式（FFmpeg `pred_intra_mode`：任一侧不可用 → DC，否则 min）。
pub(crate) fn prev_i4_mode(left: Option<u8>, top: Option<u8>) -> u8 {
    match (left, top) {
        (Some(l), Some(t)) => l.min(t),
        _ => 2,
    }
}

// ---------------------------------------------------------------------------
// 宏块解码主入口
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// ②b：P 片帧间解码（mvp / MC / skip）
// ---------------------------------------------------------------------------

/// 参考索引哨兵（FFmpeg h264pred.h / h264dec.h 逐字）。
pub(crate) const LIST_NOT_USED: i8 = -1;
pub(crate) const PART_NOT_AVAILABLE: i8 = -2;

/// 单 4x4 块的 MV/参考预测项。
#[derive(Clone, Copy)]
pub(crate) struct MvNb {
    pub(crate) mv: [i16; 2],
    pub(crate) ref_idx: i8,
}

impl MvNb {
    pub(crate) const PART: MvNb = MvNb {
        mv: [0; 2],
        ref_idx: PART_NOT_AVAILABLE,
    };
    pub(crate) const UNUSED: MvNb = MvNb {
        mv: [0; 2],
        ref_idx: LIST_NOT_USED,
    };
}

/// 当前宏块的 MV 预测网格：坐标 x ∈ −1..=4（列）、y ∈ −1..=3（行），
/// 覆盖左/上/左上/右上边沿 + 本宏块 4x4 内部（随解析逐分区填充；
/// 未填充分量 = PART_NOT_AVAILABLE，与 FFmpeg cache 预置语义一致）。
pub(crate) struct MvGrid {
    pub(crate) cells: [[MvNb; 6]; 5], // [y+1][x+1]
}

impl MvGrid {
    pub(crate) fn get(&self, x: i32, y: i32) -> MvNb {
        self.cells[(y + 1) as usize][(x + 1) as usize]
    }
    pub(crate) fn set_rect(&mut self, bx: i32, by: i32, w: i32, h: i32, mv: [i16; 2], ref_idx: i8) {
        for yy in by..by + h {
            for xx in bx..bx + w {
                self.cells[(yy + 1) as usize][(xx + 1) as usize] = MvNb { mv, ref_idx };
            }
        }
    }
}

/// 从邻块宏块取 4x4 预测项（inter：真实 mv/ref；intra：LIST_NOT_USED+0；
/// 未解码：PART_NOT_AVAILABLE）。
pub(crate) fn mv_entry(mb: &Macroblock, b: usize) -> MvNb {
    if !mb.decoded {
        return MvNb::PART;
    }
    if mb.kind == MbKind::InterP {
        MvNb {
            mv: mb.mv[b],
            ref_idx: mb.ref_idx[b],
        }
    } else {
        MvNb::UNUSED
    }
}

/// 构建 MB 起始的 MV 网格（边沿填邻块，内部 = PART_NOT_AVAILABLE）。
pub(crate) fn build_mv_grid(pic: &Picture, mbx: usize, mby: usize) -> MvGrid {
    let mut cells = [[MvNb::PART; 6]; 5];
    let inb = |x: i32, y: i32| -> bool {
        x >= 0 && y >= 0 && (x as usize) < pic.mb_width && (y as usize) < pic.mb_height
    };
    let at = |x: i32, y: i32, b: usize| -> MvNb {
        if inb(x, y) {
            mv_entry(&pic.mbs[y as usize * pic.mb_width + x as usize], b)
        } else {
            MvNb::PART
        }
    };
    // 左列（含左上角）
    for y in 0..4i32 {
        cells[(y + 1) as usize][0] = at(mbx as i32 - 1, mby as i32, 3 + y as usize * 4);
    }
    // 顶行 x ∈ 0..3（顶 MB 底行）
    for x in 0..4i32 {
        cells[0][(x + 1) as usize] = at(mbx as i32, mby as i32 - 1, x as usize + 12);
    }
    // 右上（above-right MB 的 (0,3)）；x=4 且 y ≥ 0 恒 PART（网格初值）
    cells[0][5] = at(mbx as i32 + 1, mby as i32 - 1, 12);
    // 左上角（above-left MB 的 (3,3)）
    cells[0][0] = at(mbx as i32 - 1, mby as i32 - 1, 15);
    MvGrid { cells }
}

pub(crate) fn mid3(a: i16, b: i16, c: i16) -> i16 {
    let mx = a.max(b).max(c);
    let mn = a.min(b).min(c);
    a + b + c - mx - mn
}

/// 通用 MV 预测（FFmpeg pred_motion 决策树逐字；无参考缩放——与
/// oracle 行为一致，见 FFmpeg 源：match_count 选择 + 兜底中值）。
pub(crate) fn pred_motion(g: &MvGrid, bx: i32, by: i32, pw: i32, ref_idx: i8) -> [i16; 2] {
    let a = g.get(bx - 1, by);
    let b = g.get(bx, by - 1);
    let tr = g.get(bx + pw, by - 1);
    let c = if tr.ref_idx != PART_NOT_AVAILABLE {
        tr
    } else {
        g.get(bx - 1, by - 1)
    };
    let mc = usize::from(c.ref_idx == ref_idx)
        + usize::from(b.ref_idx == ref_idx)
        + usize::from(a.ref_idx == ref_idx);
    if mc > 1 {
        [
            mid3(a.mv[0], b.mv[0], c.mv[0]),
            mid3(a.mv[1], b.mv[1], c.mv[1]),
        ]
    } else if mc == 1 {
        if a.ref_idx == ref_idx {
            a.mv
        } else if b.ref_idx == ref_idx {
            b.mv
        } else {
            c.mv
        }
    } else if b.ref_idx == PART_NOT_AVAILABLE
        && c.ref_idx == PART_NOT_AVAILABLE
        && a.ref_idx != PART_NOT_AVAILABLE
    {
        a.mv
    } else {
        [
            mid3(a.mv[0], b.mv[0], c.mv[0]),
            mid3(a.mv[1], b.mv[1], c.mv[1]),
        ]
    }
}

/// P_16x8 分区预测（FFmpeg pred_16x8_motion 逐字：先查同参考的
/// 定向邻块，未命中走 pred_motion）。`by` = 分区首行（0 或 2）。
pub(crate) fn pred_16x8(g: &MvGrid, by: i32, ref_idx: i8) -> [i16; 2] {
    if by == 0 {
        let b = g.get(0, -1);
        if b.ref_idx == ref_idx {
            return b.mv;
        }
    } else {
        let a = g.get(-1, 2);
        if a.ref_idx == ref_idx {
            return a.mv;
        }
    }
    pred_motion(g, 0, by, 4, ref_idx)
}

/// P_8x16 分区预测（FFmpeg pred_8x16_motion 逐字）。`bx` = 0 或 2。
/// n=1 捷径的 C 候选按 spec 8.4.1.3：右上不可用时以左上 D 顶替
/// （FFmpeg `fetch_diagonal_mv` 的 D 回退），参考帧匹配即取 mvC——
/// 不落入中值分支。2026-10-27 对拍修复：此前缺 D 回退，右缘 MB 的
/// n=1 分支落入中值，mvp 偏 1 qpel（§3.1 向量首差帧 36 MB(19,13)）。
pub(crate) fn pred_8x16(g: &MvGrid, bx: i32, ref_idx: i8) -> [i16; 2] {
    if bx == 0 {
        let a = g.get(-1, 0);
        if a.ref_idx == ref_idx {
            return a.mv;
        }
    } else {
        let tr = g.get(4, -1);
        let c = if tr.ref_idx != PART_NOT_AVAILABLE {
            tr
        } else {
            g.get(bx - 1, -1)
        };
        if c.ref_idx == ref_idx {
            return c.mv;
        }
    }
    pred_motion(g, bx, 0, 2, ref_idx)
}

/// P_Skip MV 预测（FFmpeg pred_pskip_motion 逐字：A=左 (3,0)、
/// B=上 (0,3)、C=右上 (0,3) 否则左上 (3,3)；任一邻块 ref0+mv0 → 零向量）。
fn pred_pskip(pic: &Picture, mbx: usize, mby: usize) -> [i16; 2] {
    let inb = |x: i32, y: i32| -> bool {
        x >= 0 && y >= 0 && (x as usize) < pic.mb_width && (y as usize) < pic.mb_height
    };
    let at = |x: i32, y: i32, b: usize| -> MvNb {
        if inb(x, y) {
            mv_entry(&pic.mbs[y as usize * pic.mb_width + x as usize], b)
        } else {
            MvNb::PART
        }
    };
    let zero = [0i16; 2];
    // 左：不可解码 → 零向量；intra → UNUSED/0
    let a = {
        let e = at(mbx as i32 - 1, mby as i32, 3);
        if e.ref_idx == PART_NOT_AVAILABLE {
            return zero;
        }
        if e.ref_idx == 0 && e.mv == zero {
            return zero;
        }
        e
    };
    let b = {
        let e = at(mbx as i32, mby as i32 - 1, 12);
        if e.ref_idx == PART_NOT_AVAILABLE {
            return zero;
        }
        if e.ref_idx == 0 && e.mv == zero {
            return zero;
        }
        e
    };
    let mut c = at(mbx as i32 + 1, mby as i32 - 1, 12);
    if c.ref_idx == PART_NOT_AVAILABLE {
        c = at(mbx as i32 - 1, mby as i32 - 1, 15);
    }
    let mc =
        usize::from(c.ref_idx == 0) + usize::from(b.ref_idx == 0) + usize::from(a.ref_idx == 0);
    if mc > 1 {
        [
            mid3(a.mv[0], b.mv[0], c.mv[0]),
            mid3(a.mv[1], b.mv[1], c.mv[1]),
        ]
    } else if mc == 1 {
        if a.ref_idx == 0 {
            a.mv
        } else if b.ref_idx == 0 {
            b.mv
        } else {
            c.mv
        }
    } else {
        [
            mid3(a.mv[0], b.mv[0], c.mv[0]),
            mid3(a.mv[1], b.mv[1], c.mv[1]),
        ]
    }
}

/// MC 一个分区到 pic 平面（luma + chroma；`(bx, by)` 为块锚点，
/// 尺寸 `w × h` 个 4x4 块；`mv` 为 QPEL）。坐标组不可收拢（各为
/// 独立语义轴），加 allow。
#[allow(clippy::too_many_arguments)]
pub(crate) fn mc_partition(
    pic: &mut Picture,
    refp: &Picture,
    mbx: usize,
    mby: usize,
    bx: i32,
    by: i32,
    w: i32,
    h: i32,
    mv: [i16; 2],
) -> Result<(), H264Error> {
    let ls = pic.luma_stride();
    let cs = pic.chroma_stride();
    let lx = (mbx as i32 * 16 + bx * 4) as usize;
    let ly = (mby as i32 * 16 + by * 4) as usize;
    let qx = lx as i64 * 4 + i64::from(mv[0]);
    let qy = ly as i64 * 4 + i64::from(mv[1]);
    let (wpx, hpx) = ((w * 4) as usize, (h * 4) as usize);
    let base = ly * ls + lx;
    mc::mc_luma_put(
        &mut pic.luma[base..],
        ls,
        &refp.luma,
        refp.luma_stride(),
        refp.mb_width * 16,
        refp.mb_height * 16,
        qx,
        qy,
        wpx,
        hpx,
    );
    let cx = lx / 2;
    let cy = ly / 2;
    let cbase = cy * cs + cx;
    mc::mc_chroma_put(
        &mut pic.cb[cbase..],
        cs,
        &refp.cb,
        refp.chroma_stride(),
        refp.mb_width * 8,
        refp.mb_height * 8,
        qx,
        qy,
        wpx / 2,
        hpx / 2,
    );
    mc::mc_chroma_put(
        &mut pic.cr[cbase..],
        cs,
        &refp.cr,
        refp.chroma_stride(),
        refp.mb_width * 8,
        refp.mb_height * 8,
        qx,
        qy,
        wpx / 2,
        hpx / 2,
    );
    Ok(())
}

/// 解码一个 P_Skip 宏块（不读位流：mv = mvp、ref = 0、无残差）。
pub(crate) fn decode_skip_mb(
    pic: &mut Picture,
    mbx: usize,
    mby: usize,
    pctx: &InterCtx<'_>,
    slice_id: u16,
    qp: u8,
) -> Result<(), H264Error> {
    if pctx.l0.is_empty() {
        return Err(H264Error::InvalidData("P_Skip without reference".into()));
    }
    let mv = pred_pskip(pic, mbx, mby);
    let refp = pctx.l0[0].clone();
    mc_partition(pic, &refp, mbx, mby, 0, 0, 4, 4, mv)?;
    let mb_xy = mby * pic.mb_width + mbx;
    let mut mb = Macroblock {
        decoded: false,
        slice_id,
        kind: MbKind::InterP,
        qp,
        skipped: true,
        mv: [mv; 16],
        ref_idx: [0; 16],
        ..Macroblock::default()
    };
    mb.cbp = 0;
    mb.decoded = true;
    pic.mbs[mb_xy] = mb;
    Ok(())
}

/// 读 ref_idx_l0（te(v)：FFmpeg 语义——1 个参考不读、2 个读 1 位
/// 取反、否则 ue 并校验 < max）。
pub(crate) fn read_ref_idx(r: &mut BitReader<'_>, max: usize) -> Result<usize, H264Error> {
    if max == 1 {
        return Ok(0);
    }
    if max == 2 {
        return Ok(usize::from(!r.u1()?));
    }
    let v = r.ue()?;
    if v >= max as u64 {
        return Err(H264Error::InvalidData(format!(
            "ref_idx_l0 {v} overflow (max {max})"
        )));
    }
    Ok(v as usize)
}

pub(crate) fn mvd_add(r: &mut BitReader<'_>, m: i16) -> Result<i16, H264Error> {
    let d = r.se()?;
    let v = i32::from(m) + (d as i32);
    i16::try_from(v).map_err(|_| H264Error::InvalidData(format!("mv {v} out of range")))
}

/// inter cbp 的 me(v) 48 项映射（FFmpeg `ff_h264_golomb_to_inter_cbp` 逐字）。
pub(crate) const GOLOMB_TO_INTER_CBP: [u8; 48] = [
    0, 16, 1, 2, 4, 8, 32, 3, 5, 10, 12, 15, 47, 7, 11, 13, 14, 6, 9, 31, 35, 37, 42, 44, 33, 34,
    36, 40, 39, 43, 45, 46, 17, 18, 20, 24, 19, 21, 26, 28, 23, 27, 29, 30, 22, 25, 38, 41,
];

/// 解码一个 P 片帧间宏块（mb_type 0..4）。解析顺序按 spec 7.3.5 +
/// FFmpeg `ff_h264_decode_mb_cavlc` 逐字：
/// 16x16/16x8/8x16：先 ref_idx（te）后 mvd（se，x/y 成对，按分区序；
/// 16x8/8x16 两分区的 ref 都在 mvd 之前读）；P_8x8(含 ref0)：4 个
/// sub_mb_type → 各 8x8 的 ref_idx → 各子分区 mvd。
#[allow(clippy::too_many_arguments)]
fn decode_inter_mb(
    r: &mut BitReader<'_>,
    pic: &mut Picture,
    mbx: usize,
    mby: usize,
    pps: &Pps,
    slice_id: u16,
    qp: &mut u8,
    pctx: &InterCtx<'_>,
    mb_type: u64,
) -> Result<(), H264Error> {
    let mb_xy = mby * pic.mb_width + mbx;
    // nnz 邻块（与 intra 路径同源；constrained 不影响 nnz 预测——
    // FFmpeg 仅对样本预测施加 type_mask）
    let left_mb = if mbx > 0 && pic.mbs[mb_xy - 1].decoded {
        Some(&pic.mbs[mb_xy - 1])
    } else {
        None
    };
    let top_mb = if mby > 0 && pic.mbs[mb_xy - pic.mb_width].decoded {
        Some(&pic.mbs[mb_xy - pic.mb_width])
    } else {
        None
    };
    let num_ref = pctx.l0.len().max(1);
    let luma_left_nnz = |b: usize| left_mb.map(|m| m.nnz[b]);
    let luma_top_nnz = |b: usize| top_mb.map(|m| m.nnz[b]);
    let mut grid = build_mv_grid(pic, mbx, mby);
    // 分区列表（块锚点 bx/by、块宽高、mv、ref）——解析完统一 MC
    let mut parts: Vec<(i32, i32, i32, i32, [i16; 2], i8)> = Vec::new();

    if mb_type < 3 {
        // 16x16 / 16x8 / 8x16
        let n_parts = if mb_type == 0 { 1 } else { 2 };
        let mut refs = [0usize; 2];
        for r_slot in refs.iter_mut().take(n_parts) {
            *r_slot = read_ref_idx(r, num_ref)?;
        }
        for (i, r0) in refs.iter().enumerate().take(n_parts) {
            let ri = *r0 as i8;
            let (bx, by, _pw) = match mb_type {
                0 => (0, 0, 4),
                1 => (0, (i * 2) as i32, 4), // 16x8
                _ => ((i * 2) as i32, 0, 2), // 8x16
            };
            let mvp = match mb_type {
                0 => pred_motion(&grid, 0, 0, 4, ri),
                1 => pred_16x8(&grid, by, ri),
                _ => pred_8x16(&grid, bx, ri),
            };
            let mv = [mvd_add(r, mvp[0])?, mvd_add(r, mvp[1])?];
            let (w, h) = if mb_type == 1 {
                (4, 2)
            } else if mb_type == 0 {
                (4, 4)
            } else {
                (2, 4)
            };
            grid.set_rect(bx, by, w, h, mv, ri);
            parts.push((bx, by, w, h, mv, ri));
        }
    } else {
        // P_8x8（mb_type 3）与 P_8x8ref0（4：ref 恒 0 不读）
        let ref0 = mb_type == 4;
        // 4 个 sub_mb_type：0=8x8 1=8x4 2=4x8 3=4x4
        let mut sub = [0u8; 4];
        for s_slot in sub.iter_mut() {
            let t = r.ue()?;
            if t > 3 {
                return Err(H264Error::InvalidData(format!(
                    "P sub_mb_type {t} out of range"
                )));
            }
            *s_slot = t as u8;
        }
        let mut refs = [0usize; 4];
        for (i8x8, r_slot) in refs.iter_mut().enumerate() {
            *r_slot = if ref0 { 0 } else { read_ref_idx(r, num_ref)? };
            let _ = i8x8;
        }
        for i8x8 in 0..4usize {
            let x0 = ((i8x8 & 1) * 2) as i32;
            let y0 = ((i8x8 >> 1) * 2) as i32;
            let ri = refs[i8x8] as i8;
            let (count, bw) = match sub[i8x8] {
                0 => (1usize, 2i32), // 8x8：一子分区，宽 2 块
                1 => (2, 2),         // 8x4：两子分区（纵向堆叠），宽 2
                2 => (2, 1),         // 4x8：两子分区（横向并列），宽 1
                _ => (4, 1),         // 4x4
            };
            for j in 0..count {
                // 子分区锚点：bw==2 → 纵向 (x0, y0+j)；bw==1 → 横向 (x0+j, y0)
                let (bx, by, w, h) = match sub[i8x8] {
                    0 => (x0, y0, 2, 2),
                    1 => (x0, y0 + j as i32, 2, 1),
                    2 => (x0 + j as i32, y0, 1, 2),
                    _ => (x0 + (j & 1) as i32, y0 + (j >> 1) as i32, 1, 1),
                };
                let mvp = pred_motion(&grid, bx, by, bw.max(w), ri);
                let mv = [mvd_add(r, mvp[0])?, mvd_add(r, mvp[1])?];
                grid.set_rect(bx, by, w, h, mv, ri);
                parts.push((bx, by, w, h, mv, ri));
            }
        }
    }

    // ---- 残差（cbp inter 表；与 intra 相同的 CAVLC 路径）----
    let cbp_v = r.ue()?;
    if cbp_v > 47 {
        return Err(H264Error::InvalidData(format!(
            "coded_block_pattern {cbp_v} out of range"
        )));
    }
    let mut mb = Macroblock {
        decoded: false,
        slice_id,
        kind: MbKind::InterP,
        cbp: GOLOMB_TO_INTER_CBP[cbp_v as usize],
        ..Macroblock::default()
    };
    if mb.cbp > 0 {
        apply_qp_delta(r, qp)?;
    }
    let mut luma_blocks = [[0i16; 16]; 16];
    let mut chroma_blocks = [[[0i16; 16]; 4]; 2];
    let qmul = dequant4_coeff(*qp);
    for &b in Z_ORDER.iter() {
        let quadrant = 2 * (b >> 3) + ((b & 3) >> 1);
        if mb.cbp & (1 << quadrant) != 0 {
            let ln = if b.is_multiple_of(4) {
                luma_left_nnz(b + 3)
            } else {
                None
            };
            let tn = if b / 4 == 0 {
                luma_top_nnz(b + 12)
            } else {
                None
            };
            let nc = nc_luma(&mb.nnz, ln, tn, b);
            mb.nnz[b] = cavlc::read_residual(
                r,
                BlockKind::LumaAc,
                nc,
                &ZIGZAG,
                &qmul,
                &mut luma_blocks[b],
            )?;
        }
    }
    let qpc = CHROMA_QP[(i32::from(*qp) + pps.chroma_qp_index_offset).clamp(0, 51) as usize];
    let cqmul = dequant4_coeff(qpc);
    if mb.cbp & 0x30 != 0 {
        for (plane, cblocks) in chroma_blocks.iter_mut().enumerate() {
            let nc = nc_chroma_dc(
                left_mb.map(|m| m.nnz_chroma_dc[plane]),
                top_mb.map(|m| m.nnz_chroma_dc[plane]),
            );
            let mut buf = [0i16; 16];
            let tc = cavlc::read_residual(
                r,
                BlockKind::ChromaDc,
                nc,
                &CHROMA_DC_SCAN,
                &[0; 16],
                &mut buf,
            )?;
            mb.nnz_chroma_dc[plane] = tc;
            let x = [buf[0], buf[1], buf[4], buf[5]];
            let dcs = chroma_dc_dequant(&x, dc_qmul(qpc))?;
            for (cblk, &dc) in cblocks.iter_mut().zip(dcs.iter()) {
                cblk[0] = dc;
            }
        }
    }
    if mb.cbp & 0x20 != 0 {
        for (plane, cblocks) in chroma_blocks.iter_mut().enumerate() {
            for (i, cblk) in cblocks.iter_mut().enumerate() {
                let base = 16 + plane * 4;
                let left_nz = if i % 2 == 0 {
                    left_mb.map(|m| m.nnz[base + i + 1])
                } else {
                    None
                };
                let top_nz = if i / 2 == 0 {
                    top_mb.map(|m| m.nnz[base + i + 2])
                } else {
                    None
                };
                let nc = nc_chroma_ac(&mb.nnz, left_nz, top_nz, plane, i);
                mb.nnz[base + i] =
                    cavlc::read_residual(r, BlockKind::ChromaAc, nc, &ZIGZAG[1..], &cqmul, cblk)?;
            }
        }
    }

    // ---- 重建：MC 全宏块 + 残差叠加 ----
    for &(bx, by, w, h, mv, ri) in &parts {
        let refp = pctx
            .l0
            .get(ri as usize)
            .ok_or_else(|| H264Error::InvalidData(format!("ref_idx {ri} out of range")))?
            .clone();
        mc_partition(pic, &refp, mbx, mby, bx, by, w, h, mv)?;
    }
    let ys = pic.luma_stride();
    for &b in Z_ORDER.iter() {
        add_block(
            &mut pic.luma,
            ys,
            mby * 16 + (b / 4) * 4,
            mbx * 16 + (b % 4) * 4,
            &mut luma_blocks[b],
        );
    }
    let cs = pic.chroma_stride();
    for (plane, data) in [&mut pic.cb, &mut pic.cr].iter_mut().enumerate() {
        for (i, cblk) in chroma_blocks[plane].iter_mut().enumerate() {
            add_block(data, cs, mby * 8 + (i / 2) * 4, mbx * 8 + (i % 2) * 4, cblk);
        }
    }

    // ---- 存储 mv/ref（冻结面；供后续宏块 mvp 与 ②c deblock）----
    for (bx, by, w, h, mv, ri) in parts {
        for yy in by..by + h {
            for xx in bx..bx + w {
                let b = (yy * 4 + xx) as usize;
                mb.mv[b] = mv;
                mb.ref_idx[b] = ri;
            }
        }
    }
    mb.qp = *qp;
    mb.decoded = true;
    pic.mbs[mb_xy] = mb;
    Ok(())
}

/// 解码一个宏块（解析 + 重建），成功后写入 `pic.mbs[mb_xy]` 并标记 decoded。
///
/// `qp` 为跨宏块传递的当前 QP（mb_qp_delta 修改；PCM 不改——FFmpeg
/// 早退语义）。
///
/// # Errors
/// - `InvalidData`：位流畸形（mb_type/cbp/qp_delta 越界、模式可用性冲突等）；
/// - `Unsupported`：合法但当前实现拒绝的形态（如 (0,0) 对角模式缺 topleft）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_macroblock(
    r: &mut BitReader<'_>,
    pic: &mut Picture,
    mbx: usize,
    mby: usize,
    pps: &Pps,
    slice_id: u16,
    qp: &mut u8,
    pctx: Option<&InterCtx<'_>>,
) -> Result<(), H264Error> {
    let mb_xy = mby * pic.mb_width + mbx;
    let constrained = pctx.map(|c| c.constrained).unwrap_or(false);
    let avail = neighbor_avail(pic, mbx, mby, slice_id, constrained);

    // 邻块 nnz 查询（可用时取值，不可用 → None）
    let left_mb = if avail.left {
        Some(&pic.mbs[mb_xy - 1])
    } else {
        None
    };
    let top_mb = if avail.top {
        Some(&pic.mbs[mb_xy - pic.mb_width])
    } else {
        None
    };
    let luma_left_nnz = |b: usize| left_mb.map(|m| m.nnz[b]);
    let luma_top_nnz = |b: usize| top_mb.map(|m| m.nnz[b]);

    let mut mb_type = r.ue()?;
    // P 片：mb_type 0..4 帧间；5..29 intra（−5 后走 intra 语义）；
    // 30 = I_PCM；I 片：0..25 原语义
    let is_p = pctx.is_some();
    if is_p {
        if mb_type > 30 {
            return Err(H264Error::InvalidData(format!(
                "p-slice mb_type {mb_type} out of range"
            )));
        }
        if mb_type < 5 {
            return decode_inter_mb(r, pic, mbx, mby, pps, slice_id, qp, pctx.unwrap(), mb_type);
        }
        mb_type -= 5;
    } else if mb_type > 25 {
        return Err(H264Error::InvalidData(format!(
            "i-slice mb_type {mb_type} out of range"
        )));
    }

    let mut mb = Macroblock {
        decoded: false,
        slice_id,
        ..Macroblock::default()
    };
    let mut luma_blocks = [[0i16; 16]; 16];
    let mut chroma_blocks = [[[0i16; 16]; 4]; 2];

    if mb_type == 25 {
        // ---- I_PCM：字节对齐后 384 字节样值直写（FFmpeg：nnz 全 16、qp=0）----
        let bit = r.bit_position() % 8;
        if bit != 0 {
            r.u((8 - bit) as u32)?;
        }
        if r.remaining_bits() < 384 * 8 {
            return Err(H264Error::InvalidData("i_pcm: truncated samples".into()));
        }
        {
            let ys = pic.luma_stride();
            for y in 0..16usize {
                for x in 0..16usize {
                    let v = r.u(8)? as u8;
                    pic.luma[(mby * 16 + y) * ys + mbx * 16 + x] = v;
                }
            }
        }
        let pcs = pic.chroma_stride();
        for data in [&mut pic.cb, &mut pic.cr] {
            for y in 0..8usize {
                for x in 0..8usize {
                    let v = r.u(8)? as u8;
                    data[(mby * 8 + y) * pcs + mbx * 8 + x] = v;
                }
            }
        }
        mb.kind = MbKind::Pcm;
        mb.qp = 0;
        mb.cbp = 0x2F;
        mb.luma_dc_coded = true;
        mb.nnz = [16; 24];
        mb.nnz_chroma_dc = [16; 2];
        mb.decoded = true;
        pic.mbs[mb_xy] = mb;
        return Ok(());
    }

    if mb_type == 0 {
        // ---- I_NxN：mb_pred = 16 个 4x4 模式（z 序解析）+ chroma 模式 ----
        let mut modes = [0u8; 16]; // 光栅
        for &b in Z_ORDER.iter() {
            let (cx, cy) = (b % 4, b / 4);
            let left = if cx > 0 {
                Some(modes[b - 1])
            } else {
                left_mb.map(|m| neighbor_i4_mode(m, 3 + cy * 4))
            };
            let top = if cy > 0 {
                Some(modes[b - 4])
            } else {
                top_mb.map(|m| neighbor_i4_mode(m, cx + 12))
            };
            let prev = prev_i4_mode(left, top);
            let mut mode = prev;
            if !r.u1()? {
                let rem = r.u(3)? as u8;
                mode = rem + u8::from(rem >= prev);
            }
            modes[b] = mode;
        }
        mb.i4_syntax = modes;
        rewrite_4x4_modes(&mut modes, avail.top, avail.left, avail.topleft)?;
        mb.i4_modes = modes;
        let chroma_mode = r.ue()? as u8;
        mb.chroma_syntax = chroma_mode;
        mb.chroma_mode = rewrite_dc_mode(chroma_mode, avail.top, avail.left, avail.topleft, 0)?;

        // coded_block_pattern me(v)
        let cbp_v = r.ue()?;
        if cbp_v > 47 {
            return Err(H264Error::InvalidData(format!(
                "coded_block_pattern {cbp_v} out of range"
            )));
        }
        mb.cbp = GOLOMB_TO_INTRA4X4_CBP[cbp_v as usize];
        mb.kind = MbKind::Intra4x4;
        if mb.cbp > 0 {
            apply_qp_delta(r, qp)?;
        }
        // 残差：luma 逐象限（z 序）；象限 q = 2·(块行/2) + 块列/2
        let qmul = dequant4_coeff(*qp);
        for &b in Z_ORDER.iter() {
            let quadrant = 2 * (b >> 3) + ((b & 3) >> 1);
            if mb.cbp & (1 << quadrant) != 0 {
                let ln = if b.is_multiple_of(4) {
                    luma_left_nnz(b + 3)
                } else {
                    None
                };
                let tn = if b / 4 == 0 {
                    luma_top_nnz(b + 12)
                } else {
                    None
                };
                let nc = nc_luma(&mb.nnz, ln, tn, b);
                mb.nnz[b] = cavlc::read_residual(
                    r,
                    BlockKind::LumaAc,
                    nc,
                    &ZIGZAG,
                    &qmul,
                    &mut luma_blocks[b],
                )?;
            }
        }
    } else {
        // ---- I16x16 ----
        let k = (mb_type - 1) as usize;
        mb.i16_mode = rewrite_dc_mode(I16_PRED[k & 3], avail.top, avail.left, avail.topleft, 2)?;
        mb.cbp = I16_CBP[k >> 2];
        mb.kind = MbKind::Intra16x16;
        let chroma_mode = r.ue()? as u8;
        mb.chroma_syntax = chroma_mode;
        mb.chroma_mode = rewrite_dc_mode(chroma_mode, avail.top, avail.left, avail.topleft, 0)?;
        // mb_qp_delta：I16 恒解析（spec：cbp>0 || Intra16x16）
        apply_qp_delta(r, qp)?;
        // 残差：luma DC 无条件（原始 level，全 zigzag；块内光栅 = z 序块号）
        let dcs = {
            let mut buf = [0i16; 16];
            let nc = nc_luma(&mb.nnz, luma_left_nnz(3), luma_top_nnz(12), 0);
            cavlc::read_residual(r, BlockKind::LumaDc, nc, &ZIGZAG, &[0; 16], &mut buf)?;
            luma_dc_dequant(&buf, dc_qmul(*qp))?
        };
        for b in 0..16usize {
            luma_blocks[b][0] = dcs[b];
        }
        // luma AC（z 序，scan 从 1 起）
        if mb.cbp & 0x0F != 0 {
            let qmul = dequant4_coeff(*qp);
            for &b in Z_ORDER.iter() {
                let ln = if b.is_multiple_of(4) {
                    luma_left_nnz(b + 3)
                } else {
                    None
                };
                let tn = if b / 4 == 0 {
                    luma_top_nnz(b + 12)
                } else {
                    None
                };
                let nc = nc_luma(&mb.nnz, ln, tn, b);
                mb.nnz[b] = cavlc::read_residual(
                    r,
                    BlockKind::LumaAc,
                    nc,
                    &ZIGZAG[1..],
                    &qmul,
                    &mut luma_blocks[b],
                )?;
            }
        }
    }

    // ---- chroma 残差（两种宏块共用；先 Cb/Cr DC，后 Cb/Cr AC）----
    let qpc = CHROMA_QP[(i32::from(*qp) + pps.chroma_qp_index_offset).clamp(0, 51) as usize];
    let cqmul = dequant4_coeff(qpc);
    if mb.cbp & 0x30 != 0 {
        for (plane, cblocks) in chroma_blocks.iter_mut().enumerate() {
            let nc = nc_chroma_dc(
                left_mb.map(|m| m.nnz_chroma_dc[plane]),
                top_mb.map(|m| m.nnz_chroma_dc[plane]),
            );
            let mut buf = [0i16; 16];
            let tc = cavlc::read_residual(
                r,
                BlockKind::ChromaDc,
                nc,
                &CHROMA_DC_SCAN,
                &[0; 16],
                &mut buf,
            )?;
            mb.nnz_chroma_dc[plane] = tc;
            let x = [buf[0], buf[1], buf[4], buf[5]];
            let dcs = chroma_dc_dequant(&x, dc_qmul(qpc))?;
            for (cblk, &dc) in cblocks.iter_mut().zip(dcs.iter()) {
                cblk[0] = dc;
            }
        }
    }
    if mb.cbp & 0x20 != 0 {
        for (plane, cblocks) in chroma_blocks.iter_mut().enumerate() {
            for (i, cblk) in cblocks.iter_mut().enumerate() {
                let base = 16 + plane * 4;
                // 邻块查值按块位置惰性求值：cx==0 才有左邻块、cy==0 才有
                // 上邻块（i%2 / i/2），否则 Option::None（防止越界索引）
                let left_nz = if i % 2 == 0 {
                    left_mb.map(|m| m.nnz[base + i + 1])
                } else {
                    None
                };
                let top_nz = if i / 2 == 0 {
                    top_mb.map(|m| m.nnz[base + i + 2])
                } else {
                    None
                };
                let nc = nc_chroma_ac(&mb.nnz, left_nz, top_nz, plane, i);
                mb.nnz[base + i] =
                    cavlc::read_residual(r, BlockKind::ChromaAc, nc, &ZIGZAG[1..], &cqmul, cblk)?;
            }
        }
    }

    // ---- 重建：luma ----
    match mb.kind {
        MbKind::Intra4x4 => {
            for &b in Z_ORDER.iter() {
                reconstruct_4x4(
                    pic,
                    mbx,
                    mby,
                    b,
                    &avail,
                    mb.i4_modes[b],
                    &mut luma_blocks[b],
                );
            }
        }
        MbKind::Intra16x16 => {
            let (top, left, tl) = gather_16x16_edges(pic, mbx, mby, &avail);
            let ys = pic.luma_stride();
            let base = mby * 16 * ys + mbx * 16;
            pred_16x16(mb.i16_mode, &top, &left, tl, &mut pic.luma[base..], ys);
            for &b in Z_ORDER.iter() {
                add_block(
                    &mut pic.luma,
                    ys,
                    mby * 16 + (b / 4) * 4,
                    mbx * 16 + (b % 4) * 4,
                    &mut luma_blocks[b],
                );
            }
        }
        MbKind::Pcm | MbKind::InterP | MbKind::InterB => return Ok(()), // PCM 已早退；inter 不走此路径
    }

    // ---- 重建：chroma（Cb、Cr）----
    let cs = pic.chroma_stride();
    for (plane, data) in [&mut pic.cb, &mut pic.cr].iter_mut().enumerate() {
        let (top, left, tl) = gather_chroma_edges(data, cs, mbx, mby, &avail);
        let base = mby * 8 * cs + mbx * 8;
        pred_8x8c(mb.chroma_mode, &top, &left, tl, &mut data[base..], cs);
        for (i, cblk) in chroma_blocks[plane].iter_mut().enumerate() {
            add_block(data, cs, mby * 8 + (i / 2) * 4, mbx * 8 + (i % 2) * 4, cblk);
        }
    }

    mb.qp = *qp;
    mb.decoded = true;
    pic.mbs[mb_xy] = mb;
    Ok(())
}

/// mb_qp_delta（FFmpeg h264_cavlc.c 逐字：越界 ±52 回绕一次，再越界 → Err）。
fn apply_qp_delta(r: &mut BitReader<'_>, qp: &mut u8) -> Result<(), H264Error> {
    let d = r.se()?;
    let Ok(d) = i8::try_from(d) else {
        return Err(H264Error::InvalidData(format!(
            "mb_qp_delta {d} out of range"
        )));
    };
    let mut q = i32::from(*qp) + i32::from(d);
    if !(0..=51).contains(&q) {
        q = if q < 0 { q + 52 } else { q - 52 };
        if !(0..=51).contains(&q) {
            return Err(H264Error::InvalidData(format!(
                "mb_qp_delta {d} leaves qp out of range"
            )));
        }
    }
    *qp = q as u8;
    Ok(())
}

/// 4x4 块边沿收集（E..H 规则 = FFmpeg h264_mb.c
/// hl_decode_mb_predict_luma 的 topright 语义）：
/// - 块列 <3：读 (4cx+4..4cx+7, 4cy−1)——象限内已重建区（z 序保证
///   `z_of(cx+1, cy−1) < z`，否则复制 D）或（cy=0）顶行；
/// - 光栅块 (3,0)（=z5）：右上 E 宏块底行，不可用 → 复制 D；
/// - 其余右列块（z7/z13/z15 等）：E..H 无真实来源（FFmpeg 在此读
///   未解码区，合法编码器不会对这些块发 DDL/VL）→ 复制 D。
pub(crate) fn gather_edges4(
    pic: &Picture,
    mbx: usize,
    mby: usize,
    b: usize,
    avail: &NeighAvail,
) -> Edges4 {
    let (cx, cy) = (b % 4, b / 4);
    let px = mbx * 16 + cx * 4;
    let py = mby * 16 + cy * 4;
    let s = pic.luma_stride();
    let at = |x: usize, y: usize| pic.luma[y * s + x];

    let mut top = [0u8; 8];
    if cy > 0 || mby > 0 {
        for (i, slot) in top.iter_mut().enumerate().take(4) {
            *slot = at(px + i, py - 1);
        }
    }
    let row_above_ok = cy > 0 || mby > 0; // py-1 有效（模式改写保证需要时不为 0）
    let can_read_er = row_above_ok && cx < 3 && (cy == 0 || z_of(cx + 1, cy - 1) < z_of(cx, cy));
    if can_read_er {
        for (i, slot) in top.iter_mut().enumerate().skip(4) {
            *slot = at(px + i, py - 1);
        }
    } else if b == 3 && avail.er {
        // 光栅 (3,0)=z5：右上 E 宏块底行
        let ex = (mbx + 1) * 16;
        let ey = mby * 16 - 1;
        for i in 0..4 {
            top[4 + i] = at(ex + i, ey);
        }
    } else {
        for i in 0..4 {
            top[4 + i] = top[3];
        }
    }
    let mut left = [0u8; 4];
    if cx > 0 || mbx > 0 {
        for (i, slot) in left.iter_mut().enumerate() {
            *slot = at(px - 1, py + i);
        }
    }
    let tl = if (cx > 0 || mbx > 0) && (cy > 0 || mby > 0) {
        at(px - 1, py - 1)
    } else {
        0
    };
    Edges4 { top, left, tl }
}

/// 4x4 块：预测 + 残差叠加（内容等价于 FFmpeg 的 nnz 判定：
/// 仅 DC 非零 → dc_add；全零 → 不加；否则 idct_add）。
pub(crate) fn reconstruct_4x4(
    pic: &mut Picture,
    mbx: usize,
    mby: usize,
    b: usize,
    avail: &NeighAvail,
    mode: u8,
    block: &mut [i16; 16],
) {
    let edges = gather_edges4(pic, mbx, mby, b, avail);
    let s = pic.luma_stride();
    let (cx, cy) = (b % 4, b / 4);
    let base = (mby * 16 + cy * 4) * s + mbx * 16 + cx * 4;
    let mut out = [0u8; 16];
    pred_4x4(mode, &edges, &mut out);
    for y in 0..4usize {
        for x in 0..4usize {
            pic.luma[base + y * s + x] = out[y * 4 + x];
        }
    }
    if block.iter().all(|&v| v == 0) {
        return;
    }
    if block[1..].iter().all(|&v| v == 0) {
        idct4x4_dc_add(&mut pic.luma[base..], s, block);
    } else {
        idct4x4_add(&mut pic.luma[base..], s, block);
    }
}

/// 残差叠加（16x16/chroma 路径）：按块内容选择——仅 DC 非零 →
/// dc_add；含任意 AC → 全 idct_add（数学上 dc_add ≡ idct_add 的仅 DC
/// 特例，按内容判定与 FFmpeg 的 nnz 判定在 I16/chroma（nnz 只计 AC）
/// 场景等价且不丢单 AC 系数）。
pub(crate) fn add_block(
    plane: &mut [u8],
    stride: usize,
    y0: usize,
    x0: usize,
    block: &mut [i16; 16],
) {
    let base = y0 * stride + x0;
    if block.iter().all(|&v| v == 0) {
        return;
    }
    if block[1..].iter().all(|&v| v == 0) {
        idct4x4_dc_add(&mut plane[base..], stride, block);
    } else {
        idct4x4_add(&mut plane[base..], stride, block);
    }
}

/// 16x16 边沿（顶行 + 左列，各 16；左上角；不可用侧填 0）。
pub(crate) fn gather_16x16_edges(
    pic: &Picture,
    mbx: usize,
    mby: usize,
    avail: &NeighAvail,
) -> ([u8; 16], [u8; 16], u8) {
    let s = pic.luma_stride();
    let (bx, by) = (mbx * 16, mby * 16);
    let mut top = [0u8; 16];
    if avail.top {
        for (i, slot) in top.iter_mut().enumerate() {
            *slot = pic.luma[(by - 1) * s + bx + i];
        }
    }
    let mut left = [0u8; 16];
    if avail.left {
        for (i, slot) in left.iter_mut().enumerate() {
            *slot = pic.luma[(by + i) * s + bx - 1];
        }
    }
    let tl = if avail.topleft {
        pic.luma[(by - 1) * s + bx - 1]
    } else {
        0
    };
    (top, left, tl)
}

/// chroma 边沿（顶行 + 左列，各 8；左上角；不可用侧填 0）。
pub(crate) fn gather_chroma_edges(
    plane: &[u8],
    stride: usize,
    mbx: usize,
    mby: usize,
    avail: &NeighAvail,
) -> ([u8; 8], [u8; 8], u8) {
    let (bx, by) = (mbx * 8, mby * 8);
    let mut top = [0u8; 8];
    if avail.top {
        for (i, slot) in top.iter_mut().enumerate() {
            *slot = plane[(by - 1) * stride + bx + i];
        }
    }
    let mut left = [0u8; 8];
    if avail.left {
        for (i, slot) in left.iter_mut().enumerate() {
            *slot = plane[(by + i) * stride + bx - 1];
        }
    }
    let tl = if avail.topleft {
        plane[(by - 1) * stride + bx - 1]
    } else {
        0
    };
    (top, left, tl)
}

// ---------------------------------------------------------------------------
// 单测：逐值对照（手算参考值）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 反量化 ----

    #[test]
    fn dequant4_coeff_class_layout() {
        // 表存 V·64·2^(qp/6)，class = (行&1)+(列&1)（FFmpeg h264_ps.c）：
        // qp=0 → class0=640、class1=832、class2=1024
        let q = dequant4_coeff(0);
        assert_eq!(q[0], 640);
        assert_eq!(q[1], 832);
        assert_eq!(q[4], 832);
        assert_eq!(q[2], 640); // (0,2)：偶行偶列 → class 0
        assert_eq!(q[8], 640); // (2,0)：偶行偶列 → class 0
        assert_eq!(q[6], 832); // (2,1)：奇列 → class 1
        assert_eq!(q[10], 640); // (2,2)：class 0
        assert_eq!(q[5], 1024); // (1,1)：class 2
        assert_eq!(q[15], 1024);
        // qp=6：整体 ×2
        let q = dequant4_coeff(6);
        assert_eq!(q[0], 1280);
        assert_eq!(q[5], 2048);
        // qp=12：×4
        assert_eq!(dequant4_coeff(12)[0], 2560);
        assert_eq!(dc_qmul(0), 640);
        assert_eq!(dc_qmul(12), 2560);
        // 反量化读路径自洽：level=1 qp=0 → (640+32)>>6 = 10
        assert_eq!((dequant4_coeff(0)[0] + 32) >> 6, 10);
        assert_eq!((dequant4_coeff(6)[0] + 32) >> 6, 20);
    }

    #[test]
    fn chroma_qp_table() {
        assert_eq!(CHROMA_QP[0], 0);
        assert_eq!(CHROMA_QP[29], 29);
        assert_eq!(CHROMA_QP[30], 29);
        assert_eq!(CHROMA_QP[33], 32);
        assert_eq!(CHROMA_QP[42], 37);
        assert_eq!(CHROMA_QP[51], 39);
    }

    // ---- 反变换 ----

    #[test]
    fn idct_dc_only_matches_dc_add() {
        let mut block = [0i16; 16];
        block[0] = 128;
        let mut dst = [100u8; 16];
        idct4x4_add(&mut dst, 4, &mut block);
        // dc = (128+32)>>6 = 2 → 全部 102
        assert!(dst.iter().all(|&v| v == 102));
        let mut dst2 = [100u8; 16];
        let mut b2 = [0i16; 16];
        b2[0] = 128;
        idct4x4_dc_add(&mut dst2, 4, &b2);
        assert_eq!(dst, dst2);
    }

    #[test]
    fn idct_single_ac_hand_computed() {
        // block[1]=256（AC）：pass1 后列 1 全 256、列 0 = 32；
        // pass2 各行加 [4,2,-2,-4]
        let mut block = [0i16; 16];
        block[1] = 256;
        let mut dst = [100u8; 16];
        idct4x4_add(&mut dst, 4, &mut block);
        for y in 0..4 {
            assert_eq!(dst[y * 4], 104, "row {y} col 0");
            assert_eq!(dst[y * 4 + 1], 102, "row {y} col 1");
            assert_eq!(dst[y * 4 + 2], 98, "row {y} col 2");
            assert_eq!(dst[y * 4 + 3], 96, "row {y} col 3");
        }
    }

    #[test]
    fn idct_clips() {
        let mut block = [0i16; 16];
        block[0] = 8000; // (8000+32)>>6=125 → 200+125 裁剪 255
        let mut dst = [200u8; 16];
        idct4x4_add(&mut dst, 4, &mut block);
        assert!(dst.iter().all(|&v| v == 255));
        let mut block2 = [0i16; 16];
        block2[0] = -8000;
        let mut dst2 = [10u8; 16];
        idct4x4_add(&mut dst2, 4, &mut block2);
        assert!(dst2.iter().all(|&v| v == 0));
    }

    #[test]
    fn luma_dc_dequant_flat_input() {
        // 全 1 输入：Hadamart 之和只有 (K=0,i=0) = 16 → 其余相消；
        // 输出 z 散射：z0 槽 = ((16·640+128)>>8) = 40，其余 0
        let x = [1i16; 16];
        let out = luma_dc_dequant(&x, 640).unwrap();
        assert_eq!(out[0], ((16i64 * 640 + 128) >> 8) as i16);
        assert!(out[1..].iter().all(|&v| v == 0));
    }

    #[test]
    fn luma_dc_dequant_matches_ffmpeg_pipeline() {
        // 实测回归（nodb.mp4 MB(1,1) 帧 0）：原始 DC level 与 qmul(7168)
        // 经 FFmpeg 语义产生的 z 序 DC 散射值（用独立 Python 参考实现
        // 对拍锁定），再按 Z_ORDER 落到光栅块
        let x: [i16; 16] = [-24, -12, 1, -6, 19, 3, -2, 1, -7, 5, 1, 1, 7, -5, -1, -1];
        let out = luma_dc_dequant(&x, 7168).unwrap();
        let expect: [i16; 16] = [
            -560, -224, 0, 224, -560, -224, 0, 224, -1736, -1288, -392, 168, -1736, -1736, -1736,
            -1176,
        ];
        assert_eq!(out, expect);
    }

    #[test]
    fn chroma_dc_dequant_flat_input() {
        let x = [1i16, 1, 1, 1];
        let out = chroma_dc_dequant(&x, 640).unwrap();
        assert_eq!(out[0], ((4i64 * 640) >> 7) as i16); // 20
        assert!(out[1..].iter().all(|&v| v == 0));
    }

    // ---- nC ----

    #[test]
    fn nc_sentinel_arithmetic() {
        assert_eq!(nc_of(4, 4), 4); // (4+4+1)>>1
        assert_eq!(nc_of(5, 0), 3); // (5+0+1)>>1
        assert_eq!(nc_of(16, 64), 16); // 单侧 → 该值
        assert_eq!(nc_of(64, 7), 7);
        assert_eq!(nc_of(64, 64), 0); // 全无 → 0
        assert_eq!(nc_of(16, 16), 16); // PCM 邻块
    }

    #[test]
    fn nc_luma_uses_cur_for_in_mb() {
        let mut cur = [0u8; 24];
        cur[0] = 5; // 块 0 已解析 nnz=5
        // 块 4（(0,1)）的 top = cur[0]，left 不可用 → nC = 5
        assert_eq!(nc_luma(&cur, None, None, 4), 5);
        // 块 1 的 left = cur[0]，top 不可用 → 5
        assert_eq!(nc_luma(&cur, None, None, 1), 5);
        // 块 0 无 in-MB 邻居：全不可用 → 0
        assert_eq!(nc_luma(&cur, None, None, 0), 0);
        assert_eq!(nc_luma(&cur, Some(2), None, 0), 2); // 单侧 → 该值
    }

    // ---- 模式改写 ----

    #[test]
    fn rewrite_chroma_modes() {
        // chroma 编号：0=DC,1=H,2=V,3=Plane
        assert_eq!(rewrite_dc_mode(0, true, true, true, 0).unwrap(), 0);
        assert_eq!(
            rewrite_dc_mode(0, false, true, true, 0).unwrap(),
            P8_DC_LEFT
        );
        assert_eq!(rewrite_dc_mode(0, true, false, true, 0).unwrap(), P8_DC_TOP);
        assert_eq!(
            rewrite_dc_mode(0, false, false, true, 0).unwrap(),
            P8_DC_128
        );
        assert_eq!(rewrite_dc_mode(1, false, true, true, 0).unwrap(), 1); // H 不要顶
        assert!(rewrite_dc_mode(1, true, false, true, 0).is_err()); // H 缺左
        assert_eq!(rewrite_dc_mode(2, true, false, true, 0).unwrap(), 2); // V 不要左
        assert!(rewrite_dc_mode(2, false, true, true, 0).is_err());
        assert!(rewrite_dc_mode(3, false, true, true, 0).is_err());
        assert!(rewrite_dc_mode(3, true, false, true, 0).is_err());
        assert!(rewrite_dc_mode(4, true, true, true, 0).is_err()); // 越界
        assert!(rewrite_dc_mode(3, true, true, false, 0).is_err()); // plane 缺角点
    }

    #[test]
    fn rewrite_16x16_modes() {
        // 16x16 编号：0=V,1=H,2=DC,3=Plane
        assert_eq!(
            rewrite_dc_mode(2, false, true, true, 2).unwrap(),
            P8_DC_LEFT
        );
        assert_eq!(rewrite_dc_mode(2, true, false, true, 2).unwrap(), P8_DC_TOP);
        assert_eq!(
            rewrite_dc_mode(2, false, false, true, 2).unwrap(),
            P8_DC_128
        );
        assert!(rewrite_dc_mode(0, false, true, true, 2).is_err()); // V 缺顶
        assert_eq!(rewrite_dc_mode(0, true, false, true, 2).unwrap(), 0); // V 不要左
        assert!(rewrite_dc_mode(1, true, false, true, 2).is_err()); // H 缺左
        assert_eq!(rewrite_dc_mode(1, false, true, true, 2).unwrap(), 1);
        assert!(rewrite_dc_mode(3, true, true, false, 2).is_err()); // plane 缺角点
    }

    #[test]
    fn rewrite_4x4_tables() {
        // 顶不可用：V/DDL/DDR/VR/HD/VL → Err；H/HU 保留；DC → DC_LEFT
        let mut m = [1u8; 16];
        rewrite_4x4_modes(&mut m, false, true, true).unwrap();
        assert_eq!(m[0], 1);
        let mut m = [2u8; 16];
        rewrite_4x4_modes(&mut m, false, true, true).unwrap();
        assert_eq!(m[0], P4_DC_LEFT);
        assert_eq!(m[5], 2); // 非行 0/列 0 不改
        let mut m = [3u8; 16];
        assert!(rewrite_4x4_modes(&mut m, false, true, true).is_err());
        // 左不可用：H/HU/DDR/VR/HD → Err；V/DDL/VL 保留；DC → DC_TOP
        let mut m = [0u8; 16];
        rewrite_4x4_modes(&mut m, true, false, true).unwrap();
        assert_eq!(m[0], 0); // V(0) 不需要左，保留
        let mut m = [2u8; 16];
        rewrite_4x4_modes(&mut m, true, false, true).unwrap();
        assert_eq!(m[0], P4_DC_TOP);
        let mut m = [8u8; 16];
        assert!(rewrite_4x4_modes(&mut m, true, false, true).is_err());
        // 双侧不可用：DC → DC_LEFT → DC_128
        let mut m = [2u8; 16];
        rewrite_4x4_modes(&mut m, false, false, true).unwrap();
        assert_eq!(m[0], P4_DC_128);
        // (0,0) 对角模式（spec 4..6=DDR/VR/HD）+ topleft 不可用 → Unsupported
        let mut m = [4u8; 16];
        assert!(rewrite_4x4_modes(&mut m, true, true, false).is_err());
        // 变体码（9..11）不触发 topleft 检查
        let mut m = [2u8; 16];
        rewrite_4x4_modes(&mut m, false, true, false).unwrap();
        assert_eq!(m[0], P4_DC_LEFT); // 9，无需 topleft
    }

    // ---- z 序 ----

    #[test]
    fn z_order_roundtrip() {
        for (z, &b) in Z_ORDER.iter().enumerate() {
            assert_eq!(z_of(b % 4, b / 4), z);
        }
        // E..H 依赖序：cx<3 且 cy>0 时，(cx+1, cy-1) 先于本块（除 z3/z11）
        for b in 0..16usize {
            let (cx, cy) = (b % 4, b / 4);
            if cx < 3 && cy > 0 {
                let zz = z_of(cx, cy);
                let early = z_of(cx + 1, cy - 1) < zz;
                match zz {
                    3 | 11 => assert!(!early, "z{zz} 不应可读 E..H"),
                    _ => assert!(early, "z{zz} 应可读 E..H"),
                }
            }
        }
    }

    // ---- 帧内预测（手算对照）----

    fn edges(t: [u8; 8], l: [u8; 4], tl: u8) -> Edges4 {
        Edges4 {
            top: t,
            left: l,
            tl,
        }
    }

    #[test]
    fn pred_4x4_v_h_dc() {
        let e = edges([10, 20, 30, 40, 50, 60, 70, 80], [1, 2, 3, 4], 9);
        let mut out = [0u8; 16];
        pred_4x4(0, &e, &mut out);
        assert_eq!(&out[0..4], &[10, 20, 30, 40]);
        pred_4x4(1, &e, &mut out);
        assert_eq!(out[0], 1);
        assert_eq!(out[15], 4);
        pred_4x4(2, &e, &mut out);
        // (10+20+30+40+1+2+3+4+4)>>3 = 114>>3 = 14
        assert!(out.iter().all(|&v| v == 14));
        pred_4x4(P4_DC_LEFT, &e, &mut out);
        assert!(out.iter().all(|&v| v == (1 + 2 + 3 + 4 + 2) >> 2));
        pred_4x4(P4_DC_TOP, &e, &mut out);
        assert!(out.iter().all(|&v| v == (10 + 20 + 30 + 40 + 2) >> 2));
        pred_4x4(P4_DC_128, &e, &mut out);
        assert!(out.iter().all(|&v| v == 128));
    }

    #[test]
    fn pred_4x4_ddl_hand_computed() {
        let e = edges([10, 11, 12, 13, 14, 15, 16, 17], [0; 4], 0);
        let mut out = [0u8; 16];
        pred_4x4(3, &e, &mut out);
        assert_eq!(out[0], f2(10, 11, 12) as u8);
        assert_eq!(out[1], f2(11, 12, 13) as u8);
        assert_eq!(out[4], f2(11, 12, 13) as u8); // 反对角一致
        assert_eq!(out[15], f2(16, 17, 17) as u8); // t7 复制
        assert_eq!(out[3], f2(13, 14, 15) as u8);
        // 全矩阵反对角一致
        for y in 0..4 {
            for x in 0..4 {
                let d = x + y;
                let expect = if d == 6 {
                    f2(16, 17, 17) as u8
                } else {
                    f2(10 + d as i32, 11 + d as i32, 12 + d as i32) as u8
                };
                assert_eq!(out[y * 4 + x], expect, "({x},{y})");
            }
        }
    }

    #[test]
    fn pred_4x4_vl_and_hu() {
        let e = edges([0, 1, 2, 3, 4, 5, 6, 7], [10, 20, 30, 40], 99);
        let mut out = [0u8; 16];
        pred_4x4(7, &e, &mut out);
        assert_eq!(out[0], f1(0, 1) as u8);
        assert_eq!(out[1], f1(1, 2) as u8);
        assert_eq!(out[15], f2(4, 5, 6) as u8);
        pred_4x4(8, &e, &mut out);
        assert_eq!(out[0], f1(10, 20) as u8);
        assert_eq!(out[15], 40); // l3 复制
        assert_eq!(out[14], 40);
        assert_eq!(out[13], 40);
    }

    #[test]
    fn pred_16x16_and_chroma_dc_quadrants() {
        let top: [u8; 16] = core::array::from_fn(|i| (i + 1) as u8); // Σ=136
        let left: [u8; 16] = core::array::from_fn(|i| (i + 17) as u8); // Σ=244
        let mut out = [0u8; 256];
        pred_16x16(2, &top, &left, 0, &mut out, 16);
        assert!(out.iter().all(|&v| v == 17)); // (136+392+16)>>5 = 17
        pred_16x16(P8_DC_TOP, &top, &left, 0, &mut out, 16);
        assert!(out.iter().all(|&v| v == 9)); // (136+8)>>4
        pred_16x16(P8_DC_LEFT, &top, &left, 0, &mut out, 16);
        assert!(out.iter().all(|&v| v == 25)); // (392+8)>>4

        let ctop: [u8; 8] = core::array::from_fn(|i| (i + 1) as u8); // s0=10,s1=26
        let cleft: [u8; 8] = core::array::from_fn(|i| (i + 9) as u8); // s2=42,s3=58
        let mut cout = [0u8; 64];
        pred_8x8c(0, &ctop, &cleft, 0, &mut cout, 8);
        assert_eq!(cout[0], ((10 + 42 + 4) >> 3) as u8); // UL = 7
        assert_eq!(cout[3], ((26 + 2) >> 2) as u8); // UR = 7
        assert_eq!(cout[4 * 8], ((58 + 2) >> 2) as u8); // LL = 15
        assert_eq!(cout[36], ((26 + 58 + 4) >> 3) as u8); // LR = 11（行4列4）
        pred_8x8c(P8_DC_LEFT, &ctop, &cleft, 0, &mut cout, 8);
        assert_eq!(cout[0], ((42 + 2) >> 2) as u8);
        assert_eq!(cout[4 * 8], ((58 + 2) >> 2) as u8);
    }

    #[test]
    fn pred_chroma_plane_uses_corner() {
        // 顶/左平坦为 8、角点 0：H 含 (i+1)·(t[4+i]−corner) 项 → 非平坦
        let ctop = [8u8; 8];
        let cleft = [8u8; 8];
        let mut cout = [0u8; 64];
        pred_8x8c(3, &ctop, &cleft, 0, &mut cout, 8);
        // H = Σ(i+1)(8−0) = 10·8 = 80；b = (17·80+16)>>5 = 42；c 同
        // i00 = 16·(8+8) − 3·42 − 3·42 + 16 = 170；行 0：(170)>>5=5 …
        assert_eq!(cout[0], ((170i32) >> 5) as u8, "corner 应参与计算");
        assert_ne!(cout[0], cout[63]);
    }

    #[test]
    fn pred_plane_flat_inputs() {
        let ctop = [8u8; 8];
        let cleft = [8u8; 8];
        let mut cout = [0u8; 64];
        pred_8x8c(3, &ctop, &cleft, 8, &mut cout, 8);
        assert!(cout.iter().all(|&v| v == 8));

        let top = [9u8; 16];
        let left = [9u8; 16];
        let mut out = [0u8; 256];
        pred_16x16(3, &top, &left, 9, &mut out, 16);
        assert!(out.iter().all(|&v| v == 9));
    }

    #[test]
    fn golomb_cbp_table_is_permutation() {
        assert_eq!(GOLOMB_TO_INTRA4X4_CBP[0], 47);
        assert_eq!(GOLOMB_TO_INTRA4X4_CBP[3], 0);
        assert_eq!(GOLOMB_TO_INTRA4X4_CBP[47], 41);
        let mut seen = [false; 48];
        for &v in GOLOMB_TO_INTRA4X4_CBP.iter() {
            assert!(!seen[usize::from(v)], "duplicate cbp {v}");
            seen[usize::from(v)] = true;
        }
        assert!(seen.iter().all(|&s| s));
    }

    #[test]
    fn i16_type_table() {
        // mb_type 1: V、cbp 0；mb_type 24: Plane、cbp 0x2F（spec 编号）
        assert_eq!(I16_PRED[0], 0);
        assert_eq!(I16_PRED[1], 1);
        assert_eq!(I16_PRED[2], 2);
        assert_eq!(I16_PRED[3], 3);
        assert_eq!(I16_CBP[0], 0);
        assert_eq!(I16_CBP[5], 0x2F);
        let k = 23usize; // mb_type 24
        assert_eq!(I16_PRED[k & 3], 3);
        assert_eq!(I16_CBP[k >> 2], 0x2F);
        // mb_type 3（k=2）= DC、cbp 0；mb_type 7（k=6）= DC、cbp 0x10
        assert_eq!(I16_PRED[2], 2);
        assert_eq!(I16_PRED[6 & 3], 2);
        assert_eq!(I16_CBP[6 >> 2], 0x10);
    }

    #[test]
    fn quadrant_bit_of_blocks() {
        // 象限位：块 (cx,cy) → q = 2·(cy/2) + cx/2
        let q = |b: usize| 2 * (b >> 3) + ((b & 3) >> 1);
        assert_eq!(q(0), 0); // (0,0)
        assert_eq!(q(2), 1); // (2,0)
        assert_eq!(q(7), 1); // (3,1)
        assert_eq!(q(8), 2); // (0,2)
        assert_eq!(q(15), 3); // (3,3)
    }
}
