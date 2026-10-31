//! `ye_codec::Encoder` 接线（M3 施工图 T5）：[`H264Encoder`] 与
//! [`encoder_factory`]（BaselineSession → trait 适配层）。
//!
//! ## 输出契约（contracts.md「H.264 编码器输出契约」的正本）
//!
//! - **包体**：AVCC 长度前缀形态（4 字节大端长度 + NAL，与 avcC 的
//!   `lengthSizeMinusOne=3` 一致）；IDR 包带内含 SPS+PPS+IDR 三个 NAL
//!   （裸流/换容器场景自足，MP4 场景与 avcC extradata 冗余但合法），
//!   P 包仅含片 NAL；
//! - **extradata**：avcC 记录（[`extradata`](H264Encoder::extradata)），
//!   构造即可用（PPS QP 中立，见 `frame.rs` 参数集定形）——`Encoder`
//!   trait 不携带配置回读面，带外参数经具体类型访问器取得；
//! - **时戳**：输出包按 `EncoderConfig.time_base` 打戳，pts = 帧号 ×
//!   帧率倒数 重标度（[`Timestamp::from_frame_index`]），**输入帧的
//!   pts 被忽略**；B 帧缺席故无重排，dts = pts；duration = 一帧时长；
//!   `stream_index` 恒 0（流概念归封装侧，由 muxer 改写）；
//! - **GOP**：`gop = 1` 全 I 帧；`gop = N` 时帧号 0, N, 2N, … 为 IDR
//!   （携带 [`PacketFlags::KEYFRAME`]）；
//! - **重建快照**：本地解码环逐帧重建经 [`recon_frames`] 暴露——
//!   位级回环验证（`tests/encode_h264.rs`）与消费方预览共用；长编码
//!   的内存代价由调用方自理（逐帧消费后 [`clear_recon`]）。
//!
//! ## 质量模式与码控
//!
//! [`QualityMode::Qp`] 透传恒定 QP；[`QualityMode::Crf`] 为**启发式**
//! 简单 CRF——基 QP = crf 值，按帧内活动度（luma 相邻差均值）加
//! 0..=4 的 ΔQP（复杂帧更粗，对齐 x264「复杂度升 QP」的方向性）。
//!
//! [`EncoderConfig::bit_rate`] 接单程码控（`encoder::rate`，m3-plan
//! §7.2 批次）：`None` 时码控缺席、行为与首轮逐字节一致；`Some(bps)` +
//! `Qp` → CBR 风味（QP 自基值受虚拟缓冲反馈游走、平均码率趋近 bps）；
//! `Some(bps)` + `Crf` → A-VBR（基 QP 走活动度启发式，码控只作上限
//! 约束，对标 ffmpeg `-crf c -maxrate b -bufsize 2b`）。2-pass/完整
//! CRF 仍归后续批次。
//!
//! ## 状态机（trait 契约）
//!
//! `flush` 后 `pull` 排空输出队列即 `Finished`（幂等），此后 `feed` →
//! `Err(InvalidState)`；未 flush 且队列空 → `NeedInput`。编码逐帧同步
//! 完成，flush 无滞留冲刷（无 B 帧无重排）。

use std::collections::VecDeque;
use std::sync::Arc;

use ye_codec::{
    CodecError, CodecId, Encoded, Encoder, EncoderConfig, Frame, Packet, PacketFlags, QualityMode,
};
use ye_pixfmt::PixelFormat;
use ye_time::{Rational, Timestamp};

use crate::encoder::frame::YuvSource;
use crate::encoder::params::{build_avcc, level_for};
use crate::encoder::pframe::BaselineSession;
use crate::encoder::rate::RateController;
use crate::error::H264Error;

/// 运动估计搜索范围（像素，双侧）。Baseline 首轮固定值——配置面归
/// 真码控批次（m3-plan §7.2）。
const SEARCH_RANGE: i32 = 16;

