//! 片层：I 帧片头解析 + 片数据（宏块循环）。
//!
//! 片头字段顺序与取值约束逐字对齐 FFmpeg `h264_slice_header_parse`
//! （first_mb_in_slice → slice_type → pps_id → frame_num → [idr_pic_id]
//! → [poc] → [redundant_pic_cnt] → dec_ref_pic_marking → slice_qp_delta
//! → [去块参数]）；片循环对齐 `decode_slice` 的 CAVLC 分支：
//! RBSP 尾随位剥离（`get_bit_length` 语义）后，按"宏块数到界或数据位
//! 耗尽"终止，越界进入下一片 → Err。
//!
//! ②a 形态收紧：仅 I 片（CAVLC）；`disable_deblocking_filter_idc` 必须
//! 为 2（禁用，②b 落地去块前如实拒绝 0/1）；场图/冗余片/数据分区
//! 一律 Unsupported 或 Err，绝不输出错帧。

use crate::bitreader::BitReader;
use crate::deblock;
use crate::error::H264Error;
use crate::inter::InterCtx;
use crate::macroblock::{Picture, decode_macroblock, decode_skip_mb};
use crate::sps::{Pps, Sps};

/// 片类型（golomb_to_pict_type = [P, B, I, SP, SI]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SliceType {
    P,
    B,
    I,
    Sp,
    Si,
}

impl SliceType {
    pub(crate) fn from_code(code: u64) -> Result<Self, H264Error> {
        if code > 9 {
            return Err(H264Error::InvalidData(format!(
                "slice_type {code} out of range"
            )));
        }
        let c = if code > 4 { code - 5 } else { code };
        Ok(match c {
            0 => SliceType::P,
            1 => SliceType::B,
            2 => SliceType::I,
            3 => SliceType::Sp,
            _ => SliceType::Si,
        })
    }
}

/// 片头解析结果。`slice_type`/`pps_id`/`idr_pic_id` 供上游（decoder.rs）
/// 诊断与后续阶段（②b/②c 的片级状态）消费。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct SliceHeader {
    pub first_mb_in_slice: usize,
    pub slice_type: SliceType,
    /// 生成的片头里 u(n) 字段之外的 nal 信息由调用方带入。
    pub pps_id: u8,
    pub frame_num: u32,
    pub idr_pic_id: u32,
    pub slice_qp: u8,
    /// P 片：NumRefIdxL0Active（I 片 = 0）。
    pub num_ref_l0: usize,
    /// P 片：ref_pic_list_modification 操作序列 (idc, value)。
    pub ref_mods: Vec<(u8, u64)>,
    /// dec_ref_pic_marking：IDR 的 long_term_reference_flag。
    pub long_term_reference_flag: bool,
    /// dec_ref_pic_marking：非 IDR 自适应标记操作 (opcode, diff, long_arg)。
    pub mmco: Vec<(u8, u64, u64)>,
    // ---- B 片 / POC / CABAC 增补（M2）----
    /// B 片：NumRefIdxL1Active；非 B = 0。
    pub num_ref_l1: usize,
    /// B 片：List1 的 ref_pic_list_modification 操作序列。
    pub ref_mods_l1: Vec<(u8, u64)>,
    /// B 片：direct_spatial_mv_pred_flag。
    pub direct_spatial_mv_pred: bool,
    /// poc type 0：pic_order_cnt_lsb。
    pub poc_lsb: u32,
    /// poc type 0：delta_pic_order_cnt_bottom。
    pub delta_poc_bottom: i32,
    /// poc type 1：delta_pic_order_cnt[0..2]。
    pub delta_poc: [i32; 2],
    /// cabac_init_idc（CABAC 且非 I 片时出现；CAVLC = 0）。
    pub cabac_init_idc: u8,
    /// 去块参数（片级）。
    pub deblock: DeblockParams,
}

/// 去块滤波片级参数（FFmpeg deblocking_filter/slice_alpha_c0_offset/
/// slice_beta_offset 同语义）。
#[derive(Debug, Clone)]
pub(crate) struct DeblockParams {
    /// 是否滤波（idc != 1）。
    pub enabled: bool,
    /// 跨片边滤波（idc != 2）。
    pub filter_across_slices: bool,
    /// slice_alpha_c0_offset（除 2 值 ×2）。
    pub alpha_offset: i32,
    /// slice_beta_offset（除 2 值 ×2）。
    pub beta_offset: i32,
}

