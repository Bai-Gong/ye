//! B 片共享层：帧间上下文、双列表 MV 预测、直接预测、双向 MC。
//!
//! 语义逐字对齐 FFmpeg：
//! - 参考列表初始序（h264_refs.c `add_sorted`/`h264_initialise_ref_list`：
//!   L0 = 过去最近→远、未来近→远；L1 = 未来近→远、过去近→远；
//!   两列相同且 >1 时 L1[0]/[1] 互换）；
//! - 直接预测（h264_direct.c `pred_spatial_direct_motion` /
//!   `pred_temp_direct_motion`，progressive 帧图分支；
//!   dist_scale_factor = `get_scale_factor`、map_col_to_list0 =
//!   `fill_colmap`）；
//! - MV 预测复用 macroblock.rs 的 pred_motion 族（列表内语义不变，
//!   跨列表邻块按 LIST_NOT_USED 参与）。
//!
//! 全库约定：无 unsafe、无 panic；数据驱动失败一律 Result。

use std::sync::Arc;

use crate::error::H264Error;
use crate::macroblock::{
    InterShape, LIST_NOT_USED, MbKind, MvGrid, MvNb, PART_NOT_AVAILABLE, Picture, mc_partition,
    mid3,
};
use crate::mc;

/// 帧间片上下文（P/B 通用；B 专属字段仅 B 片填充）。
pub(crate) struct InterCtx<'a> {
    /// List0（已含默认序 + 重排）。
    pub l0: &'a [Arc<Picture>],
    /// 加权预测表（恒等权重时 MC 走原路径）。
    pub wp: crate::slice::WpTables,
    /// PPS weighted_bipred_idc=2：按两参考图与当前图 POC 推导权重。
    pub implicit_bipred: bool,
    /// List1（B 片）。
    pub l1: &'a [Arc<Picture>],
    /// constrained_intra_pred_flag。
    pub constrained: bool,
    /// direct_8x8_inference_flag（8x8 变换门）。
    pub direct_8x8_inference: bool,
    /// 直接预测模式（direct_spatial_mv_pred_flag）。
    pub direct_spatial: bool,
    /// 当前图 POC。
    // M2 WIP（B 帧收尾）：POC 类直接预测（POC 型 spatial direct 的
    // dist_scale 换算）接线在即，写入方 decoder.rs 已就位。
    #[allow(dead_code)]
    pub cur_poc: i32,
    /// 共置图（List1[0]）。
    pub colocated: Option<Arc<Picture>>,
    /// 共置图是否长期参考（col_zero_flag 门控）。
    pub col_is_long: bool,
    /// 当前图参考 POC 表（解码该图时的两列 POC 快照，供后续共置缩放）。
    /// `[list][ref_idx]`。
    // M2 WIP（B 帧收尾）：temporal direct 的 dist_scale/map_col 消费方
    // 接线在即，写入方 decoder.rs 已就位。
    #[allow(dead_code)]
    pub ref_poc: [[i32; 32]; 2],
    /// temporal direct 的 list0 参考缩放因子（`dist_scale_factor[ref0]`）。
    pub dist_scale: [i32; 32],
    /// temporal direct 的共置参考映射：`[list][col_ref_idx]` → 当前
    /// list0 下标（无匹配 → 0；FFmpeg fill_colmap）。
    pub map_col: [[usize; 32]; 2],
}

/// 分区描述（语法层解析产物；MC 应用与 MB 存储的公共形态）。
#[derive(Clone, Copy, Debug)]
pub(crate) struct MvPart {
    /// 4x4 块锚点（宏块内）。
    pub bx: i32,
    pub by: i32,
    /// 尺寸（4x4 块数）。
    pub w: i32,
    pub h: i32,
    /// list0 (ref, mv)——None = 该分区不用 list0。
    pub l0: Option<(i8, [i16; 2])>,
    /// list1 (ref, mv)。
    pub l1: Option<(i8, [i16; 2])>,
}

