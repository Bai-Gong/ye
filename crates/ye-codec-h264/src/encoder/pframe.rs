//! P 帧编码全链（M3 施工图 T4）：运动估计 → 三模式决策 → MVD 编码 →
//! 本地解码环（MC + 残差 + 去块）。
//!
//! ## 模式决策（每宏块，Baseline 收紧为三种）
//!
//! 1. **P_Skip 试判先行**：按 [`pred_pskip_mirror`]（`macroblock.rs`
//!    `pred_pskip` 的镜像）取 skip MV，MC 出预测块后走一遍完整量化，
//!    **量化残差全零（cbp == 0）即选 skip**——0 比特且重建 = MC 输出，
//!    严格最优；重建直接调用解码侧 [`decode_skip_mb`]（MV/记录逐位
//!    同源）。
//! 2. **P_L0_16x16**：[`crate::encoder::motion`] 的整像素 SAD 穷举 +
//!    半像素细化（预测块一律经 `mc.rs` 冻结核生成，决策与重建同一块）；
//!    MVD = MV − mvp，mvp 经解码侧 [`build_mv_grid`]/[`pred_motion`]
//!    （16x16 分区、单参考 ref_idx=0 → te(v) 零位）。
//! 3. **宏块级 intra 回退**：inter 总代价（luma SAD + chroma SAD +
//!    [`INTER_BIAS`]）劣于 intra 总代价（[`decide_mb_modes`] 的
//!    min(I16 SAD, I4 SATD + I4_BIAS) + chroma SAD）时走
//!    [`encode_intra_mb`]（P 片 mb_type 一律 +5）。
//!
//! ## 码流形态（T5 定形：参数集可配 level，QP 走片头）
//!
//! SPS/PPS 与 I 帧共用（[`write_sps_nal`]/[`write_pps_nal`]：Baseline、
//! poc_type=2、单参考帧、CAVLC、无去块控制字段、PPS QP 中立）；P 片头
//! 仅多四个字段（参考数量覆盖位/列表重排位/自适应标记位——全 false，
//! 见 [`write_p_slice_header`]）。NAL 类型 1、ref_idc 3（每帧都进参考窗，
//! 解码器滑动窗口 + `max_num_ref_frames=1` ⇒ 参考恒为上一帧，与
//! [`BaselineSession`] 的单参考镜像一致）。帧号 `(prev + 1) mod 16`，
//! 无断号（解码器对断号收紧为 Unsupported）。
//!
//! ## 本地解码环（与解码器逐位同源的硬要求）
//!
//! 宏块光栅序：试判/决策 → MC 写预测（[`mc_partition`]）→ 残差量化 →
//! `add_block` 叠加 → 记录 mv/ref/nnz（下一宏块的预测子与 nC 来源）→
//! 语法写出（`decode_inter_mb` 解析序镜像：mb_skip_run 由行进中的
//! `pending_skip` 计数在非 skip 宏块前 flush，帧尾余量补一发）。
//! 每行完成即 [`deblock_row`]（滞后调度与解码器同构）。参考帧 = 上一帧
//! 的**完整去块重建**（宏块对齐 `Picture`，与解码器 DPB 里的参考同
//! 形态）。

use std::sync::Arc;

use crate::encoder::bitwriter::BitWriter;
use crate::encoder::frame::{
    FIXED_LOG2_MAX_FRAME_NUM, MbWork, NbInfo, YuvSource, crop_plane, deblock_row, encode_intra_mb,
    nal_bytes, nb_info, pad_plane, write_chroma_residuals, write_luma_ac_quadrants,
};
use crate::encoder::intra::{I4_BIAS, decide_mb_modes};
use crate::encoder::mb::{SliceKind, write_coded_block_pattern, write_mb_qp_delta};
use crate::encoder::motion::{check_range, chroma_sad, search_motion};
use crate::encoder::transform::{
    check_qp, dequant_ac, fwd_dct4x4, fwd_hadamard2x2, quant_ac, quant_chroma_dc,
};
use crate::error::H264Error;
use crate::inter::InterCtx;
use crate::macroblock::{
    CHROMA_QP, Macroblock, MbKind, MvNb, PART_NOT_AVAILABLE, Picture, Z_ORDER, ZIGZAG, add_block,
    build_mv_grid, chroma_dc_dequant, dc_qmul, decode_skip_mb, dequant4_coeff, mc_partition, mid3,
    mv_entry, neighbor_avail, pred_motion,
};

/// P 片 slice_type = 5（P，全片 P）。
const FIXED_SLICE_TYPE_P: u64 = 5;
/// inter 相对 intra 的决策偏置（近似 mvd/mb_type 语法差；只影响压缩
/// 质量不影响回环，与 `intra.rs` `I4_BIAS` 同量级）。
const INTER_BIAS: i32 = 24;

// ---------------------------------------------------------------------------
// P 片头写侧（T3 雏形形态的 P 分支）
// ---------------------------------------------------------------------------

/// P 片头（单 slice 全帧、单参考帧、滑动窗口标记）。写序 =
/// `slice.rs` `parse_slice_header` P 分支解析序的镜像：frame_num 之后
/// 依次是参考数量覆盖位 / L0 列表重排位（PPS 缺省 1 参考帧 → 均无
/// 后续字段）、自适应参考标记位（false = 滑动窗口）、slice_qp_delta
/// （`qp − 26`，PPS QP 中立——帧级 QP 统一走此处）。
fn write_p_slice_header(w: &mut BitWriter, frame_num: u32, qp: u8) -> Result<(), H264Error> {
    check_qp(qp)?;
    w.ue(0)?; // first_mb_in_slice
    w.ue(FIXED_SLICE_TYPE_P)?; // slice_type = 5（P）
    w.ue(0)?; // pic_parameter_set_id
    w.u(FIXED_LOG2_MAX_FRAME_NUM, u64::from(frame_num))?;
    // pic_order_cnt_type = 2 → 无 POC 语法
    w.u1(false); // num_ref_idx_active_override_flag（缺省 = 1 参考帧）
    w.u1(false); // ref_pic_list_modification_flag_l0
    w.u1(false); // adaptive_ref_pic_marking_mode_flag（滑动窗口）
    w.se(i64::from(qp) - 26)?; // slice_qp_delta
    // deblocking_filter_control_present_flag = 0 → 无去块语法
    Ok(())
}