impl Default for DeblockParams {
    fn default() -> Self {
        // 控制字段缺席时的缺省：全滤、跨片、零偏移
        DeblockParams {
            enabled: true,
            filter_across_slices: true,
            alpha_offset: 0,
            beta_offset: 0,
        }
    }
}

/// RBSP 有效数据位长（FFmpeg h2645_parse.c `get_bit_length`
/// 逐字：先剥尾部零字节，再剥最后字节的尾随零与停止位）。
/// 返回 0 表示无停止位（非法 RBSP）。
pub(crate) fn rbsp_data_bits(rbsp: &[u8]) -> usize {
    let mut size = rbsp.len();
    while size > 0 && rbsp[size - 1] == 0 {
        size -= 1;
    }
    if size == 0 {
        return 0;
    }
    let last = rbsp[size - 1];
    size * 8 - (last.trailing_zeros() as usize + 1)
}

/// 解析片头（不含 nal 头字节；`r` 覆盖未转义的 RBSP 全体）。
///
/// # Errors
/// - `InvalidData`：字段越界（first_mb/slice_type/qp 等）；
/// - `Unsupported`：②a 之外但位流合法的形态（场图、P/B/SP/SI 片、
///   去块 idc ≠ 2、冗余片）。
pub(crate) fn parse_slice_header(
    r: &mut BitReader<'_>,
    sps: &Sps,
    pps: &Pps,
    is_idr: bool,
    nal_ref_idc: u8,
) -> Result<SliceHeader, H264Error> {
    let mb_total = usize::from(sps.pic_width_in_mbs) * usize::from(sps.pic_height_in_map_units);

    let first_mb_in_slice = r.ue()?;
    if first_mb_in_slice as usize >= mb_total {
        return Err(H264Error::InvalidData(format!(
            "first_mb_in_slice {first_mb_in_slice} out of range ({mb_total} mbs)"
        )));
    }

    let slice_type = SliceType::from_code(r.ue()?)?;
    match slice_type {
        SliceType::I | SliceType::P | SliceType::B => {}
        SliceType::Sp | SliceType::Si => {
            return Err(H264Error::Unsupported("SP/SI slices unsupported"));
        }
    }

    let pps_id = r.ue()?;
    if pps_id > 255 {
        return Err(H264Error::InvalidData(format!(
            "pic_parameter_set_id {pps_id} out of range"
        )));
    }

    let frame_num = r.u(u32::from(sps.log2_max_frame_num_minus4) + 4)?;

    if !sps.frame_mbs_only_flag {
        return Err(H264Error::Unsupported(
            "field / MBAFF pictures unsupported (frame_mbs_only_flag=0)",
        ));
    }

    let mut idr_pic_id = 0u32;
    if is_idr {
        idr_pic_id = r.ue()? as u32;
        if idr_pic_id > 65535 {
            return Err(H264Error::InvalidData(format!(
                "idr_pic_id {idr_pic_id} out of range"
            )));
        }
    }

    // poc：按 SPS 类型消费并记录（M2 起 POC 参与计算）
    let mut poc_lsb = 0u32;
    let mut delta_poc_bottom = 0i32;
    let mut delta_poc = [0i32; 2];
    match sps.pic_order_cnt_type {
        0 => {
            poc_lsb = r.u(u32::from(sps.log2_max_pic_order_cnt_lsb_minus4) + 4)? as u32;
            if pps.bottom_field_pic_order_in_frame_present_flag {
                delta_poc_bottom = r.se()? as i32;
            }
        }
        1 if !sps.delta_pic_order_always_zero_flag => {
            delta_poc[0] = r.se()? as i32;
            if pps.bottom_field_pic_order_in_frame_present_flag {
                delta_poc[1] = r.se()? as i32;
            }
        }
        _ => {}
    }

    if pps.redundant_pic_cnt_present_flag {
        let cnt = r.ue()?;
        if cnt > 127 {
            return Err(H264Error::InvalidData(format!(
                "redundant_pic_cnt {cnt} out of range"
            )));
        }
        if cnt != 0 {
            return Err(H264Error::Unsupported("redundant slices unsupported"));
        }
    }

    // ---- P/B 片：参考帧数量覆盖 + 列表重排序（FFmpeg parse_ref_count /
    // decode_ref_pic_list_reordering 逐字语义；I 片跳过）----
    let mut num_ref_l0 = 0usize;
    let mut num_ref_l1 = 0usize;
    let mut ref_mods: Vec<(u8, u64)> = Vec::new();
    let mut ref_mods_l1: Vec<(u8, u64)> = Vec::new();
    let mut direct_spatial_mv_pred = true;
    let read_reorder =
        |r: &mut BitReader<'_>, num_ref: usize| -> Result<Vec<(u8, u64)>, H264Error> {
            let mut mods = Vec::new();
            let mut count = 0usize;
            loop {
                let op = r.ue()?;
                if op == 3 {
                    break;
                }
                if op > 2 {
                    return Err(H264Error::InvalidData(format!(
                        "modification_of_pic_nums_idc {op} invalid"
                    )));
                }
                if count >= num_ref {
                    return Err(H264Error::InvalidData(
                        "ref list modification count overflow".into(),
                    ));
                }
                let val = r.ue()?;
                mods.push((op as u8, val));
                count += 1;
            }
            Ok(mods)
        };
    let read_num_ref = |r: &mut BitReader<'_>, which: u8| -> Result<usize, H264Error> {
        if r.u1()? {
            let n = r.ue()?;
            if n > 14 {
                return Err(H264Error::InvalidData(format!(
                    "num_ref_idx_l{which}_active_minus1 {n} out of range"
                )));
            }
            return Ok(n as usize + 1);
        }
        Ok(usize::MAX) // 哨兵：未覆盖
    };
    if slice_type == SliceType::P {
        if pps.weighted_pred_flag {
            return Err(H264Error::Unsupported(
                "weighted prediction (P) lands in a later stage",
            ));
        }
        num_ref_l0 = usize::from(pps.num_ref_idx_l0_default_active_minus1) + 1;
        if let n @ 0..=15 = read_num_ref(r, 0)? {
            num_ref_l0 = n;
        }
        if r.u1()? {
            ref_mods = read_reorder(r, num_ref_l0)?;
        }
    } else if slice_type == SliceType::B {
        if pps.weighted_bipred_idc != 0 {
            return Err(H264Error::Unsupported(
                "explicit weighted biprediction lands in a later stage",
            ));
        }
        direct_spatial_mv_pred = r.u1()?;
        num_ref_l0 = usize::from(pps.num_ref_idx_l0_default_active_minus1) + 1;
        num_ref_l1 = usize::from(pps.num_ref_idx_l1_default_active_minus1) + 1;
        if let n @ 0..=15 = read_num_ref(r, 0)? {
            num_ref_l0 = n;
        }
        if let n @ 0..=15 = read_num_ref(r, 1)? {
            num_ref_l1 = n;
        }
        if r.u1()? {
            ref_mods = read_reorder(r, num_ref_l0)?;
        }
        if r.u1()? {
            ref_mods_l1 = read_reorder(r, num_ref_l1)?;
        }
    }
    // cabac_init_idc（CABAC 且非 I/SI 片；spec 7.3.3）
    let mut cabac_init_idc = 0u8;
    if pps.entropy_coding_mode_flag && slice_type != SliceType::I {
        let v = r.ue()?;
        if v > 2 {
            return Err(H264Error::InvalidData(format!(
                "cabac_init_idc {v} out of range"
            )));
        }
        cabac_init_idc = v as u8;
    }

    // dec_ref_pic_marking（I/P 参考帧；nal_ref_idc==0 不标记）。
    // MMCO 按 FFmpeg `ff_h264_decode_ref_pic_marking` 逐字
    // （idc：0=END 1=短转未用 2=长转未用 3=短转长 4=设最大长索引
    //  5=复位 6=当前帧入长窗）
    let mut mmco: Vec<(u8, u64, u64)> = Vec::new();
    let mut long_term_reference_flag = false;
    if nal_ref_idc != 0 {
        if is_idr {
            let _no_output = r.u1()?;
            long_term_reference_flag = r.u1()?;
        } else {
            let explicit = r.u1()?;
            if explicit {
                loop {
                    let opcode = r.ue()?;
                    if opcode == 0 {
                        break;
                    }
                    if opcode > 6 {
                        return Err(H264Error::InvalidData(format!(
                            "memory_management_control_operation {opcode} invalid"
                        )));
                    }
                    let mut diff = 0u64;
                    let mut long_arg = 0u64;
                    if opcode == 1 || opcode == 3 {
                        diff = r.ue()?;
                    }
                    if (2..=4).contains(&opcode) || opcode == 6 {
                        long_arg = r.ue()?;
                        if long_arg >= 32 {
                            return Err(H264Error::InvalidData(
                                "illegal long ref index in mmco".into(),
                            ));
                        }
                    }
                    mmco.push((opcode as u8, diff, long_arg));
                }
            }
        }
    }

    // slice_qp_delta
    let d = r.se()?;
    let qp = i64::from(26 + pps.pic_init_qp_minus26) + d;
    if !(0..=51).contains(&qp) {
        return Err(H264Error::InvalidData(format!(
            "slice qp {qp} out of range (init {}, delta {d})",
            26 + pps.pic_init_qp_minus26
        )));
    }

    // 去块参数（FFmpeg h264_slice.c 逐字语义）：offsets 仅当 idc != 1
    // 出现；idc 0 = 全滤、1 = 禁用、2 = 除片边界外滤；控制字段缺席时
    // idc 缺省 0（全滤、无 offsets）
    let mut deblock = DeblockParams::default();
    if pps.deblocking_filter_control_present_flag {
        let idc = r.ue()?;
        if idc > 2 {
            return Err(H264Error::InvalidData(format!(
                "disable_deblocking_filter_idc {idc} out of range"
            )));
        }
        if idc != 1 {
            let a = r.se()?;
            let b = r.se()?;
            if !(-6..=6).contains(&a) || !(-6..=6).contains(&b) {
                return Err(H264Error::InvalidData(format!(
                    "deblocking filter offsets ({a}, {b}) out of range"
                )));
            }
            deblock.alpha_offset = (a * 2) as i32;
            deblock.beta_offset = (b * 2) as i32;
        }
        deblock.enabled = idc != 1;
        deblock.filter_across_slices = idc != 2;
    }

    Ok(SliceHeader {
        first_mb_in_slice: first_mb_in_slice as usize,
        slice_type,
        pps_id: pps_id as u8,
        frame_num: frame_num as u32,
        idr_pic_id,
        slice_qp: qp as u8,
        num_ref_l0,
        ref_mods,
        long_term_reference_flag,
        mmco,
        deblock,
        num_ref_l1,
        ref_mods_l1,
        direct_spatial_mv_pred,
        poc_lsb,
        delta_poc_bottom,
        delta_poc,
        cabac_init_idc,
    })
}

