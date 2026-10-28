//! H.264 编码写侧（M3 施工图 T2 地基 + T3 帧内 + T4 帧间 + T5 收口）。
//!
//! 解码侧（`bitreader`/`cavlc`/`macroblock` 等）的写侧对偶：
//! - [`bitwriter`]：Exp-Golomb 位写入器 u(1)/u(n)/ue/se、rbsp
//!   trailing bits、防竞争写侧 escape（`nal` 读侧 `unescape_rbsp`
//!   的对偶）；
//! - [`cavlc`]：CAVLC 残差写侧——coeff_token 全 nC 桶码表、level
//!   （含 level_prefix 转义）、total_zeros、run_before，与读侧
//!   `cavlc` 模块互为镜像（位级回环测试保证两份码表一致）；
//! - [`mb`]：Baseline I/P 宏块层语法写骨架（mb_type 映射、
//!   coded_block_pattern me(v)、mb_qp_delta 写序）；
//! - [`transform`]：前向 4x4 整数 DCT / DC Hadamard 与量化——从解码侧
//!   反量化/反变换算术精确反推（推导见模块 doc）；
//! - [`intra`]：Intra16x16 / Intra4x4 / chroma 的模式合法性镜像与
//!   SAD/SATD 模式决策；
//! - [`frame`]：I 帧编码全链——参数集/slice header 写侧（T5 定形：
//!   level 可配、PPS QP 中立）+ 宏块循环 + 本地解码环（复用解码侧
//!   预测/反量化/反变换/去块，行粒度去块调度与解码器同构）；
//! - [`motion`]：整像素 SAD 运动估计（范围可配）+ 半像素细化——候选
//!   预测块一律经 `mc.rs` 冻结核生成，决策与重建同一块；
//! - [`pframe`]：P 帧编码全链 + Baseline 单参考帧会话——P_Skip /
//!   P_L0_16x16 / 宏块级 intra 回退三模式决策、MVD 中值预测编码、
//!   P 片头写侧、去块进重建环；
//! - [`params`]：参数集定形（T5）——level 查表（H.264 表 A-1/A-3
//!   子集取最小满足项）与 avcC 写侧构造（`AvcConfigRecord::parse`
//!   的写侧对偶）；
//! - [`rate`]：单程码控（虚拟缓冲反馈，M3 后续批次）——CBR（`Qp`
//!   模式 QP 游走追目标）/ A-VBR（`Crf` 模式上限封顶）的纯整数
//!   控制器，`adapter` 逐帧接线（基 QP 调制 + 包位数回灌）；
//! - [`adapter`]：`ye_codec::Encoder` 接线（T5）——[`H264Encoder`]
//!   （AVCC + avcC extradata 输出、按 `EncoderConfig.time_base` 打戳、
//!   GOP/质量模式）与 [`encoder_factory`]。
//!
//! 正确性主验收是位级回环：写侧产出经读侧（`BitReader` /
//! `cavlc::read_residual` 等 `pub(crate)` 入口）解回必须全等，且读侧
//! 恰好消费写出的位数——与「本地解码环防 drift」的编码器硬要求同构。
//! 帧级黑盒回环（编码 → NAL/码流 → `H264Decoder` → 逐像素比对本地
//! 重建）见 [`frame`] 与 [`pframe`] 测试组；全量序列对拍（三类向量
//! ×QP 档 ×ffmpeg 双解码 + PSNR 门槛）见 `crates/ye/tests/encode_h264.rs`。

pub(crate) mod adapter;
pub(crate) mod bitwriter;
pub(crate) mod cavlc;
pub(crate) mod frame;
pub(crate) mod intra;
pub(crate) mod mb;
pub(crate) mod motion;
pub(crate) mod params;
pub(crate) mod pframe;
pub(crate) mod rate;
pub(crate) mod transform;

pub use adapter::{H264Encoder, ReconFrame, encoder_factory};
pub use params::level_for;
