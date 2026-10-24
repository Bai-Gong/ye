//! ye-conformance：FFmpeg oracle 一致性框架（对标 FATE 的最小可用层）。
//!
//! 施工图①交付：向量清单（入库）+ 获取脚本（向量不入库，拉到本地
//! 缓存）+ 对拍 runner + 运行摘要行 [`report::Report::summary_line`]
//! （终验硬断言的接口）+ YUV/PCM 对拍器（比较器本体，M2 解码落地后
//! 消费）。容差按 codec 逐条写死于 [`tolerance`]，只准收紧。
//!
//! ## 分层
//!
//! - lib（本 crate 可离线单测的纯逻辑）：sha256 / manifest 解析 /
//!   容差表 / YUV/PCM 比较器 / 摘要行——`cargo test --workspace` 覆盖，
//!   不依赖网络与 ffmpeg；
//! - bin（`src/main.rs`）：编排——缓存对账、ffmpeg/ffprobe oracle
//!   调用、NAL 序列/SPS 字段对拍、逐向量输出与摘要行打印。oracle
//!   缺席或向量未取到 → skip 并落摘要行（开发期合法）；缓存文件在
//!   而 sha 不符 → **照跑不 skip**、计 `sha_ok=false`（2026-10-24
//!   施工图①裁决：sha 漂移是信号）；对拍失败 → failed 并以退出码
//!   1 收场。
//!
//! ## 零第三方依赖
//!
//! sha256 手写（FIPS 180-4，NIST 向量锁定）；ffmpeg/ffprobe 经
//! `std::process::Command` 调用，不链接。

pub mod compare;
pub mod manifest;
pub mod report;
pub mod sha256;
pub mod tolerance;