// ---------------------------------------------------------------------------
// P_Skip 预测子（macroblock.rs `pred_pskip` 的镜像）
// ---------------------------------------------------------------------------

/// P_Skip MV 预测子——`macroblock.rs` 私有 `pred_pskip` 的逐字镜像
/// （跨模块镜像的既定先例：`frame.rs` `deblock_mb` 镜像 `slice.rs`；
/// 两侧一致性由黑盒回环 0 容差兜底：镜像若漂移，skip 试判的 MV 与
/// 解码器 `decode_skip_mb` 实际使用的 MV 不同，重建必不一致）。
///
/// 语义：A=左 (3,0)、B=上 (0,3)、C=右上 (0,3) 不可得退左上 (3,3)；
/// A/B 任一 PART（未解码/出画面）→ 零向量；A/B 任一「ref0 且零 MV」
/// → 零向量；否则三者对 ref 0 的中值预测。
fn pred_pskip_mirror(pic: &Picture, mbx: usize, mby: usize) -> [i16; 2] {
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

// ---------------------------------------------------------------------------
// inter 残差量化（预测已在 pic 平面 = mc_partition 的 MC 输出）
// ---------------------------------------------------------------------------

/// inter 宏块的残差量化与 cbp 装配（`decode_inter_mb` 残差段的写侧
/// 镜像）：luma 16 块全 zigzag 槽（inter 无独立 DC 块）、chroma 与
/// intra 路径同式（DC 2x2 Hadamard + AC）——唯预测取 pic 平面现值
/// （MC 已写入），调用后须由调用方 `add_block` 叠加反量化残差。
fn inter_levels(
    pic: &mut Picture,
    src: &YuvSource<'_>,
    qp: u8,
    mbx: usize,
    mby: usize,
    mb: &mut Macroblock,
    work: &mut MbWork,
) -> Result<(), H264Error> {
    let qmul = dequant4_coeff(qp);
    let qpc = CHROMA_QP[usize::from(qp)];
    let cqmul = dequant4_coeff(qpc);
    let qd = dc_qmul(qpc);
    let ys = pic.luma_stride();
    let cs = pic.chroma_stride();

    // ---- luma：16 块（z 序维护 nnz；diff 对 MC 预测）----
    let src_stride = src.luma_stride;
    let srcp = &src.luma[mby * 16 * src_stride + mbx * 16..];
    for &b in Z_ORDER.iter() {
        let (bx, by) = ((b % 4) * 4, (b / 4) * 4);
        let mut diff = [0i16; 16];
        for k in 0..16usize {
            let s = i16::from(srcp[(by + k / 4) * src_stride + bx + k % 4]);
            let p = i16::from(pic.luma[(mby * 16 + by + k / 4) * ys + mbx * 16 + bx + k % 4]);
            diff[k] = s - p;
        }
        let coeff = fwd_dct4x4(&diff);
        let mut lv = [0i16; 16];
        let mut tc = 0u8;
        for &slot in ZIGZAG.iter() {
            let s = usize::from(slot);
            let q = quant_ac(coeff[s], qmul[s]);
            if q != 0 {
                lv[s] = i16::try_from(q)
                    .map_err(|_| H264Error::InvalidData("luma ac level exceeds i16".into()))?;
                tc += 1;
            }
        }
        mb.nnz[b] = tc;
        work.luma_levels[b] = lv;
        if tc > 0 {
            work.cbp |= 1 << (2 * (b >> 3) + ((b & 3) >> 1));
        }
        for &slot in ZIGZAG.iter() {
            let s = usize::from(slot);
            if lv[s] != 0 {
                work.luma_blocks[b][s] = dequant_ac(i32::from(lv[s]), qmul[s]) as i16;
            }
        }
    }

    // ---- chroma：两平面（预测 = MC 现值；DC 2x2 Hadamard + AC）----
    let c_stride = src.chroma_stride;
    let mut any_ac = false;
    for (plane, (src_plane, data)) in [(src.cb, &mut pic.cb), (src.cr, &mut pic.cr)]
        .into_iter()
        .enumerate()
    {
        let srcp = &src_plane[mby * 8 * c_stride + mbx * 8..];
        let mut dc_in = [0i64; 4];
        let mut coeff = [[0i32; 16]; 4];
        for i in 0..4usize {
            let (bx, by) = ((i % 2) * 4, (i / 2) * 4);
            let mut diff = [0i16; 16];
            let mut sum = 0i64;
            for k in 0..16usize {
                let s = i32::from(srcp[(by + k / 4) * c_stride + bx + k % 4]);
                let p = i32::from(data[(mby * 8 + by + k / 4) * cs + mbx * 8 + bx + k % 4]);
                diff[k] = (s - p) as i16;
                sum += i64::from(diff[k]);
            }
            dc_in[i] = 4 * sum;
            coeff[i] = fwd_dct4x4(&diff);
        }
        let t = fwd_hadamard2x2(&dc_in);
        let mut lv = [0i16; 16];
        for (k, slot) in [0usize, 1, 4, 5].into_iter().enumerate() {
            let q = quant_chroma_dc(t[k], qd);
            if q != 0 {
                lv[slot] = i16::try_from(q)
                    .map_err(|_| H264Error::InvalidData("chroma dc level exceeds i16".into()))?;
            }
        }
        work.chroma_dc_levels[plane] = lv;
        mb.nnz_chroma_dc[plane] =
            u8::try_from(lv.iter().filter(|&&v| v != 0).count()).expect("tc ≤ 4");
        let dcs = chroma_dc_dequant(&[lv[0], lv[1], lv[4], lv[5]], qd)?;
        for (i, dc) in dcs.iter().enumerate() {
            work.chroma_blocks[plane][i][0] = *dc;
        }
        for (i, co) in coeff.iter().enumerate() {
            let mut lv = [0i16; 16];
            let mut tc = 0u8;
            for &slot in ZIGZAG.iter().skip(1) {
                let s = usize::from(slot);
                let q = quant_ac(co[s], cqmul[s]);
                if q != 0 {
                    lv[s] = i16::try_from(q).map_err(|_| {
                        H264Error::InvalidData("chroma ac level exceeds i16".into())
                    })?;
                    tc += 1;
                }
            }
            mb.nnz[16 + plane * 4 + i] = tc;
            work.chroma_ac_levels[plane][i] = lv;
            any_ac |= tc > 0;
            for &slot in ZIGZAG.iter().skip(1) {
                let s = usize::from(slot);
                if lv[s] != 0 {
                    work.chroma_blocks[plane][i][s] = dequant_ac(i32::from(lv[s]), cqmul[s]) as i16;
                }
            }
        }
    }
    // chroma 位三态装配（同 frame.rs：有 AC → 0x20，否则有 DC → 0x10）
    let any_dc = mb.nnz_chroma_dc[0] > 0 || mb.nnz_chroma_dc[1] > 0;
    if any_ac {
        work.cbp |= 0x20;
    } else if any_dc {
        work.cbp |= 0x10;
    }
    Ok(())
}

/// inter 宏块的邻块 nnz 快照（`decode_inter_mb` 的取值条件镜像：
/// 左/上按「已解码」判定，无片门控——单 slice 全帧下与 avail 等价，
/// 仍按读侧原式取值）。
fn inter_nb(pic: &Picture, mbx: usize, mby: usize) -> (Option<NbInfo>, Option<NbInfo>) {
    let w = pic.mb_width;
    let xy = mby * w + mbx;
    let left = (mbx > 0 && pic.mbs[xy - 1].decoded).then(|| nb_info(pic, xy - 1));
    let top = (mby > 0 && pic.mbs[xy - w].decoded).then(|| nb_info(pic, xy - w));
    (left, top)
}

// ---------------------------------------------------------------------------
// 帧编码
// ---------------------------------------------------------------------------

/// P 帧编码产物。`frame_num` 与三个模式计数是测试断言面（skip 判定
/// 路径的独立断言依据），产品路径只消费 `nal`/`recon_*`/`pic`——
/// 计数字段的 dead_code 豁免即此理由。
#[allow(dead_code)]
pub(crate) struct EncodedPFrame {
    /// 片 NAL（P：类型 1 / IDR：类型 5，ref_idc 3；Annex B 起始码由
    /// 调用方包装）。
    pub nal: Vec<u8>,
    /// 本片头写出的 frame_num。
    pub frame_num: u32,
    /// 本地重建（宏块对齐、已去块）——下一帧的参考帧。
    pub pic: Arc<Picture>,
    /// 裁剪到显示尺寸的重建平面（黑盒回环比对域）。
    pub recon_luma: Vec<u8>,
    pub recon_cb: Vec<u8>,
    pub recon_cr: Vec<u8>,
    /// 宏块模式计数（诊断/测试面：skip 判定路径的独立断言依据）。
    pub skip_mb_count: usize,
    pub inter_mb_count: usize,
    pub intra_mb_count: usize,
}

/// P 帧编码主入口（单 slice 全帧 P 片、恒定 QP、单参考帧）。
///
/// # Errors
/// 尺寸非法 / 无参考帧 / 编码中数据驱动失败（level 越界等）→
/// [`H264Error`]（见各内部函数）。
#[allow(clippy::too_many_arguments)] // 编码入参组，聚类反而伤可读性
pub(crate) fn encode_p_frame(
    width: usize,
    height: usize,
    qp: u8,
    search_range: i32,
    src: &YuvSource<'_>,
    frame_num: u32,
    refp: &Arc<Picture>,
) -> Result<EncodedPFrame, H264Error> {
    check_qp(qp)?;
    check_range(search_range)?;
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(H264Error::InvalidArguments(
            "p-frame input needs nonzero even dimensions",
        ));
    }
    let mb_w = width.div_ceil(16);
    let mb_h = height.div_ceil(16);
    let cw = width / 2;
    let ch = height / 2;
    let fits = |plane: &[u8], stride: usize, rows: usize, cols: usize| -> bool {
        stride >= cols && plane.len() >= (rows - 1) * stride + cols
    };
    if !fits(src.luma, src.luma_stride, height, width)
        || !fits(src.cb, src.chroma_stride, ch, cw)
        || !fits(src.cr, src.chroma_stride, ch, cw)
    {
        return Err(H264Error::InvalidArguments(
            "source plane smaller than declared dimensions",
        ));
    }

    // 源面补齐到宏块对齐（frame.rs 同款）
    let (pw, ph) = (mb_w * 16, mb_h * 16);
    let (pad_cb, pad_cr) = (
        pad_plane(src.cb, src.chroma_stride, ch, cw, pw / 2, ph / 2),
        pad_plane(src.cr, src.chroma_stride, ch, cw, pw / 2, ph / 2),
    );
    let padded = YuvSource {
        luma: &pad_plane(src.luma, src.luma_stride, height, width, pw, ph),
        luma_stride: pw,
        cb: &pad_cb,
        cr: &pad_cr,
        chroma_stride: pw / 2,
    };

    let mut pic = Picture::new(mb_w, mb_h, width, height);
    let mut writer = BitWriter::new();
    write_p_slice_header(&mut writer, frame_num, qp)?;

    // decode_skip_mb 的帧间上下文（单参考帧 = 上一帧重建）
    let l0 = [refp.clone()];
    let ictx = InterCtx {
        l0: &l0,
        l1: &[],
        constrained: false,
        direct_spatial: true,
        cur_poc: 0,
        colocated: None,
        col_is_long: false,
        ref_poc: [[0; 32]; 2],
        dist_scale: [256; 32],
        map_col: [[0; 32]; 2],
        direct_8x8_inference: true,
    };

    let mut mb_qp = qp;
    let mut pending_skip: u64 = 0;
    let (mut n_skip, mut n_inter, mut n_intra) = (0usize, 0usize, 0usize);
    let refp_pic: &Picture = &l0[0];

    for mby in 0..mb_h {
        for mbx in 0..mb_w {
            // ---- P_Skip 试判：skip 预测子的 MC + 完整量化，cbp==0 即 skip ----
            let skip_mv = pred_pskip_mirror(&pic, mbx, mby);
            mc_partition(&mut pic, refp_pic, mbx, mby, 0, 0, 4, 4, skip_mv)?;
            let mut trial = Macroblock {
                decoded: false,
                slice_id: 0,
                qp,
                ..Macroblock::default()
            };
            let mut trial_work = MbWork::zero();
            inter_levels(&mut pic, &padded, qp, mbx, mby, &mut trial, &mut trial_work)?;
            if trial_work.cbp == 0 {
                // 重建走解码侧同函数（内部重算同一 pred_pskip——逐位同源）
                decode_skip_mb(&mut pic, mbx, mby, &ictx, 0, qp)?;
                let mb_xy = mby * mb_w + mbx;
                debug_assert_eq!(pic.mbs[mb_xy].mv[0], skip_mv, "skip mv mirror drift");
                pending_skip += 1;
                n_skip += 1;
                continue;
            }

            // ---- inter 候选：ME（决策经 mc 核，与重建同块）----
            let src_stride = padded.luma_stride;
            let src_mb = &padded.luma[mby * 16 * src_stride + mbx * 16..];
            let best = search_motion(refp_pic, mbx, mby, src_mb, src_stride, search_range);
            let c_off = mby * 8 * (pw / 2) + mbx * 8;
            let c_cost = chroma_sad(
                refp_pic,
                mbx,
                mby,
                best.mv,
                &padded.cb[c_off..],
                &padded.cr[c_off..],
                pw / 2,
            );
            let inter_cost = best.sad + c_cost;

            // ---- intra 候选（decide_mb_modes 副作用：I4 决策预测落
            //      本宏块平面——随后无论选哪条路径都会整块覆写）----
            let avail = neighbor_avail(&pic, mbx, mby, 0, false);
            let modes = decide_mb_modes(
                &mut pic,
                mbx,
                mby,
                &avail,
                src_mb,
                src_stride,
                &padded.cb[c_off..],
                &padded.cr[c_off..],
                pw / 2,
            )?;
            let intra_luma = if modes.use_i16 {
                modes.i16_cost
            } else {
                modes.i4_cost + I4_BIAS
            };
            let intra_cost = intra_luma + modes.chroma_cost;

            if inter_cost + INTER_BIAS <= intra_cost {
                // ---- P_L0_16x16：MC → 量化 → 叠加 → 记录 → 语法 ----
                n_inter += 1;
                writer.ue(pending_skip)?;
                pending_skip = 0;
                let grid = build_mv_grid(&pic, mbx, mby);
                let mvp = pred_motion(&grid, 0, 0, 4, 0);
                let mv = best.mv;
                mc_partition(&mut pic, refp_pic, mbx, mby, 0, 0, 4, 4, mv)?;
                let mut mb = Macroblock {
                    decoded: false,
                    slice_id: 0,
                    qp,
                    kind: MbKind::InterP,
                    ..Macroblock::default()
                };
                let mut work = MbWork::zero();
                inter_levels(&mut pic, &padded, qp, mbx, mby, &mut mb, &mut work)?;
                mb.cbp = work.cbp;
                mb.mv = [mv; 16];
                mb.ref_idx = [0; 16];
                // 残差叠加（解码侧同款：全 16 luma + 全 8 chroma 无条件）
                let ys = pic.luma_stride();
                for &b in Z_ORDER.iter() {
                    add_block(
                        &mut pic.luma,
                        ys,
                        mby * 16 + (b / 4) * 4,
                        mbx * 16 + (b % 4) * 4,
                        &mut work.luma_blocks[b],
                    );
                }
                let cs = pic.chroma_stride();
                for (plane, data) in [&mut pic.cb, &mut pic.cr].into_iter().enumerate() {
                    for (i, cblk) in work.chroma_blocks[plane].iter_mut().enumerate() {
                        add_block(data, cs, mby * 8 + (i / 2) * 4, mbx * 8 + (i % 2) * 4, cblk);
                    }
                }
                // ---- 语法（decode_inter_mb mb_type 0 的解析序镜像）----
                writer.ue(0)?; // mb_type = P_L0_16x16
                // ref_idx：单参考帧（te(v) 0 位——读侧 read_ref_idx max==1）
                writer.se(i64::from(mv[0]) - i64::from(mvp[0]))?; // mvd[0]
                writer.se(i64::from(mv[1]) - i64::from(mvp[1]))?; // mvd[1]
                write_coded_block_pattern(&mut writer, work.cbp, false)?;
                if work.cbp > 0 {
                    write_mb_qp_delta(&mut writer, 0, &mut mb_qp)?;
                }
                let (left, top) = inter_nb(&pic, mbx, mby);
                write_luma_ac_quadrants(&mut writer, &work, &mb, left, top)?;
                write_chroma_residuals(&mut writer, &work, &mb, left, top)?;
                mb.decoded = true;
                pic.mbs[mby * mb_w + mbx] = mb;
            } else {
                // ---- 宏块级 intra 回退（mb_type +5 由 SliceKind::P 承担）----
                n_intra += 1;
                writer.ue(pending_skip)?;
                pending_skip = 0;
                encode_intra_mb(
                    &mut pic,
                    &padded,
                    qp,
                    &mut writer,
                    &mut mb_qp,
                    mbx,
                    mby,
                    SliceKind::P,
                )?;
            }
        }
        // 整行完成 → 滤该行（decode_slice 滞后调度同构）
        deblock_row(&mut pic, mby, 0, mb_w);
    }
    // 帧尾余量 skip run（其后无非 skip 宏块——读侧 run 耗尽即片结束）
    if pending_skip > 0 {
        writer.ue(pending_skip)?;
    }
    writer.rbsp_trailing_bits();
    let nal = nal_bytes(1, 3, writer.take_rbsp());

    let ls = pic.luma_stride();
    let cs = pic.chroma_stride();
    let recon_luma = crop_plane(&pic.luma, ls, height, width);
    let recon_cb = crop_plane(&pic.cb, cs, ch, cw);
    let recon_cr = crop_plane(&pic.cr, cs, ch, cw);
    Ok(EncodedPFrame {
        nal,
        frame_num,
        pic: Arc::new(pic),
        recon_luma,
        recon_cb,
        recon_cr,
        skip_mb_count: n_skip,
        inter_mb_count: n_inter,
        intra_mb_count: n_intra,
    })
}

