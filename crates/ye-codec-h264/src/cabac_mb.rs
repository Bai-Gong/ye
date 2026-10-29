//! CABAC 宏块语法层（mb_skip/mb_type/子类型/参考/MVD/cbp/qp/残差/PCM）。
//!
//! 语义与上下文偏移逐字对齐 FFmpeg h264_cabac.c
//! （`decode_cabac_mb_skip`/`decode_cabac_mb_type`/`decode_cabac_p/b_mb_sub_type`/
//! `decode_cabac_mb_ref`/`decode_cabac_mb_mvd`/`decode_cabac_mb_cbp_*`/
//! `decode_cabac_mb_dqp`/`decode_cabac_residual_internal`/`ff_h264_decode_mb_cabac`）。
//! 重建/Prediction 复用 macroblock.rs / inter.rs / intra8x8.rs 的共享层。
//!
//! 上下文偏移（FFmpeg cabac_state 数字下标）：
//! 0..2 I 片 mb_type；3..5 P/B 片 intra mb_type；11..13 P skip
//!（B +13 → 24..26）；14..20 P mb_type；21..23 P 子类型；27..32 B
//! mb_type；36..39 B 子类型；40..46/47..53 mvd x/y；54..59 ref_idx；
//! 60..62 dqp；64..67 chroma pred；68/69 intra4x4 flag/rem；73..76
//! cbp luma；77..84 cbp chroma；85..101 coded_block_flag；399..400
//! transform_8x8；残差 sig/last/abs 按 cat 偏移表（下方常量）。

use crate::cabac::CabacReader;
use crate::error::H264Error;
use crate::inter::{InterCtx, MvPart, apply_parts, build_mv_grid_list, write_parts};
use crate::intra8x8;
use crate::macroblock::{
    CHROMA_QP, InterShape, Macroblock, MbKind, MvGrid, Picture, Z_ORDER, add_block,
    chroma_dc_dequant, decode_skip_mb, gather_16x16_edges, gather_chroma_edges, luma_dc_dequant,
    neighbor_avail, neighbor_i4_mode, pred_8x8c, pred_8x16, pred_16x8, pred_16x16, pred_motion,
    prev_i4_mode, reconstruct_4x4, rewrite_4x4_modes, rewrite_dc_mode,
};
use crate::sps::Pps;

// ---------------------------------------------------------------------------
// 上下文偏移（见模块文档）
// ---------------------------------------------------------------------------
const CTX_MB_TYPE_I: usize = 3; // FFmpeg I 片 intra mb_type 基址（0..2 保留未用）
const CTX_MB_TYPE_PB_INTRA: usize = 17; // FFmpeg P 片 intra 基址
const CTX_SKIP: usize = 11;
const CTX_SKIP_B: usize = 24;
const CTX_MB_TYPE_P: usize = 14;
const CTX_SUB_P: usize = 21;
const CTX_MB_TYPE_B: usize = 27;
const CTX_SUB_B: usize = 36;
const CTX_MVD_X: usize = 40;
const CTX_MVD_Y: usize = 47;
const CTX_REF: usize = 54;
const CTX_DQP: usize = 60;
const CTX_CHROMA_PRED: usize = 64;
const CTX_I4_FLAG: usize = 68;
const CTX_I4_REM: usize = 69;
const CTX_CBP_LUMA: usize = 73;
const CTX_CBP_CHROMA: usize = 77;
const CTX_T8X8: usize = 399;

