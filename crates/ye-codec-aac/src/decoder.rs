//! `ye_codec::Decoder` 实现：AAC-LC → packed f32 采样帧。
//!
//! 数据面：MP4 esds extradata（AudioSpecificConfig）/ ADTS / LOAS 三种
//! 入口 → raw data block 解析（[`crate::syntax`]）→ 解谱重量化
//! （[`crate::spectral`]）→ 立体声工具（[`crate::stereo`]）→ TNS
//! （[`crate::tns`]）→ 滤波器组合成（[`crate::filterbank::synth_frame`]
//! ——按其模块文档契约消费，纯函数 + 调用方持 tail 链）→
//! packed F32 [`AudioFrame`]。
//!
//! 状态机（trait 契约）：feed 推包；flush 排空后 feed →
//! [`CodecError::InvalidState`]；pull 按 [`Decoded`] 三态返回。AAC 无
//! 重排：feed 一包即产帧；帧 `pts` = 该包 `pts` 透传（一包多帧时首帧
//! 挂包 pts、后续帧 `None`——裸流本就无时戳，容器场景一包一帧）。
//!
//! ## 输出标定（对齐 oracle FFmpeg）
//!
//! [`crate::filterbank`] 按 ISO 标定 IMDCT（×2/M，自洽的完美重构）；
//! FFmpeg 解码器的 IMDCT 有效标定为 ×1/M（`ff_mdct_init` 把
//! `sqrt(1.0/1024)` 折进旋转因子两次，净 ×1/M）——即整体比 ISO 小
//! 一半。绝对输出幅度属解码器约定，非位流语义；对拍容差以 FFmpeg
//! PCM 为 oracle，故在调用侧把每帧样本 ×0.5
//!（[`OUTPUT_CAL`]），滤波器组文件保持 ISO 标定不动。
//!
//! 形态收紧（如实报错，绝不静默错帧）：HE-AAC/SBR 扩展、
//! PCE/CCE/多声道（>2）、非 LC object type、960 帧长（swb 表已转录但
//! 未对拍验证——收紧，放开须补 960 向量）、流中途变更采样率/声道配置。
//!
//! ## esds 消费
//!
//! MP4 的 `extradata` 为 esds box 载荷（4 字节版本/标志已由容器层剥去
//! 或保留——两种形态都识别：首字节 0x40 走描述子链，否则按裸 ASC）。
//! 描述子链：ES_Descriptor(0x40) → DecoderConfigDescriptor(0x04，
//! objectTypeIndication 须 0x40) → DecoderSpecificInfo(0x05 = ASC 位串)。

use std::collections::VecDeque;

use ye_codec::{AudioFrame, CodecError, CodecId, CodecParameters, Decoded, Decoder, Packet};
use ye_pixfmt::{ChannelLayout, SampleFormat};
use ye_time::Timestamp;

use crate::error::AacError;
use crate::filterbank::{self, BlockSequence, FrameLength, WindowInfo, WindowShape};
use crate::stereo;
use crate::syntax::{
    self, AudioSpecificConfig, BitReader, EIGHT_SHORT, LONG_START, LoasState, ONLY_LONG,
    PairCommon, RawElement,
};
use crate::tns;

/// 输出标定系数：滤波器组 ISO 标定（IMDCT ×2/M）→ FFmpeg 标定
///（×1/M，见模块文档「输出标定」）→ 归一化 f32（÷2^15）。
/// 合成链原生样本为源量化域（s16 量级）；本解码器统一输出归一化
/// f32（±1），消费方 `×32768` 即回到 s16。
const OUTPUT_CAL: f32 = 0.5 / 32768.0;

/// AAC-LC 解码器。
pub struct AacDecoder {
    /// 流配置（extradata 带外时构造即定；ADTS/LOAS 首帧也可定）。
    config: Option<AudioSpecificConfig>,
    /// 逐声道滤波器组 tail 链（首帧空）。
    synth_tails: Vec<Vec<f64>>,
    /// 逐声道上一帧 window_shape 位（true = KBD；None = 首帧，按 sine）。
    prev_shape: Vec<bool>,
    /// LOAS 流状态。
    loas: LoasState,
    /// 已解码待输出帧。
    out: VecDeque<AudioFrame>,
    /// feed 期累积失败（pull 透出）。
    pending_error: Option<CodecError>,
    flushed: bool,
    noise_state: u32,
}

