//! I 帧编码全链（M3 施工图 T3，参数集已随 T5 定形）：帧内预测 →
//! 变换量化 → 码流组装 → 本地解码环（含去块）。P 帧路径（T4，
//! `pframe.rs`）复用本模块的 intra 宏块管线（[`encode_intra_mb`] 带
//! [`SliceKind`] 参数——P 片的 intra 回退 mb_type 一律 +5）、参数集
//! 写侧（[`write_sps_nal`]/[`write_pps_nal`]）、源面补齐/裁剪
//! （[`pad_plane`]/[`crop_plane`]）与图级去块（[`deblock_row`]）。
//!
//! ## 全链结构（宏块光栅序编码，全图完成后统一去块）
//!
//! ```text
//! for mby:  for mbx:  模式决策 → 预测/残差/变换/量化（level）→
//!                     本地重建（复用解码侧同一批函数）→
//!                     语法写出（与 decode_macroblock 解析序互逆）
//! 全图完成后：去块滤波整帧（复用 deblock.rs 滤波核；I 帧边界同样滤）
//! ```
//!
//! 图级去块是位级回环的硬要求（2026-10-27 与解码器对齐）：解码侧
//! 帧内预测读**滤波前**样本、图完成后按光栅序统一滤波（`slice.rs`
//! `deblock_picture`），编码器本地重建必须以同一调度落平面，否则两侧
//! 预测边沿必然漂移。边组织循环（[`deblock_row`]/[`deblock_mb`]）镜像
//! `slice.rs` 的实现（滤波核、bS 判定与 α/β/tc 阈值函数全部复用
//! `deblock.rs` 公有面），一致性由黑盒回环测试的 0 容差兜底。
//!
//! ## 参数集写侧（T5 定形：level 可配、QP 走片头）
//!
//! [`write_sps_nal`] 产出 Baseline 参数集（profile_idc=66 +
//! constraint_set0），`level_idc` 为入参——由
//! [`crate::encoder::params::level_for`] 按 帧尺寸×帧率 查表取满足的
//! 最小值（H.264 表 A-1/A-3 子集，不含需 constraint_set3 表达的 1b）。
//! [`write_pps_nal`] 恒定 `pic_init_qp_minus26 = 0`（QP 中立——恒定
//! QP 与逐帧 QP（简单 CRF）统一走片头 `slice_qp_delta = qp − 26`，
//! avcC 因此可在首帧编码前构造）。poc_type=2（片头无 POC 语法）、
//! CAVLC、去块控制字段缺席（解码器取缺省 = 全滤）、单 slice 全帧 I 片、
//! 宏块 qp_delta 恒 0。GOP/IDR 间隔与 avcC 写侧见
//! [`crate::encoder::params`] 与 [`crate::encoder::encoder`]。
//!
//! ## 回环位级一致的构造性保证
//!
//! 本地解码环的每一步都调用解码侧同一实现：预测（`pred_*`/`gather_*`）、
//! 反量化（`dequant_ac` = `cavlc.rs` store 同式；`luma_dc_dequant`/
//! `chroma_dc_dequant` 直接调用）、反变换叠加（`reconstruct_4x4`/
//! `add_block`）、去块（`deblock.rs` 核）。CAVLC 写侧（`encoder/cavlc.rs`）
//! 与读侧互为镜像（T2 位级回环已保证）。两侧以相同顺序对相同 level
//! 序列执行相同算术，黑盒解码输出与本地重建因此逐像素相等——
//! `idr_roundtrip_*` 测试组以 0 容差验收。
//!
//! nC 的写侧重算：解析序中块 b 的 nC 只读本宏块 nnz 的 b−1（左）与
//! b−4（上）槽——两者在 z 序里都先于 b 解析，故在全部 level 计算完
//! （nnz 全量落位）之后再按解析序重算 nC，与解析时刻的取值一致；
//! Intra16x16 的 luma DC 是唯一例外（解析序最前，nC 必须以全零 nnz
//! 起算——见 [`write_mb_syntax`]）。
//!
//! ## cbp 语义注意
//!
//! Intra 宏块的 chroma 残差域是三态（无 / 仅 DC / DC+AC）而非两个
//! 独立位：bit5（AC）置位隐含 DC 也编码（解码侧读 DC 的条件是
//! `cbp & 0x30 != 0`）。故 cbp 装配取「有 AC → 0x20，否则有 DC →
//! 0x10，否则 0」，两 bit 永不同置（me(v) 表只含 0..=47）。

use crate::cavlc::BlockKind;
use crate::deblock;
use crate::encoder::bitwriter::{BitWriter, escape_rbsp};
use crate::encoder::cavlc::write_residual;
use crate::encoder::intra::{MbModes, decide_mb_modes};
use crate::encoder::mb::{SliceKind, write_coded_block_pattern, write_mb_qp_delta};
use crate::encoder::transform::{
    check_qp, dequant_ac, fwd_dct4x4, fwd_hadamard2x2, fwd_hadamard4x4, quant_ac, quant_chroma_dc,
    quant_luma_dc,
};
use crate::error::H264Error;
use crate::macroblock::{
    CHROMA_DC_SCAN, CHROMA_QP, I16_CBP, Macroblock, MbKind, NeighAvail, Picture, Z_ORDER, ZIGZAG,
    add_block, chroma_dc_dequant, dc_qmul, dequant4_coeff, gather_16x16_edges, gather_chroma_edges,
    gather_edges4, luma_dc_dequant, nc_chroma_ac, nc_chroma_dc, nc_luma, neighbor_avail, pred_8x8c,
    pred_16x16, prev_i4_mode, reconstruct_4x4,
};
use crate::slice::DeblockParams;

// ---------------------------------------------------------------------------
// 参数集与片头写侧（T5 定形：level 入参、QP 走片头）
// ---------------------------------------------------------------------------

/// 固定 profile：Baseline（constraint_set0 置位——同时是解码器 PPS
/// 扩展段门槛的关闭条件，见 `sps.rs` `parse_with_sps`）。
const FIXED_PROFILE_IDC: u8 = 66;
const FIXED_CONSTRAINT_FLAGS: u8 = 0x80; // constraint_set0_flag
/// 固定 log2_max_frame_num = 4（frame_num 字段 4 位；T4 P 帧沿用）。
pub(crate) const FIXED_LOG2_MAX_FRAME_NUM: u32 = 4;
/// I 帧一种 slice 类型：slice_type = 7（I，全片 I）。
const FIXED_SLICE_TYPE_I: u64 = 7;

/// SPS NAL（类型 7，含头字节与防竞争）。宽高为**显示尺寸**（偶数），
/// 宏块对齐余量经 frame_crop_right/bottom 表达（4:2:0 的 chroma 单位）；
/// `level_idc` 由调用方按 帧尺寸×帧率 查表给出
/// （[`crate::encoder::params::level_for`]）。
pub(crate) fn write_sps_nal(
    width: usize,
    height: usize,
    level_idc: u8,
) -> Result<Vec<u8>, H264Error> {
    write_sps_nal_color(width, height, level_idc, None)
}

