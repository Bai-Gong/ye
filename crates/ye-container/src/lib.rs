//! 封装/解封装抽象：`Stream ⇄ Packet` 的 trait 与注册表。
//!
//! 对标 libavformat。具体容器以独立 crate 落地（如
//! `ye-container-mp4`、`ye-container-mkv`），在本 crate 按同名 feature
//! re-export 汇总，由 `ye` 门面向上转发；各工具只依赖门面，不直接
//! 依赖实现 crate。
//!
//! M1 落地的 `Stream`/`Packet` 等数据类型暂定义在 `ye-container-mp4`
//! 内（与 architecture.md 数据模型同名同义），M2 解码器开始消费时
//! 上提到本 crate 的共享定义。

/// MP4 容器：解封装（progressive 与 fMP4 只读）与封装
/// （progressive 与 fMP4 写入，`Mp4Writer`）。
///
/// 需 `mp4` feature：`ye-container = { features = ["mp4"] }`，
/// 经门面开启则 `ye = { features = ["mp4"] }`（默认开启）。
#[cfg(feature = "mp4")]
pub use ye_container_mp4 as mp4;