/// 编码器本地解码环的单帧重建快照（裁剪到显示尺寸的紧凑 yuv420p）。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`（纯字节载荷）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconFrame {
    /// luma 平面（`width × height`，紧凑行主序）。
    pub luma: Vec<u8>,
    /// Cb 平面（`width/2 × height/2`）。
    pub cb: Vec<u8>,
    /// Cr 平面（`width/2 × height/2`）。
    pub cr: Vec<u8>,
}

/// H.264 Baseline 编码器（[`Encoder`] 实现，工厂见 [`encoder_factory`]）。
///
/// 输出契约与质量模式见模块文档；输入仅收 yuv420p、尺寸与配置一致的
/// 视频帧（校验失败 → `InvalidArguments`，真工厂交付即校验）。
pub struct H264Encoder {
    width: usize,
    height: usize,
    gop: u32,
    quality: QualityMode,
    session: BaselineSession,
    /// avcC 记录（构造即定：level 查表 + QP 中立 PPS）。
    extradata: Vec<u8>,
    out: VecDeque<Packet>,
    recon: Vec<ReconFrame>,
    record_recon: bool,
    color_sps: Option<Vec<u8>>,
    time_base: Rational,
    frame_rate: Rational,
    frame_index: u64,
    flushed: bool,
    /// 单程码控（`bit_rate = Some` 时在场；见模块文档「质量模式与码控」）。
    rate: Option<RateController>,
}

impl H264Encoder {
    /// 由 [`EncoderConfig`] 构造（工厂的实体入口）。
    ///
    /// # Errors
    /// 配置非法（codec 非本实现/缺视频参数/格式非 yuv420p/宽高 0 或
    /// 奇数/`gop` 为 0/质量值越界（QP、CRF 均 1..=51——0 显式拒绝，
    /// 形态收紧一律 `Err` 的库风）/帧率非正/时基非正/`bit_rate` 为 0
    /// 或超出虚拟缓冲算术界/尺寸×帧率超出 level 5.2 限额）→
    /// [`CodecError::InvalidArguments`]。
    pub fn new(config: &EncoderConfig) -> Result<Self, CodecError> {
        if config.codec != CodecId::H264 {
            return Err(CodecError::InvalidArguments(
                "h264 encoder requires codec = h264",
            ));
        }
        let Some(video) = &config.video else {
            return Err(CodecError::InvalidArguments(
                "h264 encoder requires video config",
            ));
        };
        if video.format != PixelFormat::Yuv420p {
            return Err(CodecError::InvalidArguments(
                "h264 encoder supports only yuv420p input",
            ));
        }
        if video.width == 0 || video.height == 0 || video.width % 2 != 0 || video.height % 2 != 0 {
            return Err(CodecError::InvalidArguments(
                "h264 encoder needs nonzero even dimensions",
            ));
        }
        if video.gop == 0 {
            return Err(CodecError::InvalidArguments(
                "h264 encoder needs gop >= 1 (1 = all-intra)",
            ));
        }
        match video.quality {
            QualityMode::Qp(qp) | QualityMode::Crf(qp) => {
                if qp == 0 || qp > 51 {
                    return Err(CodecError::InvalidArguments(
                        "h264 encoder quality value must be 1..=51 (0 rejected)",
                    ));
                }
            }
        }
        if video.frame_rate.num() <= 0 {
            return Err(CodecError::InvalidArguments(
                "h264 encoder needs a positive frame rate",
            ));
        }
        if config.time_base.num() <= 0 {
            return Err(CodecError::InvalidArguments(
                "h264 encoder needs a positive time base",
            ));
        }
        // 码控（bit_rate）：0/越界值在此拒收（口径见 encoder::rate）。
        // 帧率与质量模式已在上方校验，控制器构造只剩算术溢出类失败。
        let rate = match config.bit_rate {
            None => None,
            Some(0) => {
                return Err(CodecError::InvalidArguments(
                    "h264 encoder needs a positive bit rate (0 rejected)",
                ));
            }
            Some(bps) => Some(
                RateController::new(bps, video.frame_rate, video.quality)
                    .map_err(CodecError::InvalidArguments)?,
            ),
        };
        let Some(level) = level_for(video.width, video.height, video.frame_rate) else {
            return Err(CodecError::InvalidArguments(
                "frame size x frame rate exceeds level 5.2 limits",
            ));
        };
        let width = video.width as usize;
        let height = video.height as usize;
        let session =
            BaselineSession::new(width, height, level, SEARCH_RANGE).map_err(codec_err)?;
        let sps = crate::encoder::frame::write_sps_nal(width, height, level).map_err(codec_err)?;
        let pps = crate::encoder::frame::write_pps_nal().map_err(codec_err)?;
        Ok(H264Encoder {
            width,
            height,
            gop: video.gop,
            quality: video.quality,
            session,
            extradata: build_avcc(&sps, &pps),
            out: VecDeque::new(),
            recon: Vec::new(),
            record_recon: true,
            color_sps: None,
            time_base: config.time_base,
            frame_rate: video.frame_rate,
            frame_index: 0,
            flushed: false,
            rate,
        })
    }