/// coded_block_flag 基址（FFmpeg get_cabac_cbf_ctx 的 base_ctx[14]）。
// —— 变异扫描（临时诊断，不入库）：YE_CABAC_MUTATE=<n> 选中一个语义位点
// 翻转，其余位点保持原语义。0/缺省 = 无变异（对照）。
pub(crate) fn mutation(n: u8) -> bool {
    static M: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *M.get_or_init(|| {
        std::env::var("YE_CABAC_MUTATE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }) == n
}

const CBF_BASE: [usize; 14] = [
    85, 89, 93, 97, 101, 1012, 460, 464, 468, 1016, 472, 476, 480, 1020,
];

/// 残差 sig/last/abs 的 cat 偏移（FFmpeg decode_cabac_residual_internal，
/// 帧图行；场行不适用——本解码器仅帧图）。
const SIG_OFF: [usize; 14] = [
    105, 120, 134, 149, 152, 402, 484, 499, 513, 660, 528, 543, 557, 718,
];
const LAST_OFF: [usize; 14] = [
    166, 181, 195, 210, 213, 417, 572, 587, 601, 690, 616, 631, 645, 748,
];
const ABS_OFF: [usize; 14] = [
    227, 237, 247, 257, 266, 426, 952, 962, 972, 708, 982, 992, 1002, 766,
];

/// abs_level 的 node→ctx 映射与状态转移（FFmpeg 同名表逐字）。
const ABS_LEVEL1_CTX: [usize; 8] = [1, 2, 3, 4, 0, 0, 0, 0];
const ABS_LEVELGT1_CTX: [usize; 8] = [5, 5, 5, 5, 6, 7, 8, 9];
const ABS_TRANSITION: [[usize; 8]; 2] = [
    [1, 2, 3, 3, 4, 5, 6, 7], // level == 1 后
    [4, 4, 4, 4, 5, 6, 7, 7], // level > 1 后
];

// ---------------------------------------------------------------------------
// 残差解码
// ---------------------------------------------------------------------------

/// 解码一个块的残差（含 coded_block_flag）。
/// `cat`：0 luma DC（I16）/ 1 luma AC（I16）/ 2 luma inter 4x4 /
/// 3 chroma DC / 4 chroma AC / 5 luma inter 8x8。
/// `scan`：系数序→块内光栅；`qmul`：None = DC（原值，后续 Hadamard
/// 反量化）；否则按槽位融合反量化（FFmpeg STORE_BLOCK）。
/// 返回非零系数数（nnz）。
/// cat 5（luma 8x8）**不读 coded_block_flag**（FFmpeg：4:2:0 下 cbf
/// 读取仅在 4:4:4 的 8x8 路径存在，4:2:0 由 cbp 象限位直接门控）。
/// （缺陷修复登记 2026-10-26：曾误读 cbf（ctx 1012..1015）——该 bin
/// 编码器从未写出，首个真 8x8 宏块即脱轨。）
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_residual(
    r: &mut CabacReader<'_>,
    cat: usize,
    max_coeff: usize,
    scan: &[usize],
    qmul: Option<&[u32]>,
    nnz_ctx: (Option<u8>, Option<u8>),
    block: &mut [i16],
) -> Result<usize, H264Error> {
    if cat != 5 {
        // coded_block_flag（FFmpeg get_cabac_cbf_ctx：nza>0 +1、nzb>0 +2）
        let (nza, nzb) = nnz_ctx;
        let mut ctx = 0usize;
        if nza.unwrap_or(0) > 0 {
            ctx += 1;
        }
        if nzb.unwrap_or(0) > 0 {
            ctx += 2;
        }
        if r.bin(CBF_BASE[cat] + ctx) == 0 {
            return Ok(0);
        }
    }
    if max_coeff == 0 {
        return Err(H264Error::InvalidData("residual max_coeff == 0".into()));
    }

    // ---- significant / last 扫描（系数序收集 index[]）----
    let mut index = [0usize; 64];
    let mut coeff_count = 0usize;
    let sig_base = SIG_OFF[cat];
    let last_base = LAST_OFF[cat];
    if max_coeff == 64 {
        return decode_residual_8x8(r, cat, scan, qmul, block);
    }
    // 4x4/DC：sig/last 逐系数（ctxIdxInc = 系数序）。隐式末位规则
    //（FFmpeg DECODE_SIGNIFICANCE 逐字）：last-flag 命中即该系数为
    // 末位（不追加）；循环自然耗尽（无 last-flag）→ 末位系数恒
    // significant（追加 max_coeff−1）。
    let mut last_flag_hit = false;
    for last in 0..max_coeff - 1 {
        if r.bin(sig_base + last) == 1 {
            index[coeff_count] = last;
            coeff_count += 1;
            if r.bin(last_base + last) == 1 {
                last_flag_hit = true;
                break;
            }
        }
    }
    if !last_flag_hit && !mutation(1) {
        index[coeff_count] = max_coeff - 1;
        coeff_count += 1;
    }
    store_levels(r, index, coeff_count, scan, qmul, block, cat)?;
    Ok(coeff_count)
}

/// 8x8 残差（对齐 FFmpeg DECODE_SIGNIFICANCE(63, sig_off[last], last_off[last])）。
#[allow(clippy::too_many_arguments)]
fn decode_residual_8x8(
    r: &mut CabacReader<'_>,
    cat: usize,
    scan: &[usize],
    qmul: Option<&[u32]>,
    block: &mut [i16],
) -> Result<usize, H264Error> {
    let sig_base = SIG_OFF[cat];
    let last_base = LAST_OFF[cat];
    let sig_map = &crate::cabac_tables::SIG_OFF_8X8[0];
    let last_map = &crate::cabac_tables::LAST_OFF_8X8;
    let mut index = [0usize; 64];
    let mut coeff_count = 0usize;
    let mut hit_last = false;
    for last in 0..63usize {
        if r.bin(sig_base + usize::from(sig_map[last])) == 1 {
            index[coeff_count] = last;
            coeff_count += 1;
            if r.bin(last_base + usize::from(last_map[last])) == 1 {
                hit_last = true;
                break;
            }
        }
    }
    if !hit_last {
        // 循环走完：末位（63）隐含 significant（FFmpeg DECODE_SIGNIFICANCE
        // 尾部 `if (last == coefs)`：coefs=63，隐式追加的是 63——sig 循环
        // 已覆盖 0..=62，62 若显著已在 index，尾部只能补 63）。
        // （缺陷修复登记 2026-10-26：曾错补 62——sig[62]=1 时重复计 62
        // 且真末位 63 永不出现，系数反向解码整块错位。）
        index[coeff_count] = 63;
        coeff_count += 1;
    }
    store_levels(r, index, coeff_count, scan, qmul, block, cat)?;
    Ok(coeff_count)
}

/// 幅值 + 符号（FFmpeg STORE_BLOCK 逐字：node_ctx 状态机 + UEGk 后缀）。
fn store_levels(
    r: &mut CabacReader<'_>,
    index: [usize; 64],
    mut coeff_count: usize,
    scan: &[usize],
    qmul: Option<&[u32]>,
    block: &mut [i16],
    cat: usize,
) -> Result<(), H264Error> {
    let abs_base = ABS_OFF[cat];
    let mut node_ctx = 0usize;
    while coeff_count > 0 {
        coeff_count -= 1;
        let j = scan[index[coeff_count]];
        let level: i32 = if r.bin(abs_base + ABS_LEVEL1_CTX[node_ctx]) == 0 {
            // |level| == 1
            node_ctx = ABS_TRANSITION[0][node_ctx];
            let v = r.cabac.bypass_sign(-1);
            dequant_level(v, qmul, j)
        } else {
            // |level| > 1：一元前缀（<15）或 UEGk 后缀。
            // FFmpeg STORE_BLOCK 顺序：gt1 的 ctxIdx 取**转移前**的
            // node_ctx（ctx 赋值先于 transition[1]），状态转移随后。
            // 若先转移再取 ctx，除 node 7 外每个 >1 系数都读错上下文
            // （GT1[old] ≠ GT1[TRANS[1][old]]），引擎整体脱轨。
            // （缺陷修复登记 2026-10-26：真流对拍首轮定位——首个
            // 4x4 块第二个系数起 ctxIdx 即偏 +1。）
            let gt1_base = if mutation(2) {
                node_ctx = ABS_TRANSITION[1][node_ctx];
                abs_base + ABS_LEVELGT1_CTX[node_ctx]
            } else {
                let b = abs_base + ABS_LEVELGT1_CTX[node_ctx];
                node_ctx = ABS_TRANSITION[1][node_ctx];
                b
            };
            let mut abs: i32 = 2;
            let unary_cap = if mutation(8) { 14 } else { 15 };
            while abs < unary_cap && r.bin(gt1_base) == 1 {
                abs += 1;
            }
            if abs >= 15 {
                let mut k = 0i32;
                let k_limit = if mutation(7) { 16 } else { 16 + 7 };
                while r.cabac.bypass() == 1 && k < k_limit {
                    k += 1;
                }
                abs = 1;
                while k > 0 {
                    k -= 1;
                    // FFmpeg 逐字 `level += level + get_cabac_bypass(...)`：
                    // 倍增式 UEGk 后缀（语义即 abs ← 2·abs + bit），非误写。
                    abs = abs + abs + r.cabac.bypass() as i32;
                }
                abs += 14;
            }
            let v = r.cabac.bypass_sign(-abs);
            dequant_level(v, qmul, j)
        };
        // 槽位防御（i16 域）：幅值异常 → InvalidData
        let Ok(v) = i16::try_from(level) else {
            return Err(H264Error::InvalidData(format!(
                "cabac level {level} out of range"
            )));
        };
        block[j] = v;
    }
    Ok(())
}

// 临时诊断（提交前剥离）：YE_MB_COEFFS="mbx,mby" 时转储该 MB 的
// 反量化系数（4x4 块按栅格索引；系数项 = 栅格位置:值）。
fn dump_mb_coeffs(
    mbx: usize,
    mby: usize,
    qp: u8,
    mb: &crate::macroblock::Macroblock,
    luma_blocks: &[[i16; 16]; 16],
    luma8_blocks: &[[i16; 64]; 4],
    chroma_blocks: &[[[i16; 16]; 4]; 2],
) {
    use std::sync::OnceLock;
    static TARGET: OnceLock<Option<(usize, usize)>> = OnceLock::new();
    let target = TARGET.get_or_init(|| {
        std::env::var("YE_MB_COEFFS").ok().and_then(|s| {
            let (a, b) = s.split_once(',')?;
            Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
        })
    });
    if *target != Some((mbx, mby)) {
        return;
    }
    eprintln!(
        "COEFF mb=({mbx},{mby}) qp={qp} kind={:?} t8={} cbp={:#04x} i16={} i4={:?} cmode={} nnz={:?} cdc={:?}",
        mb.kind, mb.transform_8x8, mb.cbp, mb.i16_mode, mb.i4_modes, mb.chroma_mode, mb.nnz,
        mb.nnz_chroma_dc
    );
    let sparse = |blk: &[i16]| -> String {
        blk.iter()
            .enumerate()
            .filter(|(_, v)| **v != 0)
            .map(|(j, v)| format!("{j}:{v}"))
            .collect::<Vec<_>>()
            .join(",")
    };
    if mb.transform_8x8 {
        for (q, blk) in luma8_blocks.iter().enumerate() {
            eprintln!("  Q{q}[{}]", sparse(blk));
        }
    } else {
        for (b, blk) in luma_blocks.iter().enumerate() {
            eprintln!("  B{b:02}[{}]", sparse(blk));
        }
    }
    for (p, cblocks) in chroma_blocks.iter().enumerate() {
        for (i, blk) in cblocks.iter().enumerate() {
            eprintln!("  C{p}{i}[{}]", sparse(blk));
        }
    }
}

/// 反量化融合（FFmpeg：DC 原值；AC `(sign·abs·qmul[j] + 32) >> 6`）。
fn dequant_level(v: i32, qmul: Option<&[u32]>, j: usize) -> i32 {
    match qmul {
        None => v,
        Some(q) => (v * q[j] as i32 + 32) >> 6,
    }
}

// ---------------------------------------------------------------------------
// 语法元素（FFmpeg decode_cabac_* 逐字）
// ---------------------------------------------------------------------------

/// mb_skip_flag（ctx：左/上非 skip 各 +1；B 片基址 24）。
fn decode_skip(r: &mut CabacReader<'_>, pic: &Picture, mbx: usize, mby: usize, is_b: bool) -> bool {
    let mb_xy = mby * pic.mb_width + mbx;
    let mut ctx = 0usize;
    // FFmpeg decode_cabac_mb_skip / spec 9.3.3.1.1：左、上各 ctx++。
    // 左邻是 flat 索引 mb_xy-1（h264_cabac.c:1350——行首的"左邻"即
    // 上一行行末宏块，同片且已解码即计入）；上邻 mb_xy-mb_width。
    if mb_xy > 0 {
        let l = &pic.mbs[mb_xy - 1];
        if l.decoded && !l.skipped {
            ctx += 1;
        }
    }
    if mby > 0 && pic.mbs[mb_xy - pic.mb_width].decoded && !pic.mbs[mb_xy - pic.mb_width].skipped {
        ctx += 1;
    }
    let base = if is_b { CTX_SKIP_B } else { CTX_SKIP };
    r.bin(base + ctx) == 1
}

/// intra mb_type（FFmpeg decode_cabac_intra_mb_type；I 片带邻块 intra
/// ctx、P/B 片 ctx_base=3/32）。
fn decode_intra_mb_type(
    r: &mut CabacReader<'_>,
    pic: &Picture,
    mbx: usize,
    mby: usize,
    ctx_base: usize,
    intra_slice: bool,
) -> Result<u64, H264Error> {
    let mb_xy = mby * pic.mb_width + mbx;
    let mut base = ctx_base;
    if intra_slice {
        let mut ctx = 0usize;
        if mbx > 0 && matches!(pic.mbs[mb_xy - 1].kind, MbKind::Intra16x16 | MbKind::Pcm) {
            ctx += 1;
        }
        if mby > 0
            && matches!(
                pic.mbs[mb_xy - pic.mb_width].kind,
                MbKind::Intra16x16 | MbKind::Pcm
            )
        {
            ctx += 1;
        }
        if r.bin(base + ctx) == 0 {
            return Ok(0); // I_NxN
        }
        base += 2;
    } else if r.bin(base) == 0 {
        return Ok(0);
    }
    if r.cabac.terminate() {
        return Ok(25); // I_PCM
    }
    let mut mb_type = 1u64; // I16x16 族
    mb_type += 12 * u64::from(r.bin(base + 1)); // cbp_luma != 0
    if r.bin(base + 2) == 1 {
        mb_type += 4 + 4 * u64::from(r.bin(base + 2 + usize::from(intra_slice)));
    }
    mb_type += 2 * u64::from(r.bin(base + 3 + usize::from(intra_slice)));
    mb_type += u64::from(r.bin(base + 3 + 2 * usize::from(intra_slice)));
    Ok(mb_type)
}

/// P 片 mb_type（FFmpeg：14/15/16/17 树 → 0..4，与 CAVLC 同码）。
fn decode_p_mb_type(r: &mut CabacReader<'_>) -> u64 {
    if r.bin(CTX_MB_TYPE_P) == 0 {
        if r.bin(CTX_MB_TYPE_P + 1) == 0 {
            u64::from(r.bin(CTX_MB_TYPE_P + 2)) * 3 // 0=16x16, 3=8x8
        } else {
            2 - u64::from(r.bin(CTX_MB_TYPE_P + 3)) // 2=8x16, 1=16x8
        }
    } else {
        u64::MAX // intra 标记（调用方走 intra 路径）
    }
}

/// B 片 mb_type（FFmpeg 树；返回 spec 码 0..22，或 u64::MAX=intra）。
fn decode_b_mb_type(
    r: &mut CabacReader<'_>,
    pic: &Picture,
    mbx: usize,
    mby: usize,
) -> Result<u64, H264Error> {
    let mb_xy = mby * pic.mb_width + mbx;
    let mut ctx = 0usize;
    // FFmpeg：!IS_DIRECT(left_type − 1)——邻块非直接（+1 的哨兵位移）
    let is_direct = |mb: &Macroblock| mb.kind == MbKind::InterB && mb.skipped || direct_mb(pic, mb);
    // FFmpeg `!IS_DIRECT(left_type - 1)`：邻居不可用（left_type=0 → −1
    // 的 DIRECT2 位为 1）时视为 direct，**不** ctx++——此前边界 MB 误加。
    if mbx > 0 && !is_direct(&pic.mbs[mb_xy - 1]) {
        ctx += 1;
    }
    if mby > 0 && !is_direct(&pic.mbs[mb_xy - pic.mb_width]) {
        ctx += 1;
    }
    if r.bin(CTX_MB_TYPE_B + ctx) == 0 {
        return Ok(0); // B_Direct_16x16
    }
    if r.bin(CTX_MB_TYPE_B + 3) == 0 {
        return Ok(1 + u64::from(r.bin(CTX_MB_TYPE_B + 5))); // L0/L1_16x16
    }
    let mut bits = u64::from(r.bin(CTX_MB_TYPE_B + 4)) << 3;
    bits += u64::from(r.bin(CTX_MB_TYPE_B + 5)) << 2;
    bits += u64::from(r.bin(CTX_MB_TYPE_B + 5)) << 1;
    bits += u64::from(r.bin(CTX_MB_TYPE_B + 5));
    if bits < 8 {
        Ok(bits + 3)
    } else if bits == 13 {
        Ok(u64::MAX) // intra
    } else if bits == 14 {
        Ok(11) // B_L1_L0_8x16
    } else if bits == 15 {
        Ok(22) // B_8x8
    } else {
        bits = (bits << 1) + u64::from(r.bin(CTX_MB_TYPE_B + 5));
        Ok(bits - 4)
    }
}

/// 宏块是否为直接预测（direct16 或 B_8x8 的直接象限——FFmpeg
/// IS_DIRECT 按记录位判定；此处按 shape 与 skipped 近似：
/// B_Skip/B_Direct_16x16 的 skipped=true）。
fn direct_mb(_pic: &Picture, mb: &Macroblock) -> bool {
    mb.kind == MbKind::InterB && mb.direct
}

/// P 子类型（FFmpeg decode_cabac_p_mb_sub_type：0..3）。
fn decode_p_sub_type(r: &mut CabacReader<'_>) -> u64 {
    if r.bin(CTX_SUB_P) == 1 {
        0 // 8x8
    } else if r.bin(CTX_SUB_P + 1) == 0 {
        1 // 8x4
    } else if r.bin(CTX_SUB_P + 2) == 1 {
        2 // 4x8
    } else {
        3 // 4x4
    }
}

/// B 子类型（FFmpeg decode_cabac_b_mb_sub_type → spec 码 0..12）。
fn decode_b_sub_type(r: &mut CabacReader<'_>) -> u64 {
    if r.bin(CTX_SUB_B) == 0 {
        return 0; // B_Direct_8x8
    }
    if r.bin(CTX_SUB_B + 1) == 0 {
        return 1 + u64::from(r.bin(CTX_SUB_B + 3)); // L0/L1_8x8
    }
    let mut t = 3u64;
    if r.bin(CTX_SUB_B + 2) == 1 {
        if r.bin(CTX_SUB_B + 3) == 1 {
            return 11 + u64::from(r.bin(CTX_SUB_B + 3)); // L1/Bi_4x4
        }
        t += 4;
    }
    t += 2 * u64::from(r.bin(CTX_SUB_B + 3));
    t += u64::from(r.bin(CTX_SUB_B + 3));
    t
}

/// prev_intra4x4_pred_mode（FFmpeg decode_cabac_mb_intra4x4_pred_mode）。
fn decode_i4_mode(r: &mut CabacReader<'_>, pred: u8) -> u8 {
    if r.bin(CTX_I4_FLAG) == 1 {
        return pred;
    }
    let mut mode = 0u8;
    mode += r.bin(CTX_I4_REM) as u8;
    mode += 2 * r.bin(CTX_I4_REM) as u8;
    mode += 4 * r.bin(CTX_I4_REM) as u8;
    mode + u8::from(mode >= pred)
}

/// intra_chroma_pred_mode（FFmpeg decode_cabac_mb_chroma_pre_mode；spec
/// Table 9-23：ctxIdxInc = 非DC 邻块**计数**，左/上各 +1，值域 0..2）。
/// 邻块比较用**语法值**（FFmpeg 独立 chroma_pred_mode_table 存语法值），
/// 而 `Macroblock.chroma_mode` 存边界改写后的最终模式（DC 在角落被
/// rewrite_dc_mode 改写为内部变体 4..6）——按 1..=3 判「非 DC」。
/// （缺陷修复登记 2026-10-26：①改写值 ≠0 曾令角落 MB 右邻块 ctxInc 虚
/// +1；②上邻块曾误按 +2 计权（左/上应各 +1，ctxIdx 值域止于 66）——
/// 双非 DC 邻块场景此前从未被单行向量覆盖，真流对拍定位。）
fn decode_chroma_mode(r: &mut CabacReader<'_>, pic: &Picture, mbx: usize, mby: usize) -> u8 {
    let mb_xy = mby * pic.mb_width + mbx;
    let mut ctx = 0usize;
    // FFmpeg decode_cabac_mb_chroma_pre_mode：取 chroma_pred_mode_table
    // （**语法原值**）判非 DC——改写后的 4..6 变体不参与计数。
    if mbx > 0 && pic.mbs[mb_xy - 1].decoded && pic.mbs[mb_xy - 1].chroma_syntax != 0 {
        ctx += 1;
    }
    if mby > 0
        && pic.mbs[mb_xy - pic.mb_width].decoded
        && pic.mbs[mb_xy - pic.mb_width].chroma_syntax != 0
    {
        ctx += 1;
    }
    if r.bin(CTX_CHROMA_PRED + ctx) == 0 {
        return 0;
    }
    if r.bin(CTX_CHROMA_PRED + 3) == 0 {
        return 1;
    }
    if r.bin(CTX_CHROMA_PRED + 3) == 0 {
        return 2;
    }
    3
}

/// 邻块 cbp 全字（FFmpeg cbp_table：位 0..3 luma 象限、4/5 chroma、
/// 6/7 chroma DC 已编码、8 luma DC(I16) 已编码）。
fn cbp_word(m: &Macroblock) -> u16 {
    let mut w = u16::from(m.cbp);
    if m.nnz_chroma_dc[0] > 0 {
        w |= 0x40;
    }
    if m.nnz_chroma_dc[1] > 0 {
        w |= 0x80;
    }
    if m.luma_dc_coded {
        w |= 0x100;
    }
    w
}

/// 不可用邻块的 cbp 推断字（FFmpeg fill_decode_caches：
/// 当前 intra → 0x7CF、inter → 0x00F）。
const fn cbp_unavailable(cur_intra: bool) -> u16 {
    if cur_intra { 0x7CF } else { 0x00F }
}

/// cbp luma（FFmpeg decode_cabac_mb_cbp_luma）。
fn decode_cbp_luma(r: &mut CabacReader<'_>, left_cbp: u16, top_cbp: u16) -> u8 {
    let cbp_a = left_cbp;
    let cbp_b = top_cbp;
    let mut cbp = 0u8;
    let mut ctx = usize::from(cbp_a & 0x02 == 0) + 2 * usize::from(cbp_b & 0x04 == 0);
    cbp += r.bin(CTX_CBP_LUMA + ctx) as u8;
    ctx = usize::from(cbp & 0x01 == 0) + 2 * usize::from(cbp_b & 0x08 == 0);
    cbp += (r.bin(CTX_CBP_LUMA + ctx) as u8) << 1;
    ctx = usize::from(cbp_a & 0x08 == 0) + 2 * usize::from(cbp & 0x01 == 0);
    cbp += (r.bin(CTX_CBP_LUMA + ctx) as u8) << 2;
    ctx = usize::from(cbp & 0x04 == 0) + 2 * usize::from(cbp & 0x02 == 0);
    cbp += (r.bin(CTX_CBP_LUMA + ctx) as u8) << 3;
    cbp
}

/// cbp chroma（FFmpeg decode_cabac_mb_cbp_chroma：`(cbp>>4)&3` 口径，
/// DC 位不参与——cbp_table 的 DC 位在 fill_decode_caches 进 top/left_cbp
/// 前不映入 0..3 段，==2 判定按纯 chroma 值）。
fn decode_cbp_chroma(r: &mut CabacReader<'_>, left_cbp: u16, top_cbp: u16) -> u8 {
    let cbp_a = (left_cbp >> 4) & 0x03;
    let cbp_b = (top_cbp >> 4) & 0x03;
    let mut ctx = 0usize;
    if cbp_a > 0 {
        ctx += 1;
    }
    if cbp_b > 0 {
        ctx += 2;
    }
    if r.bin(CTX_CBP_CHROMA + ctx) == 0 {
        return 0;
    }
    ctx = 4 + usize::from(cbp_a == 2) + 2 * usize::from(cbp_b == 2);
    1 + r.bin(CTX_CBP_CHROMA + ctx) as u8
}

/// mb_qp_delta（FFmpeg decode_cabac_mb_dqp 逐字：首 bin ctx 由上 MB
/// delta≠0 决定；一元 |Δ| 后按奇偶定号）。
fn decode_dqp(
    r: &mut CabacReader<'_>,
    last_qscale_diff: &mut i32,
    qp: &mut u8,
) -> Result<(), H264Error> {
    if r.bin(CTX_DQP + usize::from(*last_qscale_diff != 0)) == 0 {
        *last_qscale_diff = 0;
        return Ok(());
    }
    let mut val = 1i32;
    let mut ctx = if mutation(4) { 1usize } else { 2usize };
    while r.bin(CTX_DQP + ctx) == 1 {
        ctx = 3;
        val += 1;
        if val > 2 * 51 {
            return Err(H264Error::InvalidData("cabac dqp overflow".into()));
        }
    }
    let val = if val & 1 == 1 {
        (val + 1) >> 1
    } else {
        -((val + 1) >> 1)
    };
    *last_qscale_diff = val;
    let mut q = i32::from(*qp) + val;
    if !(0..=51).contains(&q) {
        q = if q < 0 { q + 52 } else { q - 52 };
        if !(0..=51).contains(&q) {
            return Err(H264Error::InvalidData("cabac dqp out of range".into()));
        }
    }
    *qp = q as u8;
    Ok(())
}

/// ref_idx（FFmpeg decode_cabac_mb_ref：一元，ctx=(ctx>>2)+4 推进；
/// B 片邻块 direct 位不计数——近似为邻块 ref>0）。
fn decode_ref(r: &mut CabacReader<'_>, max: usize, refa: i8, refb: i8) -> Result<usize, H264Error> {
    let mut ctx = 0usize;
    if refa > 0 {
        ctx += 1;
    }
    if refb > 0 {
        ctx += 2;
    }
    let mut v = 0usize;
    while r.bin(CTX_REF + ctx) == 1 {
        v += 1;
        ctx = (ctx >> 2) + 4;
        if v >= 32 {
            return Err(H264Error::InvalidData("ref_idx overflow".into()));
        }
    }
    if v >= max {
        return Err(H264Error::InvalidData(format!("ref_idx {v} >= {max}")));
    }
    Ok(v)
}

/// mvd（FFmpeg decode_cabac_mb_mvd 逐字：ctxinc 由 amvd>2/>32 决定；
/// 一元至 9 后 UEG0 旁路后缀；返回带符号差）。`amvd` = 邻块 mvd 之和
/// （带符号）。
fn decode_mvd(r: &mut CabacReader<'_>, ctx_base: usize, amvd: i32) -> Result<i16, H264Error> {
    // FFmpeg 1497-1505：slot = ctxbase + ((amvd-3)>>31) + ((amvd-33)>>31) + 2
    // —— i32 算术右移 31：amvd<3 → −1、amvd<33 → −1，即：
    // amvd≤2 → +0、3..=32 → +1、>32 → +2。此前误 +2+(>2)+(>32) 全档偏 +2。
    let mut base = ctx_base + usize::from(amvd > 2) + usize::from(amvd > 32);
    if r.bin(base) == 0 {
        return Ok(0);
    }
    let mut mvd = 1i32;
    base = ctx_base + 3;
    while mvd < 9 && r.bin(base) == 1 {
        if mvd < 4 {
            base += 1;
        }
        mvd += 1;
    }
    if mvd >= 9 {
        let mut k = 3i32;
        while r.cabac.bypass() == 1 {
            mvd += 1 << k;
            k += 1;
            if k > 24 {
                return Err(H264Error::InvalidData("mvd overflow".into()));
            }
        }
        while k > 0 {
            k -= 1;
            mvd += (r.cabac.bypass() as i32) << k;
        }
    }
    Ok(r.cabac.bypass_sign(-mvd) as i16)
}

// ---------------------------------------------------------------------------
// MB 解码编排（FFmpeg ff_h264_decode_mb_cabac 逐段对应）
// ---------------------------------------------------------------------------

/// B mb_type 1..=21 的两分区列表使用（16x16 族两分区同值）。
/// 返回 [分区0 (L0?, L1?), 分区1 (L0?, L1?)]。
fn b_partition_lists(t: u64) -> [[bool; 2]; 2] {
    const L0: [bool; 2] = [true, false];
    const L1: [bool; 2] = [false, true];
    const BI: [bool; 2] = [true, true];
    match t {
        1 => [L0, L0],
        2 => [L1, L1],
        3 => [BI, BI],
        _ => {
            let i = ((t - 4) / 2) as usize;
            let pick = |x: usize| match x {
                0 => L0,
                1 => L1,
                _ => BI,
            };
            let (a, b) = match i {
                0 => (0, 0),
                1 => (1, 1),
                2 => (0, 1),
                3 => (1, 0),
                4 => (0, 2),
                5 => (1, 2),
                6 => (2, 0),
                7 => (2, 1),
                _ => (2, 2),
            };
            [pick(a), pick(b)]
        }
    }
}

/// 当前 MB 的 ref/mvd 解析游标（左/上邻边 + 本 MB 已解析值）。
struct RefMvdEdge {
    /// 左邻列/顶邻行的 (ref, mvd, direct)；ref：-2=不可得、-1=未用。
    left: [(i8, [i16; 2], bool); 4],
    top: [(i8, [i16; 2], bool); 4],
    cur_ref: [i8; 16],
    cur_mvd: [[i16; 2]; 16],
    cur_direct: [bool; 16],
}

impl RefMvdEdge {
    fn new(pic: &Picture, mbx: usize, mby: usize, list: u8) -> Self {
        let mb_xy = mby * pic.mb_width + mbx;
        let entry = |mb: &Macroblock, b: usize| -> (i8, [i16; 2], bool) {
            if !mb.decoded {
                return (-2, [0; 2], false);
            }
            let direct = mb.direct || mb.sub_shape.get(b / 4) == Some(&255);
            match mb.kind {
                MbKind::InterP if list == 0 => (mb.ref_idx[b], mb.mvd[b], direct),
                MbKind::InterB => {
                    let (ri, mvd) = if list == 0 {
                        (mb.ref_idx[b], mb.mvd[b])
                    } else {
                        (mb.ref_idx_l1[b], mb.mvd_l1[b])
                    };
                    (ri, mvd, direct)
                }
                _ => (-1, [0; 2], false),
            }
        };
        let mut left = [(-2, [0; 2], false); 4];
        if mbx > 0 {
            let lmb = &pic.mbs[mb_xy - 1];
            for (y, slot) in left.iter_mut().enumerate() {
                *slot = entry(lmb, 3 + y * 4);
            }
        }
        let mut top = [(-2, [0; 2], false); 4];
        if mby > 0 {
            let tmb = &pic.mbs[mb_xy - pic.mb_width];
            for (x, slot) in top.iter_mut().enumerate() {
                *slot = entry(tmb, 12 + x);
            }
        }
        RefMvdEdge {
            left,
            top,
            cur_ref: [-2; 16],
            cur_mvd: [[0; 2]; 16],
            cur_direct: [false; 16],
        }
    }

    fn ref_at(&self, bx: i32, by: i32) -> (i8, [i16; 2], bool) {
        if bx < 0 {
            return self.left[by.clamp(0, 3) as usize];
        }
        if by < 0 {
            return self.top[bx.clamp(0, 3) as usize];
        }
        let b = (by * 4 + bx) as usize;
        (self.cur_ref[b], self.cur_mvd[b], self.cur_direct[b])
    }

    // 8 个参数（超 clippy 默认上限 1 个）：矩形锚点 (bx,by,w,h) 与
    // (ref, mvd, direct) 三元组各占一席——与 [`MvPart`] 字段一一对应的
    // 落位原语，收拢进结构体会让每个调用点多拆一次包。
    #[allow(clippy::too_many_arguments)]
    fn set_rect(
        &mut self,
        bx: i32,
        by: i32,
        w: i32,
        h: i32,
        ref_idx: i8,
        mvd: [i16; 2],
        direct: bool,
    ) {
        for yy in by..by + h {
            for xx in bx..bx + w {
                let b = (yy * 4 + xx) as usize;
                self.cur_ref[b] = ref_idx;
                // FFmpeg mvd_cache 存**幅度** mvda（decode_cabac_mb_mvd 的
                // *mvda = mvd<70 ? mvd : 70，符号无关）——amvd 上下文用幅度
                // 求和；此前误存带符号差，负向 mvd 邻块的首 bin 槽位偏档。
                self.cur_mvd[b] = [mvd[0].unsigned_abs().min(70) as i16, mvd[1].unsigned_abs().min(70) as i16];
                self.cur_direct[b] = direct;
            }
        }
    }

    /// ref ctx 输入：左/上（direct 邻块不计，未用/不可得 → −1）。
    fn ref_ctx_pair(&self, bx: i32, by: i32) -> (i8, i8) {
        let (a, _, da) = self.ref_at(bx - 1, by);
        let (b, _, db) = self.ref_at(bx, by - 1);
        (
            if a > 0 && !da { a } else { -1 },
            if b > 0 && !db { b } else { -1 },
        )
    }

    /// mvd ctx 输入：左/上 mvd（axis 0=x 1=y）之和。
    fn amvd(&self, bx: i32, by: i32, axis: usize) -> i32 {
        let a = self.ref_at(bx - 1, by).1[axis];
        let b = self.ref_at(bx, by - 1).1[axis];
        i32::from(a) + i32::from(b)
    }
}

/// 读一个分区的 mvd（ref 已读）：mvp + 两轴 mvd + 游标落位。
#[allow(clippy::too_many_arguments)]
fn read_partition_mvd(
    r: &mut CabacReader<'_>,
    edge: &mut RefMvdEdge,
    grid: &MvGrid,
    bx: i32,
    by: i32,
    w: i32,
    h: i32,
    ref_idx: i8,
) -> Result<[i16; 2], H264Error> {
    let mvp = pred_motion(grid, bx, by, w, ref_idx);
    let dx = decode_mvd(r, CTX_MVD_X, edge.amvd(bx, by, 0))?;
    let dy = decode_mvd(r, CTX_MVD_Y, edge.amvd(bx, by, 1))?;
    let mv = [
        (i32::from(mvp[0]) + i32::from(dx)) as i16,
        (i32::from(mvp[1]) + i32::from(dy)) as i16,
    ];
    edge.set_rect(bx, by, w, h, ref_idx, [dx, dy], false);
    Ok(mv)
}

/// 读 8x8 象限的 ref（L0/L1 由调用方指定；n==1 → 0 不读）。
fn read_quad_ref(
    r: &mut CabacReader<'_>,
    edge: &mut RefMvdEdge,
    n: usize,
    x0: i32,
    y0: i32,
) -> Result<i8, H264Error> {
    if n <= 1 {
        edge.set_rect(x0, y0, 2, 2, 0, [0; 2], false);
        return Ok(0);
    }
    let (a, b) = edge.ref_ctx_pair(x0, y0);
    let ri = decode_ref(r, n, a, b)? as i8;
    edge.set_rect(x0, y0, 2, 2, ri, [0; 2], false);
    Ok(ri)
}

/// B 子分区几何：类型 → (子分区数, 分区列表逐个 (bx,by,w,h))。
fn b_sub_geometry(s: u8, x0: i32, y0: i32) -> Vec<(i32, i32, i32, i32)> {
    match s {
        1..=3 => vec![(x0, y0, 2, 2)],
        4..=6 => vec![(x0, y0, 2, 1), (x0, y0 + 1, 2, 1)],
        7..=9 => vec![(x0, y0, 1, 2), (x0 + 1, y0, 1, 2)],
        _ => vec![
            (x0, y0, 1, 1),
            (x0 + 1, y0, 1, 1),
            (x0, y0 + 1, 1, 1),
            (x0 + 1, y0 + 1, 1, 1),
        ],
    }
}

/// 主入口：解码一个 CABAC 宏块（解析 + 重建 + 落库）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_mb_cabac(
    r: &mut CabacReader<'_>,
    pic: &mut Picture,
    mbx: usize,
    mby: usize,
    pps: &Pps,
    slice_id: u16,
    qp: &mut u8,
    last_qscale_diff: &mut i32,
    ictx: Option<&InterCtx<'_>>,
    is_b_slice: bool,
) -> Result<(), H264Error> {
    let mb_xy = mby * pic.mb_width + mbx;
    let is_inter_slice = ictx.is_some();
    let dump = std::env::var_os("YE_MB_DUMP").is_some();
    let result = decode_mb_cabac_inner(
        r,
        pic,
        mbx,
        mby,
        pps,
        slice_id,
        qp,
        last_qscale_diff,
        ictx,
        is_b_slice,
    );
    if dump {
        let mb = &pic.mbs[mb_xy];
        eprintln!(
            "MB({mbx},{mby}) kind={:?} cbp={:#x} qp={qp} chroma={} i4={:?} nnzsum={} cdc={:?} ldc={}",
            mb.kind,
            mb.cbp,
            mb.chroma_mode,
            mb.i4_modes,
            mb.nnz.iter().map::<u32, _>(|v| u32::from(*v)).sum::<u32>(),
            mb.nnz_chroma_dc,
            mb.luma_dc_coded
        );
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn decode_mb_cabac_inner(
    r: &mut CabacReader<'_>,
    pic: &mut Picture,
    mbx: usize,
    mby: usize,
    pps: &Pps,
    slice_id: u16,
    qp: &mut u8,
    last_qscale_diff: &mut i32,
    ictx: Option<&InterCtx<'_>>,
    is_b_slice: bool,
) -> Result<(), H264Error> {
    crate::cabac::binlog::marker(&format!("{mbx},{mby}"));
    let mb_xy = mby * pic.mb_width + mbx;
    let is_inter_slice = ictx.is_some();

    // ---- skip（P_Skip / B_Skip=B_Direct）----
    if is_inter_slice && decode_skip(r, pic, mbx, mby, is_b_slice) {
        if is_b_slice {
            crate::inter::decode_direct_mb(
                ictx.expect("inter slice"),
                pic,
                mbx,
                mby,
                slice_id,
                *qp,
            )?;
            let mb = &mut pic.mbs[mb_xy];
            mb.direct = true;
            mb.sub_shape = [255; 4];
        } else {
            decode_skip_mb(pic, mbx, mby, ictx.expect("inter slice"), slice_id, *qp)
                .map_err(|e| H264Error::InvalidData(format!("skip mb ({mbx},{mby}): {e}")))?;
        }
        *last_qscale_diff = 0;
        return Ok(());
    }

    // ---- mb_type ----
    let (intra_t, inter_t): (Option<u64>, Option<(bool, u64)>) = if !is_inter_slice {
        (
            Some(decode_intra_mb_type(r, pic, mbx, mby, CTX_MB_TYPE_I, true)?),
            None,
        )
    } else if is_b_slice {
        let t = decode_b_mb_type(r, pic, mbx, mby)?;
        if t == u64::MAX {
            (
                Some(decode_intra_mb_type(r, pic, mbx, mby, 32, false)?),
                None,
            )
        } else {
            (None, Some((true, t)))
        }
    } else {
        let t = decode_p_mb_type(r);
        if t == u64::MAX {
            (
                Some(decode_intra_mb_type(
                    r,
                    pic,
                    mbx,
                    mby,
                    CTX_MB_TYPE_PB_INTRA,
                    false,
                )?),
                None,
            )
        } else {
            (None, Some((false, t)))
        }
    };

    // ---- I_PCM ----
    if intra_t == Some(25) {
        let bit = r.cabac.bit_position() % 8;
        if bit != 0 {
            let _ = r.cabac.next_bits((8 - bit) as u32);
        }
        {
            let ys = pic.luma_stride();
            for y in 0..16usize {
                for x in 0..16usize {
                    let v = r.cabac.next_bits(8) as u8;
                    pic.luma[(mby * 16 + y) * ys + mbx * 16 + x] = v;
                }
            }
        }
        let cs = pic.chroma_stride();
        for data in [&mut pic.cb, &mut pic.cr] {
            for y in 0..8usize {
                for x in 0..8usize {
                    let v = r.cabac.next_bits(8) as u8;
                    data[(mby * 8 + y) * cs + mbx * 8 + x] = v;
                }
            }
        }
        let mut mb = Macroblock {
            slice_id,
            kind: MbKind::Pcm,
            ..Macroblock::default()
        };
        mb.qp = 0;
        mb.nnz = [16; 24];
        mb.nnz_chroma_dc = [16; 2];
        mb.decoded = true;
        pic.mbs[mb_xy] = mb;
        *last_qscale_diff = 0;
        r.cabac.restart_after_pcm();
        return Ok(());
    }

    let mut mb = Macroblock {
        slice_id,
        ..Macroblock::default()
    };
    let mut luma_blocks = [[0i16; 16]; 16];
    let mut luma8_blocks = [[0i16; 64]; 4];
    let mut chroma_blocks = [[[0i16; 16]; 4]; 2];

    // ---- intra ----
    if let Some(t) = intra_t {
        let avail = neighbor_avail(
            pic,
            mbx,
            mby,
            slice_id,
            ictx.map(|c| c.constrained).unwrap_or(false),
        );
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
        if t == 0 {
            // ---- I_NxN ----
            mb.kind = MbKind::Intra4x4;
            if pps.transform_8x8_mode_flag {
                let mut nts = 0usize;
                if let Some(l) = left_mb {
                    nts += usize::from(l.transform_8x8);
                }
                if let Some(tp) = top_mb {
                    nts += usize::from(tp.transform_8x8);
                }
                mb.transform_8x8 = r.bin(CTX_T8X8 + nts) == 1;
            }
            let mut modes = [0u8; 16];
            if mb.transform_8x8 {
                // 4 个 8x8 模式（FFmpeg：i = 0,4,8,12 逐象限，pred_intra_mode
                // 读 cache[scan8[i]-1]/cache[scan8[i]-8]——右列象限（q&1==1）
                // 的左邻与下行象限（q>>1==1）的上邻落在当前 MB cache 区，
                // 是**本 MB 已解象限**的模式（fill_rectangle 后整象限同值）；
                // 其余方向取跨 MB 邻块逐块语法原值（4x4 邻=i4_syntax，I16
                // 等非 4x4 邻=2），任一侧不可用 → prev=DC(2)。
                for q in 0..4usize {
                    let left = if q & 1 == 1 {
                        Some(modes[(q - 1) * 4])
                    } else {
                        left_mb.map(|m| neighbor_i4_mode(m, 3 + 8 * (q >> 1)))
                    };
                    let top = if q >> 1 == 1 {
                        Some(modes[(q - 2) * 4])
                    } else {
                        top_mb.map(|m| neighbor_i4_mode(m, 12 + 2 * (q & 1)))
                    };
                    let prev = prev_i4_mode(left, top);
                    let mode = decode_i4_mode(r, prev);
                    for k in 0..4usize {
                        modes[q * 4 + k] = mode;
                    }
                }
                // 语法原值（改写前）落库：邻块 prev 查询走 i4_syntax
                //（FFmpeg write_back 在 check 改写之前）。
                mb.i4_syntax = modes;
                for q in 0..4usize {
                    let qa = intra8x8::quad_avail(&avail, q & 1, q >> 1);
                    let m = intra8x8::rewrite_8x8_mode(modes[q * 4], &qa)?;
                    for k in 0..4usize {
                        modes[q * 4 + k] = m;
                    }
                }
            } else {
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
                    modes[b] = decode_i4_mode(r, prev);
                }
                // 语法原值（改写前）落库：邻块 prev 查询走 i4_syntax
                //（FFmpeg write_back 在 check 改写之前）。
                mb.i4_syntax = modes;
                rewrite_4x4_modes(&mut modes, avail.top, avail.left, avail.topleft)?;
            }
            mb.i4_modes = modes;
            mb.chroma_syntax = decode_chroma_mode(r, pic, mbx, mby);
            mb.chroma_mode =
                rewrite_dc_mode(mb.chroma_syntax, avail.top, avail.left, avail.topleft, 0)?;
            let lw = left_mb.map(cbp_word).unwrap_or(cbp_unavailable(true));
            let tw = top_mb.map(cbp_word).unwrap_or(cbp_unavailable(true));
            mb.cbp = decode_cbp_luma(r, lw, tw);
            mb.cbp |= decode_cbp_chroma(r, lw, tw) << 4;
            if mb.cbp > 0 {
                decode_dqp(r, last_qscale_diff, qp)?;
            } else {
                // FFmpeg 4.4 ff_h264_decode_mb_cabac：!(cbp || IS_INTRA16x16)
                // 时 last_qscale_diff = 0——下一 MB 的 mb_qp_delta 首 bin
                // 上下文（60 + last_qscale_diff!=0）据此选择槽位。
                *last_qscale_diff = 0;
            }
        } else {
            // ---- I16x16 ----
            let k = (t - 1) as usize;
            mb.kind = MbKind::Intra16x16;
            mb.i16_mode = rewrite_dc_mode(
                crate::macroblock::I16_PRED[k & 3],
                avail.top,
                avail.left,
                avail.topleft,
                2,
            )?;
            mb.cbp = crate::macroblock::I16_CBP[k >> 2];
            mb.chroma_syntax = decode_chroma_mode(r, pic, mbx, mby);
            mb.chroma_mode =
                rewrite_dc_mode(mb.chroma_syntax, avail.top, avail.left, avail.topleft, 0)?;
            decode_dqp(r, last_qscale_diff, qp)?;
        }
        decode_residual_mb(
            r,
            &mut mb,
            pps,
            *qp,
            &mut luma_blocks,
            &mut luma8_blocks,
            &mut chroma_blocks,
            left_mb,
            top_mb,
        )?;
        dump_mb_coeffs(
            mbx,
            mby,
            *qp,
            &mb,
            &luma_blocks,
            &luma8_blocks,
            &chroma_blocks,
        );
        reconstruct_intra(
            pic,
            mbx,
            mby,
            &avail,
            &mut mb,
            &mut luma_blocks,
            &mut luma8_blocks,
            &mut chroma_blocks,
        )?;
        mb.qp = *qp;
        mb.decoded = true;
        pic.mbs[mb_xy] = mb;
        return Ok(());
    }

    // ---- inter ----
    let ctx = ictx.expect("inter slice");
    let (is_b, t) = inter_t.expect("inter path");
    let mut parts: Vec<MvPart> = Vec::new();
    let grid0 = build_mv_grid_list(pic, mbx, mby, 0);
    let mut edge0 = RefMvdEdge::new(pic, mbx, mby, 0);
    let (grid1, mut edge1) = if is_b {
        (
            Some(build_mv_grid_list(pic, mbx, mby, 1)),
            Some(RefMvdEdge::new(pic, mbx, mby, 1)),
        )
    } else {
        (None, None)
    };
    let n0 = ctx.l0.len();
    let n1 = if is_b { ctx.l1.len() } else { 0 };

    if !is_b {
        // ---- P 片（t = 0..3；CABAC 无 P_8x8ref0）----
        if t < 3 {
            mb.shape = match t {
                0 => InterShape::P16x16,
                1 => InterShape::P16x8,
                _ => InterShape::P8x16,
            };
            let n_parts = if t == 0 { 1 } else { 2 };
            // ref 先读全（FFmpeg：两分区 ref 在 mvd 前）
            let mut refs = [0i8; 2];
            for (i, ri) in refs.iter_mut().enumerate().take(n_parts) {
                let (bx, by, w, h) = p_part_pos(t, i);
                *ri = read_quad_ref(r, &mut edge0, n0, bx, by)?;
                let _ = (w, h);
            }
            for (i, &ri) in refs.iter().enumerate().take(n_parts) {
                let (bx, by, _w, _h) = p_part_pos(t, i);
                let (w, h) = (_w, _h);
                let mv = if t == 0 {
                    read_partition_mvd(r, &mut edge0, &grid0, bx, by, w, h, ri)?
                } else if t == 1 {
                    let mvp = pred_16x8(&grid0, by, ri);
                    read_partition_mvd_pred(r, &mut edge0, &grid0, bx, by, w, h, ri, mvp)?
                } else {
                    let mvp = pred_8x16(&grid0, bx, ri);
                    read_partition_mvd_pred(r, &mut edge0, &grid0, bx, by, w, h, ri, mvp)?
                };
                parts.push(MvPart {
                    bx,
                    by,
                    w,
                    h,
                    l0: Some((ri, mv)),
                    l1: None,
                });
            }
        } else {
            // P_8x8
            mb.shape = InterShape::P8x8;
            let mut subs = [0u8; 4];
            for s in subs.iter_mut() {
                *s = decode_p_sub_type(r) as u8;
            }
            let mut refs = [0i8; 4];
            for i8x8 in 0..4usize {
                let (x0, y0) = (((i8x8 & 1) * 2) as i32, ((i8x8 >> 1) * 2) as i32);
                refs[i8x8] = read_quad_ref(r, &mut edge0, n0, x0, y0)?;
                mb.sub_shape[i8x8] = subs[i8x8];
            }
            for i8x8 in 0..4usize {
                let (x0, y0) = (((i8x8 & 1) * 2) as i32, ((i8x8 >> 1) * 2) as i32);
                for (bx, by, w, h) in p_sub_geometry(subs[i8x8], x0, y0) {
                    let mv = read_partition_mvd(r, &mut edge0, &grid0, bx, by, w, h, refs[i8x8])?;
                    parts.push(MvPart {
                        bx,
                        by,
                        w,
                        h,
                        l0: Some((refs[i8x8], mv)),
                        l1: None,
                    });
                }
            }
        }
        mb.kind = MbKind::InterP;
    } else {
        // ---- B 片 ----
        mb.kind = MbKind::InterB;
        if t == 0 {
            // B_Direct_16x16（含残差）
            mb.direct = true;
            mb.sub_shape = [255; 4];
            mb.shape = InterShape::P16x16;
            if ctx.direct_spatial {
                crate::inter::pred_spatial_direct(ctx, pic, mbx, mby, false, &mut parts)?;
            } else {
                crate::inter::pred_temp_direct(ctx, pic, mbx, mby, false, &mut parts)?;
            }
        } else if t == 22 {
            // B_8x8
            mb.shape = InterShape::P8x8;
            let mut subs = [0u8; 4];
            let mut any_direct = false;
            for s in subs.iter_mut() {
                *s = decode_b_sub_type(r) as u8;
                any_direct |= *s == 0;
            }
            // 直接象限（FFmpeg：pred_direct 在 ref/mvd 解析前填充）
            if any_direct {
                let mut direct_parts = Vec::new();
                if ctx.direct_spatial {
                    crate::inter::pred_spatial_direct(ctx, pic, mbx, mby, true, &mut direct_parts)?;
                } else {
                    crate::inter::pred_temp_direct(ctx, pic, mbx, mby, true, &mut direct_parts)?;
                }
                parts.extend(direct_parts);
            }
            // refs（FFmpeg list 外层 → 象限内层；象限内按 8x8 粒度先读）
            let mut refs = [[0i8; 2]; 4]; // [quad][list]
            // `list` 是 refs 的内层维下标（另驱动 edge/n 选择）；按外层维
            //（象限）迭代会颠倒 CABAC 读取顺序（FFmpeg 为 list 外层）。
            #[allow(clippy::needless_range_loop)]
            for list in 0..2usize {
                let n = if list == 0 { n0 } else { n1 };
                let edge = if list == 0 {
                    &mut edge0
                } else {
                    edge1.as_mut().expect("b slice")
                };
                for i8x8 in 0..4usize {
                    let s = subs[i8x8];
                    if s == 0 {
                        continue; // direct：不读 ref
                    }
                    let uses = match s {
                        1 | 4 | 7 | 10 => list == 0,
                        2 | 5 | 8 | 11 => list == 1,
                        _ => true,
                    };
                    if !uses || n <= 1 {
                        continue;
                    }
                    let (x0, y0) = (((i8x8 & 1) * 2) as i32, ((i8x8 >> 1) * 2) as i32);
                    refs[i8x8][list] = read_quad_ref(r, edge, n, x0, y0)?;
                }
                // n<=1 且 uses：ref=0（读 8x8 域内落位）
                for (i8x8, &s) in subs.iter().enumerate() {
                    if s == 0 {
                        continue;
                    }
                    let uses = match s {
                        1 | 4 | 7 | 10 => list == 0,
                        2 | 5 | 8 | 11 => list == 1,
                        _ => true,
                    };
                    if uses && n <= 1 {
                        let (x0, y0) = (((i8x8 & 1) * 2) as i32, ((i8x8 >> 1) * 2) as i32);
                        edge.set_rect(x0, y0, 2, 2, 0, [0; 2], false);
                    }
                }
            }
            // mvd（FFmpeg list 外层 → 象限 → 子分区）
            // 同上：`list` 为 refs 内层维下标，重排迭代序即改码流读序。
            #[allow(clippy::needless_range_loop)]
            for list in 0..2usize {
                let edge = if list == 0 {
                    &mut edge0
                } else {
                    edge1.as_mut().expect("b slice")
                };
                let grid = if list == 0 {
                    &grid0
                } else {
                    grid1.as_ref().expect("b slice")
                };
                for i8x8 in 0..4usize {
                    let s = subs[i8x8];
                    if s == 0 {
                        continue;
                    }
                    let uses = match s {
                        1 | 4 | 7 | 10 => list == 0,
                        2 | 5 | 8 | 11 => list == 1,
                        _ => true,
                    };
                    if !uses {
                        continue;
                    }
                    let (x0, y0) = (((i8x8 & 1) * 2) as i32, ((i8x8 >> 1) * 2) as i32);
                    for (bx, by, w, h) in b_sub_geometry(s, x0, y0) {
                        let mv = read_partition_mvd(r, edge, grid, bx, by, w, h, refs[i8x8][list])?;
                        // 记入分区（本象限此 list）
                        push_b_part(&mut parts, bx, by, w, h, list, refs[i8x8][list], mv);
                    }
                }
            }
        } else {
            // B 16x16 / 16x8 / 8x16（t = 1..=21）
            let lists = b_partition_lists(t);
            let two = t >= 4;
            let shape16x8 = two && (t - 4) % 2 == 0;
            mb.shape = if !two {
                InterShape::P16x16
            } else if shape16x8 {
                InterShape::P16x8
            } else {
                InterShape::P8x16
            };
            let n_parts = if two { 2 } else { 1 };
            let part_pos = |i: usize| -> (i32, i32, i32, i32) {
                if !two {
                    (0, 0, 4, 4)
                } else if shape16x8 {
                    (0, (i * 2) as i32, 4, 2)
                } else {
                    ((i * 2) as i32, 0, 2, 4)
                }
            };
            // refs：list 外层 → 分区内层
            let mut refs = [[0i8; 2]; 2];
            for list in 0..2usize {
                if !lists.iter().any(|l| l[list]) {
                    continue;
                }
                let n = if list == 0 { n0 } else { n1 };
                let edge = if list == 0 {
                    &mut edge0
                } else {
                    edge1.as_mut().expect("b slice")
                };
                for i in 0..n_parts {
                    if !lists[i][list] {
                        continue;
                    }
                    let (bx, by, w, h) = part_pos(i);
                    if n > 1 {
                        let (a, b) = edge.ref_ctx_pair(bx, by);
                        refs[i][list] = decode_ref(r, n, a, b)? as i8;
                    } else {
                        refs[i][list] = 0;
                    }
                    edge.set_rect(bx, by, w, h, refs[i][list], [0; 2], false);
                }
            }
            // mvd：list 外层 → 分区内层（分区预测按形状）
            for list in 0..2usize {
                let edge = if list == 0 {
                    &mut edge0
                } else {
                    edge1.as_mut().expect("b slice")
                };
                let grid = if list == 0 {
                    &grid0
                } else {
                    grid1.as_ref().expect("b slice")
                };
                for i in 0..n_parts {
                    if !lists[i][list] {
                        continue;
                    }
                    let (bx, by, w, h) = part_pos(i);
                    let mv = if !two {
                        read_partition_mvd(r, edge, grid, bx, by, w, h, refs[i][list])?
                    } else if shape16x8 {
                        let mvp = pred_16x8(grid, by, refs[i][list]);
                        read_partition_mvd_pred(r, edge, grid, bx, by, w, h, refs[i][list], mvp)?
                    } else {
                        let mvp = pred_8x16(grid, bx, refs[i][list]);
                        read_partition_mvd_pred(r, edge, grid, bx, by, w, h, refs[i][list], mvp)?
                    };
                    push_b_part(&mut parts, bx, by, w, h, list, refs[i][list], mv);
                }
            }
        }
    }

/// FFmpeg `get_dct8x8_allowed`（ff_h264dec.h:810-820）：8x8 变换 flag 的
/// 读取门——sub_mb_type 内部位（ff_h264_p/b_sub_mb_type_info）任含
/// 16x8/8x16/8x8（inference=0 时另含 DIRECT2）即不允许。
/// 按 spec 子类型码：P 片仅 0（8x8 sub，内部 16x16 型）允许；
/// B 片 0（direct，inference=1 时）/1/2/3（8x8 族，16x16 型）允许。
fn sub_types_allow_8x8(sub_shape: &[u8; 4], direct_8x8_inference: bool, is_b: bool) -> bool {
    if !is_b {
        // P 片：仅 8x8 sub（码 0，内部 16x16 型）允许
        sub_shape.iter().all(|&s| s == 0)
    } else if direct_8x8_inference {
        sub_shape.iter().all(|&s| s <= 3)
    } else {
        sub_shape.iter().all(|&s| (1..=3).contains(&s))
    }
}

// ---- cbp / dqp（inter；FFmpeg：非 intra16 都读 cbp）----
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
    let lw = left_mb.map(cbp_word).unwrap_or(cbp_unavailable(false));
    let tw = top_mb.map(cbp_word).unwrap_or(cbp_unavailable(false));
    mb.cbp = decode_cbp_luma(r, lw, tw);
    mb.cbp |= decode_cbp_chroma(r, lw, tw) << 4;
    // FFmpeg partition_count==4 ⇔ P_8x8（P 片 mb_type 3）/B_8x8（码 22）
    let partition_count_4 = (!is_b && t == 3) || (is_b && t == 22);
    // FFmpeg 4.4 ff_h264_decode_mb_cabac:2338：`dct8x8_allowed && (cbp&15)
    // && !IS_INTRA` 即读 transform_size_8x8_flag——**无分区形状限制**：
    // dct8x8_allowed 初值 = pps.transform_8x8_mode（1916），仅
    // partition_count==4（P8x8/B_8x8）分支内经 get_dct8x8_allowed 用
    // sub_mb_type 收窄（2152-2153）。即 P16x16/16x8/8x16 只要
    // pps.t8 且 cbp&15 同样读此 bin（读值仅影响 mb_type 标志）。
    // 此前漏读（门限误加 shape==P8x8），P 片自首个 cbp&15≠0 的非
    // P8x8 inter 宏块起引擎错位。
    let mut allow_t8 = pps.transform_8x8_mode_flag;
    if partition_count_4 {
        // P8x8/B_8x8：FFmpeg get_dct8x8_allowed（ff_h264dec.h:810）
        allow_t8 = allow_t8 && sub_types_allow_8x8(&mb.sub_shape, ctx.direct_8x8_inference, is_b);
    }
    if allow_t8 && mb.cbp & 0x0F != 0 {
        let mut nts = 0usize;
        if let Some(l) = left_mb {
            nts += usize::from(l.transform_8x8);
        }
        if let Some(tp) = top_mb {
            nts += usize::from(tp.transform_8x8);
        }
        mb.transform_8x8 = r.bin(CTX_T8X8 + nts) == 1;
    }
    if mb.cbp > 0 {
        decode_dqp(r, last_qscale_diff, qp)?;
    } else {
        // FFmpeg 4.4：!(cbp || IS_INTRA16x16) → last_qscale_diff = 0。
        *last_qscale_diff = 0;
    }

    // ---- 残差（inter：无 luma DC；luma 走 cat2/4x4 或 cat5/8x8）----
    decode_residual_mb(
        r,
        &mut mb,
        pps,
        *qp,
        &mut luma_blocks,
        &mut luma8_blocks,
        &mut chroma_blocks,
        left_mb,
        top_mb,
    )?;
    dump_mb_coeffs(
        mbx,
        mby,
        *qp,
        &mb,
        &luma_blocks,
        &luma8_blocks,
        &chroma_blocks,
    );

    // ---- MC + 残差叠加 + 落库 ----
    apply_parts(ctx, pic, mbx, mby, &parts)?;
    write_parts(&mut mb, &parts);
    write_mvd(&mut mb, &edge0, edge1.as_ref());
    let ys = pic.luma_stride();
    if mb.transform_8x8 {
        for (q, blk) in luma8_blocks.iter_mut().enumerate() {
            add_block8(
                &mut pic.luma,
                ys,
                mby * 16 + (q >> 1) * 8,
                mbx * 16 + (q & 1) * 8,
                blk,
            );
        }
    } else {
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
    let cs = pic.chroma_stride();
    for (plane, data) in [&mut pic.cb, &mut pic.cr].iter_mut().enumerate() {
        for (i, cblk) in chroma_blocks[plane].iter_mut().enumerate() {
            add_block(data, cs, mby * 8 + (i / 2) * 4, mbx * 8 + (i % 2) * 4, cblk);
        }
    }
    mb.qp = *qp;
    mb.decoded = true;
    pic.mbs[mb_xy] = mb;
    Ok(())
}

/// P 片 16x16/16x8/8x16 分区锚点。
fn p_part_pos(t: u64, i: usize) -> (i32, i32, i32, i32) {
    match t {
        0 => (0, 0, 4, 4),
        1 => (0, (i * 2) as i32, 4, 2),
        _ => ((i * 2) as i32, 0, 2, 4),
    }
}

/// P 子分区几何（类型 0..3 → 8x8/8x4/4x8/4x4）。
fn p_sub_geometry(s: u8, x0: i32, y0: i32) -> Vec<(i32, i32, i32, i32)> {
    match s {
        0 => vec![(x0, y0, 2, 2)],
        1 => vec![(x0, y0, 2, 1), (x0, y0 + 1, 2, 1)],
        2 => vec![(x0, y0, 1, 2), (x0 + 1, y0, 1, 2)],
        _ => vec![
            (x0, y0, 1, 1),
            (x0 + 1, y0, 1, 1),
            (x0, y0 + 1, 1, 1),
            (x0 + 1, y0 + 1, 1, 1),
        ],
    }
}

/// 带 mvp 注入的 mvd 读取（16x8/8x16 分区预测专用）。
#[allow(clippy::too_many_arguments)]
fn read_partition_mvd_pred(
    r: &mut CabacReader<'_>,
    edge: &mut RefMvdEdge,
    _grid: &MvGrid,
    bx: i32,
    by: i32,
    w: i32,
    h: i32,
    ref_idx: i8,
    mvp: [i16; 2],
) -> Result<[i16; 2], H264Error> {
    let dx = decode_mvd(r, CTX_MVD_X, edge.amvd(bx, by, 0))?;
    let dy = decode_mvd(r, CTX_MVD_Y, edge.amvd(bx, by, 1))?;
    let mv = [
        (i32::from(mvp[0]) + i32::from(dx)) as i16,
        (i32::from(mvp[1]) + i32::from(dy)) as i16,
    ];
    edge.set_rect(bx, by, w, h, ref_idx, [dx, dy], false);
    Ok(mv)
}

/// B 分区累积（同几何分区两列表分两次到达时合并）。
// 8 个参数（超 clippy 默认上限 1 个）：目标 Vec + 分区几何四元组 +
// (list, ref, mv) 三元组——与 [`MvPart`] 字段一一对应的累积原语，
// 收拢进结构体会让两个调用点各多拆一次包。
#[allow(clippy::too_many_arguments)]
fn push_b_part(
    parts: &mut Vec<MvPart>,
    bx: i32,
    by: i32,
    w: i32,
    h: i32,
    list: usize,
    ref_idx: i8,
    mv: [i16; 2],
) {
    for p in parts.iter_mut() {
        if p.bx == bx && p.by == by && p.w == w && p.h == h {
            if list == 0 {
                p.l0 = Some((ref_idx, mv));
            } else {
                p.l1 = Some((ref_idx, mv));
            }
            return;
        }
    }
    let mut p = MvPart {
        bx,
        by,
        w,
        h,
        l0: None,
        l1: None,
    };
    if list == 0 {
        p.l0 = Some((ref_idx, mv));
    } else {
        p.l1 = Some((ref_idx, mv));
    }
    parts.push(p);
}

/// 解析游标 → MB 的 mvd 存储。
fn write_mvd(mb: &mut Macroblock, edge0: &RefMvdEdge, edge1: Option<&RefMvdEdge>) {
    mb.mvd = edge0.cur_mvd;
    if let Some(e1) = edge1 {
        mb.mvd_l1 = e1.cur_mvd;
    }
}

/// 8x8 残差叠加（idct8）。
fn add_block8(plane: &mut [u8], stride: usize, y0: usize, x0: usize, block: &mut [i16; 64]) {
    let base = y0 * stride + x0;
    if block.iter().all(|&v| v == 0) {
        return;
    }
    if block[1..].iter().all(|&v| v == 0) {
        intra8x8::idct8x8_dc_add(&mut plane[base..], stride, block);
    } else {
        intra8x8::idct8x8_add(&mut plane[base..], stride, block);
    }
}

// ---------------------------------------------------------------------------
// 残差编排与 intra 重建（CABAC 路径；反量化/重建核复用共享层）
// ---------------------------------------------------------------------------

/// chroma AC 块 (plane, i) 的左/上 nnz。
// M2 WIP（CABAC 对拍收尾）：nnz 不可用哨兵语义改走 decode_residual_mb
// 内闭包（chroma_nnz）后保留的 Option 形态变体，收尾轮定去留。
#[allow(dead_code)]
fn chroma_nnz_pair(
    left_mb: Option<&Macroblock>,
    top_mb: Option<&Macroblock>,
    plane: usize,
    i: usize,
    cur_nnz: &[u8; 24],
) -> (Option<u8>, Option<u8>) {
    let base = 16 + plane * 4;
    let (cx, cy) = (i % 2, i / 2);
    let left = if cx > 0 {
        Some(cur_nnz[base + i - 1])
    } else {
        left_mb.map(|m| m.nnz[base + i + 1])
    };
    let top = if cy > 0 {
        Some(cur_nnz[base + i - 2])
    } else {
        top_mb.map(|m| m.nnz[base + i + 2])
    };
    (left, top)
}

/// MB 级残差解码（intra：I16 DC/AC 或 I4x4 4x4/8x8 + chroma；
/// inter：luma 4x4/8x8 + chroma，无 luma DC）。
///
/// 上下文口径（FFmpeg fill_decode_caches + get_cabac_cbf_ctx 逐字）：
/// - nnz 类（cat 1/2/4/5）：不可用邻块 → 64（当前 intra）/ 0（inter）；
/// - DC 编码位类（cat 0 luma DC、cat 3 chroma DC）：从邻块 cbp 全字
///   的位 8 / 位 6..7 取（可用）；不可用 → 0x7CF（intra）/0x00F（inter）
///   的对应位（intra → 1、inter → 0）。
#[allow(clippy::too_many_arguments)]
fn decode_residual_mb(
    r: &mut CabacReader<'_>,
    mb: &mut Macroblock,
    pps: &Pps,
    qp: u8,
    luma_blocks: &mut [[i16; 16]; 16],
    luma8_blocks: &mut [[i16; 64]; 4],
    chroma_blocks: &mut [[[i16; 16]; 4]; 2],
    left_mb: Option<&Macroblock>,
    top_mb: Option<&Macroblock>,
) -> Result<(), H264Error> {
    use crate::macroblock::ZIGZAG;
    let cur_intra = matches!(mb.kind, MbKind::Intra4x4 | MbKind::Intra16x16 | MbKind::Pcm);
    let lw = left_mb.map(cbp_word).unwrap_or(cbp_unavailable(cur_intra));
    let tw = top_mb.map(cbp_word).unwrap_or(cbp_unavailable(cur_intra));
    let nnz_unavail: u8 = if cur_intra && !mutation(5) { 64 } else { 0 };
    // 邻块 nnz 查询（luma 4x4 块 b 的左/上）
    let luma_nnz = |nnz: &[u8; 24], b: usize| -> (u8, u8) {
        let (cx, cy) = (b % 4, b / 4);
        let a = if cx > 0 {
            nnz[b - 1]
        } else {
            left_mb.map(|m| m.nnz[3 + cy * 4]).unwrap_or(nnz_unavail)
        };
        let c = if cy > 0 {
            nnz[b - 4]
        } else {
            top_mb.map(|m| m.nnz[12 + cx]).unwrap_or(nnz_unavail)
        };
        (a, c)
    };
    // chroma AC 块 (plane, i) 的左/上 nnz
    let chroma_nnz = |nnz: &[u8; 24], plane: usize, i: usize| -> (u8, u8) {
        let base = 16 + plane * 4;
        let (cx, cy) = (i % 2, i / 2);
        let a = if cx > 0 {
            nnz[base + i - 1]
        } else {
            left_mb.map(|m| m.nnz[base + i + 1]).unwrap_or(nnz_unavail)
        };
        let c = if cy > 0 {
            nnz[base + i - 2]
        } else {
            top_mb.map(|m| m.nnz[base + i + 2]).unwrap_or(nnz_unavail)
        };
        (a, c)
    };
    // 8x8 象限 cbf：象限左上 4x4 的左/上（象限内左/上属未解区 → 当前
    // MB 内部取 0——与 FFmpeg nnz 缓存对角位初值 0x40?? 不同：
    // FFmpeg 象限内角块读缓存扫描位（内部块已含当前 MB 未解码区 =
    // 缓存初值——FFmpeg 缓存预置 0x40?? 实为扫描位旁的内区块按已填
    // 缓存取—— CABAC 残差解码时内部块尚未解、缓存为初始复制值，
    // 其左/上恰为邻边值。此处按象限左上块的邻边语义：cx>0/cy>0 恒
    // false（象限左上块 cx=cy=0 in 象限），故取邻块/不可用哨兵。）
    let quad_nnz = |nnz: &[u8; 24], q: usize| -> (u8, u8) {
        let b = 8 * (q >> 1) + 2 * (q & 1);
        let (cx, cy) = (b % 4, b / 4);
        let a = if cx > 0 {
            nnz[b - 1]
        } else {
            left_mb.map(|m| m.nnz[3 + cy * 4]).unwrap_or(nnz_unavail)
        };
        let c = if cy > 0 {
            nnz[b - 4]
        } else {
            top_mb.map(|m| m.nnz[12 + cx]).unwrap_or(nnz_unavail)
        };
        (a, c)
    };

    let is_intra16 = mb.kind == MbKind::Intra16x16;
    let list = if matches!(mb.kind, MbKind::Intra4x4 | MbKind::Intra16x16) {
        0 // intra Y
    } else {
        3 // inter Y
    };
    // ---- luma ----
    if is_intra16 {
        // DC（cat 0；ctx = cbp 全字位 8）
        let nza = ((lw >> 8) & 1) as u8;
        let nzb = ((tw >> 8) & 1) as u8;
        let mut dc = [0i16; 16];
        let n = decode_residual(
            r,
            0,
            16,
            &ZIGZAG.map(|v| v as usize),
            None,
            (Some(nza), Some(nzb)),
            &mut dc,
        )?;
        mb.luma_dc_coded = n > 0;
        mb.nnz[0] = n as u8;
        let dcs = luma_dc_dequant(&dc, pps.dequant4_coeff(qp, 0)[0])?;
        for b in 0..16usize {
            luma_blocks[b][0] = dcs[b];
        }
        if mb.cbp & 0x0F != 0 {
            let qmul = pps.dequant4_coeff(qp, 1);
            for &b in Z_ORDER.iter() {
                let nnz_snap = mb.nnz;
                let (a, c) = luma_nnz(&nnz_snap, b);
                mb.nnz[b] = decode_residual(
                    r,
                    1,
                    15,
                    &ZIGZAG[1..].iter().map(|v| *v as usize).collect::<Vec<_>>(),
                    Some(&qmul),
                    (Some(a), Some(c)),
                    &mut luma_blocks[b],
                )? as u8;
            }
        }
    } else if mb.transform_8x8 {
        let qmul = pps.dequant8_coeff(qp, if list == 0 { 0 } else { 1 });
        for (q, blk) in luma8_blocks.iter_mut().enumerate() {
            if mb.cbp & (1 << q) != 0 {
                let (a, c) = quad_nnz(&mb.nnz, q);
                let n = decode_residual(
                    r,
                    5,
                    64,
                    &intra8x8::ZIGZAG8,
                    Some(&qmul),
                    (Some(a), Some(c)),
                    blk,
                )?;
                let nnz = n as u8;
                // 光栅位回填（FFmpeg fill_rectangle(&nnz_cache[scan8[4q]],
                // 2,2,8,coeff_count,1)：象限 q 覆盖光栅块 Z_ORDER[4q..4q+4]）。
                for k in 0..4usize {
                    mb.nnz[Z_ORDER[4 * q + k]] = nnz;
                }
            }
        }
    } else {
        let qmul = pps.dequant4_coeff(qp, list);
        for &b in Z_ORDER.iter() {
            let quadrant = 2 * (b >> 3) + ((b & 3) >> 1);
            if mb.cbp & (1 << quadrant) != 0 {
                let nnz_snap = mb.nnz;
                let (a, c) = luma_nnz(&nnz_snap, b);
                mb.nnz[b] = decode_residual(
                    r,
                    2,
                    16,
                    &ZIGZAG.iter().map(|v| *v as usize).collect::<Vec<_>>(),
                    Some(&qmul),
                    (Some(a), Some(c)),
                    &mut luma_blocks[b],
                )? as u8;
            }
        }
    }
    // ---- chroma DC（cat 3；ctx = cbp 全字位 6/7）----
    let qpc_cb = CHROMA_QP[(i32::from(qp) + pps.chroma_qp_offset(0)).clamp(0, 51) as usize];
    let qpc_cr = CHROMA_QP[(i32::from(qp) + pps.chroma_qp_offset(1)).clamp(0, 51) as usize];
    if mb.cbp & 0x30 != 0 {
        for (plane, cblocks) in chroma_blocks.iter_mut().enumerate() {
            let nza = ((lw >> (6 + plane)) & 1) as u8;
            let nzb = ((tw >> (6 + plane)) & 1) as u8;
            let mut buf = [0i16; 16];
            let n = decode_residual(
                r,
                3,
                4,
                // 2x2 光栅序（FFmpeg ff_h264_chroma_dc_scan {0,1,2,3}；
                // 2x2 的 zigzag 即光栅）。
                &[0usize, 1, 2, 3],
                None,
                (Some(nza), Some(nzb)),
                &mut buf,
            )?;
            mb.nnz_chroma_dc[plane] = n as u8;
            let x = [buf[0], buf[1], buf[4], buf[5]];
            let qpc = if plane == 0 { qpc_cb } else { qpc_cr };
            let dcs = chroma_dc_dequant(&x, pps.dequant4_coeff(qpc, 1 + plane)[0])?;
            for (cblk, &dc) in cblocks.iter_mut().zip(dcs.iter()) {
                cblk[0] = dc;
            }
        }
    }
    // ---- chroma AC（cat 4）----
    if mb.cbp & 0x20 != 0 {
        for (plane, cblocks) in chroma_blocks.iter_mut().enumerate() {
            let qpc = if plane == 0 { qpc_cb } else { qpc_cr };
            let qmul = pps.dequant4_coeff(qpc, 1 + plane);
            for (i, cblk) in cblocks.iter_mut().enumerate() {
                let base = 16 + plane * 4;
                let nnz_snap = mb.nnz;
                let (a, c) = chroma_nnz(&nnz_snap, plane, i);
                mb.nnz[base + i] = decode_residual(
                    r,
                    4,
                    15,
                    &ZIGZAG[1..].iter().map(|v| *v as usize).collect::<Vec<_>>(),
                    Some(&qmul),
                    (Some(a), Some(c)),
                    cblk,
                )? as u8;
            }
        }
    }
    Ok(())
}

/// intra 重建（I16/I4x4/8x8 + chroma；残差叠加走共享核）。
#[allow(clippy::too_many_arguments)]
fn reconstruct_intra(
    pic: &mut Picture,
    mbx: usize,
    mby: usize,
    avail: &crate::macroblock::NeighAvail,
    mb: &mut Macroblock,
    luma_blocks: &mut [[i16; 16]; 16],
    luma8_blocks: &mut [[i16; 64]; 4],
    chroma_blocks: &mut [[[i16; 16]; 4]; 2],
) -> Result<(), H264Error> {
    let ys = pic.luma_stride();
    match mb.kind {
        MbKind::Intra4x4 if mb.transform_8x8 => {
            for (q, blk) in luma8_blocks.iter_mut().enumerate() {
                intra8x8::reconstruct_8x8(pic, mbx, mby, q, mb.i4_modes[q * 4], blk);
            }
        }
        MbKind::Intra4x4 => {
            for &b in Z_ORDER.iter() {
                reconstruct_4x4(pic, mbx, mby, b, avail, mb.i4_modes[b], &mut luma_blocks[b]);
            }
        }
        MbKind::Intra16x16 => {
            let (top, left, tl) = gather_16x16_edges(pic, mbx, mby, avail);
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
        _ => {}
    }
    let cs = pic.chroma_stride();
    for (plane, data) in [&mut pic.cb, &mut pic.cr].iter_mut().enumerate() {
        let (top, left, tl) = gather_chroma_edges(data, cs, mbx, mby, avail);
        let base = mby * 8 * cs + mbx * 8;
        pred_8x8c(mb.chroma_mode, &top, &left, tl, &mut data[base..], cs);
        for (i, cblk) in chroma_blocks[plane].iter_mut().enumerate() {
            add_block(data, cs, mby * 8 + (i / 2) * 4, mbx * 8 + (i % 2) * 4, cblk);
        }
    }
    Ok(())
}
