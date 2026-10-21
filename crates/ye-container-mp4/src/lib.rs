//! MP4 解封装（只读）与封装（progressive 与 fMP4）。
//!
//! 读取端对标 libavformat mov demuxer 的只读子集：解析 box 树，读出每路
//! trak 的元数据（codec、宽高/声道采样率、时基、时长）与样例表
//! （stts/ctts/stss/stsc/stsz/stco/co64），并按样例序号随机读出压缩数据
//! 组成 [`Packet`]。progressive 与 fMP4（moof/mvex，样例序号跨 fragment
//! 统一编址）均可读。
//!
//! 封装由 [`Mp4Writer`] 提供：progressive 布局（ftyp → free → mdat →
//! moov）一次成片；fMP4 写入（[`Layout::Fragmented`]）为前置 moov +
//! (moof + mdat)* 流式分片。
//!
//! [`Packet`]/[`Stream`] 本轮定义在本 crate（与 architecture.md 数据模型
//! 同名同义），M2 解码器消费时上提到共享 crate。

#![forbid(unsafe_code)]

mod box_tree;
mod error;
mod reader;
mod types;
mod writer;

pub use box_tree::{BoxInfo, BoxNode};
pub use error::Mp4Error;
pub use reader::Mp4Reader;
pub use types::{AudioInfo, FourCc, MediaKind, Packet, Stream, VideoInfo};
pub use writer::{Layout, Mp4Writer, Mp4WriterOptions, TrackConfig, WriterError};

pub use ye_time::{Rational, Timestamp};
