//! `ye_codec::Encoder` 实现：packed f32 → AAC-LC（长窗、ADTS 包体）。
//!
//! ## 输入/输出契约
//!
//! - **输入**：packed 交错 f32 [`AudioFrame`]（`SampleFormat::F32`），
//!   采样率须在 [`crate::tables::SAMPLE_RATES`] 13 档内（写 ASC 的
//!   frequencyIndex），声道 1（SCE）/ 2（CPE，`common_window=1`、
//!   `ms_present=0`，两声道各自独立编码——FFmpeg 对
//!   channelConfiguration=2 期望首元素是 CPE）。帧长任意：内部按
//!   1024 样本组帧，不足一帧的尾样在 flush 时补零成包。TDAC 长窗
//!   需要 2048 样本分析块而每帧只交付前 1024（块 = 交付帧 ∥ 下一
//!   帧），故**首帧输入先入前视缓冲**：1024 帧长输入自第二帧起
//!   feed 一帧出一包，flush 收口尾包（交付区对齐 1024 上取整），
//!   包 pts 与其交付样本严格对齐、无重排。
//! - **输出**：ADTS 包装的 [`Packet`]（7 字节头 + raw data block），
//!   一包 = 1024 样本；`pts = 帧号×1024/采样率` 重标度到
//!   `EncoderConfig.time_base`（对齐 ye-codec-h264 的打戳口径，
//!   **输入帧 pts 被忽略**），`duration` = 1024 样本时长，
//!   `stream_index = 0`，全部包带 [`PacketFlags::KEYFRAME`]
//!   （AAC 帧独立可解）。码率控制：`bit_rate`（bit/s）是唯一质量
//!   旋钮（必填），每帧预算 `bit_rate×1024/采样率` 位，超预算由
//!   [`element`] 的码率环整帧上移 sf 收敛，低于预算由 FIL 填充补齐
//!   （恒定码率语义）。
//!
//! ## 分析链（前向 MDCT）
//!
//! 每帧分析块 = 本帧交付样本 ∥ 下一帧样本（50% 重叠，尾帧补零），
//! × 65536 归一到解码侧系数域（解码器输出标定 `×1/65536` 的逆），
//! 加 sine 窗（与解码器同源：[`crate::filterbank::sine_table`]）后
//! 走共享前向核 [`crate::filterbank::mdct`]（朴素 O(N²) f64 直角
//! 三角函数累加，本轮不做 FFT 优化——窗系数与变换核同源保证 TDAC
//! 精确重构；合成侧时间对齐 = 解码器逐帧交付，实测对拍零延迟）。
//!
//! ## 带外参数
//!
//! - [`AacEncoder::audio_specific_config`]：ASC（AOT=2、freqIndex、
//!   channelConfig、GASpecificConfig 全 0）；
//! - [`AacEncoder::esds_extradata`]：完整 esds 载荷——4 字节版本/标志、
//!   ES_Descriptor 0x03 → DecoderConfig 0x04（OTI 0x40、streamType 5）→
//!   DecoderSpecificInfo 0x05 = ASC → SLConfigDescriptor 0x06
//!   （predefined=2）；
//! - 自由函数 [`esds_extradata`]：按采样率/声道数直接构造 esds
//!   （convert 组 MP4 音频模板用，不经 Encoder trait 回读——与视频侧
//!   avcC 由 convert 自建同款裁决）。
//!
//! ## 状态机（trait 契约）
//!
//! flush 后 pull 排空输出队列即 `Finished`（幂等），此后 feed →
//! `Err(InvalidState)`；未 flush 且队列空 → `NeedInput`。

mod bitwriter;
mod element;
mod psy;
mod quant;
mod section;

use std::collections::VecDeque;
use std::sync::Arc;

use ye_codec::{
    AudioFrame, CodecError, CodecId, Encoded, Encoder, EncoderConfig, Frame, Packet, PacketFlags,
};
use ye_pixfmt::{ChannelLayout, SampleFormat};
use ye_time::{Rational, Timestamp};

use crate::error::AacError;
use crate::filterbank;
use crate::tables::{self, SAMPLE_RATES};

/// 编码输入 → 解码侧系数域的标定：解码器输出 = ISO 合成 × 1/65536
/// （见解码模块「输出标定」），编码侧先乘 65536 使两向互逆、FFmpeg
/// 解码幅度 ≈ 输入幅度。
const INPUT_CAL: f64 = 65_536.0;

/// 帧长（样本/系数）。
const FRAME_LEN: usize = 1024;

