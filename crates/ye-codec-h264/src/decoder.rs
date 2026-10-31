//! `ye_codec::Decoder` 实现：H.264 → I/P 帧（②a+②b 闭环）。
//!
//! 数据面：Annex B 或 AVCC（avcC extradata）输入 → NAL 分发
//! （SPS/PPS 登记、VCL 片解码、AUD/SEI 忽略）→ 图片补全检查 →
//! 裁剪后的 yuv420p [`VideoFrame`]。
//!
//! 状态机（trait 契约）：feed 推包；flush 排空后 feed → `InvalidState`；
//! `pull` 按 [`Decoded`] 三态返回。帧输出按解码序（Baseline 无 B，
//! 无重排），`pts` 取该图首个 VCL 包的时戳。
//!
//! ②b 参考帧管理（I/P 两窗）：短窗（最新在前，携带 frame_num）+
//! 长窗（LongTermFrameIdx 0..16）；滑动窗口与 MMCO 全操作、List0
//! 默认序（短窗最新在前 + 长窗按索引）与 ref_pic_list_modification
//! 重排（FFmpeg h264_refs.c 逐字语义）。
//!
//! 形态收紧（如实报错，绝不静默错帧）：场图/MBAFF、4:2:2/4:4:4、
//! 高位深、Baseline/Main/Extended/High 系之外 profile、CAVLC 的
//! B 片、SP/SI 片、加权预测、数据分区、frame_num 断号
//! （错误处理属 ②c）→ `Unsupported`；
//! 去块滤波全支持（idc 0/1/2：图级统一滤波——帧内预测读滤波前
//! 样本，图完成后按光栅序滤波，与 FFmpeg 行滞后调度逐位等价）；
//! 位流畸形 → `InvalidData`。

use std::collections::VecDeque;
use std::sync::Arc;

use ye_codec::{CodecError, CodecId, CodecParameters, Decoded, Decoder, Frame, Packet, VideoFrame};
use ye_pixfmt::PixelFormat;
use ye_time::Timestamp;

use crate::bitreader::BitReader;
use crate::error::H264Error;
use crate::inter::InterCtx;
use crate::macroblock::Picture;
use crate::nal::{self, NalHeader};
use crate::slice::{self, SliceHeader, SliceType};
use crate::sps::{Pps, Sps};

/// H.264 解码器（CAVLC/CABAC、8bit 4:2:0、I/P/B 片、图级去块滤波）。
pub struct H264Decoder {
    sps_list: Vec<Option<Sps>>,
    pps_list: Vec<Option<Pps>>,
    /// AVCC 长度前缀字节数（Annex B 输入时为 `None`）。
    avcc_length_size: Option<usize>,
    /// 解码中的图片。
    cur: Option<PictureCtx>,
    /// 短期参考窗（front = 最新；携带 frame_num，FFmpeg short_ref 布局）。
    short_ref: Vec<(Arc<Picture>, u32)>,
    /// 长期参考窗（LongTermFrameIdx = 下标）。
    long_ref: [Option<Arc<Picture>>; 16],
    /// SPS 声明的最大参考帧数。
    max_refs: usize,
    /// 上一图 frame_num（断号检测；IDR 重置）。
    prev_frame_num: u32,
    /// log2(MaxFrameNum)。
    log2_max_frame_num: u8,
    /// 已完成待输出的帧。
    out: VecDeque<Frame>,
    /// 解码图片缓冲（输出重排；B 帧 POC 序释放）。
    dpb: Vec<DpbEntry>,
    /// POC 状态机（spec 8.3.1；JM decode_poc 同构）。
    poc: PocState,
    /// flush 后排空中暂存的失败（flush 无返回值，经 pull 透出）。
    pending_error: Option<CodecError>,
    flushed: bool,
}

/// DPB 条目：已完成、待按 POC 序输出的图片。
struct DpbEntry {
    pic: Arc<Picture>,
    pts: Option<Timestamp>,
    poc: i32,
    outputted: bool,
}

/// POC 状态（frame 图；JM VideoParameters 对应字段的子集）。
struct PocState {
    prev_lsb: i32,
    prev_msb: i32,
    prev_frame_num: u32,
    prev_frame_num_offset: i32,
    last_has_mmco_5: bool,
}

/// 一张图的解码上下文。
struct PictureCtx {
    pic: Arc<Picture>,
    sps_id: u8,
    frame_num: u32,
    /// PPS、IDR id 与 POC 语法字段；非参考 B 帧可复用 frame_num。
    identity: (u8, u32, u32, i32, [i32; 2]),
    idr: bool,
    /// nal_ref_idc != 0（本图是否入参考窗）。
    is_ref: bool,
    /// IDR 的 long_term_reference_flag。
    long_term_reference_flag: bool,
    /// 非 IDR 自适应 MMCO 操作。
    mmco: Vec<(u8, u64, u64)>,
    decoded_mb_count: usize,
    next_slice_id: u16,
    pts: Option<Timestamp>,
    /// 本图 POC。
    poc: i32,
    /// 本图解码时的参考列表快照（map_col 匹配用）。
    used_l0: Vec<Arc<Picture>>,
    used_l1: Vec<Arc<Picture>>,
}

impl PictureCtx {
    fn mb_total(&self) -> usize {
        self.pic.mb_width * self.pic.mb_height
    }
}