impl AacDecoder {
    /// 由带外参数构造：消费 esds extradata（MP4）或裸 ASC；
    /// 无 extradata 时留待首帧（ADTS/LOAS 带内配置）。
    ///
    /// # Errors
    /// extradata 存在但空/不可解析 → [`CodecError::InvalidData`]；
    /// 形态不支持 → [`CodecError::UnsupportedCodec`]（文案含原因）。
    pub fn new(params: &CodecParameters) -> Result<AacDecoder, CodecError> {
        let mut dec = AacDecoder {
            config: None,
            synth_tails: Vec::new(),
            prev_shape: Vec::new(),
            loas: LoasState::default(),
            out: VecDeque::new(),
            pending_error: None,
            flushed: false,
            noise_state: 0x6d2b79f5,
        };
        if let Some(ed) = params.extradata.as_deref() {
            if ed.is_empty() {
                return Err(CodecError::InvalidData(
                    "aac extradata present but empty".into(),
                ));
            }
            let asc = if ed.len() >= 2
                && (ed[0] == 0x03 || ed[0] == 0x05 || (ed.len() > 4 && ed[0] == 0))
            {
                // 描述子链（ES_Descriptor 0x03 / 裸 DSI 0x05 / 带版本
                // 标志前缀的 full-box 形态）——走 esds
                Self::asc_from_esds(ed).map_err(aac_err)?
            } else {
                // 裸 ASC；不足 3 字节补零对齐（ASC 最长 17 位，ffmpeg
                // 实务中存在 2 字节无填充形式——如 0x12 0x10）
                let mut buf = ed.to_vec();
                buf.resize(3, 0);
                let mut r = BitReader::new(&buf);
                AudioSpecificConfig::parse(&mut r).map_err(aac_err)?
            };
            dec.accept_config(asc).map_err(aac_err)?;
        }
        Ok(dec)
    }

    /// esds 载荷 → ASC（描述子链；容忍 4 字节版本/标志前缀）。
    fn asc_from_esds(ed: &[u8]) -> Result<AudioSpecificConfig, AacError> {
        let mut p = 0usize;
        if ed.len() > 4 && ed[0] == 0 && ed[1] == 0 && ed[2] == 0 && ed[3] == 0 {
            p = 4; // 版本/标志（full box 头，MP4 容器层保留形态）
        }
        if ed.get(p) == Some(&0x05) {
            // 裸 DecoderSpecificInfo：tag + 长度 + ASC
            p += 1;
            let payload = read_extensible_len(ed, &mut p)?;
            let asc_end = (p + payload).min(ed.len());
            return Self::asc_from_dsi(ed.get(p..asc_end).unwrap_or(&[]));
        }
        if ed.get(p) != Some(&0x03) {
            return Err(AacError::InvalidData(format!(
                "esds 首描述子 tag={:#04x}（非 ES_Descriptor 0x03）",
                ed[p]
            )));
        }
        p += 1;
        let payload = read_extensible_len(ed, &mut p)?;
        let end = (p + payload).min(ed.len());
        if p + 3 > end {
            return Err(AacError::InvalidData("esds ES_Descriptor 体截断".into()));
        }
        p += 2; // ES_ID
        let flags = ed[p];
        p += 1;
        if flags & 0x80 != 0 {
            p += 2; // dependsOn_ES_ID
        }
        if flags & 0x40 != 0 {
            let url_len = *ed.get(p).unwrap_or(&0) as usize;
            p += 1 + url_len;
        }
        if flags & 0x20 != 0 {
            p += 2; // OCR_ES_ID
        }
        if p >= end {
            return Err(AacError::InvalidData("esds 缺 DecoderConfig".into()));
        }
        if ed[p] != 0x04 {
            return Err(AacError::InvalidData(format!(
                "esds 子描述子 tag={:#04x}（非 DecoderConfig 0x04）",
                ed[p]
            )));
        }
        p += 1;
        let payload = read_extensible_len(ed, &mut p)?;
        let end2 = (p + payload).min(end);
        if p + 13 > end2 {
            return Err(AacError::InvalidData("esds DecoderConfig 体截断".into()));
        }
        let oti = ed[p];
        if oti != 0x40 {
            return Err(AacError::Unsupported(format!(
                "esds objectTypeIndication={oti:#04x}（非 MPEG-4 Audio 0x40）"
            )));
        }
        p += 13;
        if p >= end2 || ed[p] != 0x05 {
            return Err(AacError::InvalidData(
                "esds 缺 DecoderSpecificInfo（0x05）".into(),
            ));
        }
        p += 1;
        let payload = read_extensible_len(ed, &mut p)?;
        let asc_end = (p + payload).min(end2);
        let dsi = ed
            .get(p..asc_end)
            .ok_or_else(|| AacError::InvalidData("esds DSI 范围越界".into()))?;
        Self::asc_from_dsi(dsi)
    }

