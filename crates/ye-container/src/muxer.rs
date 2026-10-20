//! 封装侧抽象：[`Muxer`] 实例与 [`MuxerFactory`] 工厂。

use ye_codec::Packet;

use crate::{ContainerError, StreamTemplate, WriteSeek};

/// 封装器实例：由 [`MuxerFactory::open`] 绑定到输出后得到。
///
/// ## 实现契约（具体容器必须遵守）
///
/// - 调用顺序必须是 `write_header` → `write_packet`* → `write_trailer`；
///   顺序错误（未写头就写包、收尾后再写包）→ `Err(ContainerError::InvalidState(_))`；
/// - 包时基与模板 `time_base` 不符时，muxer **内部重标度**
///   （失败以 `Err(ContainerError::Time(..))` 透出，不静默丢弃）；
/// - `write_trailer` 给默认空实现 `Ok(())`，兼容无 trailer 的格式。
///
/// ## 线程安全
///
/// `Send + Sync` 上界：`Box<dyn Muxer>` 可跨线程传递。
pub trait Muxer: Send + Sync {
    /// 写容器头：一次性给定全部流模板。
    fn write_header(&mut self, streams: &[StreamTemplate]) -> Result<(), ContainerError>;

    /// 写一个包（时基不符时内部按模板 `time_base` 重标度）。
    fn write_packet(&mut self, packet: &Packet) -> Result<(), ContainerError>;

    /// 写容器尾。默认空实现，兼容无 trailer 的格式。
    fn write_trailer(&mut self) -> Result<(), ContainerError> {
        Ok(())
    }
}

/// 封装工厂（无 probe——输出格式由调用方点名）。
///
/// ## 线程安全
///
/// `Send + Sync` 上界：注册表持有 `Box<dyn MuxerFactory>` 字段，
/// 注册表本身因此 `Send + Sync`。
pub trait MuxerFactory: Send + Sync {
    /// 工厂名（`&'static str`，注册表按它点名）。
    fn name(&self) -> &'static str;

    /// 绑定输出并构造 [`Muxer`]。
    fn open(&self, output: Box<dyn WriteSeek>) -> Result<Box<dyn Muxer>, ContainerError>;
}