impl H264Decoder {
    /// 由带外参数构造（extradata：avcC 记录或 Annex B SPS/PPS 块）。
    ///
    /// # Errors
    /// extradata 无法识别 / SPS/PPS 解析失败 → [`CodecError::InvalidData`]。
    pub fn new(params: &CodecParameters) -> Result<Self, CodecError> {
        let mut dec = H264Decoder {
            sps_list: (0..32).map(|_| None).collect(),
            pps_list: (0..256).map(|_| None).collect(),
            avcc_length_size: None,
            cur: None,
            short_ref: Vec::new(),
            long_ref: [const { None }; 16],
            max_refs: 16,
            prev_frame_num: 0,
            log2_max_frame_num: 4,
            out: VecDeque::new(),
            dpb: Vec::new(),
            poc: PocState {
                prev_lsb: 0,
                prev_msb: 0,
                prev_frame_num: 0,
                prev_frame_num_offset: 0,
                last_has_mmco_5: false,
            },
            pending_error: None,
            flushed: false,
        };
        if let Some(ed) = params.extradata.as_deref() {
            if ed.is_empty() {
                return Err(CodecError::InvalidData(
                    "h264 extradata present but empty".into(),
                ));
            }
            if ed.len() >= 7 && ed[0] == 1 {
                // avcC 记录：SPS/PPS 带外登记，包体走长度前缀
                let rec = nal::AvcConfigRecord::parse(ed).map_err(codec_err)?;
                dec.avcc_length_size = Some(rec.length_size);
                for sps_nal in &rec.sps_nals {
                    dec.process_nal(sps_nal, None)?;
                }
                for pps_nal in &rec.pps_nals {
                    dec.process_nal(pps_nal, None)?;
                }
            } else if ed.len() >= 3 && ed[0] == 0 && ed[1] == 0 {
                // Annex B 块（常见于裸流容器）：按 SPS/PPS 登记处理
                for nal in nal::split_annexb(ed).map_err(codec_err)? {
                    dec.process_nal(nal, None)?;
                }
            } else {
                return Err(CodecError::InvalidData(format!(
                    "h264 extradata not recognized (first byte 0x{:02x}, {} bytes)",
                    ed[0],
                    ed.len()
                )));
            }
        }
        Ok(dec)
    }

    fn process_nal(&mut self, nal: &[u8], pts: Option<Timestamp>) -> Result<(), CodecError> {
        let (header, payload) = nal::nal_header_and_payload(nal).map_err(codec_err)?;
        match header.nal_unit_type {
            7 => {
                let sps = Sps::parse(nal).map_err(codec_err)?;
                let sid = sps.seq_parameter_set_id;
                self.sps_list[usize::from(sid)] = Some(sps);
                Ok(())
            }
            8 => {
                let sps_for_pps = self.sps_list.iter().find_map(|s| s.as_ref());
                let pps = Pps::parse_with_sps(nal, sps_for_pps).map_err(codec_err)?;
                let pid = pps.pic_parameter_set_id;
                self.pps_list[usize::from(pid)] = Some(pps);
                Ok(())
            }
            1 | 5 => self.process_slice(header, payload, pts),
            9 => {
                // AUD：访问单元边界——完成在途图片
                self.finish_current_picture()
            }
            6 | 12 => Ok(()), // SEI / filler：跳过
            10 | 11 => {
                // 序列/流结束
                self.finish_current_picture()
            }
            2..=4 => Err(CodecError::UnsupportedStub(
                "data partitioned slices unsupported",
            )),
            14 | 20 => Err(CodecError::UnsupportedStub(
                "MVC prefix/suffix nal units unsupported",
            )),
            _ => Err(CodecError::UnsupportedStub(
                "unsupported nal unit type in I-only decoding stage",
            )),
        }
    }

