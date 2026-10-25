//! 门面的 PNG 编码层：`ye-png` 汇总转发（`png` feature）。
//!
//! - RGBA8 → PNG 编码（施工图④ M2，encode-only）：自实现 deflate
//!   （stored / fixed-Huffman 两档）+ CRC32 + zlib 包装 + 行过滤
//!   （None/Sub 自适应）；
//! - [`encode_rgba`]：默认档（fixed-Huffman + Adaptive）一步出口，
//!   剪的抽帧缩略图管线（seek 关键帧 → 解码 → 转换 → scale → PNG）
//!   的最后一环；
//! - [`encode_rgba_with`] / [`DeflateProfile`] / [`FilterStrategy`]：
//!   显式档位与策略。
//!
//! 无损性验证归 ye-png 的 ffmpeg oracle 对拍（逐字节 0 容差），
//! 见 `crates/ye-png/tests/oracle_png.rs`。

pub use ye_png::{
    DeflateProfile, FilterStrategy, PngError, SIGNATURE, adler32, crc32, encode_rgba,
    encode_rgba_with,
};