    /// DSI 字节 → ASC（不足 3 字节补零对齐——ASC 最长 17 位，ffmpeg
    /// 实务中存在 2 字节无填充形式）。
    fn asc_from_dsi(dsi: &[u8]) -> Result<AudioSpecificConfig, AacError> {
        if dsi.is_empty() {
            return Err(AacError::InvalidData("esds DSI 为空".into()));
        }
        let mut buf = dsi.to_vec();
        buf.resize(3, 0);
        let mut r = BitReader::new(&buf);
        AudioSpecificConfig::parse(&mut r)
    }

    /// 采纳一份 ASC（首份或与既有一致校验）。
    fn accept_config(&mut self, asc: AudioSpecificConfig) -> Result<(), AacError> {
        if asc.object_type != 2 {
            return Err(AacError::Unsupported(format!(
                "objectType={}（仅 AAC-LC=2）",
                asc.object_type
            )));
        }
        if asc.frame_length_short {
            return Err(AacError::Unsupported(
                "frameLengthFlag=1（960 帧长：swb 表已备、未对拍验证，收紧）".into(),
            ));
        }
        if let Some(prev) = &self.config
            && (prev.sampling_index != asc.sampling_index
                || prev.channel_config != asc.channel_config)
        {
            return Err(AacError::Unsupported("流中途变更采样率/声道配置".into()));
        }
        if self.config.is_none() {
            let channels = if asc.channel_config == 1 { 1 } else { 2 };
            self.synth_tails = vec![Vec::new(); channels];
            self.prev_shape = vec![false; channels]; // 首帧 prev 按 sine（对齐 ffmpeg 初值）
            self.config = Some(asc);
        }
        Ok(())
    }

    /// 解一个传输包（ADTS / LOAS / 裸 raw block；ADTS 一包可含多帧）。
    fn decode_packet(&mut self, pkt: &Packet) -> Result<usize, AacError> {
        let data = pkt.data.as_ref();
        let mut produced = 0usize;
        let mut pts = pkt.pts;
        if syntax::is_adts(data) {
            let mut off = 0usize;
            while off < data.len() && syntax::is_adts(&data[off..]) {
                let hdr = syntax::parse_adts(&data[off..])?;
                let asc = AudioSpecificConfig {
                    object_type: hdr.object_type,
                    sampling_index: hdr.sampling_index,
                    sample_rate: crate::tables::SAMPLE_RATES[hdr.sampling_index as usize],
                    channel_config: hdr.channel_config,
                    frame_length_short: false,
                };
                if self.config.is_none() {
                    self.accept_config(asc)?;
                } else {
                    // 同流一致性校验（幂等采纳：配置变更会被拒）
                    self.accept_config(asc)?;
                }
                let block = &data[off + hdr.header_length..off + hdr.frame_length];
                self.decode_frame_at(block, 0, pts.take())?;
                produced += 1;
                off += hdr.frame_length;
            }
            if off < data.len() && produced == 0 {
                return Err(AacError::InvalidData(format!(
                    "ADTS 尾部残余 {} 字节无法成帧",
                    data.len() - off
                )));
            }
        } else if syntax::is_loas(data) {
            let f = self.loas.parse_frame(data)?;
            if let Some(cfg) = self.loas.config.clone() {
                // LOAS 带内 ASC（语法层已校验为 LC）；幂等采纳
                self.accept_config(cfg)?;
            }
            let block = &f.payload[..(f.bit_offset + f.payload_bits).div_ceil(8)];
            self.decode_frame_at(block, f.bit_offset, pts.take())?;
            produced += 1;
        } else {
            if self.config.is_none() {
                return Err(AacError::InvalidData(
                    "裸 raw data block 但无 AudioSpecificConfig（缺 extradata）".into(),
                ));
            }
            self.decode_frame_at(data, 0, pts.take())?;
            produced += 1;
        }
        Ok(produced)
    }

