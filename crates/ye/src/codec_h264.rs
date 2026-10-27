//! 门面的 H.264 编解码层：`ye-codec-h264` 汇总转发（`h264` feature）。
//!
//! - 解码主链（②a）：I 帧 CAVLC 闭环（Annex B / AVCC 输入 →
//!   [`ye_codec::VideoFrame`]），形态收紧一律 `Err`，见实现 crate 文档；
//! - 编码主链（M3 T5）：Baseline/CAVLC、I/P 帧、GOP 可配、恒定 QP /
//!   简单 CRF——输出 AVCC 包体 + avcC extradata（经
//!   [`H264Encoder::extradata`]），时戳按 `EncoderConfig.time_base`
//!   打戳，输出契约见实现 crate `encoder` 模块文档与
//!   docs/contracts.md「H.264 编码器输出契约」；
//! - [`register_h264`] / [`register_h264_encoder`]：把 H.264 解码/编码
//!   工厂注册进 [`ye_codec::CodecRegistry`]（替换语义——编码工厂接管
//!   `codec-stubs` 已删除的 H.264 编码桩的位置）。

pub use ye_codec_h264::{
    H264Decoder, H264Encoder, ReconFrame, decoder_factory, encoder_factory, level_for,
};

use ye_codec::{CodecId, CodecRegistry};

/// 注册 H.264 解码器（已注册时返回旧工厂——替换语义，
/// 通常用于覆盖 `codec-stubs` 的桩）。
pub fn register_h264(registry: &mut CodecRegistry) -> Option<ye_codec::DecoderFactory> {
    registry.register_decoder(CodecId::H264, decoder_factory)
}

/// 注册 H.264 编码器（已注册时返回旧工厂——替换语义）。
///
/// M3 起真实现接管 H.264 编码位（`codec-stubs` 的编码桩已删除）；
/// 工厂校验配置（yuv420p/偶尺寸/QP 1..=51/GOP ≥ 1/level 限额），
/// 非法 → `InvalidArguments`。
pub fn register_h264_encoder(registry: &mut CodecRegistry) -> Option<ye_codec::EncoderFactory> {
    registry.register_encoder(CodecId::H264, encoder_factory)
}