    /// 处理一个片 NAL（类型 1/5）。
    fn process_slice(
        &mut self,
        header: NalHeader,
        payload: &[u8],
        pts: Option<Timestamp>,
    ) -> Result<(), CodecError> {
        let is_idr = header.nal_unit_type == 5;
        if is_idr && header.nal_ref_idc == 0 {
            return Err(CodecError::InvalidData(
                "IDR slice with nal_ref_idc == 0".into(),
            ));
        }
        let rbsp = nal::unescape_rbsp(payload);
        let data_bits = slice::rbsp_data_bits(&rbsp);
        if data_bits == 0 {
            return Err(CodecError::InvalidData("empty slice rbsp".into()));
        }

        // 两段式：先读 pps_id 定位参数集，再做完整片头解析
        let (_, slice_type_code, pps_id) = {
            let mut r = BitReader::new(&rbsp);
            let first = r.ue().map_err(codec_err)?;
            let st = r.ue().map_err(codec_err)?;
            let pid = r.ue().map_err(codec_err)?;
            (first, st, pid)
        };
        let _ = SliceType::from_code(slice_type_code).map_err(codec_err)?;
        if pps_id > 255 {
            return Err(CodecError::InvalidData(format!(
                "pic_parameter_set_id {pps_id} out of range"
            )));
        }
        let pps = self.pps_list[pps_id as usize]
            .as_ref()
            .ok_or_else(|| {
                CodecError::InvalidData(format!("pps {pps_id} referenced before activation"))
            })?
            .clone();
        let sps = self.sps_list[usize::from(pps.seq_parameter_set_id)]
            .as_ref()
            .ok_or_else(|| {
                CodecError::InvalidData(format!(
                    "sps {} referenced before activation",
                    pps.seq_parameter_set_id
                ))
            })?
            .clone();

        // 参数集级门槛（M2：CABAC 与 High 档已落地，仅挡真正未实现面）
        if !matches!(
            sps.profile_idc,
            66 | 77 | 88 | 100 | 110 | 118 | 122 | 244 | 44 | 83 | 86 | 128 | 138 | 139 | 134 | 135
        ) {
            return Err(CodecError::UnsupportedStub(
                "profile not supported (accepted: Baseline/Main/Extended/High family)",
            ));
        }
        if sps.chroma_format_idc != 1 || sps.separate_colour_plane_flag {
            return Err(CodecError::UnsupportedStub(
                "only 4:2:0 chroma supported in this stage",
            ));
        }
        if sps.bit_depth_luma_minus8 != 0 || sps.bit_depth_chroma_minus8 != 0 {
            return Err(CodecError::UnsupportedStub("only 8-bit samples supported"));
        }

        // 完整片头（含形态收紧：I 片、场图、去块 idc==2、qp 界内……）
        let sh: SliceHeader = {
            let mut r = BitReader::new(&rbsp);
            slice::parse_slice_header(&mut r, &sps, &pps, is_idr, header.nal_ref_idc)
                .map_err(codec_err)?
        };

        // B 片 CAVLC：如实收紧（x264 的 B 流恒 CABAC；CAVLC-B 后续按需）
        if sh.slice_type == SliceType::B && !pps.entropy_coding_mode_flag {
            return Err(CodecError::UnsupportedStub(
                "B slices with CAVLC entropy coding land in a later stage",
            ));
        }

        // ---- 访问单元边界（I/P 通用）----
        // 非参考 B 帧可复用帧号，必须同时比较 POC 与参考标记。断号属错误处理，
        // 当前如实收紧为 Unsupported。
        let max_fn = 1u32 << u32::from(sps.log2_max_frame_num_minus4 + 4);
        if self.cur.is_some() && sh.first_mb_in_slice == 0 {
            let cur = self.cur.as_ref().expect("checked non-empty");
            let identity = (
                sh.pps_id,
                sh.idr_pic_id,
                sh.poc_lsb,
                sh.delta_poc_bottom,
                sh.delta_poc,
            );
            let new_pic = is_idr
                || cur.frame_num != sh.frame_num
                || cur.idr != is_idr
                || cur.identity != identity
                || cur.is_ref != (header.nal_ref_idc != 0);
            if !new_pic {
                return Err(CodecError::InvalidData(format!(
                    "duplicate picture start (frame_num {}, idr {is_idr})",
                    sh.frame_num
                )));
            }
            if !is_idr && sh.frame_num != (cur.frame_num + u32::from(cur.is_ref)) % max_fn {
                return Err(CodecError::UnsupportedStub(
                    "frame_num gap (error concealment lands in a later stage)",
                ));
            }
            self.finish_current_picture()?;
        }
        if self.cur.is_none() {
            if is_idr {
                // IDR：参考窗全清（spec 8.2.5）
                self.short_ref.clear();
                self.long_ref.fill(None);
            } else if self.short_ref.is_empty() && self.long_ref.iter().all(|l| l.is_none()) {
                return Err(CodecError::UnsupportedStub(
                    "stream does not start with an IDR picture",
                ));
            }
            self.max_refs = usize::from(sps.max_num_ref_frames).max(1);
            self.log2_max_frame_num = sps.log2_max_frame_num_minus4 + 4;
            let mb_w = usize::from(sps.pic_width_in_mbs);
            let mb_h = usize::from(sps.pic_height_in_map_units);
            let mut pic = Arc::new(Picture::new(
                mb_w,
                mb_h,
                sps.width() as usize,
                sps.height() as usize,
            ));
            let poc = self.compute_poc(&sps, &sh, is_idr, header.nal_ref_idc != 0);
            Arc::get_mut(&mut pic).expect("new picture").color = sps.color;
            Arc::get_mut(&mut pic)
                .expect("sole owner at picture start")
                .poc = poc;
            self.cur = Some(PictureCtx {
                pic,
                sps_id: sps.seq_parameter_set_id,
                frame_num: sh.frame_num,
                identity: (
                    sh.pps_id,
                    sh.idr_pic_id,
                    sh.poc_lsb,
                    sh.delta_poc_bottom,
                    sh.delta_poc,
                ),
                idr: is_idr,
                is_ref: header.nal_ref_idc != 0,
                long_term_reference_flag: sh.long_term_reference_flag,
                mmco: sh.mmco.clone(),
                decoded_mb_count: 0,
                next_slice_id: 0,
                pts,
                poc,
                used_l0: Vec::new(),
                used_l1: Vec::new(),
            });
        }
        let ctx = self.cur.as_mut().expect("picture ctx just ensured");
        // 帧号必须与在途图一致（多片同图）
        if ctx.frame_num != sh.frame_num || ctx.idr != is_idr {
            return Err(CodecError::InvalidData(format!(
                "slice frame_num {} does not match in-flight picture {}",
                sh.frame_num, ctx.frame_num
            )));
        }
        // 解码中的图片与激活 SPS 绑定（尺寸/裁剪不随片变化）
        let sps_active = self.sps_list[usize::from(ctx.sps_id)]
            .clone()
            .expect("sps active since picture start");

        // ---- 参考列表（P：List0；B：双列表 POC 序 + 重排）----
        let cur_poc = self.cur.as_ref().map(|c| c.poc).unwrap_or(0);
        let (list0, list1): (Vec<Arc<Picture>>, Vec<Arc<Picture>>) = match sh.slice_type {
            SliceType::P => (self.build_list0(&sh)?, Vec::new()),
            SliceType::B => {
                let (mut l0, mut l1) = crate::inter::build_b_lists(
                    &self.short_ref_with_poc(),
                    &self.long_ref,
                    cur_poc,
                    sh.num_ref_l0.max(sh.num_ref_l1).max(1),
                );
                // 长窗 POC 由 Picture 自带（build_b_lists 以 pic.poc 读取）
                // 长窗补位（build_b_lists 内已含；重排在后）
                l0 = self.apply_reorder(l0, &sh.ref_mods, sh.num_ref_l0.max(1))?;
                l1 = self.apply_reorder(l1, &sh.ref_mods_l1, sh.num_ref_l1.max(1))?;
                (l0, l1)
            }
            _ => (Vec::new(), Vec::new()),
        };
        if let Some(c) = self.cur.as_mut()
            && (!list0.is_empty() || !list1.is_empty())
        {
            c.used_l0 = list0.clone();
            c.used_l1 = list1.clone();
        }
        let ctx = self.cur.as_mut().expect("picture ctx just ensured");
        let slice_id = ctx.next_slice_id;
        ctx.next_slice_id = ctx.next_slice_id.saturating_add(1);
        let end = {
            let mut r = BitReader::new(&rbsp);
            // 跳过片头（重放到片数据）
            let _ =
                slice::parse_slice_header(&mut r, &sps_active, &pps, is_idr, header.nal_ref_idc)
                    .map_err(codec_err)?;
            let mb_total = ctx.pic.mb_width * ctx.pic.mb_height;
            // 解码期独占持有：参考窗只收已完成的图，get_mut 必得
            let pic_mut = Arc::get_mut(&mut ctx.pic).expect("sole owner during decode");
            let pctx = match sh.slice_type {
                SliceType::I => None,
                SliceType::P => Some(InterCtx {
                    l0: &list0,
                    wp: sh.wp.clone(),
                    implicit_bipred: pps.weighted_bipred_idc == 2,
                    l1: &[],
                    constrained: pps.constrained_intra_pred_flag,
                    direct_spatial: true,
                    cur_poc: 0,
                    colocated: None,
                    col_is_long: false,
                    ref_poc: [[0; 32]; 2],
                    dist_scale: [256; 32],
                    map_col: [[0; 32]; 2],
                    direct_8x8_inference: sps_active.direct_8x8_inference_flag,
                }),
                SliceType::B => {
                    let col = list1
                        .first()
                        .cloned()
                        .ok_or_else(|| CodecError::InvalidData("B slice without list1".into()))?;
                    let mut dist_scale = [256i32; 32];
                    for (i, r0) in list0.iter().enumerate().take(32) {
                        dist_scale[i] =
                            crate::inter::scale_factor(cur_poc, col.poc, r0.poc, r0.is_long_term);
                    }
                    let mut map_col = [[0usize; 32]; 2];
                    for (list, mc) in map_col.iter_mut().enumerate() {
                        let used = if list == 0 {
                            &col.used_l0
                        } else {
                            &col.used_l1
                        };
                        for (idx, entry) in used.iter().enumerate().take(32) {
                            if let Some(p) = entry {
                                mc[idx] = list0.iter().position(|q| Arc::ptr_eq(q, p)).unwrap_or(0);
                            }
                        }
                    }
                    let ref_poc = [list0_poc(&list0), list0_poc(&list1)];
                    Some(InterCtx {
                        l0: &list0,
                        l1: &list1,
                        wp: sh.wp.clone(),
                        implicit_bipred: pps.weighted_bipred_idc == 2,
                        constrained: pps.constrained_intra_pred_flag,
                        direct_spatial: sh.direct_spatial_mv_pred,
                        cur_poc,
                        colocated: Some(col.clone()),
                        col_is_long: col.is_long_term,
                        ref_poc,
                        dist_scale,
                        map_col,
                        direct_8x8_inference: sps_active.direct_8x8_inference_flag,
                    })
                }
                _ => None,
            };
            slice::decode_slice(
                &mut r,
                data_bits,
                &rbsp,
                pic_mut,
                &pps,
                slice_id,
                &sh,
                mb_total,
                pctx.as_ref(),
            )
            .map_err(codec_err)?
        };
        if let Some(pic) = Arc::get_mut(&mut ctx.pic) {
            for mb in &mut pic.mbs[sh.first_mb_in_slice..end] {
                for b in 0..16 {
                    mb.ref_poc[0][b] = usize::try_from(mb.ref_idx[b])
                        .ok()
                        .and_then(|i| list0.get(i))
                        .map(|p| p.poc);
                    mb.ref_poc[1][b] = usize::try_from(mb.ref_idx_l1[b])
                        .ok()
                        .and_then(|i| list1.get(i))
                        .map(|p| p.poc);
                }
            }
        }
        ctx.decoded_mb_count += end - sh.first_mb_in_slice;
        Ok(())
    }

