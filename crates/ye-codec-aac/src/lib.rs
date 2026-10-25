//! ye-codec-aac：AAC（MPEG-2/4 Advanced Audio Coding）解码。
//!
//! 施工图③ M2 主链：码流语法（noiseless tools 码表全量）→ 解谱重量化
//! → 立体声工具（M/S、强度）→ TNS → IMDCT+窗序列 → packed f32
//! [`ye_codec::AudioFrame`]，时戳透传；挂 [`ye_codec::Decoder`] 并经
//! [`decoder_factory`] 注册（ye 门面 `aac` feature）。
//!
//! ## 模块划分（私有模块 + 根 re-export，crate 间完整路径调用）
//!
//! - [`AacError`]：错误枚举（`error` 模块，含滤波器组错误包装）；
//! - [`AacDecoder`]/[`decoder_factory`]：Decoder 实现（`decoder` 模块：
//!   ADTS/LOAS 剥壳、esds（AudioSpecificConfig）消费、帧组装）；
//! - `syntax`：位读取器、ASC、ADTS/LOAS、raw data block 全量语法；
//! - `spectral`：逆 Huffman 解谱 + 重量化 + pulse 应用；
//! - `stereo`：M/S 蝶形与强度立体声；
//! - `tns`：TNS 全极点滤波（Levinson 系数换算）；
//! - `filterbank`：IMDCT 与窗序列纯函数核（并行实现者交付，模块文档
//!   即接口契约，主链按契约消费）；
//! - `tables`/`tables_data`：ISO/IEC 14496-3 标准码表（机械转录数据，
//!   来源与许可见 `tables.rs` 头注）与运行时生成表。
//!
//! ## 形态边界（如实报错，绝不静默错帧）
//!
//! 支持：AAC-LC（objectType 2）、1/2 声道、1024 帧长、ADTS/LOAS/裸
//! 块（裸块需 extradata）、M/S、强度立体声、TNS、pulse。
//! 不支持（`Err`）：HE-AAC/SBR、PNS 噪声码本、PCE/CCE/>2 声道、
//! AAC-Main/LTP/SSR 等 object type、960 帧长（收紧待向量）。
//!
//! ## 全库约定
//!
//! 零第三方依赖（仅 std + ye-codec/ye-pixfmt/ye-time）、不含 unsafe、
//! 数据驱动失败一律 `Result` 不 panic。

mod error;
mod syntax;
mod tables;

pub mod filterbank;

mod decoder;
mod spectral;
mod stereo;
mod tables_data;
mod tns;

pub use decoder::{AacDecoder, decoder_factory};
pub use error::AacError;
