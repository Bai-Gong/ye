//! ye-codec-aac：AAC（MPEG-2/4 Advanced Audio Coding）解码 + 编码。
//!
//! 施工图③ M2 主链：码流语法（noiseless tools 码表全量）→ 解谱重量化
//! → 立体声工具（M/S、强度）→ TNS → IMDCT+窗序列 → packed f32
//! [`ye_codec::AudioFrame`]，时戳透传；挂 [`ye_codec::Decoder`] 并经
//! [`decoder_factory`] 注册（ye 门面 `aac` feature）。
//!
//! AAC-LC 编码主链（后续批次交付）：packed f32 → 心理声学模型 +
//! 前向 MDCT（与解码器同源窗/核）→ 幂律量化 + 段化 Huffman 谱编码
//! → SCE/CPE（长窗、无 TNS/M-S）→ 码率环 + FIL 填充的恒定码率 →
//! ADTS 包体，挂 [`ye_codec::Encoder`] 并经 [`encoder_factory`] 注册；
//! 带外参数面 [`AacEncoder::audio_specific_config`]/
//! [`AacEncoder::esds_extradata`] 与自由函数 [`esds_extradata`]，
//! 见 `encoder` 模块文档。
//!
//! ## 模块划分（私有模块 + 根 re-export，crate 间完整路径调用）
//!
//! - [`AacError`]：错误枚举（`error` 模块，含滤波器组错误包装）；
//! - [`AacDecoder`]/[`decoder_factory`]：Decoder 实现（`decoder` 模块：
//!   ADTS/LOAS 剥壳、esds（AudioSpecificConfig）消费、帧组装）；
//! - [`AacEncoder`]/[`encoder_factory`]/[`esds_extradata`]：Encoder
//!   实现与带外参数（`encoder` 模块：位写入器、心理声学、量化、
//!   段化、raw data block/码率环）；
//! - `syntax`：位读取器、ASC、ADTS/LOAS、raw data block 全量语法；
//! - `spectral`：逆 Huffman 解谱 + 重量化 + pulse 应用；
//! - `stereo`：M/S 蝶形与强度立体声；
//! - `tns`：TNS 全极点滤波（Levinson 系数换算）；
//! - `filterbank`：IMDCT 与窗序列纯函数核（并行实现者交付，模块文档
//!   即接口契约，主链按契约消费；前向 MDCT 供编码侧共用）；
//! - `tables`/`tables_data`：ISO/IEC 14496-3 标准码表（机械转录数据，
//!   来源与许可见 `tables.rs` 头注）与运行时生成表。
//!
//! ## 形态边界（如实报错，绝不静默错帧）
//!
//! 解码支持：AAC-LC（objectType 2）、1/2 声道、1024 帧长、ADTS/LOAS/
//! 裸块（裸块需 extradata）、M/S、强度立体声、TNS、pulse。
//! 不支持（`Err`）：HE-AAC/SBR、PNS 噪声码本、PCE/CCE/>2 声道、
//! AAC-Main/LTP/SSR 等 object type、960 帧长（收紧待向量）。
//!
//! 编码支持：AAC-LC 长窗（1024/sine）、单声道 SCE / 立体声 CPE
//! （common_window=1、不做 M-S）、采样率 13 档、packed f32 输入、
//! ADTS 输出、`bit_rate` 唯一质量旋钮（码率环 + FIL 填充）。
//! 不支持：短窗/过渡窗、TNS/PNS/M-S/强度/预测/LTP、960 帧长、
//! 其他声道布局（`Err`）。
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
mod encoder;
mod spectral;
mod stereo;
mod tables_data;
mod tns;

pub use decoder::{AacDecoder, decoder_factory};
pub use encoder::{AacEncoder, encoder_factory, esds_extradata};
pub use error::AacError;
