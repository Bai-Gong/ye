//! 编解码器注册表（显式构造与传递，无全局单例，不用 once_cell）。

use std::collections::BTreeMap;

use crate::codec::{Decoder, DecoderFactory, Encoder, EncoderConfig, EncoderFactory};
use crate::error::CodecError;
use crate::media::CodecId;
use crate::params::CodecParameters;

/// 显式注册表：解码器/编码器工厂的 `BTreeMap`（键 [`CodecId`] 依赖
/// `Ord` derive）。`new()` 后为空，具体 codec 由 `ye-codec-*` crate
/// 经 feature 自行注册进自己构造的实例。
///
/// 同名重复注册返回旧工厂（`BTreeMap::insert` 语义）。
///
/// derive 清单：仅 `Debug`（`fn` 指针字段可 derive；
/// 不作 `Clone`/`PartialEq` 要求——工厂按指针值比较意义有限）。
#[derive(Debug)]
pub struct CodecRegistry {
    decoders: BTreeMap<CodecId, DecoderFactory>,
    encoders: BTreeMap<CodecId, EncoderFactory>,
}

impl CodecRegistry {
    /// 空注册表。
    pub fn new() -> CodecRegistry {
        CodecRegistry {
            decoders: BTreeMap::new(),
            encoders: BTreeMap::new(),
        }
    }

    /// 注册解码器工厂；该 codec 已有工厂时返回旧工厂（替换语义）。
    pub fn register_decoder(
        &mut self,
        codec: CodecId,
        factory: DecoderFactory,
    ) -> Option<DecoderFactory> {
        self.decoders.insert(codec, factory)
    }

    /// 注册编码器工厂；该 codec 已有工厂时返回旧工厂（替换语义）。
    pub fn register_encoder(
        &mut self,
        codec: CodecId,
        factory: EncoderFactory,
    ) -> Option<EncoderFactory> {
        self.encoders.insert(codec, factory)
    }

    /// 按 [`CodecParameters::codec`] 查表创建解码器。
    ///
    /// # Errors
    /// 未注册 → [`CodecError::NotRegistered`]；工厂自身拒绝 → 原样透传
    /// （如 [`CodecError::UnsupportedCodec`]/[`CodecError::InvalidArguments`]）。
    pub fn create_decoder(&self, params: &CodecParameters) -> Result<Box<dyn Decoder>, CodecError> {
        let factory = self
            .decoders
            .get(&params.codec)
            .ok_or(CodecError::NotRegistered(params.codec))?;
        factory(params)
    }

    /// 按 [`EncoderConfig::codec`] 查表创建编码器。
    ///
    /// # Errors
    /// 未注册 → [`CodecError::NotRegistered`]；工厂自身拒绝 → 原样透传。
    pub fn create_encoder(&self, config: &EncoderConfig) -> Result<Box<dyn Encoder>, CodecError> {
        let factory = self
            .encoders
            .get(&config.codec)
            .ok_or(CodecError::NotRegistered(config.codec))?;
        factory(config)
    }

    /// 是否已注册该 codec 的解码器工厂。
    pub fn supports_decoder(&self, codec: CodecId) -> bool {
        self.decoders.contains_key(&codec)
    }

    /// 是否已注册该 codec 的编码器工厂。
    pub fn supports_encoder(&self, codec: CodecId) -> bool {
        self.encoders.contains_key(&codec)
    }

    /// 已注册解码器的 codec（`BTreeMap` 键序，即 `CodecId` derive 的 `Ord`）。
    pub fn decoder_ids(&self) -> impl Iterator<Item = CodecId> + '_ {
        self.decoders.keys().copied()
    }

    /// 已注册编码器的 codec（`BTreeMap` 键序）。
    pub fn encoder_ids(&self) -> impl Iterator<Item = CodecId> + '_ {
        self.encoders.keys().copied()
    }
}