// ---------------------------------------------------------------------------
// Baseline 单参考帧会话（I+P 混合 GOP 的编码侧状态）
// ---------------------------------------------------------------------------

/// Baseline 编码会话：帧级 QP（逐帧入参——恒定 QP 由调用方传同值，
/// 简单 CRF 由 [`crate::encoder::encoder`] 的适配层逐帧推导）、单参考帧
/// （上一帧的去块重建）、frame_num 递增（mod 2^4，与 SPS
/// `log2_max_frame_num` 一致）。
///
/// 参考帧管理与解码器滑动窗口互为镜像：每帧 ref_idc=3（都进短窗）、
/// `max_num_ref_frames=1`、无 MMCO ⇒ 解码器短窗恒为「最新一帧」，
/// 编码侧即「上一帧重建」，两侧 List0[0] 恒同一图。
pub(crate) struct BaselineSession {
    width: usize,
    height: usize,
    /// SPS level_idc（查表归 [`crate::encoder::params::level_for`],
    /// 会话生命周期内不变）。
    level_idc: u8,
    search_range: i32,
    /// 上一帧重建（宏块对齐、已去块）；`None` = 尚未编出 IDR。
    ref_pic: Option<Arc<Picture>>,
    /// 上一帧的 frame_num（IDR 写 0 后从 0 递增）。
    frame_num: u32,
    /// 首个 IDR 生成后可用（会话生命周期内参数集不变——同一 level 与
    /// QP 中立 PPS）。
    sps_nal: Vec<u8>,
    pps_nal: Vec<u8>,
}