    /// 解一个 raw data block → 立体声工具 → TNS → 滤波器组 → 输出帧。
    fn decode_frame_at(
        &mut self,
        block: &[u8],
        bit_offset: usize,
        pts: Option<Timestamp>,
    ) -> Result<(), AacError> {
        let asc = self
            .config
            .clone()
            .ok_or_else(|| AacError::InvalidData("无 ASC".into()))?;
        // 逐声道 prev 窗形快照（首声道 = 元素 0 / CPE ch0，次 = CPE ch1）
        let prev_shapes: Vec<bool> = self.prev_shape.clone();
        let rdb = syntax::parse_raw_data_block_at(block, bit_offset, &asc)?;

        // 找出唯一的音频元素（SCE / CPE / LFE-as-SCE 不混叠）
        let mut channels_out: Vec<Vec<f32>> = Vec::new();
        let mut tails: Vec<Vec<f64>> = Vec::new();
        let mut new_shapes: Vec<bool> = Vec::new();
        let frame_len = if asc.frame_length_short {
            FrameLength::N960
        } else {
            FrameLength::N1024
        };
        let expect_channels = if asc.channel_config == 1 { 1 } else { 2 };

        for elem in &rdb.elements {
            match elem {
                RawElement::Single(ch) => {
                    if channels_out.len() >= expect_channels {
                        return Err(AacError::Unsupported(
                            "多音频元素（双单声道等超出 channelConfiguration 语义）".into(),
                        ));
                    }
                    let ci = channels_out.len();
                    let mut ch = ch.clone();
                    crate::pns::generate(&mut ch, &mut self.noise_state);
                    if let Some(tns_data) = ch.tns.take() {
                        tns::apply(&mut ch.coeffs, &ch.ics, &tns_data);
                    }
                    let win = window_info(&ch.ics, prev_shapes.get(ci).copied().unwrap_or(false));
                    let tail_in = self.synth_tails.get(ci).cloned().unwrap_or_default();
                    let out = filterbank::synth_frame(&ch.coeffs, &win, frame_len, &tail_in)
                        .map_err(AacError::Filterbank)?;
                    channels_out.push(calibrate(out.samples));
                    tails.push(out.tail);
                    new_shapes.push(ch.ics.window_shape);
                }
                RawElement::Pair { common, ch } => {
                    let (mut c0, mut c1) = (ch[0].clone(), ch[1].clone());
                    let default_common = PairCommon {
                        ms_present: 0,
                        ms_used: Vec::new(),
                    };
                    let pc = common.as_ref().unwrap_or(&default_common);
                    crate::pns::generate(&mut c0, &mut self.noise_state);
                    crate::pns::generate(&mut c1, &mut self.noise_state);
                    if let Some(shared) = common {
                        crate::pns::correlate(shared, &c0, &mut c1);
                    }
                    stereo::apply(pc, &mut c0, &mut c1)?;
                    if let Some(t) = c0.tns.take() {
                        tns::apply(&mut c0.coeffs, &c0.ics, &t);
                    }
                    if let Some(t) = c1.tns.take() {
                        tns::apply(&mut c1.coeffs, &c1.ics, &t);
                    }
                    let (p0, p1) = (
                        prev_shapes.first().copied().unwrap_or(false),
                        prev_shapes.get(1).copied().unwrap_or(false),
                    );
                    let (o0, o1) = (
                        filterbank::synth_frame(
                            &c0.coeffs,
                            &window_info(&c0.ics, p0),
                            frame_len,
                            self.synth_tails
                                .first()
                                .cloned()
                                .unwrap_or_default()
                                .as_slice(),
                        )
                        .map_err(AacError::Filterbank)?,
                        filterbank::synth_frame(
                            &c1.coeffs,
                            &window_info(&c1.ics, p1),
                            frame_len,
                            self.synth_tails
                                .get(1)
                                .cloned()
                                .unwrap_or_default()
                                .as_slice(),
                        )
                        .map_err(AacError::Filterbank)?,
                    );
                    channels_out.push(calibrate(o0.samples));
                    channels_out.push(calibrate(o1.samples));
                    tails.push(o0.tail);
                    tails.push(o1.tail);
                    new_shapes.push(c0.ics.window_shape);
                    new_shapes.push(c1.ics.window_shape);
                }
                RawElement::DataStream | RawElement::Fill => {}
            }
        }
        if channels_out.is_empty() {
            return Err(AacError::InvalidData("raw data block 无音频元素".into()));
        }
        if channels_out.len() != expect_channels {
            return Err(AacError::InvalidData(format!(
                "元素声道数 {} ≠ channelConfiguration 声道数 {expect_channels}",
                channels_out.len()
            )));
        }

        // 组 packed f32 交织帧
        let layout = if expect_channels == 1 {
            ChannelLayout::MONO
        } else {
            ChannelLayout::STEREO
        };
        let frames = channels_out[0].len();
        let mut interleaved = Vec::with_capacity(frames * expect_channels);
        if expect_channels == 1 {
            interleaved.extend_from_slice(&channels_out[0]);
        } else {
            for (l, r) in channels_out[0].iter().zip(channels_out[1].iter()) {
                interleaved.push(*l);
                interleaved.push(*r);
            }
        }
        let mut frame = AudioFrame::new(SampleFormat::F32, layout, asc.sample_rate, frames, pts)
            .map_err(|e| AacError::InvalidArguments(e.to_string()))?;
        frame
            .data_mut()
            .copy_from_slice(&bytemuck_f32_as_u8(&interleaved));
        self.out.push_back(frame);
        self.synth_tails = tails;
        self.prev_shape = new_shapes;
        Ok(())
    }
}