/// AAC-LC 编码器（[`Encoder`] 实现，工厂见 [`encoder_factory`]）。
///
/// 输入/输出契约与带外参数见模块文档；配置校验在构造（真工厂交付
/// 即校验：codec/格式/采样率档位/声道布局/码率必填且 >0）。
pub struct AacEncoder {
    sample_rate: u32,
    freq_index: u8,
    channels: u8,
    layout: ChannelLayout,
    /// 含 ADTS 头折算后的每帧载荷目标字节数（FIL 填充锚点）。
    payload_target: usize,
    time_base: Rational,
    swb_offset: &'static [u16],
    num_swb: usize,
    /// 逐声道前视缓冲（系数域 f64）：TDAC 长窗需 2048 样本块而每帧
    /// 只交付前 1024——块 = 交付帧 ∥ 下一帧（lookahead 1 帧）。
    carry: Vec<Vec<f64>>,
    out: VecDeque<Packet>,
    frame_index: u64,
    flushed: bool,
}

impl AacEncoder {
    /// 由 [`EncoderConfig`] 构造（工厂的实体入口）。
    ///
    /// # Errors
    /// 配置非法（codec ≠ aac / 缺音频参数 / 采样格式非 packed f32 /
    /// 采样率不在 13 档 / 声道非 1|2 / `bit_rate` 缺失或为 0 / 时基
    /// 非正 / 每帧预算超 ADTS 13 位帧长）→
    /// [`CodecError::InvalidArguments`]。
    pub fn new(config: &EncoderConfig) -> Result<AacEncoder, CodecError> {
        if config.codec != CodecId::Aac {
            return Err(CodecError::InvalidArguments(
                "aac encoder requires codec = aac",
            ));
        }
        let Some(audio) = &config.audio else {
            return Err(CodecError::InvalidArguments(
                "aac encoder requires audio config",
            ));
        };
        if audio.format != SampleFormat::F32 {
            return Err(CodecError::InvalidArguments(
                "aac encoder supports only packed f32 input",
            ));
        }
        let channels = audio.channel_layout.channels();
        if channels != 1 && channels != 2 {
            return Err(CodecError::InvalidArguments(
                "aac encoder supports only mono or stereo layouts",
            ));
        }
        let Some(freq_index) = SAMPLE_RATES.iter().position(|&r| r == audio.sample_rate) else {
            return Err(CodecError::InvalidArguments(
                "aac encoder sample rate not in standard table (13 rates)",
            ));
        };
        let Some(bit_rate) = config.bit_rate.filter(|&b| b > 0) else {
            return Err(CodecError::InvalidArguments(
                "aac encoder requires a positive bit_rate",
            ));
        };
        if config.time_base.num() <= 0 {
            return Err(CodecError::InvalidArguments(
                "aac encoder needs a positive time base",
            ));
        }
        let freq_index = freq_index as u8;
        let swb_offset = tables::swb_offset_long(freq_index as usize)
            .ok_or(CodecError::InvalidArguments("aac swb table missing"))?;
        let num_swb = tables::num_swb_long(freq_index as usize)
            .ok_or(CodecError::InvalidArguments("aac swb count missing"))?
            as usize;
        // 每帧预算（位）→ 目标帧字节数（含 7 字节 ADTS 头）
        let budget_bits = (bit_rate * FRAME_LEN as u64 / u64::from(audio.sample_rate)).max(128);
        let frame_bytes = budget_bits / 8;
        if frame_bytes > 8184 {
            return Err(CodecError::InvalidArguments(
                "aac encoder bit rate exceeds ADTS frame length limit (13 bit)",
            ));
        }
        let payload_target = frame_bytes.saturating_sub(7).max(1) as usize;
        Ok(AacEncoder {
            sample_rate: audio.sample_rate,
            freq_index,
            channels: channels as u8,
            layout: audio.channel_layout,
            payload_target,
            time_base: config.time_base,
            swb_offset,
            num_swb,
            carry: vec![Vec::new(); channels as usize],
            out: VecDeque::new(),
            frame_index: 0,
            flushed: false,
        })
    }

    /// 带外参数：AudioSpecificConfig（2 字节：AOT=2/freqIndex/
    /// channelConfig/GASpecificConfig 全 0 位）。
    #[must_use]
    pub fn audio_specific_config(&self) -> Vec<u8> {
        asc_bytes(self.freq_index, self.channels).to_vec()
    }

    /// 带外参数：完整 esds 载荷（形态见模块文档；`track_es_id` 写入
    /// ES_Descriptor 的 ES_ID，MP4 轨道号语义，无要求可传任意值）。
    #[must_use]
    pub fn esds_extradata(&self, track_es_id: u16) -> Vec<u8> {
        let avg = u32::try_from(self.payload_target_as_bitrate()).unwrap_or(0);
        build_esds(&asc_bytes(self.freq_index, self.channels), track_es_id, avg)
    }

    /// 逐帧目标码率（payload_target 反推，填 esds max/avg bitrate 用）。
    fn payload_target_as_bitrate(&self) -> u64 {
        u64::from((self.payload_target + 7) as u32) * 8 * u64::from(self.sample_rate)
            / FRAME_LEN as u64
    }

