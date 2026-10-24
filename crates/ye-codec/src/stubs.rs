//! M2+ 接口桩：H.264 / AAC 编解码器的占位实现。
//!
//! ## 桩的语义边界
//!
//! - **可注册、注册项可查**：[`register_stubs`] 把四个工厂挂进
//!   [`CodecRegistry`]，`supports_decoder`/`supports_encoder`/
//!   `decoder_ids`/`encoder_ids` 随即可见；
//! - **可构造**：经注册表 `create_decoder`/`create_encoder`（或直接调
//!   工厂）一律成功，返回 `codec_id()` 可查的桩对象——工厂不校验参数，
//!   因为尚无真实的初始化语义可校验；
//! - **不可用**：`feed`/`pull` 调用即返回 [`CodecError::UnsupportedStub`]，
//!   文案注明交付里程碑（H.264/AAC 解码 → M2，编码 → M4）；
//!   `flush` 为无操作（实现交付前状态机无意义）。
//!
//! 桩不持状态、不解析码流。真实实现按本 crate 约定以独立 crate
//! （`ye-codec-h264`、`ye-codec-aac`）落地并接入 feature 后，本模块
//! 整体删除、由真实工厂替换注册。

use crate::codec::{Decoder, Encoder, EncoderConfig};
use crate::error::CodecError;
use crate::frame::{Decoded, Encoded, Frame};
use crate::media::CodecId;
use crate::packet::Packet;
use crate::params::CodecParameters;
use crate::registry::CodecRegistry;

// ---- 桩文案（里程碑标注为裁决要求，行为测试断言其存在） ----

/// H.264 解码桩的失败文案（M2：H.264 软件解码交付）。
const H264_DECODE_MSG: &str = "h264 decode not implemented (桩：M2 交付)";
/// H.264 编码桩的失败文案（M4：H.264 软件编码交付）。
const H264_ENCODE_MSG: &str = "h264 encode not implemented (桩：M4 交付)";
/// AAC 解码桩的失败文案（M2：AAC-LC 解码交付）。
const AAC_DECODE_MSG: &str = "aac decode not implemented (桩：M2 交付)";
/// AAC 编码桩的失败文案（M4：AAC-LC 编码交付）。
const AAC_ENCODE_MSG: &str = "aac encode not implemented (桩：M4 交付)";

// ---- 桩类型（模块私有：构造只经工厂/注册表，无直接 new 的必要） ----

/// 解码器桩：`feed`/`pull` 一律 [`CodecError::UnsupportedStub`]。
struct DecoderStub {
    /// 桩代表的编解码器标识（`codec_id()` 的返回值）。
    codec: CodecId,
    /// 失败文案（含里程碑标注）。
    message: &'static str,
}

impl Decoder for DecoderStub {
    fn codec_id(&self) -> CodecId {
        self.codec
    }

    fn feed(&mut self, _packet: &Packet) -> Result<(), CodecError> {
        Err(CodecError::UnsupportedStub(self.message))
    }

    /// 无操作：实现交付前状态机无意义（见模块文档「不可用」）。
    fn flush(&mut self) {}

    fn pull(&mut self) -> Result<Decoded, CodecError> {
        Err(CodecError::UnsupportedStub(self.message))
    }
}

/// 编码器桩：`feed`/`pull` 一律 [`CodecError::UnsupportedStub`]。
struct EncoderStub {
    /// 桩代表的编解码器标识（`codec_id()` 的返回值）。
    codec: CodecId,
    /// 失败文案（含里程碑标注）。
    message: &'static str,
}

impl Encoder for EncoderStub {
    fn codec_id(&self) -> CodecId {
        self.codec
    }

    fn feed(&mut self, _frame: &Frame) -> Result<(), CodecError> {
        Err(CodecError::UnsupportedStub(self.message))
    }

    /// 无操作：实现交付前状态机无意义（见模块文档「不可用」）。
    fn flush(&mut self) {}

    fn pull(&mut self) -> Result<Encoded, CodecError> {
        Err(CodecError::UnsupportedStub(self.message))
    }
}

// ---- 工厂（签名即 [`crate::DecoderFactory`]/[`crate::EncoderFactory`]) ----

/// H.264 解码器桩工厂：任意 [`CodecParameters`] 都构造成功。
pub fn h264_decoder_factory(_params: &CodecParameters) -> Result<Box<dyn Decoder>, CodecError> {
    Ok(Box::new(DecoderStub {
        codec: CodecId::H264,
        message: H264_DECODE_MSG,
    }))
}