    /// 完成在途图片：补全检查 → 参考标记 → 入 DPB → 按 POC 序释放。
    fn finish_current_picture(&mut self) -> Result<(), CodecError> {
        let Some(mut ctx) = self.cur.take() else {
            return Ok(());
        };
        let total = ctx.mb_total();
        if ctx.decoded_mb_count != total {
            return Err(CodecError::InvalidData(format!(
                "incomplete picture: {} of {total} macroblocks decoded",
                ctx.decoded_mb_count
            )));
        }
        // 长期参考标记预测（与 mark_reference 的落窗决策同源；
        // 此刻仍独占，可写字段）
        let to_long = (ctx.idr && ctx.long_term_reference_flag)
            || ctx.mmco.iter().any(|&(op, _, _)| op == 3 || op == 6);
        if let Some(pic_mut) = Arc::get_mut(&mut ctx.pic) {
            // 图级去块（2026-10-27 对拍修复）：解码期不滤波（帧内预测读
            // 滤波前样本），图完成后、入 DPB/参考窗前统一按光栅序滤波
            // ——参考帧与输出帧均取滤波后结果（与 FFmpeg 行滞后调度
            // 逐位等价）。
            slice::deblock_picture(pic_mut);
            pic_mut.is_long_term = to_long;
            pic_mut.used_l0 = ctx.used_l0.iter().map(|p| Some(p.clone())).collect();
            pic_mut.used_l1 = ctx.used_l1.iter().map(|p| Some(p.clone())).collect();
        }
        self.mark_reference(&ctx);
        // mmco5：POC 状态复位标记
        self.poc.last_has_mmco_5 = ctx.mmco.iter().any(|&(op, _, _)| op == 5);
        // IDR：POC 归零——先按序清空在途（GOP 尾帧全部先于 IDR 输出），
        // 再入 DPB；非 IDR 走地平线规则
        if ctx.idr {
            self.flush_dpb();
        }
        self.dpb.push(DpbEntry {
            pic: ctx.pic.clone(),
            pts: ctx.pts,
            poc: ctx.poc,
            outputted: false,
        });
        if !ctx.idr {
            self.release_dpb();
        }
        Ok(())
    }

