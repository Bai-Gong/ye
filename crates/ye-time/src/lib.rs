//! 有理数时基、时间戳与帧率换算。
//!
//! 对标 libavutil 的 `AVRational` 与 `av_rescale_q`：
//! 全库时间统一以整数 tick 表示，避免浮点累积漂移；
//! tick 基数（每秒 tick 数）在落地时定，须可被常见帧率/采样率整除。
//!
//! ## 全局混流时钟 [`CANONICAL_TIME_BASE`]
//!
//! 1/7_056_000（秒/tick，即每秒 7,056,000 ticks）：
//! 7,056,000 = LCM(24, 25, 30, 50, 60, 120, 44100, 48000)，
//! 满足 architecture.md 的 tick 基数整除要求
//! （7056000/24 = 294000、/25 = 282240、/44100 = 160、/48000 = 147 均整除）。
//! 跨流时长/起点统一转到这里再对齐与比较。
//!
//! 取舍：不选 NTSC 精确基数（7056000×1001 ≈ 7.06e9）——那会让 canonical ticks
//! 在 i64 内的可用时长缩短约 1000 倍；NTSC 流有 ≤1 tick 的转写舍入
//! （≈0.14 ns），且 NTSC 只在流自身的有理数时基里精确
//! （见 [`Timestamp::to_canonical`]）。
//!
//! ## 表示与安全约定
//!
//! - 时基 = 秒/tick：`seconds = ticks · time_base`（与 FFmpeg 同向）；
//!   帧率/采样率常量（[`Rational::RATE_24`] 等）是「每秒次数」，
//!   [`Timestamp::from_frame_index`] 取其倒数作时基。
//! - 一切重标度/换算走 i128 中间量，结果须落回 i64/i32，否则 `Err(Overflow)`；
//!   重标度舍入模式对齐 `AV_ROUND_*`（默认 [`Rounding::NearInf`]），
//!   跨时基比较用 i128 精确交叉相乘（[`Timestamp::compare`]），
//!   禁止经由重标度实现比较（舍入会令不等值判等）。
//! - f64 仅用于 UI 显示与探查估算，不作权威值、不参与累积运算。
//! - 无哨兵值：没有 `AV_NOPTS_VALUE` 等价物，缺时戳一律 `Option<Timestamp>`。
//! - panic 仅出现在文档写明的「编程错误」处（本 crate 两处，见 [`Rational::new`]）；
//!   数据驱动（来自文件/码流）的路径一律 `Result`（见 [`Rational::try_new`]）。

mod error;
mod rational;
mod rounding;
mod timestamp;

pub use error::TimeError;
pub use rational::Rational;
pub use rounding::Rounding;
pub use timestamp::Timestamp;

/// 全局唯一混流时钟：1/7_056_000（秒/tick，即每秒 7,056,000 ticks）。
///
/// 7,056,000 = LCM(24, 25, 30, 50, 60, 120, 44100, 48000)，满足 architecture.md
/// 的 tick 基数整除要求；跨流时长/起点统一转到这里。
/// 常见整数速率（24/25/30/50/60/120/44100/48000）转到本时钟**无舍入**；
/// NTSC（·/1001）只在流自身的有理数时基里精确，转写有 ≤1 tick 舍入
/// （选 7056000 而非 NTSC 精确基数的取舍理由见 crate 根文档）。
pub const CANONICAL_TIME_BASE: Rational = Rational::new(1, 7_056_000);
