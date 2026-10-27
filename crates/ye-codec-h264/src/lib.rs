//! ye-codec-h264：H.264 码流层——NAL 拆包（AVCC/Annex B）与 SPS/PPS 解析。
//!
//! 施工图①的地基：M2 解码器（④）与硬解路径（⑨）共用的码流层。
//! 本 crate 只做字节/位层的拆包与参数集解析，**不含**解码逻辑
//! （Decoder trait 接线随 M2 交付）。
//!
//! ## 输入契约（docs/contracts.md §MP4 硬契约第 6 条）
//!
//! 解码器输入 = MP4 原生形态：`Packet.data` 为 **AVCC**（长度前缀
//! NAL，前缀宽度来自 avcC 的 lengthSizeMinusOne），extradata 为
//! **avcC 记录原样透传**（容器不解释；[`AvcConfigRecord::parse`]
//! 在解码器侧还原长度前缀宽度与带内 SPS/PPS）。
//! Annex B（起始码流）不是解码器输入，仅作测试工具与 ffmpeg oracle
//! 对拍格式（`h264_mp4toannexb`），见 [`split_annexb`]。
//!
//! ## 模块划分（私有模块 + 根 re-export，crate 间一律完整路径调用）
//!
//! - [`H264Error`]：错误枚举（`error` 模块）；
//! - [`BitReader`]：Exp-Golomb 位读取器 u(1)/u(n)/ue/se（`bitreader`
//!   模块，越界一律 `Err` 不 panic）；
//! - [`NalHeader`]/[`NalCategory`]/[`split_avcc`]/[`split_annexb`]/
//!   [`to_avcc`]/[`to_annexb`]/[`avcc_to_annexb`]/[`annexb_to_avcc`]/
//!   [`unescape_rbsp`]/[`AvcConfigRecord`]：NAL 层（`nal` 模块）；
//! - [`Sps`]/[`Pps`]/[`Profile`]：参数集解析（`sps` 模块，含
//!   Baseline 边界判定 [`Sps::is_baseline`]——④/⑨ 的 profile 门槛
//!   消费点）。
//! - `encoder` 目录：编码写侧（M3 施工图 T2–T5）——`BitWriter`
//!   （u(1)/u(n)/ue/se 写、rbsp trailing bits、防竞争写侧 escape）、
//!   CAVLC 写侧（与 `cavlc` 读侧互为镜像）、I 帧编码全链（参数集写侧、
//!   宏块循环与本地解码环）、运动估计（整像素 SAD 加半像素细化，候选
//!   预测块经 `mc.rs` 冻结核生成）、P 帧编码全链（P_Skip / P_L0_16x16 /
//!   宏块级 intra 回退三模式决策、MVD 中值预测编码、Baseline 单参考
//!   帧会话）、参数集定形（level 查表与 avcC 写侧）、[`Encoder`] trait
//!   接线（[`H264Encoder`]/[`encoder_factory`]：输出 AVCC 与 avcC
//!   extradata、按 `EncoderConfig.time_base` 打戳、GOP 与质量模式）。
//!
//! ## 设计要点
//!
//! - 零第三方依赖（仅 std）、不含 unsafe、零 panic：所有数据驱动
//!   失败（含位读取越界）一律 `Result`；
//! - 拆包只切边界不动载荷；防竞争去除（[`unescape_rbsp`]）在参数集
//!   解析内部进行，NAL 层字节原样往返（AVCC ⇄ Annex B 无损）；
//! - profile/level 只报告不拒绝：码流层不越权替解码器做准入。
#![allow(clippy::module_name_repetitions)] // Sps/Pps 与模块名同段是 H.264 术语，保留

mod bitreader;
mod cabac;
mod cabac_mb;
mod cabac_tables;
mod cavlc;
mod deblock;
mod decoder;
mod encoder;
mod error;
mod inter;
mod intra8x8;
mod macroblock;
mod mc;
mod nal;
mod slice;
mod sps;

pub use bitreader::BitReader;
pub use decoder::{H264Decoder, decoder_factory};
pub use encoder::{H264Encoder, ReconFrame, encoder_factory, level_for};
pub use error::H264Error;
pub use nal::{
    AvcConfigRecord, NalCategory, NalHeader, annexb_to_avcc, avcc_to_annexb,
    nal_header_and_payload, split_annexb, split_avcc, to_annexb, to_avcc, unescape_rbsp,
};
pub use sps::{Pps, Profile, Sps};
