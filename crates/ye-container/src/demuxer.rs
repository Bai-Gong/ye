//! 解封装侧抽象：[`Demuxer`] 实例与 [`DemuxerFactory`] 工厂。

use ye_codec::Packet;
use ye_time::Timestamp;

use crate::{ContainerError, MediaHeader, ProbeScore, ReadSeek};

/// 解封装器实例：由 [`DemuxerFactory::open`] 绑定到输入后得到。
///
/// ## 实现契约（具体容器必须遵守）
///
/// - `read_packet` 产出的 [`Packet`] 必须已按对应 `Stream` 的 `time_base`
///   打戳并填好 `stream_index`（重标度语义落在 demuxer，不在调用方）；
/// - EOF 后再读幂等返回 `Ok(None)`（不报错、不复活）；
/// - `seek` 的 `target` 可为**任意时基**（实现内部精确比较，如 mp4 走
///   `Timestamp::compare` i128 交叉相乘——调用方无需也不应先重标度到
///   流时基，那只会平添一次可失败的换算），返回值为指定流的时基，
///   是实际落到的**前一个关键帧**时刻。
///
/// ## 线程安全
///
/// `Send + Sync` 上界：`Box<dyn Demuxer>` 可跨线程传递
/// （jian/duan 的 Tauri 多线程后端必需）。
pub trait Demuxer: Send + Sync {
    /// 容器格式名（与工厂 [`DemuxerFactory::name`] 一致）。
    fn format_name(&self) -> &str;

    /// 解析并返回头信息（流表）。调用一次即绑定成功；具体容器自行决定
    /// 后续是否允许重复调用。
    fn read_header(&mut self) -> Result<MediaHeader, ContainerError>;

    /// 读下一个包；EOF 返回 `Ok(None)`（此后幂等）。
    fn read_packet(&mut self) -> Result<Option<Packet>, ContainerError>;

    /// 定位到显示时刻 `target`（任意时基，实现内部精确比较）之前的
    /// 关键帧，返回实际落到的解码时刻（指定流的时基）。流不存在 →
    /// `Err(ContainerError::StreamNotFound(..))`。
    fn seek(&mut self, stream_index: u32, target: Timestamp) -> Result<Timestamp, ContainerError>;
}

/// 解封装工厂：探查 + 打开。
///
/// ## 线程安全
///
/// `Send + Sync` 上界：注册表持有 `Box<dyn DemuxerFactory>` 字段，
/// 注册表本身因此 `Send + Sync`。
///
/// ## 实现契约
///
/// - `probe` 只看开头字节（注册表喂 ≤ [`crate::ContainerRegistry::PROBE_WINDOW`]
///   窗口），**不得消费输入**（窗口由注册表准备并 seek 回 0）；
/// - `open` 内部完成绑定与初始化解析（含读取头所需的寻位）。
pub trait DemuxerFactory: Send + Sync {
    /// 工厂名（`&'static str`，注册表按它点名/去重）。
    fn name(&self) -> &'static str;

    /// 对输入开头字节打分（0..100，见 [`ProbeScore`]）。
    fn probe(&self, head: &[u8]) -> ProbeScore;

    /// 绑定输入并构造 [`Demuxer`]（内部完成初始化解析）。
    fn open(&self, input: Box<dyn ReadSeek>) -> Result<Box<dyn Demuxer>, ContainerError>;
}
