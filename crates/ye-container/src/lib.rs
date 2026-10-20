//! 封装/解封装抽象：`Stream ⇄ Packet` 的 trait 与注册表。
//!
//! 对标 libavformat。具体容器以独立 crate 落地
//! （如 `ye-container-mp4`、`ye-container-mkv`）。
//!
//! ## 分层与依赖
//!
//! - 流时基/时戳/容器级时长复用 [`ye_time`]（`Rational` / `Timestamp` /
//!   `CANONICAL_TIME_BASE`），跨流比较走 `Timestamp::compare` 精确语义；
//! - 读写单位 `Packet` 与编解码参数 `CodecParameters` 复用 [`ye_codec`]
//!   （对标 libavformat 依赖 libavcodec），不直接依赖 ye-pixfmt。
//!
//! ## 线程安全
//!
//! 抽象 trait（[`Demuxer`]/[`Muxer`]/[`DemuxerFactory`]/[`MuxerFactory`]）
//! 一律声明 `Send + Sync` 上界，注册表显式构造、显式传递（无全局单例），
//! 均可跨线程共享（jian/duan 的 Tauri 多线程后端必需）。

mod demuxer;
mod error;
mod io;
mod metadata;
#[cfg(test)]
mod mock;
mod muxer;
mod probe;
mod registry;
mod stream;

pub use demuxer::{Demuxer, DemuxerFactory};
pub use error::ContainerError;
pub use io::{ReadSeek, WriteSeek};
pub use metadata::Metadata;
pub use muxer::{Muxer, MuxerFactory};
pub use probe::ProbeScore;
pub use registry::ContainerRegistry;
pub use stream::{MediaHeader, Stream, StreamTemplate};
