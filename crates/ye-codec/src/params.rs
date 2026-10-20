//! 编解码参数（容器层 ↔ 编解码器工厂的数据载体）。

use ye_pixfmt::{ChannelLayout, PixelFormat, SampleFormat};
use ye_time::Rational;

use crate::media::{CodecId, MediaType};

/// 视频参数（公有字段、无不变式），由容器层填、解码器工厂消费。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoParams {
    /// 帧宽（像素）。
    pub width: u32,
    /// 帧高（像素）。
    pub height: u32,
    /// 像素格式（容器/码流无法表达时为 `None`，由解码器探测）。
    pub pixel_format: Option<PixelFormat>,
    /// 帧率（每秒次数；容器未携带时为 `None`）。
    pub frame_rate: Option<Rational>,
}

/// 音频参数（公有字段、无不变式），由容器层填、解码器工厂消费。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioParams {
    /// 采样率（Hz）。
    pub sample_rate: u32,
    /// 声道布局（容器未携带时为 `None`）。
    pub channel_layout: Option<ChannelLayout>,
    /// 采样格式（容器未携带时为 `None`，由解码器决定）。
    pub sample_format: Option<SampleFormat>,
}

/// 编解码参数（公有字段、无不变式）：对标 FFmpeg `AVCodecParameters`。
///
/// `extradata` 容纳 SPS/PPS/ASC 等带外参数；`video`/`audio` 按
/// [`MediaType`] 至多一个有意义（契约不强制互斥，由填方负责语义）。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecParameters {
    /// 编解码器标识。
    pub codec: CodecId,
    /// 媒体类型。
    pub media_type: MediaType,
    /// 带外参数（SPS/PPS/ASC 等，容器盒子里原样搬运）。
    pub extradata: Option<Vec<u8>>,
    /// 名义码率（bit/s；可变码率或未知时 `None`）。
    pub bit_rate: Option<u64>,
    /// 视频参数（视频流才有）。
    pub video: Option<VideoParams>,
    /// 音频参数（音频流才有）。
    pub audio: Option<AudioParams>,
}

impl CodecParameters {
    /// 最小构造：`extradata`/`bit_rate`/`video`/`audio` 全 `None`，
    /// 其余字段由调用方按流信息逐个填。
    pub fn new(codec: CodecId, media_type: MediaType) -> CodecParameters {
        CodecParameters {
            codec,
            media_type,
            extradata: None,
            bit_rate: None,
            video: None,
            audio: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_defaults_all_optional_fields() {
        let p = CodecParameters::new(CodecId::H264, MediaType::Video);
        assert_eq!(p.codec, CodecId::H264);
        assert_eq!(p.media_type, MediaType::Video);
        assert_eq!(p.extradata, None);
        assert_eq!(p.bit_rate, None);
        assert_eq!(p.video, None);
        assert_eq!(p.audio, None);
    }

    #[test]
    fn parameters_are_value_comparable() {
        let a = CodecParameters::new(CodecId::Aac, MediaType::Audio);
        let mut b = CodecParameters::new(CodecId::Aac, MediaType::Audio);
        assert_eq!(a, b);
        b.audio = Some(AudioParams {
            sample_rate: 48_000,
            channel_layout: Some(ChannelLayout::STEREO),
            sample_format: Some(SampleFormat::F32),
        });
        assert_ne!(a, b);
        let mut c = CodecParameters::new(CodecId::H264, MediaType::Video);
        c.video = Some(VideoParams {
            width: 1920,
            height: 1080,
            pixel_format: Some(PixelFormat::Yuv420p),
            frame_rate: Some(Rational::RATE_25),
        });
        c.extradata = Some(vec![0x67, 0x64, 0x00]);
        assert_ne!(a, c);
    }
}