/// 跨块取 4x4 预测项（B 感知：邻块按其列表使用情况给出真实值或
/// LIST_NOT_USED；FFmpeg fill_decode_caches 的双列表语义）。
fn mv_entry_b(mb: &crate::macroblock::Macroblock, b: usize, list: u8) -> MvNb {
    use crate::macroblock::MbKind;
    if !mb.decoded {
        return MvNb::PART;
    }
    match mb.kind {
        MbKind::InterP => {
            if list == 0 {
                MvNb {
                    mv: mb.mv[b],
                    ref_idx: mb.ref_idx[b],
                }
            } else {
                MvNb::UNUSED
            }
        }
        MbKind::InterB => {
            let (mv, ri) = if list == 0 {
                (mb.mv[b], mb.ref_idx[b])
            } else {
                (mb.mv_l1[b], mb.ref_idx_l1[b])
            };
            if ri < 0 {
                MvNb::UNUSED
            } else {
                MvNb { mv, ref_idx: ri }
            }
        }
        _ => MvNb::UNUSED, // intra/PCM：LIST_NOT_USED + 0
    }
}

/// 构建 MB 起始的某列表 MV 网格（B 感知版 [`build_mv_grid`]）。
pub(crate) fn build_mv_grid_list(pic: &Picture, mbx: usize, mby: usize, list: u8) -> MvGrid {
    let mut cells = [[MvNb::PART; 6]; 5];
    let inb = |x: i32, y: i32| -> bool {
        x >= 0 && y >= 0 && (x as usize) < pic.mb_width && (y as usize) < pic.mb_height
    };
    let at = |x: i32, y: i32, b: usize| -> MvNb {
        if inb(x, y) {
            mv_entry_b(&pic.mbs[y as usize * pic.mb_width + x as usize], b, list)
        } else {
            MvNb::PART
        }
    };
    for y in 0..4i32 {
        cells[(y + 1) as usize][0] = at(mbx as i32 - 1, mby as i32, 3 + y as usize * 4);
    }
    for x in 0..4i32 {
        cells[0][(x + 1) as usize] = at(mbx as i32, mby as i32 - 1, x as usize + 12);
    }
    cells[0][5] = at(mbx as i32 + 1, mby as i32 - 1, 12);
    cells[0][0] = at(mbx as i32 - 1, mby as i32 - 1, 15);
    MvGrid { cells }
}

// ---------------------------------------------------------------------------
// 直接预测（FFmpeg h264_direct.c，progressive 帧图）
// ---------------------------------------------------------------------------

/// 共置宏块的帧间形态（共置形状推断用）。
fn col_shape(mb: &crate::macroblock::Macroblock) -> InterShape {
    mb.shape
}