    /// 输入帧校验 + 样本入缓冲。
    fn accept_frame(&mut self, frame: &AudioFrame) -> Result<(), CodecError> {
        if frame.format() != SampleFormat::F32 {
            return Err(CodecError::InvalidArguments(
                "aac encoder supports only packed f32 input",
            ));
        }
        if frame.sample_rate() != self.sample_rate
            || frame.channel_layout().channels() != u32::from(self.channels)
            || *frame.channel_layout() != self.layout
        {
            return Err(CodecError::InvalidArguments(
                "frame layout/sample rate differ from encoder config",
            ));
        }
        if frame.frames() == 0 {
            return Err(CodecError::InvalidArguments(
                "aac encoder rejects empty audio frames",
            ));
        }
        let chs = self.channels as usize;
        for (c, buf) in self.carry.iter_mut().enumerate() {
            buf.extend((0..frame.frames()).map(|i| {
                let bytes = &frame.data()[(i * chs + c) * 4..(i * chs + c) * 4 + 4];
                f64::from(f32::from_le_bytes(bytes.try_into().expect("4 字节"))) * INPUT_CAL
            }));
        }
        Ok(())
    }

    /// 自前视缓冲编码一帧：块 = carry 前 2048 样本（不足补零——flush
    /// 尾帧），交付前 1024（块后半留作下帧前半）。返回是否成帧。
    fn encode_carry_frame(&mut self) -> Result<bool, CodecError> {
        let chs = self.channels as usize;
        if self.carry.iter().all(|c| c.is_empty()) {
            return Ok(false);
        }
        // 前向 MDCT：交付帧 ∥ 下一帧 → sine 窗 → 共享核
        let win = filterbank::sine_table(2 * FRAME_LEN)
            .map_err(AacError::Filterbank)
            .map_err(aac_err)?;
        let mut coeffs: Vec<Vec<f32>> = Vec::with_capacity(chs);
        for c in 0..chs {
            let mut block_w = vec![0.0f64; 2 * FRAME_LEN];
            let n = self.carry[c].len().min(2 * FRAME_LEN);
            block_w[..n].copy_from_slice(&self.carry[c][..n]);
            for (b, w) in block_w.iter_mut().zip(win.iter()) {
                *b *= f64::from(*w);
            }
            let co = filterbank::mdct(&block_w)
                .map_err(AacError::Filterbank)
                .map_err(aac_err)?;
            coeffs.push(co);
        }
        // 交付前半（后半留作下帧块的交付半）
        for buf in &mut self.carry {
            let delivered = buf.len().min(FRAME_LEN);
            buf.drain(..delivered);
        }

        // 码率环 + FIL 填充 → ADTS 包体
        let refs: Vec<&[f32]> = coeffs.iter().map(|c| c.as_slice()).collect();
        let (payload, _) =
            element::encode_frame(&refs, self.swb_offset, self.num_swb, self.payload_target);
        let frame = element::adts_wrap(self.freq_index, self.channels, &payload);

        // 打戳：帧号 × 1024 重标度到输出时基
        let ticks = i64::try_from(
            self.frame_index
                .checked_mul(FRAME_LEN as u64)
                .ok_or_else(|| CodecError::InvalidData("frame index overflow".into()))?,
        )
        .map_err(|_| CodecError::InvalidData("frame index exceeds i64".into()))?;
        let src_tb = Rational::new(1, self.sample_rate as i32);
        let pts = Timestamp::new(ticks, src_tb)
            .rescale(self.time_base)
            .map_err(|e| CodecError::InvalidData(e.to_string()))?;
        let duration = Timestamp::new(FRAME_LEN as i64, src_tb)
            .rescale(self.time_base)
            .map_err(|e| CodecError::InvalidData(e.to_string()))?;
        let mut packet = Packet::empty(0);
        packet.data = Arc::from(frame.into_boxed_slice());
        packet.pts = Some(pts);
        packet.dts = Some(pts);
        packet.duration = Some(duration);
        packet.flags.insert(PacketFlags::KEYFRAME);
        self.out.push_back(packet);
        self.frame_index += 1;
        Ok(true)
    }
}

impl Encoder for AacEncoder {
    fn codec_id(&self) -> CodecId {
        CodecId::Aac
    }

    fn feed(&mut self, frame: &Frame) -> Result<(), CodecError> {
        if self.flushed {
            return Err(CodecError::InvalidState(
                "encoder already flushed and drained",
            ));
        }
        let Frame::Audio(audio) = frame else {
            return Err(CodecError::InvalidArguments(
                "aac encoder accepts only audio frames",
            ));
        };
        self.accept_frame(audio)?;
        // TDAC 前视：块 = 交付帧 ∥ 下一帧——缓冲攒满 2048 才能成帧
        //（首帧输入先缓冲，feed 自第二帧起逐帧出包）。
        while self.carry.iter().any(|c| c.len() >= 2 * FRAME_LEN) {
            self.encode_carry_frame()?;
        }
        Ok(())
    }

