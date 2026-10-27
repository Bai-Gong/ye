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
//! | `h264` | 否 | H.264 解码主链（`ye::codec_h264`，即 `ye-codec-h264`：Annex B/AVCC 输入，I 帧 CAVLC 闭环至 yuv420p `VideoFrame`；含 `register_h264` 注册——取代 `codec-stubs` 中 H.264 解码桩的位置） |
//! | `aac` | 否 | AAC-LC 解码主链（`ye::codec_aac`，即 `ye-codec-aac`：ADTS/LOAS/esds 输入，noiseless 码表 + 重量化 + M/S 强度 + TNS + IMDCT 至 packed f32 `AudioFrame`，时戳透传；含 `register_aac` 注册——取代 `codec-stubs` 中 AAC 解码桩的位置） |
//! | `png` | 否 | PNG 编码（`ye::png`，即 `ye-png`：encode-only，RGBA8 → PNG，自实现 deflate（stored/fixed-Huffman）+ CRC32 + zlib 包装 + None/Sub 自适应行过滤，零第三方依赖；无损性经 ffmpeg oracle 逐字节 0 容差对拍——剪的抽帧缩略图出口） |
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
//! （`ffmpeg -c copy` 等价，不转码）；[`convert`] 模块是转码泵（固定
//! 链序 解码→像素转换→scale→编码，µs 进度 + `AtomicBool` 取消）——
//! M3 集成阶段起两条路径均真实现：`convert_frames`（帧消费）与
//! `convert`（视频重编码 H.264 + MP4 封装、音频包直通，解码/编码器
//! 经注入的 [`codec::CodecRegistry`] 取得，泵本体不依赖具体实现
//! crate，故无 feature 门；像素核缺省桥接 `convert::filter_ops()`）；
//! [`stubs`] 模块是 M2+ 接口桩的聚合入口（语义边界见
//! docs/contracts.md「M2+ 接口桩契约」）。

#[cfg(feature = "aac")]
pub mod codec_aac;

#[cfg(feature = "h264")]
pub mod codec_h264;

pub mod container;
pub mod convert;

#[cfg(feature = "png")]
pub mod png;

#[cfg(any(feature = "codec-stubs", feature = "filter-stubs"))]
pub mod stubs;

pub use ye_codec as codec;
pub use ye_device as device;
pub use ye_filter as filter;
pub use ye_pixfmt as pixfmt;
pub use ye_time as time;

/// 库版本号（与 workspace 版本一致）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