/// ICS → 滤波器组窗信息（shape_prev = 上一帧 window_shape 位）。
fn window_info(ics: &crate::syntax::IcsInfo, prev_shape_bit: bool) -> WindowInfo {
    WindowInfo {
        sequence: match ics.window_sequence {
            ONLY_LONG => BlockSequence::OnlyLong,
            LONG_START => BlockSequence::LongStart,
            EIGHT_SHORT => BlockSequence::EightShort,
            _ => BlockSequence::LongStop,
        },
        shape_prev: shape_of(prev_shape_bit),
        shape_curr: shape_of(ics.window_shape),
    }
}

/// window_shape 位 → 窗族（对齐 FFmpeg：位 1 = KBD，位 0 = sine）。
fn shape_of(bit: bool) -> WindowShape {
    if bit {
        WindowShape::Kbd
    } else {
        WindowShape::Sine
    }
}

/// 滤波器组输出 → oracle 标定（ISO ×2/M → FFmpeg ×1/M，见 [`OUTPUT_CAL`]）。
fn calibrate(mut samples: Vec<f32>) -> Vec<f32> {
    for s in samples.iter_mut() {
        *s *= OUTPUT_CAL;
    }
    samples
}

/// f32 → u8 视图拷贝（零 unsafe：逐字节组装）。
fn bytemuck_f32_as_u8(src: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() * 4);
    for v in src {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out
}

/// 扩展长度字段（0x80 续位，至多 4 字节）。
fn read_extensible_len(ed: &[u8], p: &mut usize) -> Result<usize, AacError> {
    let mut len = 0usize;
    for _ in 0..4 {
        let b = *ed
            .get(*p)
            .ok_or_else(|| AacError::InvalidData("esds 长度字段截断".into()))?;
        *p += 1;
        len = (len << 7) | usize::from(b & 0x7F);
        if b & 0x80 == 0 {
            return Ok(len);
        }
    }
    Err(AacError::InvalidData("esds 长度字段超 4 字节".into()))
}

