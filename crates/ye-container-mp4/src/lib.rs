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
//! ## 时戳语义（2026-10-24 移植裁决，见 docs/contracts.md）
//!
//! - 读取端：时戳一律以**流时基**（`1/mdhd:timescale`）原生单位打戳，
//!   `Timestamp { ticks = 轨道单位, time_base = 流时基 }`，全程无舍入、
//!   无全局 tick 域（取代 m1 的 7,056,000 全局 tick 方案）；
//! - 封装端：收到的包按包自带时基解读，向目标轨道时基重标度
//!   （`Rounding::NearInf`，对齐 FFmpeg `av_rescale_q` 默认），校验与
//!   入表都在轨道单位域进行；
//! - `Stream`/`Packet` 直接使用 develop 契约类型（`ye_container::Stream`、
//!   `ye_codec::Packet`），本 crate 不再自定义；
//! - trait 面（`Demuxer`/`Muxer`）已由 `adapter` 模块接线（见下）；
//!   随机访问读、封装选项等 mp4 专属面仍保留在本 crate 的具体类型上
//!   （合流裁决记录在案）。
//!
//! 为接线阶段 [`ye_container::Demuxer::seek`] 预置的只读查询：
//! [`Mp4Reader::keyframe_index_before`] 与 [`Mp4Reader::sample_timing`]。
//!
//! 接线已完成：`adapter` 模块把本 crate 挂到 develop 契约 trait 上
//! （[`Demuxer`]/[`Muxer`] 实现、ftyp 探测与 [`ContainerRegistry`] 注册，
//! 见 `adapter.rs` 头注），本 crate 的其余具体类型与行为未动。

#![forbid(unsafe_code)]

mod adapter;
mod box_tree;
mod error;
mod reader;
mod types;
mod writer;

pub use adapter::{
    FORMAT_NAME, Mp4Demuxer, Mp4DemuxerFactory, Mp4Muxer, Mp4MuxerFactory, register_mp4,
};
pub use box_tree::{BoxInfo, BoxNode};
pub use error::Mp4Error;
pub use reader::{Mp4Reader, SampleTiming, TrackInfo};
pub use types::FourCc;
pub use writer::{Layout, Mp4Writer, Mp4WriterOptions, TrackConfig, WriterError};

// develop 契约类型经本 crate 门面一并可用（测试与接线方免多依赖）。
pub use ye_codec::{AudioParams, CodecId, MediaType, Packet, PacketFlags, VideoParams};
pub use ye_container::{ContainerRegistry, Demuxer, Muxer, Stream};
pub use ye_time::{Rational, Rounding, Timestamp};