    /// 带外参数（avcC 记录）：解码器 `CodecParameters::extradata` 与
    /// MP4 `avc1` stsd 的 `extra_data` 直填（构造即可用）。
    #[must_use]
    pub fn extradata(&self) -> &[u8] {
        &self.extradata
    }

    /// 本地解码环的逐帧重建快照（位级回环验证/预览面，见模块文档）。
    #[must_use]
    pub fn recon_frames(&self) -> &[ReconFrame] {
        &self.recon
    }

    /// 清空重建快照历史（长编码的内存控制；不影响编码状态）。
    pub fn clear_recon(&mut self) {
        self.recon.clear();
    }

    /// 当前质量模式下的本帧 QP（恒定 QP 直取；简单 CRF 按活动度调）。
    fn qp_for(&self, luma: &[u8], stride: usize) -> u8 {
        match self.quality {
            QualityMode::Qp(qp) => qp,
            QualityMode::Crf(crf) => {
                // 帧内活动度：luma 相邻像素差的绝对值均值（0..=255，
                // 平滑内容 ≈ 0..8、自然内容 ≈ 8..32、强纹理 > 32）。
                // 宽高 ≥ 2 由工厂的「非零偶数」校验保证，循环无下溢。
                let (w, h) = (self.width, self.height);
                let mut acc: u64 = 0;
                let mut n: u64 = 0;
                for y in 0..h {
                    let row = y * stride;
                    for x in 0..w - 1 {
                        acc += u64::from(
                            (i16::from(luma[row + x + 1]) - i16::from(luma[row + x]))
                                .unsigned_abs(),
                        );
                    }
                    n += (w - 1) as u64;
                }
                for y in 0..h - 1 {
                    let (row, next) = (y * stride, (y + 1) * stride);
                    for x in 0..w {
                        acc += u64::from(
                            (i16::from(luma[next + x]) - i16::from(luma[row + x])).unsigned_abs(),
                        );
                    }
                    n += w as u64;
                }
                let activity = acc / n.max(1);
                // ΔQP ∈ 0..=4：每 8 级活动度加 1（方向对齐 x264——
                // 复杂帧升 QP；启发式，无码控闭环）。
                let delta = ((activity / 8) as i16).min(4);
                (i16::from(crf) + delta).clamp(1, 51) as u8
            }
        }
    }