/// AacError → CodecError 映射。
fn aac_err(err: AacError) -> CodecError {
    match err {
        AacError::Unsupported(msg) => {
            CodecError::InvalidData(format!("unsupported aac stream: {msg}"))
        }
        AacError::Filterbank(e) => CodecError::InvalidData(format!("aac filterbank: {e}")),
        other => CodecError::InvalidData(other.to_string()),
    }
}

impl Decoder for AacDecoder {
    fn codec_id(&self) -> CodecId {
        CodecId::Aac
    }

    fn feed(&mut self, packet: &Packet) -> Result<(), CodecError> {
        if self.flushed {
            return Err(CodecError::InvalidState(
                "aac decoder already finished (flushed)",
            ));
        }
        // 解码失败即返回（不吞错）；已产帧保留在队列，pull 仍可取。
        match self.decode_packet(packet) {
            Ok(_) => Ok(()),
            Err(e) => Err(aac_err(e)),
        }
    }

    fn flush(&mut self) {
        // AAC 无重排：排空输出队列即可。滤波器组 tail 为末帧溢出样本，
        // ffmpeg 侧同样不输出（对拍长度 = 帧数×1024 一致）。
        self.flushed = true;
    }

    fn pull(&mut self) -> Result<Decoded, CodecError> {
        if let Some(err) = self.pending_error.take() {
            return Err(err);
        }
        if let Some(frame) = self.out.pop_front() {
            return Ok(Decoded::Frame(ye_codec::Frame::Audio(frame)));
        }
        if self.flushed {
            Ok(Decoded::Finished)
        } else {
            Ok(Decoded::NeedInput)
        }
    }
}