/// 解码片数据（`r` 位于片头之后），直到宏块数到界或数据位耗尽。
///
/// `data_bits`：本 NAL RBSP 的有效位长（[`rbsp_data_bits`]），
/// 与 `r` 同源。返回实际解到的下一宏块序号（= 已覆盖区间右端）。
///
/// # Errors
/// 见 [`crate::macroblock::decode_macroblock`]；越界进入下一片 →
/// `InvalidData`。
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_slice(
    r: &mut BitReader<'_>,
    data_bits: usize,
    rbsp: &[u8],
    pic: &mut Picture,
    pps: &Pps,
    slice_id: u16,
    header: &SliceHeader,
    slice_end_mb: usize,
    pctx: Option<&InterCtx<'_>>,
) -> Result<usize, H264Error> {
    if pps.entropy_coding_mode_flag {
        return decode_slice_cabac(r, rbsp, pic, pps, slice_id, header, slice_end_mb, pctx);
    }
    let mut qp = header.slice_qp;
    let mut mb_idx = header.first_mb_in_slice;
    // 去块滞后调度（FFmpeg loop_filter 调用点同构）：每解码完一整行
    // 宏块即滤波该行 [lf_x_start, 行尾)；下一行的 intra 预测 / MC
    // 因此读到滤波后的上一行（逐位对拍锁定语义）。片尾再把半行余段
    // 滤掉（多片图中余段归本片，FFmpeg lf_x_start 同构）。
    let w = pic.mb_width;
    let mut lf_x_start = header.first_mb_in_slice % w;
    // P 片 mb_skip_run（FFmpeg decode_mb_cavlc 语义：−1 = 待读；
    // 读到的 run 覆盖随后若干 P_Skip 宏块，耗尽后下一个非 skip 宏块
    // 解析前重读）
    let mut skip_run: i64 = -1;
    while mb_idx < slice_end_mb {
        // 终止条件与 FFmpeg 一致：只有「需要再读位」且位已耗尽时结束
        // 片——P_Skip 宏块不读位，不因位耗尽中断（一个尾部 mb_skip_run
        // 可覆盖整段尾部宏块）
        let need_bits = if pctx.is_some() { skip_run == -1 } else { true };
        if need_bits && r.bit_position() >= data_bits {
            break;
        }
        if pic.mbs[mb_idx].decoded {
            return Err(H264Error::InvalidData(
                "slice overlaps already-decoded macroblock".into(),
            ));
        }
        let mbx = mb_idx % pic.mb_width;
        let mby = mb_idx / pic.mb_width;
        if let Some(p) = pctx {
            if skip_run == -1 {
                let v = r.ue()?;
                if v > (pic.mb_width * pic.mb_height) as u64 {
                    return Err(H264Error::InvalidData(format!("mb_skip_run {v} invalid")));
                }
                skip_run = v as i64;
            }
            if skip_run > 0 {
                skip_run -= 1;
                decode_skip_mb(pic, mbx, mby, p, slice_id, qp)
                    .map_err(|e| H264Error::InvalidData(format!("skip mb ({mbx},{mby}): {e}")))?;
                mb_idx += 1;
                if mb_idx.is_multiple_of(w) {
                    if header.deblock.enabled {
                        deblock_row(pic, (mb_idx - 1) / w, lf_x_start, w, &header.deblock);
                    }
                    lf_x_start = 0;
                }
                continue;
            }
            skip_run -= 1; // 0 → −1：本 MB 非 skip，之后重读 run
        }
        decode_macroblock(r, pic, mbx, mby, pps, slice_id, &mut qp, pctx)
            .map_err(|e| H264Error::InvalidData(format!("mb ({mbx},{mby}): {e}")))?;
        mb_idx += 1;
        if mb_idx.is_multiple_of(w) {
            if header.deblock.enabled {
                deblock_row(pic, (mb_idx - 1) / w, lf_x_start, w, &header.deblock);
            }
            lf_x_start = 0;
        }
    }
    if mb_idx > slice_end_mb {
        return Err(H264Error::InvalidData(
            "slice overlaps with next slice boundary".into(),
        ));
    }
    // 片尾余段（未满一行）：滤 [lf_x_start, mb_idx)（行对齐时 no-op）
    if header.deblock.enabled && mb_idx % w > lf_x_start {
        deblock_row(pic, mb_idx / w, lf_x_start, mb_idx % w, &header.deblock);
    }
    Ok(mb_idx)
}