/// 空间直接预测：`is8x8` = B_8x8 语境（象限粒度），否则 16x16 语境。
/// 填充 `parts`（16x16 语境 = 单分区覆盖全 MB；8x8 语境 = 四象限各一，
/// 直接象限由本函数给出分区；返回值 = 象限是否全部零 MV（可整体
/// 视作 16x16 零——FFmpeg mb_type 折叠，仅影响内部记录，不影响像素）。
pub(crate) fn pred_spatial_direct(
    ctx: &InterCtx<'_>,
    pic: &Picture,
    mbx: usize,
    mby: usize,
    is8x8: bool,
    parts: &mut Vec<MvPart>,
) -> Result<(), H264Error> {
    // ---- ref = min(邻)（FFmpeg FFMIN3，PART_NOT_AVAILABLE 视作大值）----
    let mut refs = [0i32; 2];
    let mut mvs = [[0i16; 2]; 2];
    for list in 0..2usize {
        let grid = build_mv_grid_list(pic, mbx, mby, list as u8);
        let left = grid.get(-1, 0);
        let top = grid.get(0, -1);
        let mut c = grid.get(4, -1); // 右上；不可得退左上
        if c.ref_idx == PART_NOT_AVAILABLE {
            c = grid.get(-1, -1);
        }
        let lr = left.ref_idx;
        let tr = top.ref_idx;
        let cr = c.ref_idx;
        // FFMIN3（PART=-2 视作无符号大值参与 min；未用 -1 同样大）
        let pick = |a: i8, b: i8, c: i8| -> i8 {
            let norm = |v: i8| if v < 0 { 32 } else { v };
            let (a, b, c) = (norm(a), norm(b), norm(c));
            a.min(b).min(c)
        };
        let r = pick(lr, tr, cr);
        if (0..32).contains(&r) {
            let mc = usize::from(lr == r) + usize::from(tr == r) + usize::from(cr == r);
            let mv: [i16; 2] = if mc > 1 {
                [
                    mid3(left.mv[0], top.mv[0], c.mv[0]),
                    mid3(left.mv[1], top.mv[1], c.mv[1]),
                ]
            } else if lr == r {
                left.mv
            } else if tr == r {
                top.mv
            } else {
                c.mv
            };
            refs[list] = i32::from(r);
            mvs[list] = mv;
        } else {
            refs[list] = -1;
            mvs[list] = [0; 2];
        }
    }
    if refs[0] < 0 && refs[1] < 0 {
        refs[0] = 0;
        refs[1] = 0;
        mvs[0] = [0; 2];
        mvs[1] = [0; 2];
    }

    // ---- 共置信息 ----
    let Some(col) = ctx.colocated.clone() else {
        return Err(H264Error::InvalidData(
            "spatial direct without colocated".into(),
        ));
    };
    let mb_xy = mby * pic.mb_width + mbx;
    let col_mb = &col.mbs[mb_xy];
    let col_is16 = matches!(col_shape(col_mb), InterShape::P16x16 | InterShape::None)
        || col_mb.kind == MbKind::Intra4x4
        || col_mb.kind == MbKind::Intra16x16
        || col_mb.kind == MbKind::Pcm;

    let quad_part = |x8: i32, y8: i32, zero_flag: bool| -> MvPart {
        /// 单列表的 (ref, mv) 贡献。
        type ListMv = Option<(i8, [i16; 2])>;
        let value = |list: usize| -> ListMv {
            (refs[list] >= 0).then_some((
                refs[list] as i8,
                if zero_flag && refs[list] == 0 {
                    [0; 2]
                } else {
                    mvs[list]
                },
            ))
        };
        let (a, b) = (value(0), value(1));
        MvPart {
            bx: x8 * 2,
            by: y8 * 2,
            w: 2,
            h: 2,
            l0: a,
            l1: b,
        }
    };

    if !is8x8 && col_is16 {
        // 16x16 直接：共置 16x16/intra → col_zero 判定按 l1mv0[0]
        let zero = col_zero_flag(col_mb, 0, ctx.col_is_long);
        parts.push(quad_part(0, 0, zero));
        // 覆盖全 MB（w/h 扩到 4x4）
        if let Some(last) = parts.last_mut() {
            last.bx = 0;
            last.by = 0;
            last.w = 4;
            last.h = 4;
        }
        return Ok(());
    }
    // 象限粒度（8x8 语境，或共置非 16x16 的 16x16 语境）
    for q in 0..4usize {
        let x8 = (q & 1) as i32;
        let y8 = (q >> 1) as i32;
        let zero = col_zero_flag(col_mb, quad_center_block(x8, y8), ctx.col_is_long);
        parts.push(quad_part(x8, y8, zero));
    }
    Ok(())
}

/// col_zero_flag（FFmpeg：共置非 intra 且非长期参考，且
/// l1ref0==0 且 |mv|≤1，或 l1ref0<0 且 l1ref1==0 且 |mv1|≤1）。
/// `b`：读取共置 mv/ref 的 4x4 块下标（象限用中心块
/// `8*y8+2*x8+5`——FFmpeg `x8*3+y8*3*b4_stride` 的中心索引语义；
/// ref 物理粒度 8x8，块内任意下标同值）。
fn col_zero_flag(col_mb: &crate::macroblock::Macroblock, b: usize, col_is_long: bool) -> bool {
    use crate::macroblock::MbKind;
    if col_is_long
        || matches!(
            col_mb.kind,
            MbKind::Intra4x4 | MbKind::Intra16x16 | MbKind::Pcm
        )
    {
        return false;
    }
    let mv = col_mb.mv[b];
    let ref0 = col_mb.ref_idx[b];
    if ref0 == 0 && mv[0].abs() <= 1 && mv[1].abs() <= 1 {
        return true;
    }
    let mv1 = col_mb.mv_l1[b];
    let ref1 = col_mb.ref_idx_l1[b];
    ref0 < 0 && ref1 == 0 && mv1[0].abs() <= 1 && mv1[1].abs() <= 1
}

