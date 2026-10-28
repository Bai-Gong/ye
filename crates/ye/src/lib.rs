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
//! | `aac` | 否 | AAC-LC 解码+编码（`ye::codec_aac`，即 `ye-codec-aac`：解码——ADTS/LOAS/esds 输入，noiseless 码表 + 重量化 + M/S 强度 + TNS + IMDCT 至 packed f32 `AudioFrame`，时戳透传；编码——packed f32 输入，心理声学 + 前向 MDCT + 码率环/FIL 填充的恒定码率 → ADTS 包体（长窗 SCE/CPE），带外参数 `AacEncoder::audio_specific_config`/`esds_extradata` 与自由函数 `esds_extradata`；含 `register_aac`/`register_aac_encoder` 注册——解码侧取代 `codec-stubs` 中 AAC 解码桩的位置，编码侧接管 AAC 编码位） |
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
//! [`compose`] 模块是合成泵（M4 首轮：顺序拼接——多段 MP4 各带源内
//! 裁剪窗口 → 单个 MP4，视频 H.264 重编码（CFR 桶采样）+ 音频 AAC-LC
//! 重编码跨段连续累积，注册表注入与进度/取消契约同 convert）；
//! [`stubs`] 模块是 M2+ 接口桩的聚合入口（语义边界见
//! docs/contracts.md「M2+ 接口桩契约」）；[`waveform`] 模块是波形峰值
//! 出口（剪的波形）：纯核（packed f32 交织样本 → 每桶每声道
//! min/max/rms）无条件编译，消费助手 [`waveform::audio_peaks`]/
//! [`waveform::audio_peaks_with_count`]（probe 前置流布局校验 +
//! `convert_frames` 解码接线）随既有 `mp4`+`aac` feature 组合开启，
//! 不新增 feature 名；[`frame_grab`] 模块是抽帧门面（剪的缩略图）：
//! [`frame_grab::decode_frame_rgba`]（`convert_frames` 泵、链
//! yuv420p→rgba8、取首个 pts ≥ 时点的视频帧即置 cancel）与
//! [`frame_grab::grab_frame_png`]（组合 [`png::encode_rgba`]）随既有
//! `mp4`+`h264`+`png` feature 组合开启，不新增 feature 名。

#[cfg(feature = "aac")]
pub mod codec_aac;

#[cfg(feature = "h264")]
pub mod codec_h264;

pub mod compose;
pub mod container;
pub mod convert;

#[cfg(all(feature = "mp4", feature = "h264", feature = "png"))]
pub mod frame_grab;

#[cfg(feature = "png")]
pub mod png;

#[cfg(any(feature = "codec-stubs", feature = "filter-stubs"))]
pub mod stubs;

pub mod waveform;

pub use ye_codec as codec;
pub use ye_device as device;
pub use ye_filter as filter;
pub use ye_pixfmt as pixfmt;
pub use ye_time as time;

/// 库版本号（与 workspace 版本一致）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