    /// DPB 释放（输出重排）：释放 POC 早于「参考窗中最小 POC」的
    /// 全部待输出图（升序）。参考窗空 → 全放。
    fn release_dpb(&mut self) {
        let horizon = {
            let mut h = i32::MAX;
            for (p, _) in &self.short_ref {
                h = h.min(p.poc);
            }
            for p in self.long_ref.iter().flatten() {
                h = h.min(p.poc);
            }
            h
        };
        let mut pending: Vec<usize> = self
            .dpb
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.outputted)
            .map(|(i, _)| i)
            .collect();
        pending.sort_by_key(|&i| self.dpb[i].poc);
        for i in pending {
            if self.dpb[i].poc < horizon {
                self.dpb[i].outputted = true;
                let e = &self.dpb[i];
                if let Ok(frame) = build_frame(&e.pic, e.pts) {
                    self.out.push_back(Frame::Video(frame));
                }
            }
        }
        // 安全上限（合规流不触发；超出即按 POC 序强放）
        if self.dpb.iter().filter(|e| !e.outputted).count() > 16 {
            self.flush_dpb();
        }
        self.dpb.retain(|e| !e.outputted);
    }

    /// flush：全部待输出图按 POC 序释放（容错：build 失败跳过该帧）。
    fn flush_dpb(&mut self) {
        let mut pending: Vec<usize> = self
            .dpb
            .iter()
            .enumerate()
            .filter(|(_, e)| !e.outputted)
            .map(|(i, _)| i)
            .collect();
        pending.sort_by_key(|&i| self.dpb[i].poc);
        for i in pending {
            let e = &self.dpb[i];
            if let Ok(frame) = build_frame(&e.pic, e.pts) {
                self.out.push_back(Frame::Video(frame));
            }
        }
        self.dpb.clear();
    }

    /// POC 计算（spec 8.3.1；JM decode_poc 帧图分支逐字）。
    fn compute_poc(&mut self, sps: &Sps, sh: &SliceHeader, is_idr: bool, is_ref: bool) -> i32 {
        match sps.pic_order_cnt_type {
            0 => {
                let max_lsb = 1i32 << (i32::from(sps.log2_max_pic_order_cnt_lsb_minus4) + 4);
                let lsb = sh.poc_lsb as i32;
                if is_idr || self.poc.last_has_mmco_5 {
                    self.poc.prev_lsb = 0;
                    self.poc.prev_msb = 0;
                }
                let msb = if lsb < self.poc.prev_lsb && self.poc.prev_lsb - lsb >= max_lsb / 2 {
                    self.poc.prev_msb + max_lsb
                } else if lsb > self.poc.prev_lsb && lsb - self.poc.prev_lsb > max_lsb / 2 {
                    self.poc.prev_msb - max_lsb
                } else {
                    self.poc.prev_msb
                };
                let top = msb + lsb;
                if is_ref {
                    self.poc.prev_lsb = lsb;
                    self.poc.prev_msb = msb;
                }
                self.poc.last_has_mmco_5 = false;
                top
            }
            1 => {
                let max_fn = 1i32 << (i32::from(sps.log2_max_frame_num_minus4) + 4);
                let frame_num_offset: i32 = if is_idr {
                    0
                } else {
                    if self.poc.last_has_mmco_5 {
                        self.poc.prev_frame_num_offset = 0;
                        self.poc.prev_frame_num = 0;
                    }
                    if (sh.frame_num as i64) < (self.poc.prev_frame_num as i64) {
                        self.poc.prev_frame_num_offset + max_fn
                    } else {
                        self.poc.prev_frame_num_offset
                    }
                };
                let cycle = sps.offsets_for_ref_frames.len();
                let mut abs_frame_num = if cycle > 0 {
                    frame_num_offset + sh.frame_num as i32
                } else {
                    0
                };
                if !is_ref && abs_frame_num > 0 {
                    abs_frame_num -= 1;
                }
                let mut expected = 0i32;
                if abs_frame_num > 0 {
                    let cycle_cnt = (abs_frame_num - 1) / cycle as i32;
                    let fin = ((abs_frame_num - 1) % cycle as i32) as usize;
                    let delta_cycle: i32 = sps.offsets_for_ref_frames.iter().sum();
                    expected = cycle_cnt * delta_cycle;
                    expected += sps.offsets_for_ref_frames[..=fin].iter().sum::<i32>();
                }
                if !is_ref {
                    expected += sps.offset_for_non_ref_pic;
                }
                let top = expected + sh.delta_poc[0];
                self.poc.prev_frame_num = sh.frame_num;
                self.poc.prev_frame_num_offset = frame_num_offset;
                self.poc.last_has_mmco_5 = false;
                top
            }
            _ => {
                let max_fn = 1i32 << (i32::from(sps.log2_max_frame_num_minus4) + 4);
                let frame_num_offset: i32 = if is_idr {
                    0
                } else {
                    if self.poc.last_has_mmco_5 {
                        self.poc.prev_frame_num = 0;
                        self.poc.prev_frame_num_offset = 0;
                    }
                    if (sh.frame_num as i64) < (self.poc.prev_frame_num as i64) {
                        self.poc.prev_frame_num_offset + max_fn
                    } else {
                        self.poc.prev_frame_num_offset
                    }
                };
                let poc = if is_idr {
                    0
                } else {
                    let abs = frame_num_offset + sh.frame_num as i32;
                    if is_ref { 2 * abs } else { 2 * abs - 1 }
                };
                self.poc.prev_frame_num = sh.frame_num;
                self.poc.prev_frame_num_offset = frame_num_offset;
                self.poc.last_has_mmco_5 = false;
                poc
            }
        }
    }

    /// 短窗带 POC 视图（B 列表构建入参）。
    fn short_ref_with_poc(&self) -> Vec<(Arc<Picture>, i32)> {
        self.short_ref
            .iter()
            .map(|(p, _)| (p.clone(), p.poc))
            .collect()
    }

    /// 列表重排（FFmpeg build_ref_list 语义；B 列表同样适用）。
    fn apply_reorder(
        &self,
        mut list: Vec<Arc<Picture>>,
        mods: &[(u8, u64)],
        num_ref: usize,
    ) -> Result<Vec<Arc<Picture>>, CodecError> {
        let max_pic_num = 1u32 << u32::from(self.log2_max_frame_num);
        let mut pred = self.cur.as_ref().map(|c| c.frame_num).unwrap_or(0);
        for (index, &(op, val)) in mods.iter().enumerate() {
            if index >= num_ref {
                return Err(CodecError::InvalidData(
                    "ref list modification count overflow".into(),
                ));
            }
            let (target, is_long, pic_id): (Arc<Picture>, bool, u32) = match op {
                0 | 1 => {
                    let abs_diff = val + 1;
                    if abs_diff > u64::from(max_pic_num) {
                        return Err(CodecError::InvalidData("abs_diff_pic_num overflow".into()));
                    }
                    pred = if op == 0 {
                        (u64::from(pred) + u64::from(max_pic_num) - abs_diff)
                            % u64::from(max_pic_num)
                    } else {
                        (u64::from(pred) + abs_diff) % u64::from(max_pic_num)
                    } as u32;
                    let found = self
                        .short_ref
                        .iter()
                        .rev()
                        .find(|(_, fn_)| *fn_ == pred)
                        .map(|(p, _)| (p.clone(), false, pred));
                    let Some(e) = found else {
                        return Err(CodecError::InvalidData(
                            "reference picture missing during reorder".into(),
                        ));
                    };
                    e
                }
                _ => {
                    let li = val as usize;
                    if li > 31 {
                        return Err(CodecError::InvalidData("long_term_pic_idx overflow".into()));
                    }
                    let found = self
                        .long_ref
                        .get(li)
                        .and_then(|p| p.as_ref())
                        .map(|p| (p.clone(), true, li as u32));
                    let Some(e) = found else {
                        return Err(CodecError::InvalidData(
                            "reference picture missing during reorder".into(),
                        ));
                    };
                    e
                }
            };
            let mut i = index;
            while i + 1 < num_ref
                && i + 1 < list.len()
                && !(list[i].is_long_term == is_long
                    && (if is_long {
                        // 长项按窗口索引匹配（重建 PicNum 映射太绕，
                        // 直接 Arc 同一性判定槽位——与 FFmpeg 线扫等价：
                        // 找到同图即停）
                        Arc::ptr_eq(&list[i], &target)
                    } else {
                        // 短项按 frame_num 匹配：从短窗反查
                        self.short_ref
                            .iter()
                            .find(|(p, _)| Arc::ptr_eq(p, &list[i]))
                            .map(|(_, fn_)| *fn_ == pic_id)
                            .unwrap_or(false)
                    }))
            {
                i += 1;
            }
            while i > index && i < list.len() {
                list[i] = list[i - 1].clone();
                i -= 1;
            }
            if index < list.len() {
                list[index] = target;
            }
        }
        Ok(list.into_iter().take(num_ref).collect())
    }

    /// 图片完成后的参考帧标记（FFmpeg ff_h264_execute_ref_pic_marking
    /// 逐字语义：MMCO 全操作 + 滑动窗口 + 当前图入窗）。
    fn mark_reference(&mut self, ctx: &PictureCtx) {
        if !ctx.is_ref {
            return;
        }
        let max_fn = 1u32 << u32::from(self.log2_max_frame_num);
        let mut cur_in_long: Option<usize> = None;
        if ctx.idr {
            if ctx.long_term_reference_flag {
                self.long_ref[0] = Some(ctx.pic.clone());
                cur_in_long = Some(0);
            }
        } else {
            for &(op, diff, long_arg) in &ctx.mmco {
                let pic_num = |d: u64| {
                    ((u64::from(ctx.frame_num) + u64::from(max_fn) - d - 1) % u64::from(max_fn))
                        as u32
                };
                let li = long_arg as usize;
                match op {
                    1 => {
                        if let Some(i) = self.short_lookup(pic_num(diff)) {
                            self.short_ref.remove(i);
                        }
                    }
                    2 if li < self.long_ref.len() => {
                        self.long_ref[li] = None;
                    }
                    3 => {
                        if li < self.long_ref.len()
                            && let Some(i) = self.short_lookup(pic_num(diff))
                        {
                            let pic = self.short_ref.remove(i).0;
                            self.long_ref[li] = Some(pic);
                        }
                    }
                    4 => {
                        for (i, l) in self.long_ref.iter_mut().enumerate() {
                            if i >= li {
                                *l = None;
                            }
                        }
                    }
                    5 => {
                        self.short_ref.clear();
                        self.long_ref.fill(None);
                    }
                    6 if li < self.long_ref.len() => {
                        self.long_ref[li] = Some(ctx.pic.clone());
                        cur_in_long = Some(li);
                    }
                    _ => {}
                }
            }
            // 滑动窗口（无显式 MMCO）：短+长 ≥ 上限 → 淘汰最旧短参考
            if ctx.mmco.is_empty() {
                let long_count = self.long_ref.iter().filter(|l| l.is_some()).count();
                if !self.short_ref.is_empty() && long_count + self.short_ref.len() >= self.max_refs
                {
                    self.short_ref.pop();
                }
            }
        }
        // 当前图入短窗（front = 最新）；已入长窗则跳过
        if cur_in_long.is_none() {
            if let Some(i) = self.short_lookup(ctx.frame_num) {
                self.short_ref.remove(i);
            }
            self.short_ref.insert(0, (ctx.pic.clone(), ctx.frame_num));
        }
        self.prev_frame_num = ctx.frame_num;
    }

    /// 短窗按 frame_num 定位（FFmpeg find_short：从尾部扫描）。
    fn short_lookup(&self, frame_num: u32) -> Option<usize> {
        self.short_ref
            .iter()
            .enumerate()
            .rev()
            .find(|(_, (_, fn_))| *fn_ == frame_num)
            .map(|(i, _)| i)
    }

    /// 构建 List0（FFmpeg h264_initialise_ref_list + build_ref_list 重排
    /// 逐字语义：默认序 = 短窗最新在前 + 长窗按索引；重排自 DPB 取图、
    /// 槽位按下移放置）。
    fn build_list0(&self, sh: &SliceHeader) -> Result<Vec<Arc<Picture>>, CodecError> {
        let num_ref = sh.num_ref_l0.max(1);
        // (pic, is_long, pic_id)：pic_id 短=frame_num、长=LongTermFrameIdx
        let mut list: Vec<(Arc<Picture>, bool, u32, bool)> = self
            .short_ref
            .iter()
            .map(|(p, fn_)| (p.clone(), false, *fn_, true))
            .collect();
        for (i, l) in self.long_ref.iter().enumerate() {
            if let Some(p) = l {
                list.push((p.clone(), true, i as u32, true));
            }
        }
        if list.is_empty() {
            return Err(CodecError::InvalidData("no reference pictures".into()));
        }
        while list.len() < num_ref {
            let mut placeholder = list[0].clone();
            placeholder.3 = false;
            list.push(placeholder);
        }
        let max_pic_num = 1u32 << u32::from(self.log2_max_frame_num);
        let mut pred = sh.frame_num;
        for (index, &(op, val)) in sh.ref_mods.iter().enumerate() {
            if index >= num_ref {
                return Err(CodecError::InvalidData(
                    "ref list modification count overflow".into(),
                ));
            }
            let entry: (Arc<Picture>, bool, u32, bool) = match op {
                0 | 1 => {
                    let abs_diff = val + 1;
                    if abs_diff > u64::from(max_pic_num) {
                        return Err(CodecError::InvalidData("abs_diff_pic_num overflow".into()));
                    }
                    pred = if op == 0 {
                        (u64::from(pred) + u64::from(max_pic_num) - abs_diff)
                            % u64::from(max_pic_num)
                    } else {
                        (u64::from(pred) + abs_diff) % u64::from(max_pic_num)
                    } as u32;
                    let found = self
                        .short_ref
                        .iter()
                        .rev()
                        .find(|(_, fn_)| *fn_ == pred)
                        .map(|(p, _)| (p.clone(), false, pred, true));
                    let Some(e) = found else {
                        return Err(CodecError::InvalidData(
                            "reference picture missing during reorder".into(),
                        ));
                    };
                    e
                }
                _ => {
                    let li = val as usize;
                    if li > 31 {
                        return Err(CodecError::InvalidData("long_term_pic_idx overflow".into()));
                    }
                    let found = self
                        .long_ref
                        .get(li)
                        .and_then(|p| p.as_ref())
                        .map(|p| (p.clone(), true, li as u32, true));
                    let Some(e) = found else {
                        return Err(CodecError::InvalidData(
                            "reference picture missing during reorder".into(),
                        ));
                    };
                    e
                }
            };
            // 从 index 起找同 (long, pic_id) 槽位（FFmpeg 线扫），
            // 之后整体下移一格、放置到 index
            let mut i = index;
            while i + 1 < num_ref
                && !(list[i].1 == entry.1
                    && list[i].2 == entry.2
                    && Arc::ptr_eq(&list[i].0, &entry.0))
            {
                i += 1;
            }
            while i > index {
                list[i] = list[i - 1].clone();
                i -= 1;
            }
            list[index] = entry;
        }
        if list.iter().take(num_ref).any(|e| !e.3) {
            return Err(CodecError::InvalidData(
                "active reference slot missing after reorder".into(),
            ));
        }
        Ok(list
            .into_iter()
            .take(num_ref)
            .map(|(p, _, _, _)| p)
            .collect())
    }
}

