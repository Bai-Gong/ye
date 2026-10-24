//! 编解码器抽象：`Packet ⇄ Frame` 的 trait 与注册表。
//!
//! 对标 libavcodec。具体编解码器以独立 crate 落地
//! （如 `ye-codec-h264`、`ye-codec-aac`），由 `ye` 门面按 feature 汇总，
//! 避免单 crate 膨胀拖慢编译；独立 crate 交付前，`stubs` 模块提供
//! H.264/AAC 的接口桩（见下「桩的语义边界」）。
//!
//! ## 模块划分（私有模块 + 根 re-export，crate 间一律完整路径调用）
//!
//! - [`MediaType`]/[`CodecId`]：媒体类型与编解码器标识（`media` 模块）；
//! - [`Packet`]/[`PacketFlags`]：压缩域包与位标志（`packet` 模块）；
//! - [`Frame`]/[`VideoFrame`]/[`AudioFrame`]/[`Decoded`]/[`Encoded`]：
//!   解码输出与拉取结果枚举（`frame` 模块）；
//! - [`VideoParams`]/[`AudioParams`]/[`CodecParameters`]：编解码参数
//!   （`params` 模块）；
//! - [`Decoder`]/[`Encoder`]/[`DecoderFactory`]/[`EncoderFactory`]/
//!   [`VideoEncoderConfig`]/[`AudioEncoderConfig`]/[`EncoderConfig`]：
//!   trait、工厂与编码配置（`codec` 模块）；
//! - [`CodecRegistry`]：显式注册表（`registry` 模块）；
//! - [`register_stubs`]/[`h264_decoder_factory`]/[`h264_encoder_factory`]/
//!   [`aac_decoder_factory`]/[`aac_encoder_factory`]：M2+ 接口桩的注册
//!   与工厂（`stubs` 模块）；
//! - [`CodecError`]：本 crate 错误枚举（`error` 模块，包装
//!   `ye_time::TimeError` 与 `ye_pixfmt::PixfmtError`）。
//!
//! ## 设计要点
//!
//! - 推拉模型对标 `avcodec_send_packet`/`avcodec_receive_frame`，
//!   以 [`Decoded::NeedInput`]/[`Encoded::Finished`] 枚举分载替代
//!   FFmpeg 的 EAGAIN/EOF 哨兵返回值；
//! - 时戳自描述（[`ye_time::Timestamp`] 内含时基），缺时戳一律
//!   `Option::None`，无 `AV_NOPTS_VALUE` 等价物；
//! - 注册表是显式构造、显式传递的对象，无全局单例；
//! - **桩的语义边界**（`stubs` 模块，M2/M3 交付前）：可注册——注册项
//!   经 `supports_*`/`*_ids` 可查；可构造——`create_*`/工厂一律成功；
//!   不可用——`feed`/`pull` 调用即返回 [`CodecError::UnsupportedStub`]，
//!   文案注明交付里程碑（解码 → M2、编码 → M3），`flush` 无操作；
//! - 本 crate 零第三方依赖（仅 std + ye-time + ye-pixfmt）、不含 unsafe；
//!   panic 仅一处文档写明的编程错误：[`VideoFrame::plane`] 的平面索引
//!   越界（全库五处中的第⑤处），数据驱动失败一律 `Result`。

mod codec;
mod error;
mod frame;
mod media;
mod packet;
mod params;
mod registry;
mod stubs;

pub use codec::{
    AudioEncoderConfig, Decoder, DecoderFactory, Encoder, EncoderConfig, EncoderFactory,
    VideoEncoderConfig,
};
pub use error::CodecError;
pub use frame::{AudioFrame, Decoded, Encoded, Frame, VideoFrame};
pub use media::{CodecId, MediaType};
pub use packet::{Packet, PacketFlags};
pub use params::{AudioParams, CodecParameters, VideoParams};
pub use registry::CodecRegistry;
pub use stubs::{
    aac_decoder_factory, aac_encoder_factory, h264_decoder_factory, h264_encoder_factory,
    register_stubs,
};