/// 象限 (x8, y8) 的中心 4x4 块下标（光栅）。
fn quad_center_block(x8: i32, y8: i32) -> usize {
    (12 * y8 + 3 * x8) as usize
}

/// 时间直接预测（progressive 帧图；FFmpeg pred_temp_direct_motion）。
/// 恒按象限（direct_8x8_inference=1 语义；共置 ≥8x8 分区时四象限读
/// 同值，与 FFmpeg 的 16x16/16x8/8x16 形状折叠像素等价），共置 mv/ref
/// 取象限中心 4x4 块（FFmpeg `x8*3+y8*3*b4_stride` 中心索引）。
pub(crate) fn pred_temp_direct(
    ctx: &InterCtx<'_>,
    pic: &Picture,
    mbx: usize,
    mby: usize,
    is8x8: bool,
    parts: &mut Vec<MvPart>,
) -> Result<(), H264Error> {
    let _ = is8x8; // 形状折叠仅影响记账，像素与象限等价
    let Some(col) = ctx.colocated.clone() else {
        return Err(H264Error::InvalidData(
            "temporal direct without colocated".into(),
        ));
    };
    let mb_xy = mby * pic.mb_width + mbx;
    let col_mb = &col.mbs[mb_xy];
    use crate::macroblock::MbKind;
    let col_intra = matches!(
        col_mb.kind,
        MbKind::Intra4x4 | MbKind::Intra16x16 | MbKind::Pcm
    );

    for q in 0..4usize {
        let x8 = (q & 1) as i32;
        let y8 = (q >> 1) as i32;
        let b = quad_center_block(x8, y8);
        if col_intra {
            parts.push(MvPart {
                bx: x8 * 2,
                by: y8 * 2,
                w: 2,
                h: 2,
                l0: Some((0, [0; 2])),
                l1: Some((0, [0; 2])),
            });
            continue;
        }
        // 共置参考：优先 list0，否则 list1（FFmpeg l1ref0 ≥ 0 ? … : …）
        let (ref0, mv_col): (i32, [i16; 2]) = if col_mb.ref_idx[b] >= 0 {
            let col_ref = col_mb.ref_idx[b] as usize;
            (ctx.map_col[0][col_ref.min(31)] as i32, col_mb.mv[b])
        } else {
            let col_ref = col_mb.ref_idx_l1[b].max(0) as usize;
            (ctx.map_col[1][col_ref.min(31)] as i32, col_mb.mv_l1[b])
        };
        let scale = ctx.dist_scale[ref0.clamp(0, 31) as usize];
        let mx = ((i64::from(scale) * i64::from(mv_col[0]) + 128) >> 8) as i16;
        let my = ((i64::from(scale) * i64::from(mv_col[1]) + 128) >> 8) as i16;
        parts.push(MvPart {
            bx: x8 * 2,
            by: y8 * 2,
            w: 2,
            h: 2,
            l0: Some((ref0.clamp(0, 31) as i8, [mx, my])),
            l1: Some((0, [mx - mv_col[0], my - mv_col[1]])),
        });
    }
    Ok(())
}

// M2 WIP（B 帧收尾）：16x16 形态判定现走 pred_spatial_direct 内联的
// col_is16（含 None 形状），本变体留待 16x8/8x16 共置折叠接线时定去留。
#[allow(dead_code)]
fn col_is16ish(mb: &crate::macroblock::Macroblock) -> bool {
    matches!(mb.shape, InterShape::P16x16)
        || matches!(mb.kind, MbKind::Intra4x4 | MbKind::Intra16x16 | MbKind::Pcm)
}

/// dist_scale_factor（FFmpeg get_scale_factor 逐字；progressive 帧）：
/// `td = clip8(col_poc − ref0_poc)`；td==0 或长期参考 → 256；
/// `tx = (16384 + |td|/2)/td`；`scale = clip10((tb·tx + 32) >> 6)`。
pub(crate) fn scale_factor(cur_poc: i32, col_poc: i32, ref0_poc: i32, ref0_is_long: bool) -> i32 {
    let pocdiff = i64::from(col_poc) - i64::from(ref0_poc);
    let td = (pocdiff.clamp(-128, 127)) as i32;
    if td == 0 || ref0_is_long {
        256
    } else {
        let pocdiff0 = i64::from(cur_poc) - i64::from(ref0_poc);
        let tb = pocdiff0.clamp(-128, 127) as i32;
        let tx = (16384 + (td.unsigned_abs() >> 1)) as i32 / td;
        ((i64::from(tb) * i64::from(tx) + 32) >> 6).clamp(-1024, 1023) as i32
    }
}