impl Default for CodecRegistry {
    /// 缺省为空注册表（等价 [`CodecRegistry::new`]）。
    fn default() -> Self {
        CodecRegistry::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Frame;
    use crate::packet::Packet;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use ye_pixfmt::{ColorDesc, PixelFormat};
    use ye_time::Rational;

    // ---- 测试用 mock 解码器/编码器（crate 内状态机验证） ----

    /// 直通 mock：每个喂入的包产出一帧 16×16 灰度视频帧（pts 透传）。
    struct MockDecoder {
        codec: CodecId,
        queue: VecDeque<Frame>,
        flushed: bool,
    }

    impl crate::codec::Decoder for MockDecoder {
        fn codec_id(&self) -> CodecId {
            self.codec
        }

        fn feed(&mut self, packet: &Packet) -> Result<(), CodecError> {
            if self.flushed {
                return Err(CodecError::InvalidState(
                    "decoder already flushed and drained",
                ));
            }
            let frame = crate::frame::VideoFrame::new(
                PixelFormat::Gray8,
                16,
                16,
                ColorDesc::UNSPECIFIED,
                packet.pts,
                packet.duration,
            )?;
            self.queue.push_back(Frame::Video(frame));
            Ok(())
        }

        fn flush(&mut self) {
            self.flushed = true;
        }

        fn pull(&mut self) -> Result<crate::frame::Decoded, CodecError> {
            if let Some(frame) = self.queue.pop_front() {
                return Ok(crate::frame::Decoded::Frame(frame));
            }
            if self.flushed {
                Ok(crate::frame::Decoded::Finished)
            } else {
                Ok(crate::frame::Decoded::NeedInput)
            }
        }
    }

    /// 对称 mock：每个喂入的帧产出一个空负载包（pts 透传）。
    struct MockEncoder {
        codec: CodecId,
        queue: VecDeque<Packet>,
        flushed: bool,
    }

    impl crate::codec::Encoder for MockEncoder {
        fn codec_id(&self) -> CodecId {
            self.codec
        }

        fn feed(&mut self, frame: &Frame) -> Result<(), CodecError> {
            if self.flushed {
                return Err(CodecError::InvalidState(
                    "encoder already flushed and drained",
                ));
            }
            let mut packet = Packet::empty(0);
            packet.pts = frame.pts();
            self.queue.push_back(packet);
            Ok(())
        }

        fn flush(&mut self) {
            self.flushed = true;
        }

        fn pull(&mut self) -> Result<crate::frame::Encoded, CodecError> {
            if let Some(packet) = self.queue.pop_front() {
                return Ok(crate::frame::Encoded::Packet(packet));
            }
            if self.flushed {
                Ok(crate::frame::Encoded::Finished)
            } else {
                Ok(crate::frame::Encoded::NeedInput)
            }
        }
    }

    fn mock_decoder_factory(
        params: &CodecParameters,
    ) -> Result<Box<dyn crate::codec::Decoder>, CodecError> {
        Ok(Box::new(MockDecoder {
            codec: params.codec,
            queue: VecDeque::new(),
            flushed: false,
        }))
    }

    fn mock_encoder_factory(
        config: &EncoderConfig,
    ) -> Result<Box<dyn crate::codec::Encoder>, CodecError> {
        Ok(Box::new(MockEncoder {
            codec: config.codec,
            queue: VecDeque::new(),
            flushed: false,
        }))
    }

    /// 供“重复注册返回旧工厂”断言的第二个解码器工厂。
    fn other_decoder_factory(
        params: &CodecParameters,
    ) -> Result<Box<dyn crate::codec::Decoder>, CodecError> {
        Ok(Box::new(MockDecoder {
            codec: params.codec,
            queue: VecDeque::new(),
            flushed: false,
        }))
    }

    fn other_encoder_factory(
        config: &EncoderConfig,
    ) -> Result<Box<dyn crate::codec::Encoder>, CodecError> {
        Ok(Box::new(MockEncoder {
            codec: config.codec,
            queue: VecDeque::new(),
            flushed: false,
        }))
    }

    fn tb() -> Rational {
        Rational::new(1, 25)
    }

    // ---- testPlan §6：Decoder 状态机 ----

    #[test]
    fn decoder_state_machine() {
        let mut dec = MockDecoder {
            codec: CodecId::H264,
            queue: VecDeque::new(),
            flushed: false,
        };
        // 未 feed 时 pull == NeedInput
        assert_eq!(dec.pull(), Ok(crate::frame::Decoded::NeedInput));

        // feed 后 pull 得 Frame（pts 透传）
        let mut pkt = Packet::empty(0);
        pkt.data = Arc::from(vec![0x00, 0x01, 0x02]);
        pkt.pts = Some(ye_time::Timestamp::new(11, tb()));
        dec.feed(&pkt).unwrap();
        match dec.pull() {
            Ok(crate::frame::Decoded::Frame(f)) => {
                assert_eq!(f.media_type(), crate::media::MediaType::Video);
                assert_eq!(f.pts(), Some(ye_time::Timestamp::new(11, tb())));
            }
            other => panic!("expected frame, got {other:?}"),
        }
        // 队列已空、未 flush → NeedInput
        assert_eq!(dec.pull(), Ok(crate::frame::Decoded::NeedInput));

        // flush 后连续 pull 直到 Finished
        dec.feed(&pkt).unwrap();
        dec.flush();
        assert!(matches!(dec.pull(), Ok(crate::frame::Decoded::Frame(_))));
        assert_eq!(dec.pull(), Ok(crate::frame::Decoded::Finished));
        // 再 pull 幂等 Finished
        assert_eq!(dec.pull(), Ok(crate::frame::Decoded::Finished));
        // 再 feed → Err(InvalidState)
        assert_eq!(
            dec.feed(&pkt),
            Err(CodecError::InvalidState(
                "decoder already flushed and drained"
            ))
        );
    }

    // ---- testPlan §6：Encoder 状态机（对称） ----

    #[test]
    fn encoder_state_machine() {
        let mut enc = MockEncoder {
            codec: CodecId::H264,
            queue: VecDeque::new(),
            flushed: false,
        };
        // 未 feed 时 pull == NeedInput
        assert_eq!(enc.pull(), Ok(crate::frame::Encoded::NeedInput));

        let frame = Frame::Video(
            crate::frame::VideoFrame::new(
                PixelFormat::Gray8,
                16,
                16,
                ColorDesc::UNSPECIFIED,
                Some(ye_time::Timestamp::new(3, tb())),
                None,
            )
            .unwrap(),
        );
        enc.feed(&frame).unwrap();
        match enc.pull() {
            Ok(crate::frame::Encoded::Packet(p)) => {
                assert_eq!(p.pts, Some(ye_time::Timestamp::new(3, tb())));
            }
            other => panic!("expected packet, got {other:?}"),
        }
        assert_eq!(enc.pull(), Ok(crate::frame::Encoded::NeedInput));

        // flush 后排空 → Finished，幂等，再 feed → Err(InvalidState)
        enc.feed(&frame).unwrap();
        enc.flush();
        assert!(matches!(enc.pull(), Ok(crate::frame::Encoded::Packet(_))));
        assert_eq!(enc.pull(), Ok(crate::frame::Encoded::Finished));
        assert_eq!(enc.pull(), Ok(crate::frame::Encoded::Finished));
        assert_eq!(
            enc.feed(&frame),
            Err(CodecError::InvalidState(
                "encoder already flushed and drained"
            ))
        );
    }

    // ---- testPlan §7：注册表 ----

    #[test]
    fn registry_new_is_empty_and_default_matches() {
        let r = CodecRegistry::new();
        assert_eq!(r.decoder_ids().count(), 0);
        assert_eq!(r.encoder_ids().count(), 0);
        assert!(!r.supports_decoder(CodecId::H264));
        assert!(!r.supports_encoder(CodecId::Aac));
        let d = CodecRegistry::default();
        assert_eq!(d.decoder_ids().count(), 0);
        assert_eq!(d.encoder_ids().count(), 0);
    }

    /// fn 指针按值比较在跨 codegen unit 时地址不保证唯一（rustc 警告所指），
    /// 但本断言只依赖“同一测试二进制内具名 fn 的单态化地址稳定”，
    /// 目的是验证 `BTreeMap::insert` 返回的正是先前注册的旧工厂。
    #[allow(unpredictable_function_pointer_comparisons)]
    #[test]
    fn registry_duplicate_registration_returns_old_factory() {
        let mut r = CodecRegistry::new();
        // 首次注册返回 None
        assert_eq!(
            r.register_decoder(CodecId::H264, mock_decoder_factory),
            None
        );
        assert_eq!(
            r.register_encoder(CodecId::H264, mock_encoder_factory),
            None
        );
        // 重复注册返回旧工厂（fn 指针按值判等）
        let old_dec = r.register_decoder(CodecId::H264, other_decoder_factory);
        assert_eq!(old_dec, Some(mock_decoder_factory as DecoderFactory));
        let old_enc = r.register_encoder(CodecId::H264, other_encoder_factory);
        assert_eq!(old_enc, Some(mock_encoder_factory as EncoderFactory));
        // 替换后仍只有一个条目
        assert_eq!(r.decoder_ids().collect::<Vec<_>>(), vec![CodecId::H264]);
    }

    #[test]
    fn registry_create_decoder_not_registered() {
        let r = CodecRegistry::new();
        let params = CodecParameters::new(CodecId::H264, crate::media::MediaType::Video);
        // Ok 侧是 Box<dyn Decoder>（无 Debug/PartialEq），用 matches! 断言 Err 变体
        assert!(matches!(
            r.create_decoder(&params),
            Err(CodecError::NotRegistered(CodecId::H264))
        ));
    }

    #[test]
    fn registry_create_encoder_not_registered() {
        let r = CodecRegistry::new();
        let config = EncoderConfig {
            codec: CodecId::Opus,
            time_base: Rational::new(1, 48_000),
            bit_rate: None,
            video: None,
            audio: None,
        };
        assert!(matches!(
            r.create_encoder(&config),
            Err(CodecError::NotRegistered(CodecId::Opus))
        ));
    }

    #[test]
    fn registry_create_passes_parameters_through() {
        let mut r = CodecRegistry::new();
        r.register_decoder(CodecId::H264, mock_decoder_factory);
        r.register_encoder(CodecId::H264, mock_encoder_factory);

        // 工厂收到的正是传入的 &CodecParameters：mock 以 params.codec 构造，
        // 断言 codec_id 字段即验证了透传
        let mut params = CodecParameters::new(CodecId::H264, crate::media::MediaType::Video);
        params.bit_rate = Some(2_500_000);
        let dec = r.create_decoder(&params).unwrap();
        assert_eq!(dec.codec_id(), params.codec);

        let config = EncoderConfig {
            codec: CodecId::H264,
            time_base: tb(),
            bit_rate: Some(2_500_000),
            video: Some(crate::codec::VideoEncoderConfig {
                width: 640,
                height: 360,
                format: PixelFormat::Yuv420p,
                frame_rate: Rational::RATE_25,
                gop: 50,
                quality: crate::codec::QualityMode::Qp(24),
            }),
            audio: None,
        };
        let enc = r.create_encoder(&config).unwrap();
        assert_eq!(enc.codec_id(), config.codec);
    }

    #[test]
    fn registry_supports_and_ids_consistent() {
        let mut r = CodecRegistry::new();
        r.register_decoder(CodecId::Hevc, mock_decoder_factory);
        r.register_decoder(CodecId::Vp9, mock_decoder_factory);
        r.register_encoder(CodecId::Flac, mock_encoder_factory);

        for codec in [CodecId::H264, CodecId::Hevc, CodecId::Vp9, CodecId::Aac] {
            assert_eq!(
                r.supports_decoder(codec),
                r.decoder_ids().any(|c| c == codec),
                "decoder {codec:?}"
            );
        }
        for codec in [CodecId::Flac, CodecId::Opus] {
            assert_eq!(
                r.supports_encoder(codec),
                r.encoder_ids().any(|c| c == codec),
                "encoder {codec:?}"
            );
        }
        // decoder/encoder 两表互不串扰
        assert!(!r.supports_encoder(CodecId::Hevc));
        assert!(!r.supports_decoder(CodecId::Flac));
        // ids 为 BTreeMap 键序（CodecId derive Ord：H264 < Hevc < Vp9）
        assert_eq!(
            r.decoder_ids().collect::<Vec<_>>(),
            vec![CodecId::Hevc, CodecId::Vp9]
        );
    }
}
