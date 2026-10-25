//! 门面的 AAC 编解码层：`ye-codec-aac` 汇总转发（`aac` feature）。
//!
//! - AAC-LC 解码主链（施工图③ M2）：ADTS/LOAS/esds 入口 → packed f32
//!   [`ye_codec::AudioFrame`]，时戳透传；形态收紧一律 `Err`（PNS/SBR/
//!   PCE/CCE/多声道/非 LC/960 帧长），见实现 crate 文档；
//! - [`register_aac`]：把 AAC 解码器工厂注册进
//!   [`ye_codec::CodecRegistry`]（替换语义，可覆盖同名桩）。

pub use ye_codec_aac::{AacDecoder, decoder_factory};

use ye_codec::{CodecId, CodecRegistry};

/// 注册 AAC 解码器（已注册时返回旧工厂——替换语义，
/// 通常用于覆盖 `codec-stubs` 的桩）。
pub fn register_aac(registry: &mut CodecRegistry) -> Option<ye_codec::DecoderFactory> {
    registry.register_decoder(CodecId::Aac, decoder_factory)
}