impl Decoder for H264Decoder {
    fn codec_id(&self) -> CodecId {
        CodecId::H264
    }

    fn feed(&mut self, packet: &Packet) -> Result<(), CodecError> {
        if self.flushed {
            return Err(CodecError::InvalidState("feed after flush is not allowed"));
        }
        if packet.data.is_empty() {
            return Ok(());
        }
        let nals: Vec<Vec<u8>> = match self.avcc_length_size {
            Some(ls) => nal::split_avcc(&packet.data, ls)
                .map_err(codec_err)?
                .into_iter()
                .map(|n| n.to_vec())
                .collect(),
            None => nal::split_annexb(&packet.data)
                .map_err(codec_err)?
                .into_iter()
                .map(|n| n.to_vec())
                .collect(),
        };
        for nal in &nals {
            self.process_nal(nal, packet.pts)?;
        }
        Ok(())
    }

    fn flush(&mut self) {
        if self.flushed {
            return;
        }
        if let Err(e) = self.finish_current_picture() {
            self.pending_error = Some(e);
        }
        self.flush_dpb();
        self.flushed = true;
    }

    fn pull(&mut self) -> Result<Decoded, CodecError> {
        if let Some(e) = self.pending_error.take() {
            return Err(e);
        }
        if let Some(f) = self.out.pop_front() {
            return Ok(Decoded::Frame(f));
        }
        if self.flushed {
            Ok(Decoded::Finished)
        } else {
            Ok(Decoded::NeedInput)
        }
    }
}