// ---------------------------------------------------------------------------
// CABAC 片数据循环（spec 7.3.4 CABAC 分支；FFmpeg decode_slice 语义）
// ---------------------------------------------------------------------------

/// CABAC 片循环：逐 MB 解码，`end_of_slice_flag`（terminate bin）收尾；
/// 去块滞后调度与 CAVLC 路径同构（整行滤波）。
#[allow(clippy::too_many_arguments)]
fn decode_slice_cabac(
    r: &mut BitReader<'_>,
    rbsp: &[u8],
    pic: &mut Picture,
    pps: &Pps,
    slice_id: u16,
    header: &SliceHeader,
    slice_end_mb: usize,
    pctx: Option<&InterCtx<'_>>,
) -> Result<usize, H264Error> {
    use crate::cabac::{Cabac, CabacCtx};
    use crate::cabac_mb::decode_mb_cabac;
    let byte_pos = r.bit_position().div_ceil(8);
    let ctx_model = CabacCtx::init(
        header.slice_qp,
        usize::from(header.cabac_init_idc),
        header.slice_type == SliceType::I,
    );
    let mut reader = crate::cabac::CabacReader::new(Cabac::new(rbsp, byte_pos), ctx_model);
    let is_b = header.slice_type == SliceType::B;
    let mut qp = header.slice_qp;
    let mut last_qscale_diff = 0i32;
    let mut mb_idx = header.first_mb_in_slice;
    let w = pic.mb_width;
    let mut lf_x_start = header.first_mb_in_slice % w;
    loop {
        if mb_idx >= slice_end_mb {
            return Err(H264Error::InvalidData(
                "slice data exceeds picture boundary (end_of_slice_flag missing)".into(),
            ));
        }
        if pic.mbs[mb_idx].decoded {
            return Err(H264Error::InvalidData(
                "slice overlaps already-decoded macroblock".into(),
            ));
        }
        let mbx = mb_idx % pic.mb_width;
        let mby = mb_idx / pic.mb_width;
        decode_mb_cabac(
            &mut reader,
            pic,
            mbx,
            mby,
            pps,
            slice_id,
            &mut qp,
            &mut last_qscale_diff,
            pctx,
            is_b,
        )
        .map_err(|e| H264Error::InvalidData(format!("cabac mb ({mbx},{mby}): {e}")))?;
        mb_idx += 1;
        if mb_idx.is_multiple_of(w) {
            if header.deblock.enabled {
                deblock_row(pic, (mb_idx - 1) / w, lf_x_start, w, &header.deblock);
            }
            lf_x_start = 0;
        }
        if reader.cabac.terminate() {
            break; // end_of_slice_flag = 1
        }
        if std::env::var_os("YE_DBG_FORCE_END").is_some() && mb_idx + 1 >= slice_end_mb {
            eprintln!("DBG force end at mb {mb_idx} (terminate=false)");
            break;
        }
    }
    if mb_idx > slice_end_mb {
        return Err(H264Error::InvalidData(
            "slice overlaps with next slice boundary".into(),
        ));
    }
    if header.deblock.enabled && mb_idx % w > lf_x_start {
        deblock_row(pic, mb_idx / w, lf_x_start, mb_idx % w, &header.deblock);
    }
    Ok(mb_idx)
}