pub(crate) fn write_sps_nal_color(
    width: usize,
    height: usize,
    level_idc: u8,
    color: Option<ye_pixfmt::ColorDesc>,
) -> Result<Vec<u8>, H264Error> {
    let mb_w = width.div_ceil(16);
    let mb_h = height.div_ceil(16);
    let crop_r = (mb_w * 16 - width) / 2;
    let crop_b = (mb_h * 16 - height) / 2;
    let mut w = BitWriter::new();
    w.u(8, u64::from(FIXED_PROFILE_IDC))?;
    w.u(8, u64::from(FIXED_CONSTRAINT_FLAGS))?;
    w.u(8, u64::from(level_idc))?;
    w.ue(0)?; // seq_parameter_set_id
    w.ue(0)?; // log2_max_frame_num_minus4
    w.ue(2)?; // pic_order_cnt_type = 2（片头无 POC 语法）
    w.ue(1)?; // max_num_ref_frames（Baseline 单参考帧）
    w.u1(false); // gaps_in_frame_num_value_allowed_flag
    w.ue((mb_w - 1) as u64)?;
    w.ue((mb_h - 1) as u64)?;
    w.u1(true); // frame_mbs_only_flag
    w.u1(true); // direct_8x8_inference_flag
    if crop_r > 0 || crop_b > 0 {
        w.u1(true); // frame_cropping_flag
        w.ue(0)?; // frame_crop_left
        w.ue(crop_r as u64)?; // frame_crop_right（chroma 单位）
        w.ue(0)?; // frame_crop_top
        w.ue(crop_b as u64)?; // frame_crop_bottom
    } else {
        w.u1(false);
    }
    w.u1(color.is_some());
    if let Some(color) = color {
        w.u1(false); // aspect_ratio_info_present_flag
        w.u1(false); // overscan_info_present_flag
        w.u1(true); // video_signal_type_present_flag
        w.u(3, 5)?; // unspecified video format
        w.u1(color.range == ye_pixfmt::ColorRange::Jpeg);
        w.u1(true); // colour_description_present_flag
        w.u(8, u64::from(color.primaries.to_code()))?;
        w.u(8, u64::from(color.transfer.to_code()))?;
        w.u(8, u64::from(color.space.to_code()))?;
        w.u1(false); // chroma_loc_info_present_flag
        w.u1(false); // timing_info_present_flag
        w.u1(false); // nal_hrd_parameters_present_flag
        w.u1(false); // vcl_hrd_parameters_present_flag
        w.u1(false); // pic_struct_present_flag
        w.u1(false); // bitstream_restriction_flag
    }
    w.rbsp_trailing_bits();
    Ok(nal_bytes(7, 3, w.take_rbsp()))
}

/// PPS NAL（类型 8）。QP 中立：`pic_init_qp_minus26 = 0`，帧级 QP 一律
/// 由片头 `slice_qp_delta` 承载（恒定 QP 与逐帧 QP 统一，avcC 可在
/// 首帧前构造）；CAVLC、无去块控制字段（片头不出现 → 解码器取
/// 缺省 = 全滤，I 帧边界同样滤波）。
pub(crate) fn write_pps_nal() -> Result<Vec<u8>, H264Error> {
    let mut w = BitWriter::new();
    w.ue(0)?; // pic_parameter_set_id
    w.ue(0)?; // seq_parameter_set_id
    w.u1(false); // entropy_coding_mode_flag（CAVLC）
    w.u1(false); // bottom_field_pic_order_in_frame_present_flag
    w.ue(0)?; // num_slice_groups_minus1
    w.ue(0)?; // num_ref_idx_l0_default_active_minus1
    w.ue(0)?; // num_ref_idx_l1_default_active_minus1
    w.u1(false); // weighted_pred_flag
    w.u(2, 0)?; // weighted_bipred_idc
    w.se(0)?; // pic_init_qp_minus26（QP 中立，帧级 QP 走 slice_qp_delta）
    w.se(0)?; // pic_init_qs_minus26
    w.se(0)?; // chroma_qp_index_offset
    w.u1(false); // deblocking_filter_control_present_flag
    w.u1(false); // constrained_intra_pred_flag
    w.u1(false); // redundant_pic_cnt_present_flag
    w.rbsp_trailing_bits();
    Ok(nal_bytes(8, 3, w.take_rbsp()))
}

/// IDR 片头（宏块数据之前的部分；单 slice 覆盖全帧）。写序 =
/// `slice.rs` `parse_slice_header` 解析序的镜像；`qp` 为本帧 QP
/// （`slice_qp_delta = qp − 26`，PPS 恒 26——见 [`write_pps_nal`]）。
fn write_slice_header(w: &mut BitWriter, qp: u8) -> Result<(), H264Error> {
    check_qp(qp)?;
    w.ue(0)?; // first_mb_in_slice
    w.ue(FIXED_SLICE_TYPE_I)?; // slice_type
    w.ue(0)?; // pic_parameter_set_id
    w.u(FIXED_LOG2_MAX_FRAME_NUM, 0)?; // frame_num
    w.ue(0)?; // idr_pic_id
    // pic_order_cnt_type = 2 → 无 POC 语法；I 片无参考列表段
    w.u1(false); // no_output_of_prior_pics_flag（IDR 标记段）
    w.u1(false); // long_term_reference_flag
    w.se(i64::from(qp) - 26)?; // slice_qp_delta
    // deblocking_filter_control_present_flag = 0 → 无去块语法
    Ok(())
}

/// NAL 组装：头字节（禁零 + ref_idc + 类型）+ 防竞争载荷。
pub(crate) fn nal_bytes(nal_type: u8, ref_idc: u8, rbsp: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 8 + 8);
    out.push((ref_idc << 5) | nal_type);
    out.extend(escape_rbsp(&rbsp));
    out
}

// ---------------------------------------------------------------------------
// 输入输出形态
// ---------------------------------------------------------------------------

/// 编码输入源（yuv420p 平面视图；行距独立于宽度）。
#[derive(Clone, Copy)]
pub(crate) struct YuvSource<'a> {
    pub luma: &'a [u8],
    pub luma_stride: usize,
    pub cb: &'a [u8],
    pub cr: &'a [u8],
    pub chroma_stride: usize,
}

/// I 帧编码产物：NAL 三件套（Annex B 起始码由调用方包装）与本地解码
/// 环的重建帧（裁剪到显示尺寸的紧凑平面），外加**宏块对齐的完整重建
/// [`Picture`]**（已含去块；T4 P 帧编码以其作单参考帧——解码器 DPB
/// 里的参考同为此形态）。
pub(crate) struct EncodedIFrame {
    pub sps_nal: Vec<u8>,
    pub pps_nal: Vec<u8>,
    pub idr_nal: Vec<u8>,
    pub recon_luma: Vec<u8>,
    pub recon_cb: Vec<u8>,
    pub recon_cr: Vec<u8>,
    /// 本地重建（宏块对齐、已去块）——后续 P 帧的参考帧。
    pub pic: Picture,
}

/// 源面补齐到宏块对齐：右/下边缘按最近像素复制（最终裁剪丢弃——
/// 两侧经码流对同一内容编解码，不破坏回环）。
pub(crate) fn pad_plane(
    plane: &[u8],
    stride: usize,
    rows: usize,
    cols: usize,
    full_w: usize,
    full_h: usize,
) -> Vec<u8> {
    let mut out = vec![0u8; full_w * full_h];
    for y in 0..full_h {
        let sy = y.min(rows - 1);
        for x in 0..full_w {
            let sx = x.min(cols - 1);
            out[y * full_w + x] = plane[sy * stride + sx];
        }
    }
    out
}

/// 平面裁剪到显示尺寸（紧凑行主序）。
pub(crate) fn crop_plane(plane: &[u8], stride: usize, rows: usize, cols: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(rows * cols);
    for y in 0..rows {
        out.extend_from_slice(&plane[y * stride..y * stride + cols]);
    }
    out
}

// ---------------------------------------------------------------------------
// 帧编码器
// ---------------------------------------------------------------------------

/// I 帧编码主入口（单 slice 全帧 I 片、帧级 QP）。
///
/// `level_idc` 写进 SPS（查表归 [`crate::encoder::params::level_for`]）；
/// `qp` 经片头 `slice_qp_delta` 承载（PPS QP 中立）。
///
/// # Errors
/// 尺寸非法（0/奇数/超宏块域）、QP 越界、源面长度不足 →
/// [`H264Error::InvalidArguments`]；编码中数据驱动失败（level 越界等）
/// → [`H264Error::InvalidData`]。
pub(crate) fn encode_idr_frame(
    width: usize,
    height: usize,
    level_idc: u8,
    src: &YuvSource<'_>,
    qp: u8,
) -> Result<EncodedIFrame, H264Error> {
    check_qp(qp)?;
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(H264Error::InvalidArguments(
            "i-frame input needs nonzero even dimensions",
        ));
    }
    let mb_w = width.div_ceil(16);
    let mb_h = height.div_ceil(16);
    if mb_w > 65535 || mb_h > 65535 {
        return Err(H264Error::InvalidArguments("picture exceeds sps range"));
    }
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

    let sps_nal = write_sps_nal(width, height, level_idc)?;
    let pps_nal = write_pps_nal()?;

    // 源面补齐到宏块对齐（解码图片恒为宏块网格；补齐区右/下边缘
    // 复制，最终裁剪丢弃——两侧经码流对同一内容编解码，不破坏回环）
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
    write_slice_header(&mut writer, qp)?;

    let mut mb_qp = qp;
    for mby in 0..mb_h {
        for mbx in 0..mb_w {
            encode_intra_mb(
                &mut pic,
                &padded,
                qp,
                &mut writer,
                &mut mb_qp,
                mbx,
                mby,
                SliceKind::I,
            )?;
        }
    }
    // 全图编码完成后统一去块（2026-10-27 与解码器对齐：帧内预测读
    // 滤波前样本，图完成再按光栅序滤波——解码器 decode_slice 同日
    // 改为同语义，回环才逐位一致）
    for mby in 0..mb_h {
        deblock_row(&mut pic, mby, 0, mb_w);
    }
    writer.rbsp_trailing_bits();
    let idr_nal = nal_bytes(5, 3, writer.take_rbsp());

    // 本地重建帧（已含去块）裁剪到显示尺寸
    let ls = pic.luma_stride();
    let cs = pic.chroma_stride();
    let (recon_luma, recon_cb, recon_cr) = (
        crop_plane(&pic.luma, ls, height, width),
        crop_plane(&pic.cb, cs, ch, cw),
        crop_plane(&pic.cr, cs, ch, cw),
    );
    Ok(EncodedIFrame {
        sps_nal,
        pps_nal,
        idr_nal,
        recon_luma,
        recon_cb,
        recon_cr,
        pic,
    })
}

