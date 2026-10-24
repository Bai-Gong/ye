//! 媒体处理图：帧级的格式转换、缩放、色彩处理等变换的组装与调度。
//!
//! 对标 libavfilter（lavfi）。同时负责系统内存与设备内存
//! （`ye-device`）之间的搬运编排。
//!
//! ## 模块划分（私有模块 + 根 re-export，crate 间一律完整路径调用）
//!
//! - [`FilterDescriptor`]/[`PadDescriptor`]：滤镜与垫的静态描述
//!   （`descriptor` 模块）；
//! - [`FilterGraph`]/[`PadRef`]/[`NodeId`]：图的组装与拓扑排序接口
//!   （`graph` 模块）；
//! - [`FilterError`]：本 crate 错误枚举（`error` 模块）。
//!
//! ## 桩的语义边界（M4 交付前）
//!
//! 本 crate 当前只落地处理图最小契约 + 桩：
//! - **可构造**：描述符是纯数据；`FilterGraph::add_filter`/`connect`
//!   真实生效（`connect` 仅做节点/垫索引校验，不做去重与成环检测）；
//! - **可查询**：图结构（节点数/边数/描述符/连线）可完整读回；
//! - **不可用**：`FilterGraph::topological_order` 调用即返回
//!   [`FilterError::Unsupported`]，文案注明交付里程碑（M4）——真实的
//!   排序、调度与帧处理随实现交付。
//!
//! ## 设计要点
//!
//! - 零第三方依赖（仅 std + ye-codec），不含 unsafe；数据驱动失败
//!   一律 `Result`，无 panic 路径；
//! - 垫的媒体类型复用 [`ye_codec::MediaType`]：帧来自 `ye-codec`，
//!   与 libavfilter 依赖 libavcodec 的方向对称；
//! - 注册表哲学与 `ye-codec` 一致：图是显式构造、显式传递的对象，
//!   无全局单例。

mod descriptor;
mod error;
mod graph;

pub use descriptor::{FilterDescriptor, PadDescriptor};
pub use error::FilterError;
pub use graph::{FilterGraph, NodeId, PadRef};
