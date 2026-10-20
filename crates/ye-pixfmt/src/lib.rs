//! 像素格式、色彩空间、采样布局的定义与转换描述。
//!
//! 对标 libavutil 的 `AVPixelFormat` / `AVColorSpace` 家族。
//! 本 crate 只承载元数据描述；实际转换内核归 `ye-filter`。
//!
//! 模块划分（私有模块 + 根 re-export，crate 间一律完整路径调用）：
//! - [`PixelFormat`] / [`PlaneLayout`] / [`FrameLayout`] / [`DEFAULT_STRIDE_ALIGN`]：
//!   像素格式描述子与帧布局计算（`pixel` 模块）；
//! - [`ColorPrimaries`] / [`ColorTransfer`] / [`ColorSpace`] / [`ColorRange`] /
//!   [`ChromaLocation`] / [`ColorDesc`]：H.273 色彩描述族（`color` 模块）；
//! - [`SampleFormat`]：音频采样格式（`sample` 模块）；
//! - [`ChannelLayout`]：声道布局（`channel` 模块）；
//! - [`PixfmtError`]：本 crate 错误枚举（`error` 模块）。
//!
//! 全 crate 零第三方依赖（仅 std）、不含 unsafe。允许 panic 的位置仅两处
//! 文档写明的编程错误：[`PixelFormat::subsample_log2`] 与 [`FrameLayout::plane`]
//! 的平面索引越界（全库五处中的第③④处）；面向数据输入的路径一律返回
//! [`Result`] 或使用饱和运算。

mod channel;
mod color;
mod error;
mod pixel;
mod sample;

pub use channel::ChannelLayout;
pub use color::{ChromaLocation, ColorDesc, ColorPrimaries, ColorRange, ColorSpace, ColorTransfer};
pub use error::PixfmtError;
pub use pixel::{DEFAULT_STRIDE_ALIGN, FrameLayout, PixelFormat, PlaneLayout};
pub use sample::SampleFormat;
