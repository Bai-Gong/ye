//! 门面的 H.264 编解码层：`ye-codec-h264` 汇总转发（`h264` feature）。
//!
//! - 解码主链（②a）：I 帧 CAVLC 闭环（Annex B / AVCC 输入 →
//!   [`ye_codec::VideoFrame`]），形态收紧一律 `Err`，见实现 crate 文档；
//! - [`register_h264`]：把 H.264 解码器工厂注册进
//!   [`ye_codec::CodecRegistry`]（替换语义，可覆盖同名桩）。

pub use ye_codec_h264::{H264Decoder, decoder_factory};

use ye_codec::{CodecId, CodecRegistry};

/// 注册 H.264 解码器（已注册时返回旧工厂——替换语义，
/// 通常用于覆盖 `codec-stubs` 的桩）。
pub fn register_h264(registry: &mut CodecRegistry) -> Option<ye_codec::DecoderFactory> {
    registry.register_decoder(CodecId::H264, decoder_factory)
}
