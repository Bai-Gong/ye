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
//! | `mp4`   | 是   | MP4 容器读写（`ye::container::mp4`，即 `ye-container-mp4`：解封装 + `Mp4Writer` 封装，progressive 与 fMP4） |
//!
//! 新容器/编解码实现 crate 的接入规则：实现 crate 挂到对应抽象层的
//! optional 依赖与同名 feature 下，再在本表汇总转发（见
//! docs/contracts.md「门面与 feature」一节）。

pub use ye_codec as codec;
pub use ye_container as container;
pub use ye_device as device;
pub use ye_filter as filter;
pub use ye_pixfmt as pixfmt;
pub use ye_time as time;

/// 库版本号（与 workspace 版本一致）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
