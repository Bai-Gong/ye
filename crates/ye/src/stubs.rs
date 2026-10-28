//! M2+ 接口桩的门面聚合点（features `codec-stubs` / `filter-stubs`）。
//!
//! 桩的本体随 [`ye_codec`](crate::codec)（根级 re-export 的桩面）与
//! [`ye_filter`](crate::filter)（最小契约）**无条件编译**——尚无独立
//! 实现 crate 可作 optional 依赖裁剪，桩函数（`register_stubs` 等）与
//! 处理图类型路径因此恒可达。这两个 feature 的作用是把
//! **「本构建依赖桩」显式记进依赖声明**：消费方（剪/锻做 M2+ 接口
//! 试验）显式开启对应 feature，桩换真实现时移除 feature 即得全仓
//! 检索点；默认关闭，防桩悄悄进产品路径。
//!
//! 桩的语义边界（可构造 / 可查询 / 调用即 Unsupported）、里程碑文案
//! 与替换路径见 docs/contracts.md「M2+ 接口桩契约」一节。

/// 编解码桩（`ye-codec` 根级 re-export 的桩面）：H.264/AAC 解码占位
/// （编码桩已全部随真实现删除——H.264 2026-10-26 M3、AAC-LC 2026-10-26
/// M3 补齐，`register_h264_encoder`/`register_aac_encoder` 接管编码
/// 注册位，见 [`crate::codec_h264`]/[`crate::codec_aac`]）。
///
/// 注册后 `supports_*`/`decoder_ids` 可查，`create_decoder` 一律成功；
/// `feed`/`pull` 调用即 [`ye::codec::CodecError::UnsupportedStub`]
/// （文案含交付去向标注），`flush` 无操作。
#[cfg(feature = "codec-stubs")]
pub mod codec {
    pub use ye_codec::{aac_decoder_factory, h264_decoder_factory, register_stubs};
}

/// 处理图（`ye-filter`）：图组装与拓扑排序真实生效（M3 处理图最小集
/// 已交付 `topological_order` 真实现——原桩随其删除，2026-10-26）。
/// `filter-stubs` feature 保留为「本构建经此聚合入口依赖处理图」的
/// 显式标记（与 `codec-stubs` 对称）。
#[cfg(feature = "filter-stubs")]
pub mod filter {
    pub use ye_filter::{
        FilterDescriptor, FilterError, FilterGraph, FilterParams, NodeId, PadDescriptor, PadRef,
    };
}
