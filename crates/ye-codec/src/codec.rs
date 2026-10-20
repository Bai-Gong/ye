//! 编解码器 trait（[`Decoder`]/[`Encoder`]）、工厂 fn 指针与编码配置。

use ye_pixfmt::{ChannelLayout, PixelFormat, SampleFormat};
use ye_time::Rational;

use crate::error::CodecError;
use crate::frame::{Decoded, Encoded, Frame};
use crate::media::CodecId;
use crate::packet::Packet;
use crate::params::CodecParameters;

/// 解码器 trait：对标 FFmpeg `avcodec_send_packet`/`avcodec_receive_frame`
/// 的推拉模型，实现内部持重排缓冲以支持 B 帧/解码器延迟。
///
/// ## 状态机契约
///
/// - `feed(&Packet)`：送入压缩包；**flush 排空后再 feed →
///   `Err(InvalidState(..))`**（对齐 FFmpeg `AVERROR_EOF`，走 `Result` 不 panic）；
/// - `flush()`：进入排空模式，随后持续 `pull` 直到返回
///   [`Decoded::Finished`]；
/// - `pull()`：有输出 → [`Decoded::Frame`]；暂无输出且未排空 →
///   [`Decoded::NeedInput`]；排空完毕 → [`Decoded::Finished`]，且此后
///   `pull` **幂等**返回 `Finished`。
///
/// ## 时戳语义
///
/// 收到的包时基任由解封装器决定，实现不得假设特定基数；输出帧的时戳
/// 语义由各实现自行文档化。
///
/// ## 线程安全
///
/// `Send + Sync` 上界使 `Box<dyn Decoder>` 可跨线程传递
/// （无上界时 `Box<dyn Decoder>: Send` 报 E0277，实测 rustc 1.99.0；
/// jian/duan 的 Tauri 多线程后端必需）。
pub trait Decoder: Send + Sync {
    /// 该解码器实现的编解码器标识。
    fn codec_id(&self) -> CodecId;

    /// 送入一个压缩包（推入内部缓冲，实际解码时机由实现决定）。
    ///
    /// # Errors
    /// 数据驱动失败 → [`CodecError::InvalidData`]；
    /// flush 排空后误用 → [`CodecError::InvalidState`]。
    fn feed(&mut self, packet: &Packet) -> Result<(), CodecError>;

    /// 进入排空模式：排空内部重排缓冲，随后 `pull` 直到 `Finished`。
    fn flush(&mut self);

    /// 拉取解码输出（见 trait 文档的状态机契约）。
    ///
    /// # Errors
    /// 数据驱动失败 → [`CodecError::InvalidData`] 等相应变体。
    fn pull(&mut self) -> Result<Decoded, CodecError>;
}

/// 编码器 trait，与 [`Decoder`] 对称（帧 → 压缩包）。
///
/// 状态机契约与线程安全上界同 [`Decoder`]：
/// flush 排空后再 feed → `Err(InvalidState(..))`；`Finished` 后 `pull` 幂等。
/// 输出包的时戳以 `EncoderConfig.time_base` 打戳（契约见 [`crate::Packet`]）。
pub trait Encoder: Send + Sync {
    /// 该编码器实现的编解码器标识。
    fn codec_id(&self) -> CodecId;

    /// 送入一帧待编码。
    ///
    /// # Errors
    /// 帧数据/参数不合法 → [`CodecError::InvalidData`]/[`CodecError::InvalidArguments`]；
    /// flush 排空后误用 → [`CodecError::InvalidState`]。
    fn feed(&mut self, frame: &Frame) -> Result<(), CodecError>;

    /// 进入排空模式：冲刷内部缓冲，随后 `pull` 直到 `Finished`。
    fn flush(&mut self);

    /// 拉取编码输出（见 [`Decoder`] 的状态机契约，对称适用）。
    ///
    /// # Errors
    /// 相应的 [`CodecError`] 变体。
    fn pull(&mut self) -> Result<Encoded, CodecError>;
}

/// 解码器工厂：普通 `fn` 指针（零依赖、天然 `Send + Sync`；
/// 闭包需求出现时再演进）。
///
/// 工厂消费容器层填好的 [`CodecParameters`]（含 `extradata` 等带外参数）。
pub type DecoderFactory = fn(&CodecParameters) -> Result<Box<dyn Decoder>, CodecError>;

/// 编码器工厂：普通 `fn` 指针，语义同 [`DecoderFactory`]，消费 [`EncoderConfig`]。
pub type EncoderFactory = fn(&EncoderConfig) -> Result<Box<dyn Encoder>, CodecError>;

/// 视频编码参数（公有字段、无不变式）。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoEncoderConfig {
    /// 输出帧宽（像素）。
    pub width: u32,
    /// 输出帧高（像素）。
    pub height: u32,
    /// 输入帧的像素格式（编码器按此解析送入的帧）。
    pub format: PixelFormat,
    /// 目标帧率（每秒次数）。
    pub frame_rate: Rational,
}

/// 音频编码参数（公有字段、无不变式）。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioEncoderConfig {
    /// 采样率（Hz）。
    pub sample_rate: u32,
    /// 声道布局。
    pub channel_layout: ChannelLayout,
    /// 输入帧的采样格式。
    pub format: SampleFormat,
}

/// 编码配置（公有字段、无不变式）：编码器工厂的完整输入。
///
/// `time_base` 是**输出包时戳的时基**（编码器打戳、封装器重标度的依据）；
/// `video`/`audio` 按 codec 的媒体类型二选一填。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderConfig {
    /// 目标编解码器标识。
    pub codec: CodecId,
    /// 输出包时戳的时基（秒/tick）。
    pub time_base: Rational,
    /// 目标码率（bit/s；`None` = 由编码器自选质量模式）。
    pub bit_rate: Option<u64>,
    /// 视频参数（视频编码时必填）。
    pub video: Option<VideoEncoderConfig>,
    /// 音频参数（音频编码时必填）。
    pub audio: Option<AudioEncoderConfig>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_config_is_value_comparable() {
        let a = EncoderConfig {
            codec: CodecId::H264,
            time_base: Rational::new(1, 25),
            bit_rate: Some(4_000_000),
            video: Some(VideoEncoderConfig {
                width: 1920,
                height: 1080,
                format: PixelFormat::Yuv420p,
                frame_rate: Rational::RATE_25,
            }),
            audio: None,
        };
        let b = a.clone();
        assert_eq!(a, b);
        let mut c = b.clone();
        c.bit_rate = None;
        assert_ne!(a, c);
        let audio = EncoderConfig {
            codec: CodecId::Opus,
            time_base: Rational::new(1, 48_000),
            bit_rate: Some(96_000),
            video: None,
            audio: Some(AudioEncoderConfig {
                sample_rate: 48_000,
                channel_layout: ChannelLayout::STEREO,
                format: SampleFormat::F32,
            }),
        };
        assert_ne!(a, audio);
    }

    /// 全库约定：六个抽象 trait 的 `Box<dyn …>` 必须可跨线程传递。
    #[test]
    fn trait_objects_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Box<dyn Decoder>>();
        assert_send_sync::<Box<dyn Encoder>>();
        assert_send_sync::<DecoderFactory>();
        assert_send_sync::<EncoderFactory>();
    }
}