/// H.264 编码器桩工厂：任意 [`EncoderConfig`] 都构造成功。
pub fn h264_encoder_factory(_config: &EncoderConfig) -> Result<Box<dyn Encoder>, CodecError> {
    Ok(Box::new(EncoderStub {
        codec: CodecId::H264,
        message: H264_ENCODE_MSG,
    }))
}

/// AAC 解码器桩工厂：任意 [`CodecParameters`] 都构造成功。
pub fn aac_decoder_factory(_params: &CodecParameters) -> Result<Box<dyn Decoder>, CodecError> {
    Ok(Box::new(DecoderStub {
        codec: CodecId::Aac,
        message: AAC_DECODE_MSG,
    }))
}

/// AAC 编码器桩工厂：任意 [`EncoderConfig`] 都构造成功。
pub fn aac_encoder_factory(_config: &EncoderConfig) -> Result<Box<dyn Encoder>, CodecError> {
    Ok(Box::new(EncoderStub {
        codec: CodecId::Aac,
        message: AAC_ENCODE_MSG,
    }))
}

/// 一次性注册全部桩（H.264/AAC × 解码/编码）到 `registry`。
///
/// 已有同 codec 工厂时按注册表的替换语义覆盖（返回值被丢弃）。
/// 语义边界见模块文档：注册后可查、可构造，`feed`/`pull` 不可用。
pub fn register_stubs(registry: &mut CodecRegistry) {
    registry.register_decoder(CodecId::H264, h264_decoder_factory);
    registry.register_decoder(CodecId::Aac, aac_decoder_factory);
    registry.register_encoder(CodecId::H264, h264_encoder_factory);
    registry.register_encoder(CodecId::Aac, aac_encoder_factory);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::VideoFrame;
    use ye_pixfmt::{ColorDesc, PixelFormat};
    use ye_time::Rational;

    fn stub_registry() -> CodecRegistry {
        let mut r = CodecRegistry::new();
        register_stubs(&mut r);
        r
    }

    fn h264_video_params() -> CodecParameters {
        CodecParameters::new(CodecId::H264, crate::media::MediaType::Video)
    }

    fn aac_audio_params() -> CodecParameters {
        CodecParameters::new(CodecId::Aac, crate::media::MediaType::Audio)
    }

    fn h264_encoder_config() -> EncoderConfig {
        EncoderConfig {
            codec: CodecId::H264,
            time_base: Rational::new(1, 25),
            bit_rate: Some(4_000_000),
            video: Some(crate::codec::VideoEncoderConfig {
                width: 1920,
                height: 1080,
                format: PixelFormat::Yuv420p,
                frame_rate: Rational::RATE_25,
            }),
            audio: None,
        }
    }

    fn aac_encoder_config() -> EncoderConfig {
        EncoderConfig {
            codec: CodecId::Aac,
            time_base: Rational::new(1, 48_000),
            bit_rate: Some(128_000),
            video: None,
            audio: Some(crate::codec::AudioEncoderConfig {
                sample_rate: 48_000,
                channel_layout: ye_pixfmt::ChannelLayout::STEREO,
                format: ye_pixfmt::SampleFormat::F32,
            }),
        }
    }

    fn gray_frame() -> Frame {
        Frame::Video(
            VideoFrame::new(
                PixelFormat::Gray8,
                16,
                16,
                ColorDesc::UNSPECIFIED,
                None,
                None,
            )
            .unwrap(),
        )
    }

    /// 断言 `result` 是 `UnsupportedStub`，且文案含里程碑标注与编解码器名。
    fn expect_stub_err<T: core::fmt::Debug>(
        result: Result<T, CodecError>,
        marker: &str,
        codec: &str,
    ) {
        match result {
            Err(CodecError::UnsupportedStub(msg)) => {
                assert!(msg.contains(marker), "msg missing milestone: {msg}");
                assert!(msg.contains(codec), "msg missing codec name: {msg}");
            }
            other => panic!("expected UnsupportedStub, got {other:?}"),
        }
    }

    // ---- 可注册、注册项可查 ----

    #[test]
    fn register_stubs_makes_all_four_queryable() {
        let r = stub_registry();
        // supports_* 可查
        assert!(r.supports_decoder(CodecId::H264));
        assert!(r.supports_decoder(CodecId::Aac));
        assert!(r.supports_encoder(CodecId::H264));
        assert!(r.supports_encoder(CodecId::Aac));
        // 未注册的 codec 不受影响
        assert!(!r.supports_decoder(CodecId::Hevc));
        assert!(!r.supports_encoder(CodecId::Flac));
        // ids 可查（BTreeMap 键序：H264 < Aac，CodecId derive Ord）
        assert_eq!(
            r.decoder_ids().collect::<Vec<_>>(),
            vec![CodecId::H264, CodecId::Aac]
        );
        assert_eq!(
            r.encoder_ids().collect::<Vec<_>>(),
            vec![CodecId::H264, CodecId::Aac]
        );
    }

    // ---- 可构造 ----

    #[test]
    fn stubs_are_creatable_via_registry_and_factories() {
        let r = stub_registry();
        let dec = r.create_decoder(&h264_video_params()).unwrap();
        assert_eq!(dec.codec_id(), CodecId::H264);
        let aac_dec = r.create_decoder(&aac_audio_params()).unwrap();
        assert_eq!(aac_dec.codec_id(), CodecId::Aac);
        let enc = r.create_encoder(&h264_encoder_config()).unwrap();
        assert_eq!(enc.codec_id(), CodecId::H264);
        let aac_enc = r.create_encoder(&aac_encoder_config()).unwrap();
        assert_eq!(aac_enc.codec_id(), CodecId::Aac);
        // 工厂也可直接调用（不经注册表）
        let direct = h264_decoder_factory(&h264_video_params()).unwrap();
        assert_eq!(direct.codec_id(), CodecId::H264);
    }

    // ---- 不可用：解码桩 feed/pull 一律 UnsupportedStub，文案含里程碑 ----

    #[test]
    fn h264_decoder_stub_errors_with_m2_marker() {
        let r = stub_registry();
        let mut dec = r.create_decoder(&h264_video_params()).unwrap();
        let pkt = Packet::empty(0);
        // feed 与 pull 都失败，变体为 UnsupportedStub
        expect_stub_err(dec.feed(&pkt), "桩：M2 交付", "h264");
        expect_stub_err(dec.pull(), "桩：M2 交付", "h264");
        // Display 文案同样含里程碑标注
        let display = dec.feed(&pkt).unwrap_err().to_string();
        assert!(
            display.contains("桩：M2 交付"),
            "display missing milestone: {display}"
        );
        // flush 无操作：调用后行为不变
        dec.flush();
        assert!(matches!(dec.pull(), Err(CodecError::UnsupportedStub(_))));
    }

    #[test]
    fn aac_decoder_stub_errors_with_m2_marker() {
        let r = stub_registry();
        let mut dec = r.create_decoder(&aac_audio_params()).unwrap();
        assert_eq!(dec.codec_id(), CodecId::Aac);
        match dec.pull() {
            Err(CodecError::UnsupportedStub(msg)) => {
                assert!(msg.contains("桩：M2 交付"), "msg missing milestone: {msg}");
                assert!(msg.contains("aac"), "msg missing codec name: {msg}");
            }
            other => panic!("expected UnsupportedStub, got {other:?}"),
        }
        assert!(dec.feed(&Packet::empty(1)).is_err());
    }

    // ---- 不可用：编码桩 feed/pull 一律 UnsupportedStub，文案含里程碑 ----

    #[test]
    fn encoder_stubs_error_with_m4_marker() {
        let r = stub_registry();
        let frame = gray_frame();

        let mut h264 = r.create_encoder(&h264_encoder_config()).unwrap();
        expect_stub_err(h264.feed(&frame), "桩：M4 交付", "h264");
        expect_stub_err(h264.pull(), "桩：M4 交付", "h264");
        let display = h264.feed(&frame).unwrap_err().to_string();
        assert!(
            display.contains("桩：M4 交付"),
            "display missing milestone: {display}"
        );

        let mut aac = r.create_encoder(&aac_encoder_config()).unwrap();
        match aac.feed(&frame) {
            Err(CodecError::UnsupportedStub(msg)) => {
                assert!(msg.contains("桩：M4 交付"), "msg missing milestone: {msg}");
                assert!(msg.contains("aac"), "msg missing codec name: {msg}");
            }
            other => panic!("expected UnsupportedStub, got {other:?}"),
        }
        // flush 无操作
        aac.flush();
        assert!(matches!(aac.pull(), Err(CodecError::UnsupportedStub(_))));
    }

    // ---- 桩是 trait 对象安全的常规公民 ----

    #[test]
    fn stub_objects_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        let r = stub_registry();
        let dec = r.create_decoder(&h264_video_params()).unwrap();
        let enc = r.create_encoder(&h264_encoder_config()).unwrap();
        assert_send_sync::<Box<dyn Decoder>>();
        assert_send_sync::<Box<dyn Encoder>>();
        // 桩对象本身满足上界（move 进闭包编译即验证）
        let send_dec = move || drop(dec);
        let send_enc = move || drop(enc);
        send_dec();
        send_enc();
    }
}
