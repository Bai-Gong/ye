use ye_codec::{
    AudioEncoderConfig, AudioFrame, CodecId, Decoded, Decoder, Encoded, Encoder, EncoderConfig,
    Frame, Packet, PacketFlags,
};
use ye_codec_pcm::{PcmDecoder, PcmEncoder, PcmFormat, parameters};
use ye_pixfmt::{ChannelLayout, SampleFormat};
use ye_time::{Rational, Timestamp};
fn packet(data: Vec<u8>) -> Packet {
    Packet {
        data: data.into(),
        stream_index: 0,
        pts: Some(Timestamp::new(17, Rational::new(1, 48000))),
        dts: None,
        duration: None,
        flags: PacketFlags::KEYFRAME,
    }
}
fn decode(format: PcmFormat, data: Vec<u8>) -> Vec<f32> {
    let mut d = PcmDecoder::new(&parameters(format, 48000, ChannelLayout::MONO).unwrap()).unwrap();
    d.feed(&packet(data)).unwrap();
    let Decoded::Frame(Frame::Audio(a)) = d.pull().unwrap() else {
        panic!()
    };
    assert_eq!(a.pts().unwrap().ticks(), 17);
    a.data()
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect()
}
#[test]
fn pcm_extremes_and_unsigned_midpoint() {
    assert_eq!(
        decode(PcmFormat::U8, (0..=255).collect()),
        (0..=255)
            .map(|v| (v as f32 - 128.) / 128.)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        decode(
            PcmFormat::S16,
            [-32768i16, -1, 0, 1, 32767]
                .into_iter()
                .flat_map(i16::to_le_bytes)
                .collect()
        ),
        [-1., -1. / 32768., 0., 1. / 32768., 32767. / 32768.]
    );
    assert_eq!(
        decode(
            PcmFormat::S24,
            vec![0, 0, 128, 255, 255, 255, 0, 0, 0, 1, 0, 0, 255, 255, 127]
        ),
        [-1., -1. / 8388608., 0., 1. / 8388608., 8388607. / 8388608.]
    );
    assert_eq!(
        decode(
            PcmFormat::S32,
            [i32::MIN, -1, 0, 1, i32::MAX]
                .into_iter()
                .flat_map(i32::to_le_bytes)
                .collect()
        ),
        [-1., -1. / 2147483648., 0., 1. / 2147483648., 1.]
    );
    assert_eq!(
        decode(
            PcmFormat::F32,
            [-2.5f32, 0., 2.5]
                .into_iter()
                .flat_map(f32::to_le_bytes)
                .collect()
        ),
        [-2.5, 0., 2.5]
    );
}
fn config() -> EncoderConfig {
    EncoderConfig {
        codec: CodecId::Pcm,
        time_base: Rational::new(1, 48000),
        bit_rate: None,
        video: None,
        audio: Some(AudioEncoderConfig {
            sample_rate: 48000,
            channel_layout: ChannelLayout::MONO,
            format: SampleFormat::F32,
        }),
    }
}
fn frame(values: &[f32]) -> Frame {
    let mut a = AudioFrame::new(
        SampleFormat::F32,
        ChannelLayout::MONO,
        48000,
        values.len(),
        None,
    )
    .unwrap();
    for (out, v) in a.data_mut().chunks_exact_mut(4).zip(values) {
        out.copy_from_slice(&v.to_le_bytes());
    }
    Frame::Audio(a)
}
#[test]
fn encoder_clips_integer_preserves_float_and_counts_samples() {
    for (fmt, expected) in [
        (PcmFormat::U8, vec![0, 0, 128, 255, 255]),
        (
            PcmFormat::S16,
            [-32768i16, -32768, 0, 32767, 32767]
                .into_iter()
                .flat_map(i16::to_le_bytes)
                .collect(),
        ),
        (
            PcmFormat::F32,
            [-2f32, -1., 0., 1., 2.]
                .into_iter()
                .flat_map(f32::to_le_bytes)
                .collect(),
        ),
    ] {
        let mut e = PcmEncoder::new(&config(), fmt).unwrap();
        e.feed(&frame(&[-2., -1., 0., 1., 2.])).unwrap();
        let Encoded::Packet(p) = e.pull().unwrap() else {
            panic!()
        };
        assert_eq!(&*p.data, expected);
        assert_eq!(p.duration.unwrap().ticks(), 5);
        assert_eq!(p.pts.unwrap().ticks(), 0);
        e.feed(&frame(&[0.])).unwrap();
        e.flush();
        let Encoded::Packet(p) = e.pull().unwrap() else {
            panic!()
        };
        assert_eq!(p.pts.unwrap().ticks(), 5);
        assert!(matches!(e.pull().unwrap(), Encoded::Finished));
        assert!(e.feed(&frame(&[0.])).is_err());
    }
}
#[test]
fn invalid_pcm_rejected_without_partial_output() {
    let mut d = PcmDecoder::new(&parameters(PcmFormat::S24, 44100, ChannelLayout::STEREO).unwrap())
        .unwrap();
    assert!(d.feed(&packet(vec![0; 5])).is_err());
    assert!(matches!(d.pull().unwrap(), Decoded::NeedInput));
    let mut d =
        PcmDecoder::new(&parameters(PcmFormat::F32, 48000, ChannelLayout::MONO).unwrap()).unwrap();
    for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert!(d.feed(&packet(v.to_le_bytes().to_vec())).is_err());
    }
    let mut e = PcmEncoder::new(&config(), PcmFormat::S16).unwrap();
    assert!(e.feed(&frame(&[0., f32::NAN])).is_err());
    assert!(matches!(e.pull().unwrap(), Encoded::NeedInput));
    assert!(parameters(PcmFormat::S16, 0, ChannelLayout::MONO).is_err());
    let mut p = parameters(PcmFormat::S16, 48000, ChannelLayout::MONO).unwrap();
    p.extradata = Some(vec![1, 20]);
    assert!(PcmDecoder::new(&p).is_err());
}