    /// 排空：余下样本（≥ 1）逐帧补零成包（交付区对齐 1024 上取整；
    /// 至多两包——最后的全零交付半不再成帧）。
    fn flush(&mut self) {
        if !self.flushed {
            while self.carry.iter().any(|c| !c.is_empty()) {
                if self.encode_carry_frame().is_err() {
                    break; // 编码失败如实放弃余量（错误经 pull 队列外无通道）
                }
            }
        }
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

/// 工厂：消费 [`EncoderConfig`]（`CodecRegistry::create_encoder` 的
/// 注册目标，门面注册见 `ye::codec_aac::register_aac_encoder`）。
///
/// # Errors
/// 见 [`AacEncoder::new`]（真工厂交付即校验）。
pub fn encoder_factory(config: &EncoderConfig) -> Result<Box<dyn Encoder>, CodecError> {
    Ok(Box::new(AacEncoder::new(config)?))
}

/// ASC 位串（16 位 = 2 字节）：AOT=2、freqIndex、channelConfig、
/// GASpecificConfig{frameLengthFlag=0, dependsOnCoreCoder=0,
/// extensionFlag=0}。
fn asc_bytes(freq_index: u8, channel_config: u8) -> [u8; 2] {
    let mut w = bitwriter::BitWriter::new();
    w.put(2, 5); // audioObjectType = AAC-LC
    w.put(u32::from(freq_index), 4);
    w.put(u32::from(channel_config), 4);
    w.put(0, 1); // frameLengthFlag
    w.put(0, 1); // dependsOnCoreCoder
    w.put(0, 1); // extensionFlag
    let b = w.into_bytes();
    [b[0], b[1]]
}

/// 组装完整 esds 载荷（结构见 [`AacEncoder::esds_extradata`]；
/// 长度字段按内容计算，全 < 128 单字节长度形态）。
fn build_esds(asc: &[u8], es_id: u16, avg_bitrate: u32) -> Vec<u8> {
    let mut out = vec![0, 0, 0, 0]; // version/flags（full box 头形态）
    // DecoderSpecificInfo = ASC
    let mut dsi = vec![0x05, asc.len() as u8];
    dsi.extend_from_slice(asc);
    // SLConfigDescriptor（predefined = 2，MP4）
    let slc = [0x06, 0x01, 0x02];
    // DecoderConfigDescriptor：体 = 13 字节定长 + DSI + SLC
    let mut dcd_payload = vec![0x40]; // objectTypeIndication = MPEG-4 Audio
    dcd_payload.push(0x15); // streamType=5(audio)<<2 | reserved 1
    dcd_payload.extend_from_slice(&[0, 0, 0]); // bufferSizeDB
    dcd_payload.extend_from_slice(&avg_bitrate.to_be_bytes()); // maxBitrate
    dcd_payload.extend_from_slice(&avg_bitrate.to_be_bytes()); // avgBitrate
    dcd_payload.extend_from_slice(&dsi);
    dcd_payload.extend_from_slice(&slc);
    let mut dcd = vec![0x04, dcd_payload.len() as u8];
    dcd.extend_from_slice(&dcd_payload);
    // ES_Descriptor：ES_ID + flags + DecoderConfig
    let mut es_payload = Vec::new();
    es_payload.extend_from_slice(&es_id.to_be_bytes());
    es_payload.push(0); // flags：无依赖/URL/OCR
    es_payload.extend_from_slice(&dcd);
    out.push(0x03);
    out.push(es_payload.len() as u8);
    out.extend_from_slice(&es_payload);
    out
}

/// 按采样率/声道数构造完整 esds 载荷（AOT=2 的 ASC 内嵌）。
///
/// 消费方：`ye::convert` 的 MP4 音频轨模板（架构裁决：convert 不经
/// `Encoder` trait 回读带外参数，与视频侧 avcC 自建同款）。
/// 采样率不在 [`SAMPLE_RATES`] 13 档、或声道数非 1/2 → `None`。
#[must_use]
pub fn esds_extradata(sample_rate: u32, channels: u8) -> Option<Vec<u8>> {
    let idx = SAMPLE_RATES.iter().position(|&r| r == sample_rate)?;
    if channels != 1 && channels != 2 {
        return None;
    }
    Some(build_esds(&asc_bytes(idx as u8, channels), 0, 0))
}

/// [`AacError`] → [`CodecError`]（编码路径只经滤波器组错误，归
/// `InvalidData`；配置校验已在 [`AacEncoder::new`] 以静态文案完成）。
fn aac_err(err: AacError) -> CodecError {
    CodecError::InvalidData(err.to_string())
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::AacDecoder;
    use crate::filterbank::{BlockSequence, FrameLength, WindowInfo, WindowShape, synth_frame};
    use crate::syntax::{AudioSpecificConfig, BitReader};
    use crate::tables::{cbrt43, codebook_shape, pow2sf, swb_offset_long};
    use ye_codec::{CodecParameters, Decoded, Decoder, MediaType};

    fn base_config() -> EncoderConfig {
        EncoderConfig {
            codec: CodecId::Aac,
            time_base: Rational::new(1, 44_100),
            bit_rate: Some(64_000),
            video: None,
            audio: Some(ye_codec::AudioEncoderConfig {
                sample_rate: 44_100,
                channel_layout: ChannelLayout::STEREO,
                format: SampleFormat::F32,
            }),
        }
    }

    /// 合成 1024 样本 packed f32 帧（声道 c 相位不同的 AM 正弦）。
    fn sine_frame(channels: usize, t: u32) -> AudioFrame {
        let mut f = AudioFrame::new(
            SampleFormat::F32,
            if channels == 1 {
                ChannelLayout::MONO
            } else {
                ChannelLayout::STEREO
            },
            44_100,
            FRAME_LEN,
            None,
        )
        .unwrap();
        let mut s = Vec::with_capacity(FRAME_LEN * channels);
        for i in 0..FRAME_LEN {
            let x = i as f64 + f64::from(t) * 1024.0;
            let v = 0.5
                * (2.0 * std::f64::consts::PI * 440.0 * x / 44_100.0).sin()
                * (0.5 + 0.5 * (2.0 * std::f64::consts::PI * 0.5 * x / 44_100.0).sin());
            for c in 0..channels {
                let vc = if c == 0 { v } else { v * 0.8 };
                s.push((vc as f32).to_le_bytes());
            }
        }
        f.data_mut().copy_from_slice(&s.concat());
        f
    }

    /// 配置校验矩阵：逐项非法 → InvalidArguments；基准可构造。
    #[test]
    fn factory_validates_config() {
        assert!(AacEncoder::new(&base_config()).is_ok());
        let mut bad = base_config();
        bad.codec = CodecId::H264;
        assert!(matches!(
            AacEncoder::new(&bad),
            Err(CodecError::InvalidArguments(_))
        ));
        let bad = EncoderConfig {
            codec: CodecId::Aac,
            time_base: Rational::new(1, 44_100),
            bit_rate: Some(64_000),
            video: None,
            audio: None,
        };
        assert!(matches!(
            AacEncoder::new(&bad),
            Err(CodecError::InvalidArguments(_))
        ));
        let mut bad = base_config();
        bad.audio.as_mut().unwrap().format = SampleFormat::F32P;
        assert!(matches!(
            AacEncoder::new(&bad),
            Err(CodecError::InvalidArguments(_))
        ));
        let mut bad = base_config();
        bad.audio.as_mut().unwrap().sample_rate = 44_100 + 1;
        assert!(matches!(
            AacEncoder::new(&bad),
            Err(CodecError::InvalidArguments(_))
        ));
        // 声道 > 2 拒绝
        let mut bad = base_config();
        bad.audio.as_mut().unwrap().channel_layout = ChannelLayout::try_from_mask(0x7).unwrap(); // 3.0
        assert!(matches!(
            AacEncoder::new(&bad),
            Err(CodecError::InvalidArguments(_))
        ));
        for br in [None, Some(0)] {
            let mut bad = base_config();
            bad.bit_rate = br;
            assert!(
                matches!(AacEncoder::new(&bad), Err(CodecError::InvalidArguments(_))),
                "bit_rate {br:?} 须拒收"
            );
        }
        let mut bad = base_config();
        bad.time_base = Rational::new(0, 1);
        assert!(matches!(
            AacEncoder::new(&bad),
            Err(CodecError::InvalidArguments(_))
        ));
        // ADTS 帧长上限（64×sample_rate 位/帧）
        let mut bad = base_config();
        bad.bit_rate = Some(3_000_000);
        assert!(matches!(
            AacEncoder::new(&bad),
            Err(CodecError::InvalidArguments(_))
        ));
        // 13 档采样率全可构造（逐档单声道）
        for (i, &rate) in SAMPLE_RATES.iter().enumerate() {
            let cfg = EncoderConfig {
                codec: CodecId::Aac,
                time_base: Rational::new(1, rate as i32),
                bit_rate: Some(64_000),
                video: None,
                audio: Some(ye_codec::AudioEncoderConfig {
                    sample_rate: rate,
                    channel_layout: ChannelLayout::MONO,
                    format: SampleFormat::F32,
                }),
            };
            let enc = AacEncoder::new(&cfg).unwrap();
            assert_eq!(u32::from(enc.freq_index), i as u32);
        }
    }

    /// 状态机：feed 出包、flush 补尾包、Finished 幂等、后 feed 拒；
    /// 输入校验（空帧/视频帧/采样率/声道数不符）。
    #[test]
    fn encoder_state_machine() {
        let mut enc = AacEncoder::new(&base_config()).unwrap();
        assert!(matches!(enc.pull(), Ok(Encoded::NeedInput)));
        // TDAC 前视：首帧输入先缓冲（块 = 交付帧 ∥ 下一帧），无包
        enc.feed(&Frame::Audio(sine_frame(2, 0))).unwrap();
        assert!(matches!(enc.pull(), Ok(Encoded::NeedInput)), "首帧缓冲");
        enc.feed(&Frame::Audio(sine_frame(2, 1))).unwrap();
        assert!(matches!(enc.pull(), Ok(Encoded::Packet(_))));
        assert!(matches!(enc.pull(), Ok(Encoded::NeedInput)));
        // 尾帧不足 1024：flush 补零成包
        let partial =
            AudioFrame::new(SampleFormat::F32, ChannelLayout::STEREO, 44_100, 300, None).unwrap();
        enc.feed(&Frame::Audio(partial)).unwrap();
        enc.flush();
        assert!(matches!(enc.pull(), Ok(Encoded::Packet(_))));
        assert!(
            matches!(enc.pull(), Ok(Encoded::Packet(_))),
            "flush 第二包（交付半补零）"
        );
        assert!(matches!(enc.pull(), Ok(Encoded::Finished)));
        assert!(matches!(enc.pull(), Ok(Encoded::Finished)));
        assert!(matches!(
            enc.feed(&Frame::Audio(sine_frame(2, 2))),
            Err(CodecError::InvalidState(_))
        ));
        // 输入校验
        let mut enc2 = AacEncoder::new(&base_config()).unwrap();
        let empty =
            AudioFrame::new(SampleFormat::F32, ChannelLayout::STEREO, 44_100, 0, None).unwrap();
        assert!(matches!(
            enc2.feed(&Frame::Audio(empty)),
            Err(CodecError::InvalidArguments(_))
        ));
        assert!(matches!(
            enc2.feed(&Frame::Video(
                ye_codec::VideoFrame::new(
                    ye_pixfmt::PixelFormat::Yuv420p,
                    2,
                    2,
                    ye_pixfmt::ColorDesc::UNSPECIFIED,
                    None,
                    None
                )
                .unwrap()
            )),
            Err(CodecError::InvalidArguments(_))
        ));
        let wrong_rate =
            AudioFrame::new(SampleFormat::F32, ChannelLayout::STEREO, 48_000, 16, None).unwrap();
        assert!(matches!(
            enc2.feed(&Frame::Audio(wrong_rate)),
            Err(CodecError::InvalidArguments(_))
        ));
        let mono_frame = sine_frame(1, 0);
        assert!(matches!(
            enc2.feed(&Frame::Audio(mono_frame)),
            Err(CodecError::InvalidArguments(_))
        ));
    }

    /// 打戳：pts = 帧号×1024/44100 重标度到 time_base（1/90000 验
    /// 舍入路径：NearInf = 四舍五入远离零）；duration/stream/KEYFRAME。
    #[test]
    fn packet_stamping() {
        let mut cfg = base_config();
        cfg.time_base = Rational::new(1, 90_000); // 非采样率时基：验重标度
        let mut enc = AacEncoder::new(&cfg).unwrap();
        for t in 0..3u32 {
            enc.feed(&Frame::Audio(sine_frame(2, t))).unwrap();
        }
        enc.flush(); // 前视尾帧收口（3 输入 → 3 包）
        for i in 0..3u64 {
            let Encoded::Packet(p) = enc.pull().unwrap() else {
                panic!("packet {i}");
            };
            let want_ticks = ((i * 1024 * 90_000) as f64 / 44_100.0).round() as i64;
            let want = Timestamp::new(want_ticks, Rational::new(1, 90_000));
            assert_eq!(p.pts, Some(want), "frame {i}");
            assert_eq!(p.dts, p.pts);
            let dur = ((1024u64 * 90_000) as f64 / 44_100.0).round() as i64;
            assert_eq!(
                p.duration,
                Some(Timestamp::new(dur, Rational::new(1, 90_000)))
            );
            assert_eq!(p.stream_index, 0);
            assert!(p.flags.contains(PacketFlags::KEYFRAME));
            assert!(p.data.len() > 7 && p.data[0] == 0xFF, "ADTS 包体");
        }
        assert!(matches!(enc.pull(), Ok(Encoded::Finished)), "flush 后排空");
    }

    /// ASC/esds 构造 → 自家解析回环（句法层 + 解码器入口）。
    #[test]
    fn asc_and_esds_roundtrip() {
        let enc = AacEncoder::new(&base_config()).unwrap();
        let asc = enc.audio_specific_config();
        assert_eq!(asc, vec![0x12, 0x10], "44.1k 立体声 ASC 经典值");
        let mut r = BitReader::new(&asc);
        let parsed = AudioSpecificConfig::parse(&mut r).unwrap();
        assert_eq!(
            (
                parsed.object_type,
                parsed.sample_rate,
                parsed.channel_config
            ),
            (2, 44_100, 2)
        );
        assert_eq!(r.remaining(), 0);

        // esds：自家解码器 esds 消费路径
        let esds = enc.esds_extradata(0x0100);
        let mut params = CodecParameters::new(CodecId::Aac, MediaType::Audio);
        params.extradata = Some(esds);
        let dec = AacDecoder::new(&params).unwrap();

        // 自由函数：合法/非法
        assert!(esds_extradata(44_100, 2).is_some());
        assert!(esds_extradata(48_000, 1).is_some());
        assert!(esds_extradata(44_101, 2).is_none(), "非标采样率");
        assert!(esds_extradata(44_100, 0).is_none());
        assert!(esds_extradata(44_100, 3).is_none());
        assert!(esds_extradata(44_100, 6).is_none());
        let free = esds_extradata(48_000, 1).unwrap();
        assert_eq!(free[..4], [0, 0, 0, 0], "full box 版本/标志头");
        assert_eq!(free[4], 0x03, "ES_Descriptor");
        // 与方法形态一致性：48k 单声道方法版
        let cfg = EncoderConfig {
            codec: CodecId::Aac,
            time_base: Rational::new(1, 48_000),
            bit_rate: Some(64_000),
            video: None,
            audio: Some(ye_codec::AudioEncoderConfig {
                sample_rate: 48_000,
                channel_layout: ChannelLayout::MONO,
                format: SampleFormat::F32,
            }),
        };
        let enc48 = AacEncoder::new(&cfg).unwrap();
        // 方法版填 avgBitrate（非 0），自由函数填 0：仅比特率字段不同
        let m = enc48.esds_extradata(0);
        assert_eq!(m.len(), free.len());
        assert_ne!(m, free, "方法版带码率字段");
        let _ = dec; // 构造成功即验证 esds 可消费
    }

    /// AM 正弦生成器（系数域 f64 ×65536）：声道 c、帧 t、`valid` 有效
    /// 样本数（不足 1024 补零，与编码器 flush 补零口径一致）。
    fn am_sine_scaled(c: usize, t: u32, valid: usize) -> Vec<f64> {
        (0..FRAME_LEN)
            .map(|i| {
                if i >= valid {
                    return 0.0;
                }
                let x = i as f64 + f64::from(t) * 1024.0;
                let s = 0.5
                    * (2.0 * std::f64::consts::PI * 440.0 * x / 44_100.0).sin()
                    * (0.5 + 0.5 * (2.0 * std::f64::consts::PI * 0.5 * x / 44_100.0).sin());
                let vc = if c == 0 { s } else { s * 0.8 };
                vc * INPUT_CAL
            })
            .collect()
    }

    /// 编码 → 自家解码逐样本回环：解码 PCM == 编码侧量化谱的 IMDCT
    /// 逆变换（位级口径：同表 requant + 同滤波器组合成 × 1/65536，
    /// 期望逐位相等——断言 1e-9 只留 f32 运算次序保险）。
    #[test]
    fn encode_decode_pcm_roundtrip() {
        // 编码侧：2 整帧 + 700 样本尾帧（flush 补零）
        let mut enc = AacEncoder::new(&base_config()).unwrap();
        let payload_target = ((64_000 * FRAME_LEN as u64 / 44_100) / 8).saturating_sub(7) as usize;
        enc.feed(&Frame::Audio(sine_frame(2, 0))).unwrap();
        enc.feed(&Frame::Audio(sine_frame(2, 1))).unwrap();
        let partial =
            AudioFrame::new(SampleFormat::F32, ChannelLayout::STEREO, 44_100, 700, None).unwrap();
        let mut bytes = Vec::new();
        for i in 0..700 {
            for c in 0..2 {
                let v = (am_sine_scaled(c, 2, 700)[i] / INPUT_CAL) as f32;
                bytes.extend_from_slice(&v.to_le_bytes());
            }
        }
        let mut partial = partial;
        partial.data_mut().copy_from_slice(&bytes);
        enc.feed(&Frame::Audio(partial)).unwrap();
        enc.flush();

        // 解码侧：ADTS 包喂自家解码器
        let params = CodecParameters::new(CodecId::Aac, MediaType::Audio);
        let mut dec = AacDecoder::new(&params).unwrap();
        let mut decoded: Vec<f32> = Vec::new();
        while let Ok(Encoded::Packet(p)) = enc.pull() {
            dec.feed(&p).unwrap();
            while let Ok(Decoded::Frame(Frame::Audio(f))) = dec.pull() {
                for c in f.data().as_chunks::<4>().0 {
                    decoded.push(f32::from_le_bytes(*c));
                }
            }
        }
        dec.flush();
        assert_eq!(decoded.len(), 3 * 1024 * 2, "3 帧 × 1024 × 2 声道");

        // 参考侧：按交付帧序重建——帧 f 的分析块 = 输入全局位置
        // [f·1024, f·1024+2048)（尾部补零；TDAC 前视与编码器一致），
        // MDCT → encode_frame（同预算，确定性）→ requant → synth_frame
        //（合成 tail 跨帧链接）→ ×1/65536 交织。
        let win_tab = crate::filterbank::sine_table(2 * FRAME_LEN).unwrap();
        let win = WindowInfo {
            sequence: BlockSequence::OnlyLong,
            shape_prev: WindowShape::Sine,
            shape_curr: WindowShape::Sine,
        };
        let swb = swb_offset_long(4).unwrap();
        let num_swb = crate::tables::num_swb_long(4).unwrap() as usize;
        let total_valid = 1024 + 1024 + 700;
        let sample_at = |c: usize, pos: usize| -> f64 {
            if pos >= total_valid {
                return 0.0;
            }
            let x = pos as f64;
            let s = 0.5
                * (2.0 * std::f64::consts::PI * 440.0 * x / 44_100.0).sin()
                * (0.5 + 0.5 * (2.0 * std::f64::consts::PI * 0.5 * x / 44_100.0).sin());
            (if c == 0 { s } else { s * 0.8 }) * INPUT_CAL
        };
        let mut synth_tails: Vec<Vec<f64>> = vec![Vec::new(); 2];
        let mut reference: Vec<f32> = Vec::new();
        for f in 0..3usize {
            let mut coeffs = Vec::new();
            for c in 0..2 {
                let mut block = vec![0.0f64; 2 * FRAME_LEN];
                for (b, pos) in block.iter_mut().zip(f * FRAME_LEN..) {
                    *b = sample_at(c, pos);
                }
                for (b, w) in block.iter_mut().zip(win_tab.iter()) {
                    *b *= f64::from(*w);
                }
                coeffs.push(crate::filterbank::mdct(&block).unwrap());
            }
            let refs: Vec<&[f32]> = coeffs.iter().map(|c| c.as_slice()).collect();
            let (_payload, cqs) = element::encode_frame(&refs, swb, num_swb, payload_target);
            let mut outs = Vec::new();
            for (c, cq) in cqs.iter().enumerate() {
                let spec = reference_spectrum(cq, swb);
                let o = synth_frame(&spec, &win, FrameLength::N1024, &synth_tails[c]).unwrap();
                synth_tails[c] = o.tail;
                outs.push(o.samples);
            }
            for (l, r) in outs[0].iter().zip(outs[1].iter()) {
                reference.push(*l / 65_536.0);
                reference.push(*r / 65_536.0);
            }
        }

        let mut max_diff = 0.0f32;
        for (i, (a, b)) in decoded.iter().zip(reference.iter()).enumerate() {
            let d = (a - b).abs();
            max_diff = max_diff.max(d);
            assert!(
                d < 1e-9,
                "样本 {i} 差 {d}（应位级一致：同一 requant/合成链）"
            );
        }
        println!("roundtrip max |Δ| = {max_diff}");
    }

    /// 参考重建：ChannelQuant 量化谱 → 解码侧同表 requant（值表 ×
    /// pow2sf 乘子），位级复刻 `spectral::decode_and_requant` 的运算。
    fn reference_spectrum(cq: &element::ChannelQuant, swb: &[u16]) -> Vec<f32> {
        let mut spec = vec![0.0f32; 1024];
        for j in 0..cq.max_sfb as usize {
            let cb = cq.codebooks[j];
            if cb == 0 {
                continue;
            }
            let mult = pow2sf(i32::from(cq.chain[j]) - 100);
            let (lo, hi) = (swb[j] as usize, swb[j + 1] as usize);
            let vals = codebook_shape(cb).unwrap().vals;
            for (k, &q) in cq.quants[lo..hi].iter().enumerate() {
                let k = lo + k;
                if q == 0 {
                    continue;
                }
                let a = q.abs();
                let v = match cb {
                    // 符号内嵌族（值表自带正负）：直接取值，不再配符号
                    1 | 2 => vals[(q + 1) as usize],
                    5 | 6 => vals[(q + 4) as usize],
                    // 幅值表族（3/4/7..=11）：值表为 |q|^{4/3}，另配符号
                    _ => {
                        let mag = if a <= 15 {
                            vals[a as usize]
                        } else {
                            cbrt43(a as u32).unwrap()
                        };
                        if q < 0 { -mag } else { mag }
                    }
                };
                spec[k] = v * mult;
            }
        }
        spec
    }
}
