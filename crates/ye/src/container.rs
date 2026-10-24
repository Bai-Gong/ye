//! 门面的容器层：契约 re-export、具体容器汇总与 remux 助手。
//!
//! - `pub use ye_container::*`：契约层（[`ContainerRegistry`] / [`Demuxer`] /
//!   [`Muxer`] 等）经门面直接可用，路径 `ye::container::<Item>` 不变；
//! - `mp4` feature 开启时追加 `ye::container::mp4`（`ye-container-mp4`
//!   全量 re-export，含 trait 接线的 [`register_mp4`]）；
//! - [`remux`]：跨容器重封装直通助手（不转码），自 m1 的 ye-cli 实现
//!   上提（模式照搬，时戳换成流内自描述 `Timestamp`）。
//!
//! 接线说明（2026-10-24）：contracts.md「门面与 feature」一节设想的
//! `ye-container/mp4 → dep:ye-container-mp4` 转发在合流后不可行——
//! `ye-container-mp4` 依赖 `ye-container`（契约类型），抽象层反向依赖
//! 实现层会成环。feature 汇总上提到本门面：`ye/mp4 → dep:ye-container-mp4`，
//! re-export 落在本模块的 `mp4` 子模块。文档同步归集成阶段。

pub use ye_container::*;

#[cfg(feature = "mp4")]
pub mod mp4 {
    //! MP4 容器（`ye-container-mp4`）：读写实现 + 契约 trait 接线。
    pub use ye_container_mp4::*;
}

mod remux;

pub use remux::{RemuxStats, remux};