/// 已编码邻块的信息快照（nnz 链与 4x4 模式 prev 链的跨宏块来源）。
#[derive(Clone, Copy)]
pub(crate) struct NbInfo {
    pub nnz: [u8; 24],
    pub nnz_cdc: [u8; 2],
    pub kind: MbKind,
    pub i4: [u8; 16],
}

pub(crate) fn nb_info(pic: &Picture, idx: usize) -> NbInfo {
    let m = &pic.mbs[idx];
    NbInfo {
        nnz: m.nnz,
        nnz_cdc: m.nnz_chroma_dc,
        kind: m.kind,
        i4: m.i4_modes,
    }
}

/// `neighbor_i4_mode` 的快照版（消费 [`NbInfo`]；非 I4 邻块贡献 DC=2）。
pub(crate) fn nb_i4(nb: &NbInfo, slot: usize) -> u8 {
    if nb.kind == MbKind::Intra4x4 {
        nb.i4[slot]
    } else {
        2
    }
}

/// 一个宏块的量化产物（写侧）与反量化块（本地重建侧）。
pub(crate) struct MbWork {
    /// luma AC/Intra4x4 全块的量化 level（全零块即全零数组）。
    pub luma_levels: [[i16; 16]; 16],
    /// Intra16x16 的 luma DC level（16 系数光栅）；I4/inter 为 `None`。
    pub luma_dc: Option<[i16; 16]>,
    /// chroma DC level（平面 → 槽 0/1/4/5）。
    pub chroma_dc_levels: [[i16; 16]; 2],
    /// chroma AC level（平面 → 4 块）。
    pub chroma_ac_levels: [[[i16; 16]; 4]; 2],
    /// 反量化后的重建域块（`reconstruct_4x4`/`add_block` 入参）。
    pub luma_blocks: [[i16; 16]; 16],
    pub chroma_blocks: [[[i16; 16]; 4]; 2],
    /// 6 位语义 cbp（chroma 域三态，见模块 doc）。
    pub cbp: u8,
}

impl MbWork {
    /// 全零初值（inter 路径的增量装配起点）。
    pub(crate) fn zero() -> Self {
        MbWork {
            luma_levels: [[0; 16]; 16],
            luma_dc: None,
            chroma_dc_levels: [[0; 16]; 2],
            chroma_ac_levels: [[[0; 16]; 4]; 2],
            luma_blocks: [[0; 16]; 16],
            chroma_blocks: [[[0; 16]; 4]; 2],
            cbp: 0,
        }
    }
}

/// 编码一个 intra 宏块：决策 → level（解析序）→ 本地重建 → 语法写出。
/// `slice` 决定 mb_type 偏移（P 片的 intra 回退：I4 → 5、I16 → +5；
/// 读侧 `decode_macroblock` 的 P 片 −5 与本参数互逆）。
#[allow(clippy::too_many_arguments)] // 编码器循环态，聚类反而伤可读性
pub(crate) fn encode_intra_mb(
    pic: &mut Picture,
    src: &YuvSource<'_>,
    qp: u8,
    writer: &mut BitWriter,
    mb_qp: &mut u8,
    mbx: usize,
    mby: usize,
    slice: SliceKind,
) -> Result<(), H264Error> {
    let avail = neighbor_avail(pic, mbx, mby, 0, false);
    let mb_xy = mby * pic.mb_width + mbx;
    let left = (avail.left && mbx > 0).then(|| nb_info(pic, mb_xy - 1));
    let top = (avail.top && mby > 0).then(|| nb_info(pic, mb_xy - pic.mb_width));

    // ---- 模式决策（I4 决策把逐块预测写进本宏块 luma 区域）----
    let src_stride = src.luma_stride;
    let c_stride = src.chroma_stride;
    let y_off = mby * 16 * src_stride + mbx * 16;
    let c_off = mby * 8 * c_stride + mbx * 8;
    let modes = decide_mb_modes(
        pic,
        mbx,
        mby,
        &avail,
        &src.luma[y_off..],
        src_stride,
        &src.cb[c_off..],
        &src.cr[c_off..],
        c_stride,
    )?;

    let qpc = CHROMA_QP[usize::from(qp)];
    let qmul = dequant4_coeff(qp);
    let cqmul = dequant4_coeff(qpc);
    let mut mb = Macroblock {
        decoded: false,
        slice_id: 0,
        qp,
        ..Macroblock::default()
    };
    mb.kind = if modes.use_i16 {
        MbKind::Intra16x16
    } else {
        MbKind::Intra4x4
    };
    mb.i16_mode = modes.i16_mode;
    mb.i4_modes = modes.i4_modes;
    mb.chroma_mode = modes.chroma_mode;

    // ---- level 计算（解析序维护 nnz 链）----
    let mut work = MbWork::zero();
    if modes.use_i16 {
        luma16_levels(
            pic,
            src,
            qp,
            &qmul,
            mbx,
            mby,
            &avail,
            modes.i16_mode,
            &mut mb,
            left,
            top,
            &mut work,
        )?;
    } else {
        luma4_levels(
            pic, src, &qmul, mbx, mby, &avail, &modes, &mut mb, left, top, &mut work,
        )?;
    }
    chroma_levels(
        pic,
        src,
        qp,
        &cqmul,
        mbx,
        mby,
        &avail,
        modes.chroma_mode,
        &mut mb,
        left,
        top,
        &mut work,
    )?;
    mb.cbp = work.cbp;

    // ---- 本地重建（解码侧同函数，写平面）----
    reconstruct_mb(pic, &avail, mbx, mby, &modes, &mut work);

    // ---- 语法写出（解析序镜像）----
    write_mb_syntax(writer, &work, &modes, &mb, left, top, mb_qp, slice)?;

    mb.decoded = true;
    pic.mbs[mb_xy] = mb;
    Ok(())
}

