//! 门面的 AAC 编解码层：`ye-codec-aac` 汇总转发（`aac` feature）。
//!
//! - AAC-LC 解码主链（施工图③ M2）：ADTS/LOAS/esds 入口 → packed f32
//!   [`ye_codec::AudioFrame`]，时戳透传；形态收紧一律 `Err`（PNS/SBR/
//!   PCE/CCE/多声道/非 LC/960 帧长），见实现 crate 文档；
//! - AAC-LC 编码主链：packed f32 → 心理声学 + 前向 MDCT → 码率环 +
//!   FIL 填充的恒定码率 → ADTS 包体（长窗、SCE/CPE），输出契约见
//!   实现 crate `encoder` 模块文档；带外参数经具体类型访问器
//!   （[`AacEncoder::audio_specific_config`]/
//!   [`AacEncoder::esds_extradata`]）与自由函数 [`esds_extradata`]
//!   （convert 组 MP4 音频模板用）取得；
//! - [`register_aac`] / [`register_aac_encoder`]：把 AAC 解码/编码
//!   工厂注册进 [`ye_codec::CodecRegistry`]（替换语义，可覆盖同名桩）。

pub use ye_codec_aac::{AacDecoder, AacEncoder, decoder_factory, encoder_factory, esds_extradata};

use ye_codec::{CodecId, CodecRegistry};

/// 注册 AAC 解码器（已注册时返回旧工厂——替换语义，
/// 通常用于覆盖 `codec-stubs` 的桩）。
pub fn register_aac(registry: &mut CodecRegistry) -> Option<ye_codec::DecoderFactory> {
    registry.register_decoder(CodecId::Aac, decoder_factory)
}

/// 注册 AAC 编码器（已注册时返回旧工厂——替换语义，
/// 通常用于覆盖 `codec-stubs` 的桩）。
///
/// 工厂校验配置（packed f32/采样率 13 档/声道 1|2/码率必填且 >0），
/// 非法 → `InvalidArguments`。
pub fn register_aac_encoder(registry: &mut CodecRegistry) -> Option<ye_codec::EncoderFactory> {
    registry.register_encoder(CodecId::Aac, encoder_factory)
}