    /// 一帧的编码主链：基 QP（恒定 QP/活动度启发式）→ 码控调制 →
    /// GOP 判定 → 会话编码 → AVCC 包体 → 打戳入队 → 重建快照 →
    /// 包位数回灌码控。
    fn encode_frame(&mut self, video: &ye_codec::VideoFrame) -> Result<(), CodecError> {
        let layout = video.layout();
        let (ls, cs) = (layout.plane(0).stride, layout.plane(1).stride);
        let yuv = YuvSource {
            luma: video.plane(0),
            luma_stride: ls,
            cb: video.plane(1),
            cr: video.plane(2),
            chroma_stride: cs,
        };
        // 基 QP 先走恒定 QP / 活动度启发式（&self 借用），码控在场时
        // 再做虚拟缓冲调制（CBR 游走 / A-VBR 封顶取大）——随后才动
        // &mut session，借用序天然成立
        let base_qp = self.qp_for(video.plane(0), ls);
        let qp = match &self.rate {
            Some(rc) => rc.next_qp(base_qp),
            None => base_qp,
        };
        let idr = self.frame_index.is_multiple_of(u64::from(self.gop));
        let encoded = if idr {
            self.session.encode_idr(&yuv, qp).map_err(codec_err)?
        } else {
            self.session.encode_p(&yuv, qp).map_err(codec_err)?
        };

        // AVCC 包体：4 字节大端长度前缀 + NAL；IDR 带内 SPS/PPS 前置
        let mut data = Vec::with_capacity(encoded.nal.len() + 16);
        if idr {
            let (sps, pps) = self.session.param_sets();
            push_avcc_nal(&mut data, self.color_sps.as_deref().unwrap_or(sps));
            push_avcc_nal(&mut data, pps);
        }
        push_avcc_nal(&mut data, &encoded.nal);
        // 包体位数（含 IDR 带内参数集——容器/网络侧的真实传输位数），
        // 出包后回灌码控的虚拟缓冲
        let frame_bits = i64::try_from(data.len())
            .ok()
            .and_then(|len| len.checked_mul(8))
            .ok_or_else(|| CodecError::InvalidData("packet size exceeds i64 bit count".into()))?;

        // 打戳：帧号 × 帧率倒数 重标度到输出时基（无 B 帧 → dts = pts）
        let idx = i64::try_from(self.frame_index)
            .map_err(|_| CodecError::InvalidData("frame index exceeds i64".into()))?;
        let pts = Timestamp::from_frame_index(idx, self.frame_rate)?.rescale(self.time_base)?;
        let duration = Timestamp::from_frame_index(1, self.frame_rate)?.rescale(self.time_base)?;
        let mut packet = Packet::empty(0);
        packet.data = Arc::from(data.into_boxed_slice());
        packet.pts = Some(pts);
        packet.dts = Some(pts);
        packet.duration = Some(duration);
        if idr {
            packet.flags.insert(PacketFlags::KEYFRAME);
        }
        self.out.push_back(packet);
        if self.record_recon {
            self.recon.push(ReconFrame {
                luma: encoded.recon_luma,
                cb: encoded.recon_cb,
                cr: encoded.recon_cr,
            });
        }
        if let Some(rc) = self.rate.as_mut() {
            rc.report_frame(frame_bits, qp);
        }
        self.frame_index += 1;
        Ok(())
    }
}

impl Encoder for H264Encoder {
    fn codec_id(&self) -> CodecId {
        CodecId::H264
    }

    fn feed(&mut self, frame: &Frame) -> Result<(), CodecError> {
        if self.flushed {
            return Err(CodecError::InvalidState(
                "encoder already flushed and drained",
            ));
        }
        let Frame::Video(video) = frame else {
            return Err(CodecError::InvalidArguments(
                "h264 encoder accepts only video frames",
            ));
        };
        if video.format() != PixelFormat::Yuv420p {
            return Err(CodecError::InvalidArguments(
                "h264 encoder supports only yuv420p input",
            ));
        }
        if video.width() as usize != self.width || video.height() as usize != self.height {
            return Err(CodecError::InvalidArguments(
                "frame dimensions differ from encoder config",
            ));
        }
        if self.frame_index == 0 && video.color() != ye_pixfmt::ColorDesc::UNSPECIFIED {
            let level = level_for(self.width as u32, self.height as u32, self.frame_rate)
                .ok_or(CodecError::InvalidArguments("level"))?;
            let sps = crate::encoder::frame::write_sps_nal_color(
                self.width,
                self.height,
                level,
                Some(video.color()),
            )
            .map_err(codec_err)?;
            let pps = crate::encoder::frame::write_pps_nal().map_err(codec_err)?;
            self.extradata = build_avcc(&sps, &pps);
            self.color_sps = Some(sps);
        }
        self.encode_frame(video)
    }