/// B 片默认列表构建（FFmpeg add_sorted 语义；返回 (l0, l1)）。
/// `short`：短窗（最新在前）＋ `(pic, poc)`；`long`：按索引。
pub(crate) fn build_b_lists(
    short: &[(Arc<Picture>, i32)],
    long: &[Option<Arc<Picture>>],
    cur_poc: i32,
    max_refs: usize,
) -> (Vec<Arc<Picture>>, Vec<Arc<Picture>>) {
    let build = |future_first: bool| -> Vec<Arc<Picture>> {
        let mut out: Vec<Arc<Picture>> = Vec::new();
        // 阶段①：按方向取近侧（升序）
        let dir_pick = |want_future: bool, out: &mut Vec<Arc<Picture>>| {
            loop {
                // 选满足方向且最接近 cur 的（未来取最小 POC、过去取最大）
                let mut best: Option<(i32, usize)> = None;
                for (i, (_, poc)) in short.iter().enumerate() {
                    if out.len() >= max_refs {
                        break;
                    }
                    let used = out.iter().any(|p| Arc::ptr_eq(p, &short[i].0));
                    if used {
                        continue;
                    }
                    let is_future = *poc > cur_poc;
                    if is_future != want_future {
                        continue;
                    }
                    let better = match best {
                        None => true,
                        Some((bp, _)) => {
                            if want_future {
                                *poc < bp
                            } else {
                                *poc > bp
                            }
                        }
                    };
                    if better {
                        best = Some((*poc, i));
                    }
                }
                let Some((_, i)) = best else { break };
                if out.len() < max_refs {
                    out.push(short[i].0.clone());
                } else {
                    break;
                }
            }
        };
        dir_pick(future_first, &mut out);
        dir_pick(!future_first, &mut out);
        // 阶段②：长窗按索引
        for l in long {
            if out.len() >= max_refs {
                break;
            }
            if let Some(p) = l
                && !out.iter().any(|q| Arc::ptr_eq(q, p))
            {
                out.push(p.clone());
            }
        }
        // 阶段③：短窗余量按窗序补足（FFmpeg build_def_list 的短窗补位）
        for (p, _) in short {
            if out.len() >= max_refs {
                break;
            }
            if !out.iter().any(|q| Arc::ptr_eq(q, p)) {
                out.push(p.clone());
            }
        }
        out
    };
    let l0 = build(false); // L0：过去最近优先，再未来近优先
    let mut l1 = build(true); // L1：未来近优先，再过去近优先
    // FFmpeg：两列相同且 >1 → L1[0]/[1] 互换
    if l0.len() == l1.len()
        && l1.len() > 1
        && l0.iter().zip(l1.iter()).all(|(a, b)| Arc::ptr_eq(a, b))
    {
        l1.swap(0, 1);
    }
    (l0, l1)
}

/// B_Skip / B_Direct_16x16 的整块直接预测 + MC + MB 落库
/// （无残差路径；残差路径由语法层在 direct 之后叠加）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_direct_mb(
    ctx: &InterCtx<'_>,
    pic: &mut Picture,
    mbx: usize,
    mby: usize,
    slice_id: u16,
    qp: u8,
) -> Result<(), H264Error> {
    let mut parts = Vec::new();
    if ctx.direct_spatial {
        pred_spatial_direct(ctx, pic, mbx, mby, false, &mut parts)?;
    } else {
        pred_temp_direct(ctx, pic, mbx, mby, false, &mut parts)?;
    }
    apply_parts(ctx, pic, mbx, mby, &parts)?;
    let mb_xy = mby * pic.mb_width + mbx;
    let mut mb = crate::macroblock::Macroblock {
        decoded: false,
        slice_id,
        kind: MbKind::InterB,
        qp,
        shape: InterShape::P16x16,
        ..crate::macroblock::Macroblock::default()
    };
    write_parts(&mut mb, &parts);
    mb.skipped = true;
    mb.decoded = true;
    pic.mbs[mb_xy] = mb;
    Ok(())
}