impl BaselineSession {
    /// 构造（尺寸/level/搜索范围全量先验；QP 逐帧入参）。
    ///
    /// # Errors
    /// 尺寸为 0 或奇数、搜索范围越界 → [`H264Error::InvalidArguments`]。
    pub(crate) fn new(
        width: usize,
        height: usize,
        level_idc: u8,
        search_range: i32,
    ) -> Result<Self, H264Error> {
        check_range(search_range)?;
        if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
            return Err(H264Error::InvalidArguments(
                "session needs nonzero even dimensions",
            ));
        }
        Ok(BaselineSession {
            width,
            height,
            level_idc,
            search_range,
            ref_pic: None,
            frame_num: 0,
            sps_nal: Vec::new(),
            pps_nal: Vec::new(),
        })
    }

    /// SPS/PPS NAL（extradata 用；首个 IDR 之后有效）。
    pub(crate) fn param_sets(&self) -> (&[u8], &[u8]) {
        (&self.sps_nal, &self.pps_nal)
    }

    /// 编码 IDR：重置 frame_num、参考窗换为本帧重建（I 帧管线复用
    /// `frame.rs` [`crate::encoder::frame::encode_idr_frame`]）。
    ///
    /// # Errors
    /// 见 [`crate::encoder::frame::encode_idr_frame`]（QP 越界等）。
    pub(crate) fn encode_idr(
        &mut self,
        src: &YuvSource<'_>,
        qp: u8,
    ) -> Result<EncodedPFrame, H264Error> {
        let r = crate::encoder::frame::encode_idr_frame(
            self.width,
            self.height,
            self.level_idc,
            src,
            qp,
        )?;
        self.sps_nal = r.sps_nal.clone();
        self.pps_nal = r.pps_nal.clone();
        self.frame_num = 0;
        let pic = Arc::new(r.pic);
        self.ref_pic = Some(pic.clone());
        let mb_total = self.width.div_ceil(16) * self.height.div_ceil(16);
        Ok(EncodedPFrame {
            nal: r.idr_nal,
            frame_num: 0,
            recon_luma: r.recon_luma,
            recon_cb: r.recon_cb,
            recon_cr: r.recon_cr,
            pic,
            skip_mb_count: 0,
            inter_mb_count: 0,
            intra_mb_count: mb_total,
        })
    }

    /// 编码 P 帧（参考 = 上一帧重建；frame_num 递增 mod 16）。
    ///
    /// # Errors
    /// 无参考帧（未先编 IDR）或 QP 越界 →
    /// [`H264Error::InvalidArguments`]；余见 [`encode_p_frame`]。
    pub(crate) fn encode_p(
        &mut self,
        src: &YuvSource<'_>,
        qp: u8,
    ) -> Result<EncodedPFrame, H264Error> {
        let Some(refp) = self.ref_pic.clone() else {
            return Err(H264Error::InvalidArguments(
                "p-frame needs a reference picture (encode an idr frame first)",
            ));
        };
        let frame_num = (self.frame_num + 1) % 16;
        let r = encode_p_frame(
            self.width,
            self.height,
            qp,
            self.search_range,
            src,
            frame_num,
            &refp,
        )?;
        self.frame_num = frame_num;
        self.ref_pic = Some(r.pic.clone());
        Ok(r)
    }
}

