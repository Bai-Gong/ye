//! # 百工·冶 (Ye)
//!
//! 对标 FFmpeg（libav\*）的媒体编解码与处理基础库——门面 crate。
//!
//! 各创作工具只应依赖 `ye`，由它统一 re-export 各分层能力；
//! 具体编解码器与容器（如未来的 `ye-codec-h264`、`ye-container-mp4`）
//! 以独立 crate 落地，在本 crate 按 feature 汇总开启。

pub use ye_codec as codec;
pub use ye_container as container;
pub use ye_device as device;
pub use ye_filter as filter;
pub use ye_pixfmt as pixfmt;
pub use ye_time as time;

/// 库版本号（与 workspace 版本一致）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