/// 分区 MC 应用（list0 put + list1 avg；FFmpeg h264_mb.c 双向语义）。
pub(crate) fn apply_parts(
    ctx: &InterCtx<'_>,
    pic: &mut Picture,
    mbx: usize,
    mby: usize,
    parts: &[MvPart],
) -> Result<(), H264Error> {
    for p in parts {
        // list0：put（首分区覆盖写）
        if let Some((ri, mv)) = p.l0 {
            let list = ctx.l0;
            let refp = list
                .get(ri as usize)
                .ok_or_else(|| H264Error::InvalidData(format!("ref_idx l0 {ri} out of range")))?
                .clone();
            mc_partition(pic, &refp, mbx, mby, p.bx, p.by, p.w, p.h, mv)?;
            if !ctx.wp.is_identity() {
                let w = ctx.wp.l0.get(ri as usize).copied().unwrap_or_default();
                weight_partition(pic, mbx, mby, p.bx, p.by, p.w, p.h, w, &ctx.wp);
            }
        }
        // list1：avg（与已写结果平均）；仅 list1（无 list0）时先 put
        if let Some((ri, mv)) = p.l1 {
            let list = ctx.l1;
            let refp = list
                .get(ri as usize)
                .ok_or_else(|| H264Error::InvalidData(format!("ref_idx l1 {ri} out of range")))?
                .clone();
            if let Some((ri0, _)) = p.l0 {
                // B 双向显式加权（idc==2）需要 biweight 组合——本路径覆盖
                // idc==0 的 rnd 平均（x264 默认）；显式加权流会在此前被拒。
                let mut w1 = 32;
                let ref0 = &ctx.l0[ri0 as usize];
                if ctx.implicit_bipred && !ref0.is_long_term && !refp.is_long_term {
                    let td = (refp.poc - ref0.poc).clamp(-128, 127);
                    let tb = (ctx.cur_poc - ref0.poc).clamp(-128, 127);
                    if td != 0 {
                        let tx = (16384 + (td.abs() >> 1)) / td;
                        let scale = (tb * tx + 32) >> 8;
                        if (-64..=128).contains(&scale) {
                            w1 = scale;
                        }
                    }
                }
                mc_partition_avg(pic, &refp, mbx, mby, p.bx, p.by, p.w, p.h, mv, w1)?;
            } else {
                mc_partition(pic, &refp, mbx, mby, p.bx, p.by, p.w, p.h, mv)?;
                if !ctx.wp.is_identity() {
                    let w = ctx.wp.l1.get(ri as usize).copied().unwrap_or_default();
                    weight_partition(pic, mbx, mby, p.bx, p.by, p.w, p.h, w, &ctx.wp);
                }
            }
        }
    }
    Ok(())
}

/// 分区区域加权（FFmpeg h264weight `weight_h264_pixels` 逐字：
/// dst = clip((dst·w + (o<<ld) + (1<<(ld−1))) >> ld)）。
#[allow(clippy::too_many_arguments)]
fn weight_partition(
    pic: &mut Picture,
    mbx: usize,
    mby: usize,
    bx: i32,
    by: i32,
    w: i32,
    h: i32,
    wt: crate::slice::WpWeights,
    tab: &crate::slice::WpTables,
) {
    let ls = pic.luma_stride();
    let cs = pic.chroma_stride();
    let lx = (mbx as i32 * 16 + bx * 4) as usize;
    let ly = (mby as i32 * 16 + by * 4) as usize;
    let (wpx, hpx) = ((w * 4) as usize, (h * 4) as usize);
    let apply = |plane: &mut [u8],
                 stride: usize,
                 base: usize,
                 pw: usize,
                 ph: usize,
                 (wt, off): (i32, i32),
                 ld: u32| {
        if wt == (1i32 << ld) && off == 0 {
            return;
        }
        let bias = if ld > 0 {
            (off << ld) + (1 << (ld - 1))
        } else {
            off
        };
        for y in 0..ph {
            let row = &mut plane[base + y * stride..base + y * stride + pw];
            for px in row.iter_mut() {
                *px = ((i32::from(*px) * wt + bias) >> ld).clamp(0, 255) as u8;
            }
        }
    };
    apply(
        &mut pic.luma,
        ls,
        ly * ls + lx,
        wpx,
        hpx,
        wt.y,
        tab.luma_log2_denom,
    );
    let (cx, cy) = (lx / 2, ly / 2);
    let cbase = cy * cs + cx;
    apply(
        &mut pic.cb,
        cs,
        cbase,
        wpx / 2,
        hpx / 2,
        wt.u,
        tab.chroma_log2_denom,
    );
    apply(
        &mut pic.cr,
        cs,
        cbase,
        wpx / 2,
        hpx / 2,
        wt.v,
        tab.chroma_log2_denom,
    );
}