    /// 编码逐帧同步完成，无滞留冲刷（无 B 帧无重排）——仅置排空标记。
    fn flush(&mut self) {
        self.flushed = true;
    }

    fn pull(&mut self) -> Result<Encoded, CodecError> {
        if let Some(packet) = self.out.pop_front() {
            return Ok(Encoded::Packet(packet));
        }
        if self.flushed {
            Ok(Encoded::Finished)
        } else {
            Ok(Encoded::NeedInput)
        }
    }
}

/// 追加一个 AVCC NAL（4 字节大端长度前缀 + NAL 字节）。
fn push_avcc_nal(out: &mut Vec<u8>, nal: &[u8]) {
    out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
    out.extend_from_slice(nal);
}

/// [`H264Error`] → [`CodecError`]（编码侧映射：`InvalidArguments` 是
/// 调用方配置/帧数据错误，**保序透传**——与解码侧 `codec_err` 把它归
/// `InvalidData` 的选择不同，编码输入来自调用方而非容器）。
fn codec_err(e: H264Error) -> CodecError {
    match e {
        H264Error::InvalidData(m) => CodecError::InvalidData(m),
        H264Error::InvalidArguments(m) => CodecError::InvalidArguments(m),
        H264Error::Unsupported(m) => CodecError::UnsupportedStub(m),
    }
}

/// 工厂：消费 [`EncoderConfig`]（`CodecRegistry::create_encoder` 的
/// 注册目标，门面注册见 `ye::codec_h264::register_h264_encoder`）。
///
/// # Errors
/// 见 [`H264Encoder::new`]（真工厂交付即校验，对照桩的「工厂不校验」
/// 语义——桩已删除）。
pub fn encoder_factory(config: &EncoderConfig) -> Result<Box<dyn Encoder>, CodecError> {
    let mut encoder = H264Encoder::new(config)?;
    encoder.record_recon = false;
    Ok(Box::new(encoder))
}