/// 图片 → 裁剪后的 yuv420p [`VideoFrame`]（行复制，含奇数尺寸的
/// chroma 上取整）。
fn build_frame(pic: &Picture, pts: Option<Timestamp>) -> Result<VideoFrame, H264Error> {
    let w = pic.width;
    let h = pic.height;
    let mut vf = VideoFrame::new(
        PixelFormat::Yuv420p,
        w as u32,
        h as u32,
        pic.color,
        pts,
        None,
    )
    .map_err(|e| H264Error::InvalidData(format!("frame alloc failed: {e:?}")))?;
    let copy_in = |dst: &mut [u8], dst_stride: usize, src: &[u8], src_stride: usize, rows, cols| {
        for y in 0..rows {
            dst[y * dst_stride..y * dst_stride + cols]
                .copy_from_slice(&src[y * src_stride..y * src_stride + cols]);
        }
    };
    let cw = w.div_ceil(2);
    let ch = h.div_ceil(2);
    let ls = vf.layout().plane(0).stride;
    let cs = vf.layout().plane(1).stride;
    copy_in(vf.plane_mut(0), ls, &pic.luma, pic.luma_stride(), h, w);
    copy_in(vf.plane_mut(1), cs, &pic.cb, pic.chroma_stride(), ch, cw);
    copy_in(vf.plane_mut(2), cs, &pic.cr, pic.chroma_stride(), ch, cw);
    Ok(vf)
}