/// avg 变体分区 MC（luma/chroma 均与现值 rnd 平均）。
#[allow(clippy::too_many_arguments)]
fn mc_partition_avg(
    pic: &mut Picture,
    refp: &Picture,
    mbx: usize,
    mby: usize,
    bx: i32,
    by: i32,
    w: i32,
    h: i32,
    mv: [i16; 2],
    w1: i32,
) -> Result<(), H264Error> {
    let lx = (mbx as i32 * 16 + bx * 4) as usize;
    let ly = (mby as i32 * 16 + by * 4) as usize;
    let qx = lx as i64 * 4 + i64::from(mv[0]);
    let qy = ly as i64 * 4 + i64::from(mv[1]);
    let ls = pic.luma_stride();
    let cs = pic.chroma_stride();
    for (dst, src, stride, src_stride, sw, sh, unit, chroma) in [
        (
            &mut pic.luma,
            &refp.luma,
            ls,
            refp.luma_stride(),
            refp.mb_width * 16,
            refp.mb_height * 16,
            4usize,
            false,
        ),
        (
            &mut pic.cb,
            &refp.cb,
            cs,
            refp.chroma_stride(),
            refp.mb_width * 8,
            refp.mb_height * 8,
            2usize,
            true,
        ),
        (
            &mut pic.cr,
            &refp.cr,
            cs,
            refp.chroma_stride(),
            refp.mb_width * 8,
            refp.mb_height * 8,
            2usize,
            true,
        ),
    ] {
        let (pw, ph) = (w as usize * unit, h as usize * unit);
        let mut tmp = [0u8; 256];
        if chroma {
            mc::mc_chroma_put(&mut tmp, pw, src, src_stride, sw, sh, qx, qy, pw, ph);
        } else {
            mc::mc_luma_put(&mut tmp, pw, src, src_stride, sw, sh, qx, qy, pw, ph);
        }
        let base =
            (mby * 4 * unit + by as usize * unit) * stride + mbx * 4 * unit + bx as usize * unit;
        for y in 0..ph {
            for x in 0..pw {
                let i = base + y * stride + x;
                dst[i] = ((i32::from(dst[i]) * (64 - w1) + i32::from(tmp[y * pw + x]) * w1 + 32)
                    >> 6)
                    .clamp(0, 255) as u8;
            }
        }
    }
    Ok(())
}

/// 把分区写入 Macroblock（mv/ref 双列表 + 形状）。
pub(crate) fn write_parts(mb: &mut crate::macroblock::Macroblock, parts: &[MvPart]) {
    for p in parts {
        for yy in p.by..p.by + p.h {
            for xx in p.bx..p.bx + p.w {
                let b = (yy * 4 + xx) as usize;
                match p.l0 {
                    Some((ri, mv)) => {
                        mb.mv[b] = mv;
                        mb.ref_idx[b] = ri;
                    }
                    None => {
                        mb.ref_idx[b] = LIST_NOT_USED;
                    }
                }
                match p.l1 {
                    Some((ri, mv)) => {
                        mb.mv_l1[b] = mv;
                        mb.ref_idx_l1[b] = ri;
                    }
                    None => {
                        mb.ref_idx_l1[b] = LIST_NOT_USED;
                    }
                }
            }
        }
    }
}
