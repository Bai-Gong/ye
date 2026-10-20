//! 流对象与封装头信息：[`Stream`] / [`MediaHeader`] / [`StreamTemplate`]。

use ye_codec::{CodecParameters, MediaType};
use ye_time::{Rational, Timestamp};

use crate::Metadata;

/// 解封装侧的流对象（公有字段、无不变式）。
///
/// ## 字段语义
///
/// - `index`：流索引，容器保证从 0 连续递增（本类型为纯数据载体，不校验）；
/// - `codec`：编解码参数，复用 ye-codec 的 [`CodecParameters`]；
/// - `time_base`：本流时基（秒/tick），`start_time`/`duration` 均以它计；
/// - `start_time`/`duration`：流级起点/时长，缺失为 `None`（无哨兵值）；
/// - `nb_frames`：已知帧数（估算或精确，来源归容器实现）；
/// - `metadata`：流级 tag 字典。
///
/// ## derive 清单
///
/// `Debug, Clone, PartialEq, Eq`（公有字段数据载体，无不变式、无 Copy）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    /// 流索引（容器保证从 0 连续）。
    pub index: u32,
    /// 流类型（视频/音频/字幕等）。
    pub media_type: MediaType,
    /// 编解码参数（复用 ye-codec）。
    pub codec: CodecParameters,
    /// 本流时基（秒/tick）：`start_time`/`duration`/包时戳的计 tick 基数。
    pub time_base: Rational,
    /// 流级起点（以 [`Stream::time_base`] 计），缺失为 `None`。
    pub start_time: Option<Timestamp>,
    /// 流级时长（以 [`Stream::time_base`] 计），缺失为 `None`。
    pub duration: Option<Timestamp>,
    /// 已知帧数（估算或精确）。
    pub nb_frames: Option<u64>,
    /// 流级元数据 tag。
    pub metadata: Metadata,
}

/// 解封装头信息（容器级）。
///
/// ## 时基约定
///
/// 容器级 `duration`/`start_time` 以 [`ye_time::CANONICAL_TIME_BASE`] 计
/// （跨流可比）；跨流精确比较用 [`Timestamp::compare`]（i128 精确交叉相乘，
/// 无舍入），禁止经由重标度比较。流级时间以各 [`Stream::time_base`] 计。
///
/// `streams` 的顺序即流索引顺序（`streams[i].index == i` 由容器实现保证）。
///
/// ## derive 清单
///
/// `Debug, Clone, PartialEq, Eq`（数据载体，无不变式、无 Copy）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaHeader {
    /// 容器格式名（如 `"mp4"`、`"matroska"`；具体取值归容器实现）。
    pub format_name: String,
    /// 流表：顺序即 index 顺序。
    pub streams: Vec<Stream>,
    /// 容器级时长（[`ye_time::CANONICAL_TIME_BASE`] 计），缺失为 `None`。
    pub duration: Option<Timestamp>,
    /// 容器级起点（[`ye_time::CANONICAL_TIME_BASE`] 计），缺失为 `None`。
    pub start_time: Option<Timestamp>,
    /// 总码率（bit/s，估算值），缺失为 `None`。
    pub bit_rate: Option<u64>,
    /// 容器级元数据 tag。
    pub metadata: Metadata,
}

/// 封装输入模板（公有字段、无不变式）：`write_header` 一次性给定。
///
/// ## 字段语义
///
/// - `time_base`：该流封装**目标时基**——`write_packet` 收到的包若时基
///   与之不符，muxer 内部按它重标度（失败以 `Err(ContainerError::Time(..))`
///   透出）；
/// - `frame_rate`：标称帧率（仅视频流有意义，可缺省）。
///
/// ## derive 清单
///
/// `Debug, Clone, PartialEq, Eq`（数据载体，无不变式、无 Copy）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamTemplate {
    /// 流类型（视频/音频/字幕等）。
    pub media_type: MediaType,
    /// 编解码参数（复用 ye-codec）。
    pub codec: CodecParameters,
    /// 封装目标时基（秒/tick）：`write_packet` 按它重标度。
    pub time_base: Rational,
    /// 标称帧率（仅视频流有意义）。
    pub frame_rate: Option<Rational>,
    /// 流级元数据 tag。
    pub metadata: Metadata,
}