// ---------------------------------------------------------------------------
// 测试：P 片头回环 + skip 判定独立用例 + I+P 序列黑盒回环（0 容差）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitreader::BitReader;
    use crate::decoder::H264Decoder;
    use crate::nal::{nal_header_and_payload, unescape_rbsp};
    use crate::slice::{SliceType, parse_slice_header};
    use crate::sps::{Pps, Sps};
    use ye_codec::{
        CodecId, CodecParameters, Decoded, Decoder, Frame, MediaType, Packet, VideoFrame,
    };

    /// 确定性 LCG（对齐 frame.rs / ye-filter 测试先例）。
    struct Lcg(u32);
    impl Lcg {
        fn next_u8(&mut self) -> u8 {
            self.0 = self.0.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (self.0 >> 16) as u8
        }
    }

    /// 合成运动帧 t：滚动渐变底 + 两个平移 LCG 纹理块（块 A 右下
    /// 1px/帧、块 B 左移 1.5px/帧——半像素速度迫使 ME 走细化路径）。
    fn motion_frame(w: usize, h: usize, t: u32) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let (cw, ch) = (w / 2, h / 2);
        let mut luma = vec![0u8; w * h];
        let mut cb = vec![0u8; cw * ch];
        let mut cr = vec![0u8; cw * ch];
        for y in 0..h {
            for x in 0..w {
                // 滚动渐变：x*2 斜率上每帧 +3 ≈ 左滚 1.5px
                luma[y * w + x] = ((x * 2 + y + t as usize * 3) & 0xFF) as u8;
            }
        }
        for y in 0..ch {
            for x in 0..cw {
                cb[y * cw + x] = ((x + 2 * y + t as usize) & 0xFF) as u8;
                cr[y * cw + x] = ((2 * x + y + 2 * t as usize) & 0xFF) as u8;
            }
        }
        let draw = |luma: &mut [u8], x0: usize, y0: usize, size: usize, seed: u32| {
            let mut rng = Lcg(seed);
            for y in 0..size {
                for x in 0..size {
                    let v = rng.next_u8();
                    let (px, py) = (x0 + x, y0 + y);
                    if px < w && py < h {
                        luma[py * w + px] = v;
                    }
                }
            }
        };
        draw(
            &mut luma,
            12 + t as usize,
            8 + t as usize,
            24,
            0xA5A5_0000 ^ t,
        );
        let bx = w - 28 - (t as usize * 3) / 2;
        draw(&mut luma, bx, 20 + t as usize / 2, 16, 0x5EED_0000 ^ t);
        (luma, cb, cr)
    }

    /// Annex B 包装。
    fn annexb(nal: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1];
        v.extend_from_slice(nal);
        v
    }

    /// 黑盒解码：extradata（Annex B SPS/PPS）+ 逐帧包（Annex B 片 NAL）
    /// → 输出帧序列（输出顺序）。
    fn decode_stream(extradata: &[u8], pkts: &[Vec<u8>]) -> Vec<VideoFrame> {
        let mut params = CodecParameters::new(CodecId::H264, MediaType::Video);
        params.extradata = Some(extradata.to_vec());
        let mut dec = H264Decoder::new(&params).expect("decoder new");
        for p in pkts {
            let mut pkt = Packet::empty(0);
            pkt.data = Arc::from(p.clone().into_boxed_slice());
            dec.feed(&pkt).expect("feed");
        }
        dec.flush();
        let mut out = Vec::new();
        while let Decoded::Frame(Frame::Video(f)) = dec.pull().expect("pull") {
            out.push(f);
        }
        assert!(matches!(dec.pull().unwrap(), Decoded::Finished));
        out
    }

    /// 逐像素比对（0 容差）：解码输出 vs 本地重建平面。
    fn assert_frame_eq(frame: &VideoFrame, f: &EncodedPFrame, w: usize, h: usize, ctx: &str) {
        let (cw, ch) = (w / 2, h / 2);
        let check =
            |name: &str, got: &[u8], gstride: usize, want: &[u8], rows: usize, cols: usize| {
                for y in 0..rows {
                    for x in 0..cols {
                        assert_eq!(
                            got[y * gstride + x],
                            want[y * cols + x],
                            "{ctx} {name} pixel ({x},{y})"
                        );
                    }
                }
            };
        let ls = frame.layout().plane(0).stride;
        let cs = frame.layout().plane(1).stride;
        check("luma", frame.plane(0), ls, &f.recon_luma, h, w);
        check("cb", frame.plane(1), cs, &f.recon_cb, ch, cw);
        check("cr", frame.plane(2), cs, &f.recon_cr, ch, cw);
    }

    fn yuv<'a>(luma: &'a [u8], cb: &'a [u8], cr: &'a [u8], w: usize) -> YuvSource<'a> {
        YuvSource {
            luma,
            luma_stride: w,
            cb,
            cr,
            chroma_stride: w / 2,
        }
    }

    /// 渐变纹理帧（平移用例的参考底）。
    fn gradient_frame(w: usize, h: usize, seed: u32) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let (cw, ch) = (w / 2, h / 2);
        let mut rng = Lcg(seed);
        let mut luma = vec![0u8; w * h];
        let mut cb = vec![0u8; cw * ch];
        let mut cr = vec![0u8; cw * ch];
        for y in 0..h {
            for x in 0..w {
                luma[y * w + x] = ((x * 3 + y * 5 + usize::from(rng.next_u8() / 4)) & 0xFF) as u8;
            }
        }
        for y in 0..ch {
            for x in 0..cw {
                cb[y * cw + x] = ((x * 4 + y * 3 + 40) & 0xFF) as u8;
                cr[y * cw + x] = ((x * 2 + y * 7 + 90) & 0xFF) as u8;
            }
        }
        (luma, cb, cr)
    }

    // ------------------------------------------------------------------
    // P 片头写侧 → 解析回环
    // ------------------------------------------------------------------

    /// P 片头字段逐项回读（含 P 专属段：参考数量/列表重排/自适应标记
    /// 全缺席时的缺省语义）。
    #[test]
    fn p_slice_header_write_parse_roundtrip() {
        let (w, h, qp) = (64usize, 48usize, 22u8);
        let (luma, cb, cr) = gradient_frame(w, h, 7);
        let mut s = BaselineSession::new(w, h, 30, 4).unwrap();
        s.encode_idr(&yuv(&luma, &cb, &cr, w), qp).unwrap();
        let f = s.encode_p(&yuv(&luma, &cb, &cr, w), qp).unwrap();
        assert_eq!(f.frame_num, 1);

        let (sps_nal, pps_nal) = s.param_sets();
        let sps = Sps::parse(sps_nal).unwrap();
        let pps = Pps::parse_with_sps(pps_nal, Some(&sps)).unwrap();
        let (header, payload) = nal_header_and_payload(&f.nal).unwrap();
        assert_eq!(header.nal_unit_type, 1, "P 片 NAL 类型 = 非 IDR");
        assert_eq!(header.nal_ref_idc, 3, "P 帧进参考窗");
        let rbsp = unescape_rbsp(payload);
        let mut r = BitReader::new(&rbsp);
        let sh = parse_slice_header(&mut r, &sps, &pps, false, 3).unwrap();
        assert_eq!(sh.first_mb_in_slice, 0);
        assert_eq!(sh.slice_type, SliceType::P);
        assert_eq!(sh.pps_id, 0);
        assert_eq!(sh.frame_num, 1);
        assert_eq!(sh.num_ref_l0, 1, "单参考帧缺省");
        assert!(sh.ref_mods.is_empty());
        assert!(sh.mmco.is_empty(), "滑动窗口标记");
        assert_eq!(sh.slice_qp, qp);
        assert!(sh.deblock.enabled, "去块控制字段缺席 → 全滤");
        assert!(sh.deblock.filter_across_slices);
    }

    // ------------------------------------------------------------------
    // P_Skip 判定路径独立用例
    // ------------------------------------------------------------------

    /// 静止 P 帧（源 == IDR 重建）：逐宏块 skip（预测子 0 的 MC 精确
    /// 复制参考 → 量化残差全零），整帧重建 == 参考帧（平移 (0,0) 的
    /// 特例），码流只含片头 + 一发 mb_skip_run。
    #[test]
    fn static_frame_encodes_all_skip() {
        let (w, h) = (64usize, 48usize);
        let (luma, cb, cr) = gradient_frame(w, h, 11);
        let mut s = BaselineSession::new(w, h, 30, 4).unwrap();
        let idr = s.encode_idr(&yuv(&luma, &cb, &cr, w), 20).unwrap();
        let f = s
            .encode_p(&yuv(&idr.recon_luma, &idr.recon_cb, &idr.recon_cr, w), 20)
            .unwrap();
        assert_eq!(f.skip_mb_count, 12, "全部 12 个宏块 skip");
        assert_eq!(f.inter_mb_count, 0);
        assert_eq!(f.intra_mb_count, 0);
        assert!(f.pic.mbs.iter().all(|m| m.skipped));
        // 重建 == 参考帧
        assert_eq!(f.recon_luma, idr.recon_luma);
        assert_eq!(f.recon_cb, idr.recon_cb);
        assert_eq!(f.recon_cr, idr.recon_cr);
        // skip run 唯一一发：片数据近似空
        assert!(f.nal.len() < 16, "nal {} bytes", f.nal.len());

        // 黑盒回环
        let (sps, pps) = s.param_sets();
        let mut extradata = annexb(sps);
        extradata.extend_from_slice(&annexb(pps));
        let pkts = [annexb(&idr.nal), annexb(&f.nal)];
        let frames = decode_stream(&extradata, &pkts);
        assert_eq!(frames.len(), 2);
        assert_frame_eq(&frames[0], &idr, w, h, "idr");
        assert_frame_eq(&frames[1], &f, w, h, "p");
    }

    /// 平移 P 帧的 skip 判定独立用例：源 = IDR 重建左移 (2,2) px
    /// （luma MV (+2,+2)、chroma (+1,+1)——两轴偶像素保证 chroma MC
    /// 为纯整数平移，残差精确为零）。首行/首列宏块的 skip 预测子退
    /// 零向量 → 必须走 inter；内部宏块（左/上/右上邻块已带真实 MV）
    /// 预测子命中 → skip。断言：
    /// 1) 指定内部宏块 (2,2) 确实按 skip 编码（mb.skipped）；
    /// 2) 该宏块 16x16 重建值 == 参考帧按 (2,2) 平移（独立手写期望，
    ///    不经 mc 核）；
    /// 3) 黑盒解码逐像素一致。
    #[test]
    fn skip_mb_reconstruction_equals_translated_reference() {
        let (w, h) = (64usize, 48usize); // 4x3 宏块、宏块对齐（无裁剪差）
        let (luma, cb, cr) = gradient_frame(w, h, 23);
        let mut s = BaselineSession::new(w, h, 30, 4).unwrap();
        let idr = s.encode_idr(&yuv(&luma, &cb, &cr, w), 20).unwrap();

        // 源 = 重建左移 (2,2)：src(x,y) = ref(min(x+2), min(y+2))
        let shift = |src: &[u8], sw: usize, sh: usize, dx: usize, dy: usize| -> Vec<u8> {
            let mut out = vec![0u8; src.len()];
            for y in 0..sh {
                for x in 0..sw {
                    let sx = (x + dx).min(sw - 1);
                    let sy = (y + dy).min(sh - 1);
                    out[y * sw + x] = src[sy * sw + sx];
                }
            }
            out
        };
        let (pluma, pcb, pcr) = (
            shift(&idr.recon_luma, w, h, 2, 2),
            shift(&idr.recon_cb, w / 2, h / 2, 1, 1),
            shift(&idr.recon_cr, w / 2, h / 2, 1, 1),
        );
        let f = s.encode_p(&yuv(&pluma, &pcb, &pcr, w), 20).unwrap();

        // 首行 4 + 首列 2 = 6 个 inter（预测子退零），内部 6 个 skip
        assert_eq!(f.skip_mb_count, 6, "内部宏块 skip");
        assert_eq!(f.inter_mb_count, 6, "首行/首列 inter");
        assert_eq!(f.intra_mb_count, 0);
        let mb22 = &f.pic.mbs[2 * 4 + 2]; // MB (2,2)
        assert!(mb22.skipped, "MB(2,2) 须按 P_Skip 编码");
        assert_eq!(mb22.kind, MbKind::InterP);
        assert_eq!(mb22.mv[0], [8, 8], "skip MV = 预测子 (+2,+2)px");

        // MB(2,2) 重建 == 参考平移（独立手写期望：手写 clamp 平移）
        for y in 32..48usize {
            for x in 32..48usize {
                let ex = idr.recon_luma[(y + 2).min(47) * w + (x + 2).min(63)];
                assert_eq!(
                    f.recon_luma[y * w + x],
                    ex,
                    "skip MB(2,2) luma ({x},{y}) 须等于参考平移值"
                );
            }
        }
        let (cw, ch) = (w / 2, h / 2);
        for y in 16..24usize {
            for x in 16..24usize {
                let ex = idr.recon_cb[(y + 1).min(ch - 1) * cw + (x + 1).min(cw - 1)];
                assert_eq!(f.recon_cb[y * cw + x], ex, "skip MB(2,2) cb ({x},{y})");
            }
        }

        // 黑盒回环（整帧）
        let (sps, pps) = s.param_sets();
        let mut extradata = annexb(sps);
        extradata.extend_from_slice(&annexb(pps));
        let pkts = [annexb(&idr.nal), annexb(&f.nal)];
        let frames = decode_stream(&extradata, &pkts);
        assert_eq!(frames.len(), 2);
        assert_frame_eq(&frames[0], &idr, w, h, "idr");
        assert_frame_eq(&frames[1], &f, w, h, "p");
    }

    // ------------------------------------------------------------------
    // I+P 混合序列黑盒回环（验收主用例）
    // ------------------------------------------------------------------

    /// 合成平移运动序列（滚动渐变 + 平移纹理块）≥30 帧、I+P 混合
    /// （每 10 帧 IDR），编码为完整码流后自家解码器逐帧逐像素比对
    /// 本地重建（0 容差）。
    #[test]
    fn ip_motion_sequence_roundtrip_32frames() {
        let (w, h, qp, range) = (96usize, 64usize, 26u8, 8);
        let mut s = BaselineSession::new(w, h, 30, range).unwrap();
        let mut recons: Vec<EncodedPFrame> = Vec::new();
        let mut pkts: Vec<Vec<u8>> = Vec::new();
        let mut total_inter = 0usize;
        let mut total_intra = 0usize;
        let mut total_i4 = 0usize;
        for t in 0..32u32 {
            let (luma, cb, cr) = motion_frame(w, h, t);
            let src = yuv(&luma, &cb, &cr, w);
            let f = if t % 10 == 0 {
                s.encode_idr(&src, qp).unwrap()
            } else {
                s.encode_p(&src, qp).unwrap()
            };
            assert_eq!(
                f.skip_mb_count + f.inter_mb_count + f.intra_mb_count,
                24,
                "t={t} 计数须覆盖全部宏块"
            );
            total_inter += f.inter_mb_count;
            total_intra += f.intra_mb_count;
            total_i4 += f
                .pic
                .mbs
                .iter()
                .filter(|m| m.kind == MbKind::Intra4x4)
                .count();
            pkts.push(annexb(&f.nal));
            recons.push(f);
        }
        assert!(total_inter > 0, "运动序列必有 inter 宏块");
        assert!(total_intra > 0, "渐变/纹理混合内容应触发 intra 回退");
        assert!(
            total_i4 > 0,
            "纹理块内容应触发 P 片 Intra4x4（mb_type=5 + prev/rem 链路径）"
        );

        let (sps, pps) = s.param_sets();
        let mut extradata = annexb(sps);
        extradata.extend_from_slice(&annexb(pps));
        let frames = decode_stream(&extradata, &pkts);
        assert_eq!(frames.len(), 32, "帧数须与送入一致");
        for (i, (fr, enc)) in frames.iter().zip(&recons).enumerate() {
            assert_frame_eq(fr, enc, w, h, &format!("frame {i}"));
        }
    }

    /// frame_num 回绕：IDR + 17 个 P（1..15 后回绕 0,1），解码器须无
    /// 断号拒绝且逐帧一致。
    #[test]
    fn frame_num_wraps_across_16() {
        let (w, h) = (32usize, 16usize);
        let mut s = BaselineSession::new(w, h, 30, 2).unwrap();
        let mut recons: Vec<EncodedPFrame> = Vec::new();
        let mut pkts: Vec<Vec<u8>> = Vec::new();
        let mut rng = Lcg(0xC0FF_EE01);
        for t in 0..18u32 {
            let mut luma = vec![0u8; w * h];
            let cb = vec![80u8; w * h / 4];
            let cr = vec![160u8; w * h / 4];
            for v in luma.iter_mut() {
                *v = rng.next_u8();
            }
            let src = yuv(&luma, &cb, &cr, w);
            let f = if recons.is_empty() {
                s.encode_idr(&src, 30).unwrap()
            } else {
                s.encode_p(&src, 30).unwrap()
            };
            if t == 16 {
                assert_eq!(f.frame_num, 0, "15 之后回绕到 0");
            }
            if t == 17 {
                assert_eq!(f.frame_num, 1);
            }
            pkts.push(annexb(&f.nal));
            recons.push(f);
        }
        let (sps, pps) = s.param_sets();
        let mut extradata = annexb(sps);
        extradata.extend_from_slice(&annexb(pps));
        let frames = decode_stream(&extradata, &pkts);
        assert_eq!(frames.len(), 18);
        for (i, (fr, enc)) in frames.iter().zip(&recons).enumerate() {
            assert_frame_eq(fr, enc, w, h, &format!("frame {i}"));
        }
    }

    /// 场景切换 → 宏块级 intra 回退：平坦参考 + 竖条纹 P 帧，全部
    /// 宏块选 intra（inter 对平坦参考无增益，V/H 预测近似零代价）。
    #[test]
    fn scene_change_falls_back_to_intra() {
        let (w, h) = (64usize, 48usize);
        let flat = (
            vec![128u8; w * h],
            vec![128u8; w * h / 4],
            vec![128u8; w * h / 4],
        );
        let mut s = BaselineSession::new(w, h, 30, 8).unwrap();
        let idr = s
            .encode_idr(&yuv(&flat.0, &flat.1, &flat.2, w), 24)
            .unwrap();
        let mut luma = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                luma[y * w + x] = if (x / 2) % 2 == 0 { 200 } else { 60 };
            }
        }
        let cb = vec![128u8; w * h / 4];
        let cr = vec![128u8; w * h / 4];
        let f = s.encode_p(&yuv(&luma, &cb, &cr, w), 24).unwrap();
        assert_eq!(f.intra_mb_count, 12, "场景切换全部 intra 回退");
        assert_eq!(f.inter_mb_count, 0);
        assert_eq!(f.skip_mb_count, 0);

        let (sps, pps) = s.param_sets();
        let mut extradata = annexb(sps);
        extradata.extend_from_slice(&annexb(pps));
        let frames = decode_stream(&extradata, &[annexb(&idr.nal), annexb(&f.nal)]);
        assert_eq!(frames.len(), 2);
        assert_frame_eq(&frames[1], &f, w, h, "intra-in-p");
    }

    // ------------------------------------------------------------------
    // 输入校验
    // ------------------------------------------------------------------

    /// 会话先验：奇尺寸 / QP 越界 / 搜索范围越界；无参考先编 P 拒收。
    #[test]
    fn session_validates_inputs() {
        assert!(BaselineSession::new(31, 16, 30, 4).is_err());
        assert!(BaselineSession::new(32, 0, 30, 4).is_err());
        assert!(BaselineSession::new(32, 16, 30, 65).is_err());
        assert!(BaselineSession::new(32, 16, 30, -1).is_err());

        let (luma, cb, cr) = gradient_frame(32, 16, 3);
        let mut s = BaselineSession::new(32, 16, 30, 4).unwrap();
        assert!(matches!(
            s.encode_p(&yuv(&luma, &cb, &cr, 32), 20),
            Err(H264Error::InvalidArguments(_))
        ));
        // 逐帧 QP 越界（QP 语义 0..=51，52 拒收）
        assert!(matches!(
            s.encode_idr(&yuv(&luma, &cb, &cr, 32), 52),
            Err(H264Error::InvalidArguments(_))
        ));
        // 源面过小
        s.encode_idr(&yuv(&luma, &cb, &cr, 32), 20).unwrap();
        let small = YuvSource {
            luma: &luma[..100],
            luma_stride: 32,
            cb: &cb,
            cr: &cr,
            chroma_stride: 16,
        };
        assert!(matches!(
            s.encode_p(&small, 20),
            Err(H264Error::InvalidArguments(_))
        ));
    }
}
