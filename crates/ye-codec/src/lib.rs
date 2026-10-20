//! 编解码器抽象：`Packet ⇄ Frame` 的 trait 与注册表。
//!
//! 对标 libavcodec。具体编解码器以独立 crate 落地
//! （如 `ye-codec-h264`、`ye-codec-aac`），由 `ye` 门面按 feature 汇总，
//! 避免单 crate 膨胀拖慢编译。