// ---------------------------------------------------------------------------
// ②c：去块主链接线（边组织循环；滤波核与阈值全部来自 deblock.rs）。
// 边序与 QP 取值按 FFmpeg h264_loopfilter.c `h264_filter_mb_fast_internal`
// 逐字（progressive、4x4 DCT、4:2:0）：逐 MB luma 垂直边 0..3 →
// luma 水平边 0..3 → chroma 垂直边 0,2 → chroma 水平边 0,2；MB 边
// 用两侧 QP 平均、内部边用本 MB QP；bS 按边两侧 4x4 块对逐段判定
// （deblock::boundary_strength_gated）。
// ---------------------------------------------------------------------------

/// 滤一行宏块 [x0, x1) 的全部去块边（含它们与上一行/左邻 MB 的边界）。
fn deblock_row(pic: &mut Picture, y: usize, x0: usize, x1: usize, dp: &DeblockParams) {
    for x in x0..x1 {
        deblock_mb(pic, x, y, dp);
    }
}

/// 滤单个宏块（16x16 luma + 8x8 Cb/Cr）。
fn deblock_mb(pic: &mut Picture, mbx: usize, mby: usize, dp: &DeblockParams) {
    let w = pic.mb_width;
    let mb_xy = mby * w + mbx;
    let cur_qp = pic.mbs[mb_xy].qp;
    // 邻块存在性（含跨片：去块可用任何已解码邻块——FFmpeg 去块与
    // constrained_intra_pred 无关）
    let left_ok = mbx > 0 && pic.mbs[mb_xy - 1].decoded;
    let top_ok = mby > 0 && pic.mbs[mb_xy - w].decoded;
    if !pic.mbs[mb_xy].decoded {
        return;
    }

    let ls = pic.luma_stride();
    let cs = pic.chroma_stride();
    let y0 = mby * 16;
    let x0 = mbx * 16;

    // ---- luma 垂直边 e = 0..4（列 x0 + 4e，p 块列 e−1 | q 块列 e） ----
    // 8x8 变换的块内部边（e ∈ {1,3}，跨 8x8 象限内的 4x4 边）bS = 0
    //（spec 8.8.2.4：transform_size_8x8_flag=1 的两侧块间边不滤；
    // e=1 在象限 (0,*)/(1,*) 内部、e=3 在 (2,*)/(3,*) 内部——仅当
    // 本 MB 8x8 时跳过；跨 MB 边（e=0）不受影响）
    for e in 0..4usize {
        if e % 2 == 1 && pic.mbs[mb_xy].transform_8x8 {
            continue;
        }
        let mut bs = [0u8; 4];
        let mut any = false;
        let (p_src, q_src): (Option<usize>, Option<usize>) = if e == 0 {
            (if left_ok { Some(mb_xy - 1) } else { None }, Some(mb_xy))
        } else {
            (Some(mb_xy), Some(mb_xy))
        };
        for (i, b) in bs.iter_mut().enumerate() {
            // 垂直边段 i：p = 块 (e−1, i)，q = 块 (e, i)
            let p_blk = if e == 0 { 3 + 4 * i } else { (e - 1) + 4 * i };
            let q_blk = e + 4 * i;
            *b = bs_of(pic, p_src, p_blk, q_src, q_blk, e == 0, dp);
            any |= *b != 0;
        }
        if !any {
            continue;
        }
        let qp = if e == 0 {
            let l = pic.mbs[mb_xy - 1].qp;
            deblock::edge_qp_luma(l, cur_qp)
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

    // ---- luma 水平边 e = 0..4（行 y0 + 4e） ----
    for e in 0..4usize {
        if e % 2 == 1 && pic.mbs[mb_xy].transform_8x8 {
            continue;
        }
        let mut bs = [0u8; 4];
        let mut any = false;
        let (p_src, q_src): (Option<usize>, Option<usize>) = if e == 0 {
            (if top_ok { Some(mb_xy - w) } else { None }, Some(mb_xy))
        } else {
            (Some(mb_xy), Some(mb_xy))
        };
        for (i, b) in bs.iter_mut().enumerate() {
            // 水平边段 i：p = 块 (i, e−1)，q = 块 (i, e)
            let p_blk = if e == 0 { 12 + i } else { i + 4 * (e - 1) };
            let q_blk = i + 4 * e;
            *b = bs_of(pic, p_src, p_blk, q_src, q_blk, e == 0, dp);
            any |= *b != 0;
        }
        if !any {
            continue;
        }
        let qp = if e == 0 {
            let t = pic.mbs[mb_xy - w].qp;
            deblock::edge_qp_luma(t, cur_qp)
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

    // ---- chroma 垂直边 e ∈ {0, 2}（列 cx0 + 2e，8 样本） ----
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
            *b = bs_of(pic, p_src, p_blk, q_src, q_blk, e == 0, dp);
            any |= *b != 0;
        }
        if !any {
            continue;
        }
        let qp = if e == 0 {
            let l = pic.mbs[mb_xy - 1].qp;
            deblock::edge_qp_chroma(l, cur_qp)
        } else {
            i32::from(deblock::chroma_qp_of(cur_qp))
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

    // ---- chroma 水平边 e ∈ {0, 2} ----
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
            *b = bs_of(pic, p_src, p_blk, q_src, q_blk, e == 0, dp);
            any |= *b != 0;
        }
        if !any {
            continue;
        }
        let qp = if e == 0 {
            let t = pic.mbs[mb_xy - w].qp;
            deblock::edge_qp_chroma(t, cur_qp)
        } else {
            i32::from(deblock::chroma_qp_of(cur_qp))
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

/// 段级 bS：两侧块均为本帧已解码块（跨片时经 `filter_across_slices`
/// 门控；片内边恒可判）。
fn bs_of(
    pic: &Picture,
    p_src: Option<usize>,
    p_blk: usize,
    q_src: Option<usize>,
    q_blk: usize,
    mb_boundary: bool,
    dp: &DeblockParams,
) -> u8 {
    // 边两侧块不可得（画面边界）→ 该边不存在（段 bS=0）
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
// 单测
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rbsp_data_bits_strips_trailing() {
        // 数据位 + 停止位 1 + 填充 0
        assert_eq!(rbsp_data_bits(&[0b1000_0000]), 0); // 只有停止位
        assert_eq!(rbsp_data_bits(&[0xFF, 0b0100_0000]), 9); // 8 + 1
        assert_eq!(rbsp_data_bits(&[0xFF, 0b0000_0001]), 15); // 8 + 7（停止位是末位 1）
        assert_eq!(rbsp_data_bits(&[0xFF, 0x00, 0x00]), 7); // 尾零字节剥除后再剥停止位
        assert_eq!(rbsp_data_bits(&[]), 0);
        assert_eq!(rbsp_data_bits(&[0x00]), 0); // 无停止位
    }

    #[test]
    fn slice_type_codes() {
        assert_eq!(SliceType::from_code(2).unwrap(), SliceType::I);
        assert_eq!(SliceType::from_code(7).unwrap(), SliceType::I);
        assert_eq!(SliceType::from_code(0).unwrap(), SliceType::P);
        assert_eq!(SliceType::from_code(5).unwrap(), SliceType::P);
        assert_eq!(SliceType::from_code(1).unwrap(), SliceType::B);
        assert_eq!(SliceType::from_code(9).unwrap(), SliceType::Si);
        assert!(SliceType::from_code(10).is_err());
    }
}