/// Intra16x16 luma：预测落平面 → 逐块 DCT → DC Hadamard 量化 →
/// AC 逐块量化（z 序 nnz 链）→ 反量化成重建块。
#[allow(clippy::too_many_arguments)] // 编码入参组，聚类反而伤可读性
fn luma16_levels(
    pic: &mut Picture,
    src: &YuvSource<'_>,
    qp: u8,
    qmul: &[u32; 16],
    mbx: usize,
    mby: usize,
    avail: &NeighAvail,
    mode: u8,
    mb: &mut Macroblock,
    left: Option<NbInfo>,
    top: Option<NbInfo>,
    work: &mut MbWork,
) -> Result<(), H264Error> {
    let ys = pic.luma_stride();
    {
        let (t, l, tl) = gather_16x16_edges(pic, mbx, mby, avail);
        let base = mby * 16 * ys + mbx * 16;
        pred_16x16(mode, &t, &l, tl, &mut pic.luma[base..], ys);
    }
    let src_stride = src.luma_stride;
    let src = &src.luma[mby * 16 * src_stride + mbx * 16..];
    // 逐块差分 + 前向 DCT；DC 块输入 D[b] = 4·Σr_b（transform.rs doc）
    let mut dc_in = [0i64; 16];
    let mut coeff = [[0i32; 16]; 16];
    for b in 0..16usize {
        let (bx, by) = ((b % 4) * 4, (b / 4) * 4);
        let mut diff = [0i16; 16];
        let mut sum = 0i64;
        for k in 0..16usize {
            let s = i32::from(src[(by + k / 4) * src_stride + bx + k % 4]);
            let p = i32::from(pic.luma[(mby * 16 + by + k / 4) * ys + mbx * 16 + bx + k % 4]);
            diff[k] = (s - p) as i16;
            sum += i64::from(diff[k]);
        }
        dc_in[b] = 4 * sum;
        coeff[b] = fwd_dct4x4(&diff);
    }
    // DC：Hadamard + 量化（解析序最前，nnz 链以全零起算——见模块 doc）
    let t = fwd_hadamard4x4(&dc_in);
    let qd = dc_qmul(qp);
    let mut dc_levels = [0i16; 16];
    for i in 0..16usize {
        let q = quant_luma_dc(t[i], qd);
        dc_levels[i] = i16::try_from(q)
            .map_err(|_| H264Error::InvalidData("luma dc level exceeds i16".into()))?;
    }
    work.luma_dc = Some(dc_levels);
    // AC：z 序量化 + nnz；反量化落重建块
    let mut any_ac = false;
    for &b in Z_ORDER.iter() {
        let mut lv = [0i16; 16];
        let mut tc = 0u8;
        for &slot in ZIGZAG.iter().skip(1) {
            let s = usize::from(slot);
            let q = quant_ac(coeff[b][s], qmul[s]);
            if q != 0 {
                lv[s] = i16::try_from(q)
                    .map_err(|_| H264Error::InvalidData("luma ac level exceeds i16".into()))?;
                tc += 1;
            }
        }
        mb.nnz[b] = tc;
        work.luma_levels[b] = lv;
        any_ac |= tc > 0;
    }
    if any_ac {
        work.cbp |= 0x0F;
    }
    // 重建块：DC 经解码侧反量化（喂最终 level），AC 逐槽反量化
    let dcs = luma_dc_dequant(&dc_levels, qd)?;
    for (b, dc) in dcs.iter().enumerate() {
        work.luma_blocks[b][0] = *dc;
        for &slot in ZIGZAG.iter().skip(1) {
            let s = usize::from(slot);
            let lv = work.luma_levels[b][s];
            if lv != 0 {
                work.luma_blocks[b][s] = dequant_ac(i32::from(lv), qmul[s]) as i16;
            }
        }
    }
    let _ = (left, top);
    Ok(())
}

/// Intra4x4 luma：逐块**重新**预测（边沿含邻块重建值，与解码器逐位
/// 同源）→ DCT → 全 16 槽量化 → 反量化落重建块。
#[allow(clippy::too_many_arguments)] // 编码入参组，聚类反而伤可读性
fn luma4_levels(
    pic: &mut Picture,
    src: &YuvSource<'_>,
    qmul: &[u32; 16],
    mbx: usize,
    mby: usize,
    avail: &NeighAvail,
    modes: &MbModes,
    mb: &mut Macroblock,
    left: Option<NbInfo>,
    top: Option<NbInfo>,
    work: &mut MbWork,
) -> Result<(), H264Error> {
    let src_stride = src.luma_stride;
    let src = &src.luma[mby * 16 * src_stride + mbx * 16..];
    let mut cbp = 0u8;
    for &b in Z_ORDER.iter() {
        let (cx, cy) = (b % 4, b / 4);
        // 重新预测：平面此刻含本宏块已重建块（z 序靠前）+ 决策期预测
        //（当前块区域，被 out 覆盖不读）——邻块边沿与解码器同源
        let edges = gather_edges4(pic, mbx, mby, b, avail);
        let mut out = [0u8; 16];
        crate::macroblock::pred_4x4(modes.i4_modes[b], &edges, &mut out);
        let mut diff = [0i16; 16];
        for k in 0..16usize {
            let s = src[(cy * 4 + k / 4) * src_stride + cx * 4 + k % 4];
            diff[k] = i16::from(s) - i16::from(out[k]);
        }
        let x2 = fwd_dct4x4(&diff);
        let mut lv = [0i16; 16];
        let mut tc = 0u8;
        for &slot in ZIGZAG.iter() {
            let s = usize::from(slot);
            let q = quant_ac(x2[s], qmul[s]);
            if q != 0 {
                lv[s] = i16::try_from(q)
                    .map_err(|_| H264Error::InvalidData("luma ac level exceeds i16".into()))?;
                tc += 1;
            }
        }
        mb.nnz[b] = tc;
        work.luma_levels[b] = lv;
        if tc > 0 {
            cbp |= 1 << (2 * (b >> 3) + ((b & 3) >> 1));
        }
        // 反量化落重建块（全零块保持全零——reconstruct_4x4 的加法分支
        // 与解码侧同款按内容选择）
        for &slot in ZIGZAG.iter() {
            let s = usize::from(slot);
            if lv[s] != 0 {
                work.luma_blocks[b][s] = dequant_ac(i32::from(lv[s]), qmul[s]) as i16;
            }
        }
        // 立即重建本块（z 序）——后续块的预测边沿须读**重建后**的邻居
        //（与解码器逐位同源；残差与重建若用不同平面状态会引入系统性
        // 偏差）。I16/chroma 的预测只用宏块外边沿，无此约束。
        reconstruct_4x4(
            pic,
            mbx,
            mby,
            b,
            avail,
            modes.i4_modes[b],
            &mut work.luma_blocks[b],
        );
    }
    work.cbp |= cbp;
    let _ = (left, top);
    Ok(())
}

