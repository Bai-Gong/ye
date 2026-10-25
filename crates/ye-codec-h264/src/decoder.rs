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
//! 形态收紧（如实报错，绝不静默错帧）：CABAC、场图/MBAFF、
//! 4:2:2/4:4:4、高位深、Baseline/Main/Extended 之外 profile、
//! 去块未禁用、B/SP/SI 片、加权预测、数据分区、frame_num 断号
//! （错误处理属 ②c）→ `Unsupported`；
//! 位流畸形 → `InvalidData`。

use std::collections::VecDeque;
use std::sync::Arc;

use ye_codec::{CodecError, CodecId, CodecParameters, Decoded, Decoder, Frame, Packet, VideoFrame};
use ye_pixfmt::{ColorDesc, PixelFormat};
use ye_time::Timestamp;

use crate::bitreader::BitReader;
use crate::error::H264Error;
use crate::macroblock::{PFrameCtx, Picture};
use crate::nal::{self, NalHeader};
use crate::slice::{self, SliceHeader, SliceType};
use crate::sps::{Pps, Sps};

/// H.264 解码器（②a：CAVLC、8bit 4:2:0、I 片、去块禁用）。
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
    /// flush 后排空中暂存的失败（flush 无返回值，经 pull 透出）。
    pending_error: Option<CodecError>,
    flushed: bool,
}

/// 一张图的解码上下文。
struct PictureCtx {
    pic: Arc<Picture>,
    sps_id: u8,
    frame_num: u32,
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
                let pps = Pps::parse(nal).map_err(codec_err)?;
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

        // ②a 参数集级门槛
        if pps.entropy_coding_mode_flag {
            return Err(CodecError::UnsupportedStub(
                "CABAC entropy coding not implemented in this stage",
            ));
        }
        if !matches!(sps.profile_idc, 66 | 77 | 88) {
            return Err(CodecError::UnsupportedStub(
                "profile beyond Baseline/Main/Extended not supported in this stage",
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

        // ---- 访问单元边界（I/P 通用）----
        // IDR 恒为新图；非 IDR 以帧号变化判定。P 帧断号属错误处理（②c），
        // 当前如实收紧为 Unsupported。
        let max_fn = 1u32 << u32::from(sps.log2_max_frame_num_minus4 + 4);
        if self.cur.is_some() && sh.first_mb_in_slice == 0 {
            let cur = self.cur.as_ref().expect("checked non-empty");
            let new_pic = is_idr || cur.frame_num != sh.frame_num || cur.idr != is_idr;
            if !new_pic {
                return Err(CodecError::InvalidData(format!(
                    "duplicate picture start (frame_num {}, idr {is_idr})",
                    sh.frame_num
                )));
            }
            if !is_idr && sh.frame_num != (cur.frame_num + 1) % max_fn {
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
                for l in &mut self.long_ref {
                    *l = None;
                }
            } else if self.short_ref.is_empty() && self.long_ref.iter().all(|l| l.is_none()) {
                return Err(CodecError::UnsupportedStub(
                    "stream does not start with an IDR picture",
                ));
            }
            self.max_refs = usize::from(sps.max_num_ref_frames).max(1);
            self.log2_max_frame_num = sps.log2_max_frame_num_minus4 + 4;
            let mb_w = usize::from(sps.pic_width_in_mbs);
            let mb_h = usize::from(sps.pic_height_in_map_units);
            self.cur = Some(PictureCtx {
                pic: Arc::new(Picture::new(
                    mb_w,
                    mb_h,
                    sps.width() as usize,
                    sps.height() as usize,
                )),
                sps_id: sps.seq_parameter_set_id,
                frame_num: sh.frame_num,
                idr: is_idr,
                is_ref: header.nal_ref_idc != 0,
                long_term_reference_flag: sh.long_term_reference_flag,
                mmco: sh.mmco.clone(),
                decoded_mb_count: 0,
                next_slice_id: 0,
                pts,
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

        // ---- P 片：构建 List0（默认序 + 重排）----
        let list0: Vec<Arc<Picture>> = if sh.slice_type == SliceType::P {
            self.build_list0(&sh)?
        } else {
            Vec::new()
        };
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
            let pctx = if sh.slice_type == SliceType::P {
                Some(PFrameCtx {
                    refs: &list0,
                    constrained: pps.constrained_intra_pred_flag,
                })
            } else {
                None
            };
            slice::decode_slice(
                &mut r,
                data_bits,
                pic_mut,
                &pps,
                slice_id,
                &sh,
                mb_total,
                pctx.as_ref(),
            )
            .map_err(codec_err)?
        };
        ctx.decoded_mb_count += end - sh.first_mb_in_slice;
        Ok(())
    }

    /// 完成在途图片：补全检查通过 → 组装 yuv420p 帧入队。
    fn finish_current_picture(&mut self) -> Result<(), CodecError> {
        let Some(ctx) = self.cur.take() else {
            return Ok(());
        };
        let total = ctx.mb_total();
        if ctx.decoded_mb_count != total {
            return Err(CodecError::InvalidData(format!(
                "incomplete picture: {} of {total} macroblocks decoded",
                ctx.decoded_mb_count
            )));
        }
        self.mark_reference(&ctx);
        let frame = build_frame(&ctx.pic, ctx.pts).map_err(codec_err)?;
        self.out.push_back(Frame::Video(frame));
        Ok(())
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
                        for l in &mut self.long_ref {
                            *l = None;
                        }
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
        let mut list: Vec<(Arc<Picture>, bool, u32)> = self
            .short_ref
            .iter()
            .map(|(p, fn_)| (p.clone(), false, *fn_))
            .collect();
        for (i, l) in self.long_ref.iter().enumerate() {
            if let Some(p) = l {
                list.push((p.clone(), true, i as u32));
            }
        }
        if list.len() < num_ref {
            return Err(CodecError::InvalidData(format!(
                "not enough reference pictures ({} < {num_ref})",
                list.len()
            )));
        }
        let max_pic_num = 1u32 << u32::from(self.log2_max_frame_num);
        let mut pred = sh.frame_num;
        for (index, &(op, val)) in sh.ref_mods.iter().enumerate() {
            if index >= num_ref {
                return Err(CodecError::InvalidData(
                    "ref list modification count overflow".into(),
                ));
            }
            let entry: (Arc<Picture>, bool, u32) = match op {
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
        Ok(list.into_iter().take(num_ref).map(|(p, _, _)| p).collect())
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
        ColorDesc::UNSPECIFIED,
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

/// 工厂：消费 [`CodecParameters`]（extradata 可为 avcC 或 Annex B）。
///
/// # Errors
/// 构造失败（extradata 无法识别等）原样透出。
pub fn decoder_factory(params: &CodecParameters) -> Result<Box<dyn Decoder>, CodecError> {
    Ok(Box::new(H264Decoder::new(params)?))
}