/// [`H264Error`] → [`CodecError`]（形态收紧走 `UnsupportedStub` 桶——
/// 语义为"本阶段未实现/拒绝"，文案里写明缘由）。
fn codec_err(e: H264Error) -> CodecError {
    match e {
        H264Error::InvalidData(m) => CodecError::InvalidData(m),
        // 解码路径不会产生 InvalidArguments（纯容器层入参错误）；
        // 若出现按数据问题上报，文案原样保留
        H264Error::InvalidArguments(m) => CodecError::InvalidData(m.to_string()),
        H264Error::Unsupported(m) => CodecError::UnsupportedStub(m),
    }
}

/// 列表 POC 快照（InterCtx::ref_poc 用；缺项 0）。
fn list0_poc(list: &[Arc<Picture>]) -> [i32; 32] {
    let mut a = [0i32; 32];
    for (i, p) in list.iter().enumerate().take(32) {
        a[i] = p.poc;
    }
    a
}

/// 工厂：消费 [`CodecParameters`]（extradata 可为 avcC 或 Annex B）。
///
/// # Errors
/// 构造失败（extradata 无法识别等）原样透出。
pub fn decoder_factory(params: &CodecParameters) -> Result<Box<dyn Decoder>, CodecError> {
    Ok(Box::new(H264Decoder::new(params)?))
}