/// chroma 残差：两平面预测 → 逐块 DCT → DC 2x2 Hadamard 量化 →
/// AC 量化（raster 序 nnz 链）→ 反量化落重建块。cbp 的 chroma 位
/// 三态装配（见模块 doc）。
#[allow(clippy::too_many_arguments)] // 编码入参组，聚类反而伤可读性
fn chroma_levels(
    pic: &mut Picture,
    src: &YuvSource<'_>,
    qp: u8,
    cqmul: &[u32; 16],
    mbx: usize,
    mby: usize,
    avail: &NeighAvail,
    mode: u8,
    mb: &mut Macroblock,
    left: Option<NbInfo>,
    top: Option<NbInfo>,
    work: &mut MbWork,
) -> Result<(), H264Error> {
    let qd = dc_qmul(CHROMA_QP[usize::from(qp)]);
    let src_stride = src.chroma_stride;
    let cs = pic.chroma_stride();
    let mut any_dc = false;
    let mut any_ac = false;
    for (plane, (src_plane, data)) in [(src.cb, &mut pic.cb), (src.cr, &mut pic.cr)]
        .into_iter()
        .enumerate()
    {
        {
            let (t, l, tl) = gather_chroma_edges(data, cs, mbx, mby, avail);
            let base = mby * 8 * cs + mbx * 8;
            pred_8x8c(mode, &t, &l, tl, &mut data[base..], cs);
        }
        let srcp = &src_plane[mby * 8 * src_stride + mbx * 8..];
        // 逐 4x4 块差分 + DCT；DC 输入 D[i] = 4·Σr_i
        let mut dc_in = [0i64; 4];
        let mut coeff = [[0i32; 16]; 4];
        for i in 0..4usize {
            let (bx, by) = ((i % 2) * 4, (i / 2) * 4);
            let mut diff = [0i16; 16];
            let mut sum = 0i64;
            for k in 0..16usize {
                let s = i32::from(srcp[(by + k / 4) * src_stride + bx + k % 4]);
                let p = i32::from(data[(mby * 8 + by + k / 4) * cs + mbx * 8 + bx + k % 4]);
                diff[k] = (s - p) as i16;
                sum += i64::from(diff[k]);
            }
            dc_in[i] = 4 * sum;
            coeff[i] = fwd_dct4x4(&diff);
        }
        // DC：2x2 Hadamard + 量化（槽 0/1/4/5 = CHROMA_DC_SCAN）
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
        any_dc |= mb.nnz_chroma_dc[plane] > 0;
        // 重建块 DC（解码侧反量化，喂最终 level）
        let dcs = chroma_dc_dequant(&[lv[0], lv[1], lv[4], lv[5]], qd)?;
        for (i, dc) in dcs.iter().enumerate() {
            work.chroma_blocks[plane][i][0] = *dc;
        }
        // AC：raster 序（解析序）量化 + nnz + 反量化
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
    // chroma 位三态装配：有 AC → 0x20（隐含 DC），否则有 DC → 0x10
    if any_ac {
        work.cbp |= 0x20;
    } else if any_dc {
        work.cbp |= 0x10;
    }
    let _ = (left, top);
    Ok(())
}

/// 本地重建：镜像 `decode_macroblock` 的重建段（预测重放 + 残差叠加）。
/// Intra4x4 的 luma 已在 [`luma4_levels`] 里逐块即时重建（z 序块间
/// 预测依赖），此处只处理 Intra16x16 的 luma 与两条 chroma 路径。
fn reconstruct_mb(
    pic: &mut Picture,
    avail: &NeighAvail,
    mbx: usize,
    mby: usize,
    modes: &MbModes,
    work: &mut MbWork,
) {
    let ys = pic.luma_stride();
    if modes.use_i16 {
        let (t, l, tl) = gather_16x16_edges(pic, mbx, mby, avail);
        let base = mby * 16 * ys + mbx * 16;
        pred_16x16(modes.i16_mode, &t, &l, tl, &mut pic.luma[base..], ys);
        for &b in Z_ORDER.iter() {
            add_block(
                &mut pic.luma,
                ys,
                mby * 16 + (b / 4) * 4,
                mbx * 16 + (b % 4) * 4,
                &mut work.luma_blocks[b],
            );
        }
    }
    let cs = pic.chroma_stride();
    for (plane, data) in [&mut pic.cb, &mut pic.cr].into_iter().enumerate() {
        let (t, l, tl) = gather_chroma_edges(data, cs, mbx, mby, avail);
        let base = mby * 8 * cs + mbx * 8;
        pred_8x8c(modes.chroma_mode, &t, &l, tl, &mut data[base..], cs);
        for (i, cblk) in work.chroma_blocks[plane].iter_mut().enumerate() {
            add_block(data, cs, mby * 8 + (i / 2) * 4, mbx * 8 + (i % 2) * 4, cblk);
        }
    }
}

/// prev_intra4x4_pred_mode 链（解码侧 `decode_macroblock` 的同式）：
/// 块内左/上取本宏块已解析原始码，跨宏块取邻块**改写后**存储模式。
fn prev_for_block(b: usize, raws: &[u8; 16], left: Option<NbInfo>, top: Option<NbInfo>) -> u8 {
    let (cx, cy) = (b % 4, b / 4);
    let l = if cx > 0 {
        Some(raws[b - 1])
    } else {
        left.map(|n| nb_i4(&n, 3 + cy * 4))
    };
    let t = if cy > 0 {
        Some(raws[b - 4])
    } else {
        top.map(|n| nb_i4(&n, cx + 12))
    };
    prev_i4_mode(l, t)
}

/// prev_intra4x4_pred_mode 写（u(1) 直取 / u(1)0 + u(3) rem）。
/// prev ≥ 9（DC 变体域）时 rem = raw（决策侧已保证 raw ≤ 7）。
fn write_i4_mode(w: &mut BitWriter, raw: u8, prev: u8) -> Result<(), H264Error> {
    if prev <= 8 && raw == prev {
        w.u1(true);
        Ok(())
    } else {
        w.u1(false);
        let rem = raw - u8::from(raw > prev);
        w.u(3, u64::from(rem))
    }
}

/// luma AC 残差写出（象限门控、z 序、全 zigzag 扫描）——I_NxN 与 P 帧
/// 间共用（读侧 `decode_macroblock` I4 分支与 `decode_inter_mb` 的
/// 残差段同一段代码，写侧同理同一段）。
pub(crate) fn write_luma_ac_quadrants(
    w: &mut BitWriter,
    work: &MbWork,
    mb: &Macroblock,
    left: Option<NbInfo>,
    top: Option<NbInfo>,
) -> Result<(), H264Error> {
    for &b in Z_ORDER.iter() {
        let quadrant = 2 * (b >> 3) + ((b & 3) >> 1);
        if work.cbp & (1 << quadrant) == 0 {
            continue;
        }
        let ln = (b % 4 == 0).then(|| left.map(|n| n.nnz[b + 3])).flatten();
        let tn = (b / 4 == 0).then(|| top.map(|n| n.nnz[b + 12])).flatten();
        let nc = nc_luma(&mb.nnz, ln, tn, b);
        let tc = write_residual(w, BlockKind::LumaAc, nc, &ZIGZAG, &work.luma_levels[b])?;
        debug_assert_eq!(tc, mb.nnz[b]);
    }
    Ok(())
}

/// chroma 残差写出（先两平面 DC，后两平面 AC；解析序）——全部宏块
/// 类型共用（cbp 位门控由调用方保证）。
pub(crate) fn write_chroma_residuals(
    w: &mut BitWriter,
    work: &MbWork,
    mb: &Macroblock,
    left: Option<NbInfo>,
    top: Option<NbInfo>,
) -> Result<(), H264Error> {
    if work.cbp & 0x30 != 0 {
        for plane in 0..2usize {
            let nc = nc_chroma_dc(
                left.map(|n| n.nnz_cdc[plane]),
                top.map(|n| n.nnz_cdc[plane]),
            );
            write_residual(
                w,
                BlockKind::ChromaDc,
                nc,
                &CHROMA_DC_SCAN,
                &work.chroma_dc_levels[plane],
            )?;
        }
    }
    if work.cbp & 0x20 != 0 {
        for plane in 0..2usize {
            for i in 0..4usize {
                let base = 16 + plane * 4;
                let left_nz = (i % 2 == 0)
                    .then(|| left.map(|n| n.nnz[base + i + 1]))
                    .flatten();
                let top_nz = (i / 2 == 0)
                    .then(|| top.map(|n| n.nnz[base + i + 2]))
                    .flatten();
                let nc = nc_chroma_ac(&mb.nnz, left_nz, top_nz, plane, i);
                write_residual(
                    w,
                    BlockKind::ChromaAc,
                    nc,
                    &ZIGZAG[1..],
                    &work.chroma_ac_levels[plane][i],
                )?;
            }
        }
    }
    Ok(())
}

/// 宏块层语法写出（`decode_macroblock` 解析序的镜像）。`slice` 仅影响
/// intra 的 mb_type 值（P 片 +5；inter 宏块不走本函数——mb_type/mvd/
/// cbp 写序见 `pframe.rs`，其中 luma/chroma 残差段复用本模块的两个
/// 公共写出函数）。
#[allow(clippy::too_many_arguments)] // 写出参数组，聚类反而伤可读性
fn write_mb_syntax(
    w: &mut BitWriter,
    work: &MbWork,
    modes: &MbModes,
    mb: &Macroblock,
    left: Option<NbInfo>,
    top: Option<NbInfo>,
    mb_qp: &mut u8,
    slice: SliceKind,
) -> Result<(), H264Error> {
    let p_off = u64::from(slice == SliceKind::P) * 5;
    match mb.kind {
        MbKind::Intra4x4 => {
            w.ue(p_off)?; // mb_type（I 片 0 / P 片 5）
            for &b in Z_ORDER.iter() {
                let prev = prev_for_block(b, &modes.i4_raws, left, top);
                write_i4_mode(w, modes.i4_raws[b], prev)?;
            }
            w.ue(u64::from(modes.chroma_raw))?; // intra_chroma_pred_mode
            write_coded_block_pattern(w, work.cbp, true)?;
            if work.cbp > 0 {
                write_mb_qp_delta(w, 0, mb_qp)?;
            }
            write_luma_ac_quadrants(w, work, mb, left, top)?;
        }
        MbKind::Intra16x16 => {
            let variant = I16_CBP.iter().position(|&c| c == work.cbp).ok_or_else(|| {
                H264Error::InvalidData(format!(
                    "i16 cbp {:#04x} not in I16_CBP (chroma bits must be 3-state)",
                    work.cbp
                ))
            })?;
            w.ue(p_off + 1 + 4 * variant as u64 + u64::from(modes.i16_raw))?; // mb_type
            w.ue(u64::from(modes.chroma_raw))?; // intra_chroma_pred_mode
            write_mb_qp_delta(w, 0, mb_qp)?; // I16 恒写
            // luma DC（解析序最前：nC 以全零 luma nnz 起算）
            let nc0 = nc_luma(&[0; 24], left.map(|n| n.nnz[3]), top.map(|n| n.nnz[12]), 0);
            let dc = work
                .luma_dc
                .ok_or_else(|| H264Error::InvalidData("i16 without dc levels".into()))?;
            write_residual(w, BlockKind::LumaDc, nc0, &ZIGZAG, &dc)?;
            // luma AC（z 序；scan 从 1 起）
            if work.cbp & 0x0F != 0 {
                for &b in Z_ORDER.iter() {
                    let ln = (b % 4 == 0).then(|| left.map(|n| n.nnz[b + 3])).flatten();
                    let tn = (b / 4 == 0).then(|| top.map(|n| n.nnz[b + 12])).flatten();
                    let nc = nc_luma(&mb.nnz, ln, tn, b);
                    let tc = write_residual(
                        w,
                        BlockKind::LumaAc,
                        nc,
                        &ZIGZAG[1..],
                        &work.luma_levels[b],
                    )?;
                    debug_assert_eq!(tc, mb.nnz[b]);
                }
            }
        }
        MbKind::Pcm | MbKind::InterP | MbKind::InterB => {
            return Err(H264Error::InvalidData(
                "unexpected mb kind in intra syntax writer".into(),
            ));
        }
    }
    write_chroma_residuals(w, work, mb, left, top)
}

// ---------------------------------------------------------------------------
// 去块：行粒度调度（镜像 slice.rs 私有组织循环；核与判定全用 deblock.rs）
// ---------------------------------------------------------------------------

/// 滤一行宏块 [x0, x1) 的全部去块边（含它们与上一行/左邻 MB 的边界）。
/// 缺省参数 = 全滤、跨片、零偏移（PPS 不发去块控制字段的解码侧缺省）。
pub(crate) fn deblock_row(pic: &mut Picture, y: usize, x0: usize, x1: usize) {
    for x in x0..x1 {
        deblock_mb(pic, x, y);
    }
}

/// 滤单个宏块（16x16 luma + 8x8 Cb/Cr）——`slice.rs` `deblock_mb` 的
/// 镜像（边序/QP 取值/bS 段判定逐字；DeblockParams 取缺省）。
fn deblock_mb(pic: &mut Picture, mbx: usize, mby: usize) {
    let dp = DeblockParams::default();
    let w = pic.mb_width;
    let mb_xy = mby * w + mbx;
    if !pic.mbs[mb_xy].decoded {
        return;
    }
    let cur_qp = pic.mbs[mb_xy].qp;
    let left_ok = mbx > 0 && pic.mbs[mb_xy - 1].decoded;
    let top_ok = mby > 0 && pic.mbs[mb_xy - w].decoded;

    let ls = pic.luma_stride();
    let cs = pic.chroma_stride();
    let y0 = mby * 16;
    let x0 = mbx * 16;

    // ---- luma 垂直边 e = 0..4（列 x0 + 4e；p 块列 e−1 | q 块列 e）----
    for e in 0..4usize {
        let mut bs = [0u8; 4];
        let mut any = false;
        let (p_src, q_src): (Option<usize>, Option<usize>) = if e == 0 {
            (if left_ok { Some(mb_xy - 1) } else { None }, Some(mb_xy))
        } else {
            (Some(mb_xy), Some(mb_xy))
        };
        for (i, b) in bs.iter_mut().enumerate() {
            let p_blk = if e == 0 { 3 + 4 * i } else { (e - 1) + 4 * i };
            let q_blk = e + 4 * i;
            *b = bs_of(pic, p_src, p_blk, q_src, q_blk, e == 0, &dp);
            any |= *b != 0;
        }
        if !any {
            continue;
        }
        let qp = if e == 0 {
            deblock::edge_qp_luma(pic.mbs[mb_xy - 1].qp, cur_qp)
        } else {
            i32::from(cur_qp)
        };
        let alpha = deblock::alpha_of(qp, dp.alpha_offset);
        let beta = deblock::beta_of(qp, dp.beta_offset);
        let ex = x0 + 4 * e;
        if bs[0] == 4 {
            deblock::filter_luma_v_intra(&mut pic.luma, ls, ex, y0, alpha, beta);
        } else {
            let tc0 = [
                deblock::tc0_of(qp, dp.alpha_offset, bs[0], false),
                deblock::tc0_of(qp, dp.alpha_offset, bs[1], false),
                deblock::tc0_of(qp, dp.alpha_offset, bs[2], false),
                deblock::tc0_of(qp, dp.alpha_offset, bs[3], false),
            ];
            deblock::filter_luma_v(&mut pic.luma, ls, ex, y0, alpha, beta, &bs, &tc0);
        }
    }

    // ---- luma 水平边 e = 0..4（行 y0 + 4e）----
    for e in 0..4usize {
        let mut bs = [0u8; 4];
        let mut any = false;
        let (p_src, q_src): (Option<usize>, Option<usize>) = if e == 0 {
            (if top_ok { Some(mb_xy - w) } else { None }, Some(mb_xy))
        } else {
            (Some(mb_xy), Some(mb_xy))
        };
        for (i, b) in bs.iter_mut().enumerate() {
            let p_blk = if e == 0 { 12 + i } else { i + 4 * (e - 1) };
            let q_blk = i + 4 * e;
            *b = bs_of(pic, p_src, p_blk, q_src, q_blk, e == 0, &dp);
            any |= *b != 0;
        }
        if !any {
            continue;
        }
        let qp = if e == 0 {
            deblock::edge_qp_luma(pic.mbs[mb_xy - w].qp, cur_qp)
        } else {
            i32::from(cur_qp)
        };
        let alpha = deblock::alpha_of(qp, dp.alpha_offset);
        let beta = deblock::beta_of(qp, dp.beta_offset);
        let ey = y0 + 4 * e;
        if bs[0] == 4 {
            deblock::filter_luma_h_intra(&mut pic.luma, ls, x0, ey, alpha, beta);
        } else {
            let tc0 = [
                deblock::tc0_of(qp, dp.alpha_offset, bs[0], false),
                deblock::tc0_of(qp, dp.alpha_offset, bs[1], false),
                deblock::tc0_of(qp, dp.alpha_offset, bs[2], false),
                deblock::tc0_of(qp, dp.alpha_offset, bs[3], false),
            ];
            deblock::filter_luma_h(&mut pic.luma, ls, x0, ey, alpha, beta, &bs, &tc0);
        }
    }

    // ---- chroma 垂直边 e ∈ {0, 2}（列 cx0 + 2e，8 样本）----
    let cx0 = mbx * 8;
    let cy0 = mby * 8;
    for e in [0usize, 2] {
        let mut bs = [0u8; 4];
        let mut any = false;
        let (p_src, q_src): (Option<usize>, Option<usize>) = if e == 0 {
            (if left_ok { Some(mb_xy - 1) } else { None }, Some(mb_xy))
        } else {
            (Some(mb_xy), Some(mb_xy))
        };
        for (i, b) in bs.iter_mut().enumerate() {
            let p_blk = if e == 0 { 3 + 4 * i } else { (e - 1) + 4 * i };
            let q_blk = e + 4 * i;
            *b = bs_of(pic, p_src, p_blk, q_src, q_blk, e == 0, &dp);
            any |= *b != 0;
        }
        if !any {
            continue;
        }
        let qp = if e == 0 {
            deblock::edge_qp_chroma(pic.mbs[mb_xy - 1].qp, cur_qp, dp.chroma_qp_offset)
        } else {
            i32::from(deblock::chroma_qp_of(cur_qp, dp.chroma_qp_offset))
        };
        let alpha = deblock::alpha_of(qp, dp.alpha_offset);
        let beta = deblock::beta_of(qp, dp.beta_offset);
        let ex = cx0 + 2 * e;
        if bs[0] == 4 {
            deblock::filter_chroma_v_intra(&mut pic.cb, cs, ex, cy0, alpha, beta);
            deblock::filter_chroma_v_intra(&mut pic.cr, cs, ex, cy0, alpha, beta);
        } else {
            let tc0 = [
                deblock::tc0_of(qp, dp.alpha_offset, bs[0], true),
                deblock::tc0_of(qp, dp.alpha_offset, bs[1], true),
                deblock::tc0_of(qp, dp.alpha_offset, bs[2], true),
                deblock::tc0_of(qp, dp.alpha_offset, bs[3], true),
            ];
            deblock::filter_chroma_v(&mut pic.cb, cs, ex, cy0, alpha, beta, &bs, &tc0);
            deblock::filter_chroma_v(&mut pic.cr, cs, ex, cy0, alpha, beta, &bs, &tc0);
        }
    }

    // ---- chroma 水平边 e ∈ {0, 2}----
    for e in [0usize, 2] {
        let mut bs = [0u8; 4];
        let mut any = false;
        let (p_src, q_src): (Option<usize>, Option<usize>) = if e == 0 {
            (if top_ok { Some(mb_xy - w) } else { None }, Some(mb_xy))
        } else {
            (Some(mb_xy), Some(mb_xy))
        };
        for (i, b) in bs.iter_mut().enumerate() {
            let p_blk = if e == 0 { 12 + i } else { i + 4 * (e - 1) };
            let q_blk = i + 4 * e;
            *b = bs_of(pic, p_src, p_blk, q_src, q_blk, e == 0, &dp);
            any |= *b != 0;
        }
        if !any {
            continue;
        }
        let qp = if e == 0 {
            deblock::edge_qp_chroma(pic.mbs[mb_xy - w].qp, cur_qp, dp.chroma_qp_offset)
        } else {
            i32::from(deblock::chroma_qp_of(cur_qp, dp.chroma_qp_offset))
        };
        let alpha = deblock::alpha_of(qp, dp.alpha_offset);
        let beta = deblock::beta_of(qp, dp.beta_offset);
        let ey = cy0 + 2 * e;
        if bs[0] == 4 {
            deblock::filter_chroma_h_intra(&mut pic.cb, cs, cx0, ey, alpha, beta);
            deblock::filter_chroma_h_intra(&mut pic.cr, cs, cx0, ey, alpha, beta);
        } else {
            let tc0 = [
                deblock::tc0_of(qp, dp.alpha_offset, bs[0], true),
                deblock::tc0_of(qp, dp.alpha_offset, bs[1], true),
                deblock::tc0_of(qp, dp.alpha_offset, bs[2], true),
                deblock::tc0_of(qp, dp.alpha_offset, bs[3], true),
            ];
            deblock::filter_chroma_h(&mut pic.cb, cs, cx0, ey, alpha, beta, &bs, &tc0);
            deblock::filter_chroma_h(&mut pic.cr, cs, cx0, ey, alpha, beta, &bs, &tc0);
        }
    }
}

/// 段级 bS（`slice.rs` `bs_of` 镜像；跨片门控恒开——单 slice 全帧）。
fn bs_of(
    pic: &Picture,
    p_src: Option<usize>,
    p_blk: usize,
    q_src: Option<usize>,
    q_blk: usize,
    mb_boundary: bool,
    dp: &DeblockParams,
) -> u8 {
    let (Some(pi), Some(qi)) = (p_src, q_src) else {
        return 0;
    };
    deblock::boundary_strength_gated(
        &pic.mbs[pi],
        p_blk,
        &pic.mbs[qi],
        q_blk,
        mb_boundary,
        dp.filter_across_slices,
    )
}

// ---------------------------------------------------------------------------
// 测试：参数集/片头回环 + 帧级黑盒回环（M3 §3.1 口径：逐像素 0 容差）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitreader::BitReader;
    use crate::decoder::H264Decoder;
    use crate::nal::{nal_header_and_payload, unescape_rbsp};
    use crate::slice::{SliceType, parse_slice_header, rbsp_data_bits};
    use crate::sps::{Pps, Sps};
    use std::sync::Arc;
    use ye_codec::{
        CodecId, CodecParameters, Decoded, Decoder, Frame, MediaType, Packet, VideoFrame,
    };

    /// 确定性 LCG（对齐 ye-filter 测试先例 video/mod.rs 的向量风格）。
    struct Lcg(u32);
    impl Lcg {
        fn next_u8(&mut self) -> u8 {
            self.0 = self.0.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (self.0 >> 16) as u8
        }
    }

    /// 三类合成向量（M3 §3.4）：0=纯色、1=渐变、2=LCG 纹理。
    fn gen_frame(w: usize, h: usize, family: u32, seed: u32) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let (cw, ch) = (w / 2, h / 2);
        let mut luma = vec![0u8; w * h];
        let mut cb = vec![0u8; cw * ch];
        let mut cr = vec![0u8; cw * ch];
        let mut rng = Lcg(seed.wrapping_mul(2_654_435_761).wrapping_add(1));
        for y in 0..h {
            for x in 0..w {
                luma[y * w + x] = match family {
                    0 | 3 => 128,
                    1 => ((x * 3 + y * 5) & 0xFF) as u8,
                    _ => rng.next_u8(),
                };
            }
        }
        for y in 0..ch {
            for x in 0..cw {
                cb[y * cw + x] = match family {
                    0 | 3 => 72,
                    1 => ((x * 4 + y * 3 + 40) & 0xFF) as u8,
                    _ => rng.next_u8(),
                };
                cr[y * cw + x] = match family {
                    0 | 3 => 148,
                    1 => ((x * 2 + y * 7 + 90) & 0xFF) as u8,
                    _ => rng.next_u8(),
                };
            }
        }
        (luma, cb, cr)
    }

    /// Annex B 包装。
    fn annexb(nal: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1];
        v.extend_from_slice(nal);
        v
    }

    /// 黑盒回环单例：合成帧 → [`encode_idr_frame`] → 自家 `H264Decoder`
    ///（经 NAL/码流路径：extradata = Annex B SPS/PPS，包体 = Annex B
    /// IDR，不经内部结构接线）→ 输出帧与本地重建逐像素比对（0 容差）。
    fn roundtrip_case(w: usize, h: usize, qp: u8, family: u32, seed: u32) {
        let (luma, cb, cr) = gen_frame(w, h, family, seed);
        let src = YuvSource {
            luma: &luma,
            luma_stride: w,
            cb: &cb,
            cr: &cr,
            chroma_stride: w / 2,
        };
        let enc = encode_idr_frame(w, h, 30, &src, qp)
            .unwrap_or_else(|e| panic!("encode {w}x{h} qp{qp} fam{family}: {e}"));

        // 黑盒解码（对齐 lib.rs 输入契约的 Annex B 分支）
        let mut params = CodecParameters::new(CodecId::H264, MediaType::Video);
        let mut extradata = annexb(&enc.sps_nal);
        extradata.extend_from_slice(&annexb(&enc.pps_nal));
        params.extradata = Some(extradata);
        let mut dec = H264Decoder::new(&params)
            .unwrap_or_else(|e| panic!("decoder new {w}x{h} qp{qp} fam{family}: {e:?}"));
        let mut pkt = Packet::empty(0);
        pkt.data = Arc::from(annexb(&enc.idr_nal).into_boxed_slice());
        dec.feed(&pkt)
            .unwrap_or_else(|e| panic!("feed {w}x{h} qp{qp} fam{family}: {e:?}"));
        dec.flush();
        let frame = match dec.pull().unwrap() {
            Decoded::Frame(Frame::Video(f)) => f,
            other => panic!("expected frame, got {other:?} ({w}x{h} qp{qp} fam{family})"),
        };
        assert!(matches!(dec.pull().unwrap(), Decoded::Finished));

        compare_planes(&frame, &enc, w, h, qp, family);
    }

    /// 逐像素比对（0 容差）：解码输出（裁剪后 yuv420p）vs 本地重建。
    fn compare_planes(
        frame: &VideoFrame,
        enc: &EncodedIFrame,
        w: usize,
        h: usize,
        qp: u8,
        fam: u32,
    ) {
        let (cw, ch) = (w / 2, h / 2);
        let check =
            |name: &str, got: &[u8], gstride: usize, want: &[u8], rows: usize, cols: usize| {
                for y in 0..rows {
                    for x in 0..cols {
                        assert_eq!(
                            got[y * gstride + x],
                            want[y * cols + x],
                            "{name} pixel ({x},{y}) mismatch {w}x{h} qp{qp} fam{fam}"
                        );
                    }
                }
            };
        let ls = frame.layout().plane(0).stride;
        let cs = frame.layout().plane(1).stride;
        check("luma", frame.plane(0), ls, &enc.recon_luma, h, w);
        check("cb", frame.plane(1), cs, &enc.recon_cb, ch, cw);
        check("cr", frame.plane(2), cs, &enc.recon_cr, ch, cw);
    }

    /// 帧级黑盒回环主组：三类向量 × 尺寸（极小 32x16、非宏块对齐
    /// 40x24、宏块对齐 64x48）× QP 域覆盖（0 / 12 / 26 / 40 / 51）。
    #[test]
    fn idr_roundtrip_three_families_sizes_qps() {
        for family in 0..3u32 {
            for (w, h) in [(32usize, 16usize), (40, 24), (64, 48)] {
                for qp in [0u8, 12, 26, 40, 51] {
                    roundtrip_case(w, h, qp, family, 0xA5A5_1234 + family * 7 + u32::from(qp));
                }
            }
        }
    }

    /// 大些的纹理帧（去块跨多行/列；随机种子扰动）。
    #[test]
    fn idr_roundtrip_noise_96x64() {
        for qp in [17u8, 33] {
            roundtrip_case(96, 64, qp, 2, 0x5EED_0000 + u32::from(qp));
        }
    }

    /// 纯色帧在低 QP 的重建应与源完全一致（位级回环之外的合理性质：
    /// Intra16x16 DC 预测后残差为零）。
    #[test]
    fn idr_roundtrip_solid_near_lossless_low_qp() {
        let (w, h, qp) = (48usize, 32usize, 10u8);
        let (luma, cb, cr) = gen_frame(w, h, 0, 1);
        let src = YuvSource {
            luma: &luma,
            luma_stride: w,
            cb: &cb,
            cr: &cr,
            chroma_stride: w / 2,
        };
        let enc = encode_idr_frame(w, h, 30, &src, qp).unwrap();
        assert_eq!(enc.recon_luma, luma);
        assert_eq!(enc.recon_cb, cb);
        assert_eq!(enc.recon_cr, cr);
    }

    /// 参数集写侧（T5 定形）→ 解析回环：字段逐项相等（SPS
    /// 宽高裁剪/POC 类型/level 透传，PPS 的 QP 中立/CAVLC/去块缺席）。
    #[test]
    fn sps_pps_write_parse_roundtrip() {
        for (w, h) in [(32usize, 16usize), (40, 24), (64, 48), (160, 128)] {
            for level in [10u8, 30, 40] {
                let sps_nal = write_sps_nal(w, h, level).unwrap();
                let sps = Sps::parse(&sps_nal).unwrap();
                assert_eq!(sps.profile_idc, 66);
                assert!(sps.constraint_set(0));
                assert_eq!(sps.level_idc, level, "level_idc 须逐字节透传");
                assert_eq!(sps.seq_parameter_set_id, 0);
                assert_eq!(sps.pic_order_cnt_type, 2);
                assert_eq!(sps.max_num_ref_frames, 1);
                assert_eq!(sps.pic_width_in_mbs as usize, w.div_ceil(16));
                assert_eq!(sps.pic_height_in_map_units as usize, h.div_ceil(16));
                assert!(sps.frame_mbs_only_flag);
                assert_eq!(sps.width() as usize, w);
                assert_eq!(sps.height() as usize, h);
                assert!(!sps.vui_parameters_present_flag);
                assert!(sps.is_baseline());
            }

            let pps_nal = write_pps_nal().unwrap();
            let sps_nal = write_sps_nal(w, h, 30).unwrap();
            let sps = Sps::parse(&sps_nal).unwrap();
            let pps = Pps::parse_with_sps(&pps_nal, Some(&sps)).unwrap();
            assert_eq!(pps.pic_parameter_set_id, 0);
            assert_eq!(pps.seq_parameter_set_id, 0);
            assert!(!pps.entropy_coding_mode_flag);
            assert!(!pps.deblocking_filter_control_present_flag);
            assert!(!pps.constrained_intra_pred_flag);
            assert!(!pps.redundant_pic_cnt_present_flag);
            assert_eq!(pps.pic_init_qp_minus26, 0, "PPS QP 中立");
            assert_eq!(pps.chroma_qp_index_offset, 0);
            // Baseline + set0 → 扩展段不解析
            assert!(!pps.transform_8x8_mode_flag);
        }
    }

    /// 片头雏形 → 解析回环：经自家 parse_slice_header 读回字段（含
    /// 去块缺省 = 全滤）。
    #[test]
    fn slice_header_write_parse_roundtrip() {
        let (w, h) = (48usize, 32usize);
        let (luma, cb, cr) = gen_frame(w, h, 1, 7);
        let src = YuvSource {
            luma: &luma,
            luma_stride: w,
            cb: &cb,
            cr: &cr,
            chroma_stride: w / 2,
        };
        let enc = encode_idr_frame(w, h, 30, &src, 23).unwrap();
        let sps = Sps::parse(&enc.sps_nal).unwrap();
        let pps = Pps::parse_with_sps(&enc.pps_nal, Some(&sps)).unwrap();
        let (header, payload) = nal_header_and_payload(&enc.idr_nal).unwrap();
        assert_eq!(header.nal_unit_type, 5);
        assert_eq!(header.nal_ref_idc, 3);
        let rbsp = unescape_rbsp(payload);
        let mut r = BitReader::new(&rbsp);
        let sh = parse_slice_header(&mut r, &sps, &pps, true, 3).unwrap();
        assert_eq!(sh.first_mb_in_slice, 0);
        assert_eq!(sh.slice_type, SliceType::I);
        assert_eq!(sh.pps_id, 0);
        assert_eq!(sh.frame_num, 0);
        assert_eq!(sh.idr_pic_id, 0);
        assert_eq!(sh.slice_qp, 23);
        assert!(!sh.long_term_reference_flag);
        assert!(sh.deblock.enabled, "I 帧边界必须同样滤波（缺省全滤）");
        assert!(sh.deblock.filter_across_slices);
    }

    /// 输入校验：奇数尺寸 / 零尺寸 / QP 越界 / 源面过小。
    #[test]
    fn encode_input_validation() {
        let (luma, cb, cr) = gen_frame(32, 16, 0, 1);
        let src = YuvSource {
            luma: &luma,
            luma_stride: 32,
            cb: &cb,
            cr: &cr,
            chroma_stride: 16,
        };
        assert!(matches!(
            encode_idr_frame(31, 16, 30, &src, 20),
            Err(H264Error::InvalidArguments(_))
        ));
        assert!(matches!(
            encode_idr_frame(32, 0, 30, &src, 20),
            Err(H264Error::InvalidArguments(_))
        ));
        assert!(matches!(
            encode_idr_frame(32, 16, 30, &src, 52),
            Err(H264Error::InvalidArguments(_))
        ));
        let small = YuvSource {
            luma: &luma[..100],
            luma_stride: 32,
            cb: &cb,
            cr: &cr,
            chroma_stride: 16,
        };
        assert!(matches!(
            encode_idr_frame(32, 16, 30, &small, 20),
            Err(H264Error::InvalidArguments(_))
        ));
    }

    /// 码流形态：IDR NAL 的 RBSP 尾随位合法；码流紧凑（远小于 PCM
    /// 直写形态）。
    #[test]
    fn idr_nal_has_valid_trailing_bits() {
        let (w, h) = (32usize, 16usize);
        let (luma, cb, cr) = gen_frame(w, h, 1, 3);
        let src = YuvSource {
            luma: &luma,
            luma_stride: w,
            cb: &cb,
            cr: &cr,
            chroma_stride: w / 2,
        };
        let enc = encode_idr_frame(w, h, 30, &src, 26).unwrap();
        let (_, payload) = nal_header_and_payload(&enc.idr_nal).unwrap();
        let rbsp = unescape_rbsp(payload);
        assert!(rbsp_data_bits(&rbsp) > 0, "须有停止位");
        assert!(enc.idr_nal.len() < 384 * 8 + 64);
    }
}
