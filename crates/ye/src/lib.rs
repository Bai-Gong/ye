//! # 百工·冶 (Ye)
//!
//! 对标 FFmpeg（libav\*）的媒体编解码与处理基础库——门面 crate。
//!
//! 各创作工具只应依赖 `ye`，由它统一 re-export 各分层能力；
//! 具体编解码器与容器（如 `ye-codec-h264`、`ye-container-mp4`）
//! 以独立 crate 落地，在本 crate 按 feature 汇总开启。
//!
//! ## Feature
//!
//! | feature | 默认 | 开启内容 |
//! |---------|------|----------|
//! | `mp4`   | 是   | MP4 容器读写（`ye::container::mp4`，即 `ye-container-mp4`：解封装 + 封装，progressive 与 fMP4；含 `Demuxer`/`Muxer` trait 接线与 `register_mp4` 注册） |
//! | `codec-stubs` | 否 | M2+ 编解码桩的聚合入口 `ye::stubs::codec`（H.264/AAC × 解码/编码；注册可查、可构造，`feed`/`pull` 即 `UnsupportedStub`——桩本体在 `ye-codec` 恒编译，本 feature 把「依赖桩」记进依赖声明） |
//! | `filter-stubs` | 否 | M2+ 处理图桩的聚合入口 `ye::stubs::filter`（`FilterGraph` 组装真实生效，`topological_order` 即 `Unsupported`——语义同上） |
//!
//! 新容器/编解码实现 crate 的接入规则：实现 crate 挂到本门面的
//! optional 依赖与同名 feature 下，再在 [`container`](mod@container)
//! 模块汇总转发（contracts.md「门面与 feature」一节所设想的抽象层
//! 转发在合流后因依赖方向不可行，汇总点上提至本门面，见
//! `container.rs` 头注）。
//!
//! 此外 [`container`] 模块提供跨容器 [`remux`](container::remux) 助手
//! （`ffmpeg -c copy` 等价，不转码）；[`stubs`] 模块是 M2+ 接口桩的
//! 聚合入口（语义边界见 docs/contracts.md「M2+ 接口桩契约」）。

pub mod container;

#[cfg(any(feature = "codec-stubs", feature = "filter-stubs"))]
pub mod stubs;

pub use ye_codec as codec;
pub use ye_device as device;
pub use ye_filter as filter;
pub use ye_pixfmt as pixfmt;
pub use ye_time as time;

/// 库版本号（与 workspace 版本一致）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
