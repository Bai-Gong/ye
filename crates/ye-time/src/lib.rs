//! 有理数时基、时间戳与帧率换算。
//!
//! 对标 libavutil 的 `AVRational` 与 `av_rescale_q`：
//! 全库时间统一以整数 tick 表示，避免浮点累积漂移；
//! tick 基数（每秒 tick 数）在落地时定，须可被常见帧率/采样率整除。