/// 解码器工厂（签名 = [`ye_codec::DecoderFactory`]）。
pub fn decoder_factory(params: &CodecParameters) -> Result<Box<dyn Decoder>, CodecError> {
    Ok(Box::new(AacDecoder::new(params)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use ye_time::Rational;

    #[test]
    fn independent_cpe_band_counts_decode_without_stereo_indexing() {
        let mut bits = Vec::new();
        let mut write = |value: u32, len: u32| {
            for n in (0..len).rev() {
                bits.push(((value >> n) & 1) as u8);
            }
        };
        write(1, 3); // CPE
        write(0, 4); // element tag
        write(0, 1); // common_window = false
        for max_sfb in [4, 2] {
            write(100, 8); // global gain
            write(0, 1);
            write(0, 2);
            write(0, 1); // reserved, ONLY_LONG, shape
            write(max_sfb, 6);
            write(0, 1); // max_sfb, predictor absent
            write(0, 4);
            write(max_sfb, 5); // one ZERO_HCB section
            write(0, 1);
            write(0, 1);
            write(0, 1); // no pulse, TNS, gain control
        }
        write(7, 3); // END
        let mut bytes = vec![0u8; bits.len().div_ceil(8)];
        for (n, bit) in bits.into_iter().enumerate() {
            bytes[n / 8] |= bit << (7 - n % 8);
        }
        let mut params = CodecParameters::new(CodecId::Aac, ye_codec::MediaType::Audio);
        params.extradata = Some(vec![0x12, 0x10]);
        let mut decoder = AacDecoder::new(&params).unwrap();
        decoder.feed(&packet(&bytes, Some(0))).unwrap();
        let Decoded::Frame(ye_codec::Frame::Audio(frame)) = decoder.pull().unwrap() else {
            panic!("expected PCM");
        };
        assert_eq!(frame.data().len(), 1024 * 2 * 4);
        assert!(frame.data().iter().all(|byte| *byte == 0));
    }

    fn packet(data: &[u8], pts_ticks: Option<i64>) -> Packet {
        let mut p = Packet::empty(0);
        p.data = Arc::from(data.to_vec());
        p.pts = pts_ticks.map(|t| Timestamp::new(t, Rational::new(1, 44_100)));
        p
    }

    /// 状态机：裸无配置包 → InvalidData；flush 后 feed → InvalidState；
    /// pull 排空与 Finished 幂等。
    #[test]
    fn state_machine_contract() {
        let mut dec = AacDecoder::new(&CodecParameters::new(
            CodecId::Aac,
            ye_codec::MediaType::Audio,
        ))
        .unwrap();
        assert_eq!(dec.codec_id(), CodecId::Aac);
        // 无配置裸块 → InvalidData
        assert!(matches!(
            dec.feed(&packet(&[0x00, 0x00], None)),
            Err(CodecError::InvalidData(_))
        ));
        assert_eq!(dec.pull().unwrap(), Decoded::NeedInput);
        dec.flush();
        assert_eq!(dec.pull().unwrap(), Decoded::Finished);
        assert_eq!(dec.pull().unwrap(), Decoded::Finished, "Finished 幂等");
        assert!(matches!(
            dec.feed(&packet(&[], None)),
            Err(CodecError::InvalidState(_))
        ));
    }

    /// esds 解析：真实 mp4 形态（版本/标志 + 0x03 描述子链，DSI ASC=
    /// 0x1210 = LC/44.1k/立体声）。
    #[test]
    fn esds_descriptor_chain() {
        // ES_Descriptor: tag 0x03 len 0x19: ES_ID 0x0100 flags 0x00
        //   DCD: tag 0x04 len 0x15: OTI 0x40 + 13 字节 + DSI
        //   DSI: tag 0x05 len 2: 0x12 0x10
        let mut esds = vec![0x00, 0x00, 0x00, 0x00, 0x03, 0x19, 0x01, 0x00, 0x00];
        let mut dcd = vec![0x04, 0x15, 0x40];
        dcd.extend_from_slice(&[0x15, 0, 0, 0, 0, 1, 0xf4, 0, 0, 1, 0xf2, 0x8c]); // 13-1=12 字节余体
        dcd.extend_from_slice(&[0x05, 0x02, 0x12, 0x10]);
        esds.extend_from_slice(&dcd);
        let params = {
            let mut p = CodecParameters::new(CodecId::Aac, ye_codec::MediaType::Audio);
            p.extradata = Some(esds);
            p
        };
        let dec = AacDecoder::new(&params).unwrap();
        let cfg = dec.config.unwrap();
        assert_eq!(
            (cfg.object_type, cfg.sample_rate, cfg.channel_config),
            (2, 44_100, 2)
        );
        // 非法 OTI
        let mut bad = vec![0x00, 0x00, 0x00, 0x00, 0x03, 0x19, 0x01, 0x00, 0x00];
        let mut dcd = vec![0x04, 0x15, 0x66];
        dcd.extend_from_slice(&[0u8; 12]);
        dcd.extend_from_slice(&[0x05, 0x02, 0x12, 0x10]);
        bad.extend_from_slice(&dcd);
        let mut p = CodecParameters::new(CodecId::Aac, ye_codec::MediaType::Audio);
        p.extradata = Some(bad);
        assert!(matches!(
            AacDecoder::new(&p),
            Err(CodecError::InvalidData(_))
        ));
    }

    /// 裸 ASC（0x12 0x10 = LC/44.1k/立体声）。
    #[test]
    fn bare_asc_extradata() {
        let mut p = CodecParameters::new(CodecId::Aac, ye_codec::MediaType::Audio);
        p.extradata = Some(vec![0x12, 0x10]); // LC/44.1k/立体声
        let dec = AacDecoder::new(&p).unwrap();
        assert_eq!(dec.config.unwrap().sample_rate, 44_100);
        // 空 extradata → InvalidData
        let mut p2 = CodecParameters::new(CodecId::Aac, ye_codec::MediaType::Audio);
        p2.extradata = Some(vec![]);
        assert!(matches!(
            AacDecoder::new(&p2),
            Err(CodecError::InvalidData(_))
        ));
    }

    /// 工厂可构造。
    #[test]
    fn factory_creates_decoder() {
        let params = CodecParameters::new(CodecId::Aac, ye_codec::MediaType::Audio);
        let dec = decoder_factory(&params).unwrap();
        assert_eq!(dec.codec_id(), CodecId::Aac);
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<AacDecoder>();
    }
}