// ---------------------------------------------------------------------------
// 单元测试：配置校验 / 状态机 / 打戳与 GOP / avcC 回环 / 黑盒解码冒烟
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::H264Decoder;
    use ye_codec::{CodecParameters, Decoded, Decoder, MediaType, VideoFrame};
    use ye_pixfmt::ColorDesc;

    /// 测试基准配置（96x64@25、GOP 12、QP 24）。
    fn base_config() -> EncoderConfig {
        EncoderConfig {
            codec: CodecId::H264,
            time_base: Rational::new(1, 25),
            bit_rate: None,
            video: Some(ye_codec::VideoEncoderConfig {
                width: 96,
                height: 64,
                format: PixelFormat::Yuv420p,
                frame_rate: Rational::RATE_25,
                gop: 12,
                quality: QualityMode::Qp(24),
            }),
            audio: None,
        }
    }

    /// 合成渐变帧（确定性；t 控制相位）。
    fn frame_t(w: u32, h: u32, t: u32) -> VideoFrame {
        let mut f = VideoFrame::new(
            PixelFormat::Yuv420p,
            w,
            h,
            ColorDesc::UNSPECIFIED,
            None,
            None,
        )
        .unwrap();
        let ls = f.layout().plane(0).stride;
        let cs = f.layout().plane(1).stride;
        for y in 0..h as usize {
            for x in 0..w as usize {
                f.plane_mut(0)[y * ls + x] = ((x * 3 + y * 5 + t as usize * 2) & 0xFF) as u8;
            }
        }
        for y in 0..(h / 2) as usize {
            for x in 0..(w / 2) as usize {
                let v = ((x * 4 + y * 3 + 40 + t as usize) & 0xFF) as u8;
                f.plane_mut(1)[y * cs + x] = v;
                f.plane_mut(2)[y * cs + x] = v;
            }
        }
        f
    }

    /// 配置校验矩阵：逐项非法 → InvalidArguments，基准配置可构造。
    #[test]
    fn factory_validates_config() {
        assert!(H264Encoder::new(&base_config()).is_ok());

        let mut bad = base_config();
        bad.codec = CodecId::Hevc;
        assert!(matches!(
            H264Encoder::new(&bad),
            Err(CodecError::InvalidArguments(_))
        ));

        let bad = EncoderConfig {
            codec: CodecId::H264,
            time_base: Rational::new(1, 25),
            bit_rate: None,
            video: None,
            audio: None,
        };
        assert!(matches!(
            H264Encoder::new(&bad),
            Err(CodecError::InvalidArguments(_))
        ));

        let mut bad = base_config();
        bad.video.as_mut().unwrap().format = PixelFormat::Nv12;
        assert!(matches!(
            H264Encoder::new(&bad),
            Err(CodecError::InvalidArguments(_))
        ));

        for (w, h) in [(0u32, 64u32), (96, 0), (95, 64), (96, 63)] {
            let mut bad = base_config();
            let v = bad.video.as_mut().unwrap();
            v.width = w;
            v.height = h;
            assert!(
                matches!(H264Encoder::new(&bad), Err(CodecError::InvalidArguments(_))),
                "({w},{h}) 须拒收"
            );
        }

        let mut bad = base_config();
        bad.video.as_mut().unwrap().gop = 0;
        assert!(matches!(
            H264Encoder::new(&bad),
            Err(CodecError::InvalidArguments(_))
        ));

        for q in [0u8, 52] {
            let mut bad = base_config();
            bad.video.as_mut().unwrap().quality = QualityMode::Qp(q);
            assert!(
                matches!(H264Encoder::new(&bad), Err(CodecError::InvalidArguments(_))),
                "qp {q} 须拒收"
            );
            let mut bad = base_config();
            bad.video.as_mut().unwrap().quality = QualityMode::Crf(q);
            assert!(
                matches!(H264Encoder::new(&bad), Err(CodecError::InvalidArguments(_))),
                "crf {q} 须拒收"
            );
        }

        let mut bad = base_config();
        bad.video.as_mut().unwrap().frame_rate = Rational::new(0, 1);
        assert!(matches!(
            H264Encoder::new(&bad),
            Err(CodecError::InvalidArguments(_))
        ));

        let mut bad = base_config();
        bad.time_base = Rational::new(0, 1);
        assert!(matches!(
            H264Encoder::new(&bad),
            Err(CodecError::InvalidArguments(_))
        ));

        // 码控参数：bit_rate = 0 拒收；有效值可构造；越界（虚拟缓冲
        // 2×bps 溢出 i64）拒收
        let mut bad = base_config();
        bad.bit_rate = Some(0);
        assert!(matches!(
            H264Encoder::new(&bad),
            Err(CodecError::InvalidArguments(_))
        ));
        let mut ok = base_config();
        ok.bit_rate = Some(300_000);
        assert!(H264Encoder::new(&ok).is_ok());
        let mut bad = base_config();
        bad.bit_rate = Some(u64::MAX);
        assert!(matches!(
            H264Encoder::new(&bad),
            Err(CodecError::InvalidArguments(_))
        ));

        // 尺寸×帧率超 level 5.2（8K@120）
        let mut bad = base_config();
        let v = bad.video.as_mut().unwrap();
        v.width = 7680;
        v.height = 4320;
        v.frame_rate = Rational::new(120, 1);
        assert!(matches!(
            H264Encoder::new(&bad),
            Err(CodecError::InvalidArguments(_))
        ));
    }

    /// 状态机：未 feed 时 NeedInput；flush 后排空 → Finished 幂等；
    /// 再 feed → InvalidState。
    #[test]
    fn encoder_state_machine() {
        let mut enc = H264Encoder::new(&base_config()).unwrap();
        assert!(matches!(enc.pull(), Ok(Encoded::NeedInput)));

        let frame = frame_t(96, 64, 0);
        enc.feed(&Frame::Video(frame.clone())).unwrap();
        assert!(matches!(enc.pull(), Ok(Encoded::Packet(_))));
        assert!(matches!(enc.pull(), Ok(Encoded::NeedInput)));

        enc.feed(&Frame::Video(frame)).unwrap();
        enc.flush();
        assert!(matches!(enc.pull(), Ok(Encoded::Packet(_))));
        assert!(matches!(enc.pull(), Ok(Encoded::Finished)));
        assert!(matches!(enc.pull(), Ok(Encoded::Finished)));
        let any_frame = Frame::Video(frame_t(96, 64, 1));
        assert!(matches!(
            enc.feed(&any_frame),
            Err(CodecError::InvalidState(_))
        ));
    }

    /// 打戳与 GOP：GOP=3 的 7 帧 → IDR@{0,3,6}；pts/dts/duration 按
    /// time_base 打戳（帧号 × 帧率倒数重标度）；KEYFRAME 只在 IDR。
    #[test]
    fn stamping_and_gop_cadence() {
        let mut cfg = base_config();
        cfg.video.as_mut().unwrap().gop = 3;
        // 时基 = 1/90000（非 1/25）：验证重标度（90000/25 = 3600/帧）
        cfg.time_base = Rational::new(1, 90_000);
        let mut enc = H264Encoder::new(&cfg).unwrap();

        for t in 0..7u32 {
            enc.feed(&Frame::Video(frame_t(96, 64, t))).unwrap();
        }
        for i in 0..7u64 {
            let Encoded::Packet(p) = enc.pull().unwrap() else {
                panic!("packet {i}");
            };
            let want = Timestamp::new((i * 3600) as i64, Rational::new(1, 90_000));
            assert_eq!(p.pts, Some(want), "frame {i} pts");
            assert_eq!(p.dts, Some(want), "无 B 帧 → dts == pts");
            assert_eq!(
                p.duration,
                Some(Timestamp::new(3600, Rational::new(1, 90_000)))
            );
            assert_eq!(p.stream_index, 0);
            let key = p.flags.contains(PacketFlags::KEYFRAME);
            assert_eq!(key, i % 3 == 0, "frame {i} keyframe");
            // AVCC：4 字节大端长度前缀；IDR 包带内三个 NAL、P 包一个
            let mut pos = 0usize;
            let mut nals = 0usize;
            while pos < p.data.len() {
                let len = u32::from_be_bytes([
                    p.data[pos],
                    p.data[pos + 1],
                    p.data[pos + 2],
                    p.data[pos + 3],
                ]) as usize;
                pos += 4 + len;
                nals += 1;
            }
            assert_eq!(pos, p.data.len(), "AVCC 前缀须恰好覆盖全包");
            assert_eq!(nals, if key { 3 } else { 1 }, "frame {i} nal count");
        }
        assert!(matches!(enc.pull(), Ok(Encoded::NeedInput)));
    }

    /// 帧校验：音频帧 / 非 yuv420p / 尺寸不符 → InvalidArguments。
    #[test]
    fn feed_rejects_mismatched_frames() {
        let mut enc = H264Encoder::new(&base_config()).unwrap();
        let audio = Frame::Audio(
            ye_codec::AudioFrame::new(
                ye_pixfmt::SampleFormat::I16,
                ye_pixfmt::ChannelLayout::MONO,
                48_000,
                16,
                None,
            )
            .unwrap(),
        );
        assert!(matches!(
            enc.feed(&audio),
            Err(CodecError::InvalidArguments(_))
        ));
        let wrong_dims = frame_t(64, 64, 0);
        assert!(matches!(
            enc.feed(&Frame::Video(wrong_dims)),
            Err(CodecError::InvalidArguments(_))
        ));
    }

    /// avcC（extradata 访问器）→ 解析回环 + 黑盒解码冒烟：经自家
    /// `H264Decoder`（AVCC 输入 + avcC extradata）解码 IDR 包得帧，
    /// 输出与本地重建逐像素一致（§3.1 位级口径的最小样例；全量序列
    /// 归 `crates/ye/tests/encode_h264.rs`）。
    #[test]
    fn extradata_roundtrip_and_decode_smoke() {
        let mut enc = H264Encoder::new(&base_config()).unwrap();
        enc.feed(&Frame::Video(frame_t(96, 64, 0))).unwrap();
        let Encoded::Packet(p) = enc.pull().unwrap() else {
            panic!("idr packet");
        };

        let rec = crate::nal::AvcConfigRecord::parse(enc.extradata()).unwrap();
        assert_eq!(rec.profile_indication, 66);
        assert_eq!(
            rec.level_indication,
            level_for(96, 64, Rational::RATE_25).unwrap()
        );
        assert_eq!(rec.length_size, 4);

        let mut params = CodecParameters::new(CodecId::H264, MediaType::Video);
        params.extradata = Some(enc.extradata().to_vec());
        let mut dec = H264Decoder::new(&params).unwrap();
        dec.feed(&p).unwrap();
        dec.flush();
        let Ok(Decoded::Frame(Frame::Video(f))) = dec.pull() else {
            panic!("expected frame");
        };
        assert!(matches!(dec.pull(), Ok(Decoded::Finished)));

        let recon = &enc.recon_frames()[0];
        let ls = f.layout().plane(0).stride;
        let cs = f.layout().plane(1).stride;
        for y in 0..64usize {
            for x in 0..96usize {
                assert_eq!(f.plane(0)[y * ls + x], recon.luma[y * 96 + x], "({x},{y})");
            }
        }
        for y in 0..32usize {
            for x in 0..48usize {
                assert_eq!(f.plane(1)[y * cs + x], recon.cb[y * 48 + x], "cb ({x},{y})");
                assert_eq!(f.plane(2)[y * cs + x], recon.cr[y * 48 + x], "cr ({x},{y})");
            }
        }
    }

    /// 简单 CRF：可构造、逐帧 QP 落在 1..=51、qp_for 活动度单调
    /// （平滑帧不加档、强纹理帧加档）；恒定 QP 模式直取。
    #[test]
    fn crf_mode_maps_deterministically() {
        let mut cfg = base_config();
        cfg.video.as_mut().unwrap().quality = QualityMode::Crf(24);
        let enc = H264Encoder::new(&cfg).unwrap();

        // 平滑帧（纯色）：活动度 0 → ΔQP 0
        let mut flat = VideoFrame::new(
            PixelFormat::Yuv420p,
            96,
            64,
            ColorDesc::UNSPECIFIED,
            None,
            None,
        )
        .unwrap();
        for v in flat.plane_mut(0).iter_mut() {
            *v = 128;
        }
        assert_eq!(enc.qp_for(flat.plane(0), flat.layout().plane(0).stride), 24);

        // 强纹理帧（棋盘 ±32）：活动度 64 → ΔQP 4（crf 24 → 28）
        let mut busy = flat.clone();
        let ls = busy.layout().plane(0).stride;
        for y in 0..64usize {
            for x in 0..96usize {
                busy.plane_mut(0)[y * ls + x] = if (x + y) % 2 == 0 { 96 } else { 160 };
            }
        }
        assert_eq!(enc.qp_for(busy.plane(0), ls), 28);

        // CRF 边界：crf 50 + ΔQP 4 → 钳到 51
        let mut cfg_edge = cfg.clone();
        cfg_edge.video.as_mut().unwrap().quality = QualityMode::Crf(50);
        let enc_edge = H264Encoder::new(&cfg_edge).unwrap();
        assert_eq!(enc_edge.qp_for(busy.plane(0), ls), 51);

        // 恒定 QP：无视内容直取
        let mut cfg_qp = cfg;
        cfg_qp.video.as_mut().unwrap().quality = QualityMode::Qp(33);
        let enc_qp = H264Encoder::new(&cfg_qp).unwrap();
        assert_eq!(enc_qp.qp_for(busy.plane(0), ls), 33);
        assert_eq!(
            enc_qp.qp_for(flat.plane(0), flat.layout().plane(0).stride),
            33
        );
    }
}
