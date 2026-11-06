//! Offline, exact-pixel/sample animation tests; real encoders/muxer for the final-output case.
use std::{
    io::Cursor,
    sync::atomic::{AtomicBool, Ordering},
};
use ye::{
    animation::{ClipAnimation, Interpolation, Keyframe, KeyframeTrack, TimelineOptions},
    codec::{
        AudioFrame, AudioParams, CodecError, CodecId, CodecParameters, CodecRegistry, Decoded,
        Decoder, Frame, MediaType, Packet, PacketFlags, QualityMode, VideoFrame, VideoParams,
    },
    compose::ComposeSettings,
    container::{
        ContainerError, ContainerRegistry, Demuxer, DemuxerFactory, MediaHeader, Metadata,
        ProbeScore, ReadSeek, Stream,
    },
    convert::{AudioEncodeSettings, ConvertError},
    pixfmt::{ChannelLayout, ColorDesc, PixelFormat, SampleFormat},
    time::{Rational, Timestamp},
    timeline::{self, AudioMix, Layer, TimelineClip, Transform},
};

struct FixtureFactory;
struct FixtureDemux {
    tag: u8,
    next: i64,
}
fn audio(tag: u8) -> bool {
    tag >= 4
}
fn sample_rate(tag: u8) -> u32 {
    if tag >= 7 { 44100 } else { 48000 }
}
fn clock(tag: u8) -> Rational {
    Rational::new(
        1,
        if audio(tag) {
            sample_rate(tag) as i32
        } else {
            10
        },
    )
}
fn params(tag: u8) -> CodecParameters {
    let mut p = CodecParameters::new(
        CodecId::Unknown,
        if audio(tag) {
            MediaType::Audio
        } else {
            MediaType::Video
        },
    );
    p.extradata = Some(vec![tag]);
    if audio(tag) {
        p.audio = Some(AudioParams {
            sample_rate: sample_rate(tag),
            channel_layout: Some(ChannelLayout::STEREO),
            sample_format: Some(SampleFormat::F32),
        });
    } else {
        p.video = Some(VideoParams {
            width: 4,
            height: 4,
            pixel_format: Some(PixelFormat::Rgba8),
            frame_rate: Some(Rational::new(10, 1)),
        });
    }
    p
}
impl DemuxerFactory for FixtureFactory {
    fn name(&self) -> &'static str {
        "animation-fixture"
    }
    fn probe(&self, head: &[u8]) -> ProbeScore {
        if head.first().is_some_and(|tag| (1..=8).contains(tag)) {
            ProbeScore::new(100).unwrap()
        } else {
            ProbeScore::new(0).unwrap()
        }
    }
    fn open(&self, mut input: Box<dyn ReadSeek>) -> Result<Box<dyn Demuxer>, ContainerError> {
        let mut tag = [0];
        input.read_exact(&mut tag)?;
        Ok(Box::new(FixtureDemux {
            tag: tag[0],
            next: 0,
        }))
    }
}
impl Demuxer for FixtureDemux {
    fn format_name(&self) -> &str {
        "animation-fixture"
    }
    fn read_header(&mut self) -> Result<MediaHeader, ContainerError> {
        let p = params(self.tag);
        let tb = clock(self.tag);
        let duration = Timestamp::new(
            if audio(self.tag) {
                i64::from(sample_rate(self.tag)) * 2
            } else {
                20
            },
            tb,
        );
        Ok(MediaHeader {
            format_name: self.format_name().into(),
            streams: vec![Stream {
                index: 0,
                media_type: p.media_type,
                codec: p,
                time_base: tb,
                start_time: Some(Timestamp::new(0, tb)),
                duration: Some(duration),
                nb_frames: None,
                metadata: Metadata::new(),
            }],
            duration: Some(duration),
            start_time: None,
            bit_rate: None,
            metadata: Metadata::new(),
        })
    }
    fn read_packet(&mut self) -> Result<Option<Packet>, ContainerError> {
        let end = if audio(self.tag) {
            i64::from(sample_rate(self.tag)) * 2
        } else {
            20
        };
        if self.next >= end {
            return Ok(None);
        }
        let count = if audio(self.tag) {
            1024.min(end - self.next)
        } else {
            1
        };
        let pts = Timestamp::new(self.next, clock(self.tag));
        self.next += count;
        Ok(Some(Packet {
            data: vec![self.tag].into(),
            stream_index: 0,
            pts: Some(pts),
            dts: Some(pts),
            duration: Some(Timestamp::new(count, clock(self.tag))),
            flags: PacketFlags::KEYFRAME,
        }))
    }
    fn seek(&mut self, index: u32, target: Timestamp) -> Result<Timestamp, ContainerError> {
        assert_eq!(index, 0);
        let tick = target
            .rescale_with(clock(self.tag), ye::time::Rounding::Down)?
            .ticks()
            .max(0);
        self.next = if audio(self.tag) {
            tick / 1024 * 1024
        } else {
            tick
        };
        Ok(Timestamp::new(self.next, clock(self.tag)))
    }
}
struct FixtureDecoder {
    tag: u8,
    frame: Option<Frame>,
    flushed: bool,
}
fn decoder(p: &CodecParameters) -> Result<Box<dyn Decoder>, CodecError> {
    Ok(Box::new(FixtureDecoder {
        tag: p.extradata.as_ref().unwrap()[0],
        frame: None,
        flushed: false,
    }))
}
impl Decoder for FixtureDecoder {
    fn codec_id(&self) -> CodecId {
        CodecId::Unknown
    }
    fn feed(&mut self, p: &Packet) -> Result<(), CodecError> {
        self.frame = Some(if audio(self.tag) {
            let n = p.duration.unwrap().ticks() as usize;
            let mut a = AudioFrame::new(
                SampleFormat::F32,
                ChannelLayout::STEREO,
                sample_rate(self.tag),
                n,
                p.pts,
            )?;
            for (i, pair) in a.data_mut().chunks_exact_mut(8).enumerate() {
                let values = match self.tag {
                    4 => [0.25f32, -0.5],
                    5 => [f32::MAX; 2],
                    6 => [-f32::MAX; 2],
                    7 | 8 => {
                        let polarity =
                            if ((p.pts.unwrap().ticks() as usize + i) / 2).is_multiple_of(2) {
                                1.0
                            } else {
                                -1.0
                            };
                        let polarity = if self.tag == 7 { polarity } else { -polarity };
                        [f32::MAX * polarity; 2]
                    }
                    _ => unreachable!(),
                };
                pair[..4].copy_from_slice(&values[0].to_le_bytes());
                pair[4..].copy_from_slice(&values[1].to_le_bytes());
            }
            Frame::Audio(a)
        } else {
            let mut v = VideoFrame::new(
                PixelFormat::Rgba8,
                4,
                4,
                ColorDesc::UNSPECIFIED,
                p.pts,
                p.duration,
            )?;
            let stride = v.layout().plane(0).stride;
            for y in 0..4 {
                for x in 0..4 {
                    let color = match self.tag {
                        1 => [255, 0, 0, 255],
                        2 => [0, 255, 0, 255],
                        _ => match (x / 2, y / 2) {
                            (0, 0) => [255, 0, 0, 255],
                            (1, 0) => [0, 255, 0, 255],
                            (0, 1) => [0, 0, 255, 255],
                            _ => [255; 4],
                        },
                    };
                    v.plane_mut(0)[y * stride + x * 4..y * stride + x * 4 + 4]
                        .copy_from_slice(&color);
                }
            }
            Frame::Video(v)
        });
        Ok(())
    }
    fn flush(&mut self) {
        self.flushed = true;
    }
    fn pull(&mut self) -> Result<Decoded, CodecError> {
        Ok(match self.frame.take() {
            Some(f) => Decoded::Frame(f),
            None if self.flushed => Decoded::Finished,
            None => Decoded::NeedInput,
        })
    }
}
fn registries() -> (ContainerRegistry, CodecRegistry) {
    let mut c = ContainerRegistry::new();
    c.register_demuxer(Box::new(FixtureFactory));
    let mut k = CodecRegistry::new();
    k.register_decoder(CodecId::Unknown, decoder);
    (c, k)
}
fn settings() -> ComposeSettings {
    ComposeSettings {
        width: 4,
        height: 4,
        frame_rate: Rational::new(10, 1),
        gop: 10,
        quality: QualityMode::Qp(18),
        bit_rate: None,
        audio: AudioEncodeSettings::Drop,
    }
}
fn clip(tag: u8, start: i64, end: i64, source_in: i64, layer: Layer) -> TimelineClip {
    TimelineClip {
        stream_index: None,
        input: Box::new(Cursor::new(vec![tag])),
        source_in_us: source_in,
        source_out_us: source_in + end - start,
        timeline_start_us: start,
        timeline_end_us: end,
        layer,
    }
}
fn track(points: &[(i64, f64)], interpolation: Interpolation) -> Option<KeyframeTrack> {
    Some(KeyframeTrack {
        keyframes: points
            .iter()
            .map(|&(time_us, value)| Keyframe {
                time_us,
                value,
                interpolation,
            })
            .collect(),
    })
}
fn pixel(f: &VideoFrame, x: usize, y: usize) -> [u8; 3] {
    let i = y * f.layout().plane(0).stride + x * 4;
    f.plane(0)[i..i + 3].try_into().unwrap()
}
fn frames(
    clips: Vec<TimelineClip>,
    s: &ComposeSettings,
    duration: i64,
    options: TimelineOptions,
) -> (Vec<VideoFrame>, Vec<[f32; 2]>) {
    let (c, k) = registries();
    let mut video = Vec::new();
    let mut pcm = Vec::new();
    timeline::render_frames_with_options(
        &c,
        &k,
        clips,
        s,
        duration,
        &options,
        &mut |f| {
            match f {
                Frame::Video(v) => video.push(v),
                Frame::Audio(a) => pcm.extend(a.data().chunks_exact(8).map(|p| {
                    [
                        f32::from_le_bytes(p[..4].try_into().unwrap()),
                        f32::from_le_bytes(p[4..].try_into().unwrap()),
                    ]
                })),
            };
            Ok(())
        },
        &|_, _| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    (video, pcm)
}

#[test]
fn dissolve_tracks_stay_attached_to_input_indices_after_z_sort() {
    let layers = vec![
        clip(
            2,
            200000,
            600000,
            500000,
            Layer::Video(Transform {
                z: 10,
                ..Transform::default()
            }),
        ),
        clip(1, 0, 800000, 0, Layer::Video(Transform::default())),
    ];
    let options = TimelineOptions {
        clips: vec![
            ClipAnimation {
                fade_in_us: 200000,
                ..Default::default()
            },
            ClipAnimation::default(),
        ],
    };
    let (video, _) = frames(layers, &settings(), 800000, options);
    assert_eq!(video.len(), 8);
    for (index, color) in [
        (0, [255, 0, 0]),
        (2, [255, 0, 0]),
        (3, [128, 128, 0]),
        (4, [0, 255, 0]),
        (5, [0, 255, 0]),
        (6, [255, 0, 0]),
    ] {
        assert_eq!(pixel(&video[index], 2, 2), color, "frame {index}");
    }
}

#[test]
fn transform_tracks_use_clip_local_time_and_hold_exact_boundaries() {
    let mut s = settings();
    s.width = 8;
    s.height = 8;
    let animation = ClipAnimation {
        x: track(&[(0, -2.0), (200000, 2.0)], Interpolation::Hold),
        y: track(&[(0, -2.0), (200000, 2.0)], Interpolation::Hold),
        rotation: track(&[(0, 0.0), (200000, 90.0)], Interpolation::Hold),
        scale: track(&[(0, 1.0), (200000, 0.5)], Interpolation::Hold),
        opacity: track(&[(0, 1.0), (200000, 0.5)], Interpolation::Hold),
        ..Default::default()
    };
    let (v, _) = frames(
        vec![clip(
            3,
            200000,
            600000,
            500000,
            Layer::Video(Transform::default()),
        )],
        &s,
        800000,
        TimelineOptions {
            clips: vec![animation],
        },
    );
    assert_eq!(pixel(&v[1], 0, 0), [0; 3]);
    assert_eq!(pixel(&v[2], 0, 0), [255, 0, 0]);
    assert_eq!(pixel(&v[3], 3, 0), [0, 255, 0]);
    assert_eq!(pixel(&v[4], 5, 5), [0, 0, 128]);
    assert_eq!(pixel(&v[4], 6, 5), [128, 0, 0]);
    assert_eq!(pixel(&v[4], 5, 6), [128, 128, 128]);
    assert_eq!(pixel(&v[4], 6, 6), [0, 128, 0]);
    assert_eq!(pixel(&v[6], 5, 5), [0; 3]);
}

#[test]
fn audio_gain_pan_and_fades_are_evaluated_per_sample() {
    let mut s = settings();
    s.audio = AudioEncodeSettings::Aac {
        bitrate_bps: 128000,
    };
    let animation = ClipAnimation {
        gain: track(
            &[(0, 0.0), (200000, 2.0), (400000, 0.0)],
            Interpolation::Linear,
        ),
        pan: track(&[(0, -1.0), (200000, 1.0)], Interpolation::Linear),
        fade_in_us: 100000,
        fade_out_us: 100000,
        ..Default::default()
    };
    let (_, pcm) = frames(
        vec![clip(
            4,
            100000,
            500000,
            500000,
            Layer::Audio(AudioMix::default()),
        )],
        &s,
        600000,
        TimelineOptions {
            clips: vec![animation],
        },
    );
    assert_eq!(pcm.len(), 28800);
    for (index, actual) in pcm.iter().enumerate() {
        let local = index as f64 / 48000.0 - 0.1;
        let expected = if !(0.0..0.4).contains(&local) {
            [0.0, 0.0]
        } else {
            let gain = if local <= 0.2 {
                local * 10.0
            } else {
                (0.4 - local) * 10.0
            };
            let pan = (-1.0 + local * 10.0).min(1.0);
            let envelope = (local / 0.1).min(1.0) * ((0.4 - local) / 0.1).min(1.0);
            [
                0.25 * gain * envelope * (1.0 - pan.max(0.0)),
                -0.5 * gain * envelope * (1.0 + pan.min(0.0)),
            ]
        };
        for ch in 0..2 {
            assert!(
                (f64::from(actual[ch]) - expected[ch]).abs() < 2e-7,
                "sample {index} ch{ch}: {} != {}",
                actual[ch],
                expected[ch]
            );
        }
    }
}

#[test]
fn complementary_audio_fades_preserve_constant_signal() {
    let mut s = settings();
    s.audio = AudioEncodeSettings::Aac {
        bitrate_bps: 128000,
    };
    let clips = vec![
        clip(4, 0, 400000, 0, Layer::Audio(AudioMix::default())),
        clip(4, 200000, 600000, 0, Layer::Audio(AudioMix::default())),
    ];
    let (_, pcm) = frames(
        clips,
        &s,
        600000,
        TimelineOptions {
            clips: vec![
                ClipAnimation {
                    fade_out_us: 200000,
                    ..Default::default()
                },
                ClipAnimation {
                    fade_in_us: 200000,
                    ..Default::default()
                },
            ],
        },
    );
    for (index, sample) in pcm.iter().enumerate().skip(16).take(28800 - 32) {
        assert!(
            (sample[0] - 0.25).abs() < 1e-7 && (sample[1] + 0.5).abs() < 1e-7,
            "crossfade dip at {index}: {sample:?}"
        );
    }
}

#[test]
fn preflight_rejects_malformed_animation_before_reading_sources() {
    let c = || clip(1, 0, 400000, 0, Layer::Video(Transform::default()));
    let invalids = vec![
        ClipAnimation {
            x: Some(KeyframeTrack::default()),
            ..Default::default()
        },
        ClipAnimation {
            x: track(&[(0, 0.0), (0, 1.0)], Interpolation::Linear),
            ..Default::default()
        },
        ClipAnimation {
            x: track(&[(1, 0.0), (0, 1.0)], Interpolation::Linear),
            ..Default::default()
        },
        ClipAnimation {
            x: track(&[(-1, 0.0)], Interpolation::Linear),
            ..Default::default()
        },
        ClipAnimation {
            x: track(&[(400001, 0.0)], Interpolation::Linear),
            ..Default::default()
        },
        ClipAnimation {
            x: track(&[(0, f64::NAN)], Interpolation::Linear),
            ..Default::default()
        },
        ClipAnimation {
            rotation: track(&[(0, f64::INFINITY)], Interpolation::Linear),
            ..Default::default()
        },
        ClipAnimation {
            scale: track(&[(0, 0.0)], Interpolation::Linear),
            ..Default::default()
        },
        ClipAnimation {
            opacity: track(&[(0, 1.01)], Interpolation::Linear),
            ..Default::default()
        },
        ClipAnimation {
            gain: track(&[(0, 1.0)], Interpolation::Linear),
            ..Default::default()
        },
        ClipAnimation {
            fade_in_us: -1,
            ..Default::default()
        },
        ClipAnimation {
            fade_in_us: 300000,
            fade_out_us: 200000,
            ..Default::default()
        },
        ClipAnimation {
            fade_in_us: i64::MAX,
            fade_out_us: i64::MAX,
            ..Default::default()
        },
    ];
    for animation in invalids {
        let options = TimelineOptions {
            clips: vec![animation],
        };
        assert!(
            !timeline::check_timeline_with_options(&[c()], &settings(), 400000, &options)
                .is_empty()
        );
        assert!(matches!(
            timeline::render_frames_with_options(
                &ContainerRegistry::new(),
                &CodecRegistry::new(),
                vec![c()],
                &settings(),
                400000,
                &options,
                &mut |_| panic!("no output"),
                &|_, _| {},
                &AtomicBool::new(false)
            ),
            Err(ConvertError::InvalidArguments(_))
        ));
    }
    assert!(
        !timeline::check_timeline_with_options(
            &[c()],
            &settings(),
            400000,
            &TimelineOptions {
                clips: vec![ClipAnimation::default(); 2]
            }
        )
        .is_empty()
    );
    for (gain, pan) in [(2.1, 0.0), (1.0, 1.1), (f64::NAN, 0.0)] {
        let a = clip(4, 0, 400000, 0, Layer::Audio(AudioMix::default()));
        assert!(
            !timeline::check_timeline_with_options(
                &[a],
                &settings(),
                400000,
                &TimelineOptions {
                    clips: vec![ClipAnimation {
                        gain: track(&[(0, gain)], Interpolation::Linear),
                        pan: track(&[(0, pan)], Interpolation::Linear),
                        ..Default::default()
                    }]
                }
            )
            .is_empty()
        );
    }
}

#[test]
fn cancellation_and_sink_errors_stop_delivery() {
    let (c, k) = registries();
    let cancel = AtomicBool::new(false);
    let mut delivered = 0;
    let result = timeline::render_frames_with_options(
        &c,
        &k,
        vec![clip(1, 0, 400000, 0, Layer::Video(Transform::default()))],
        &settings(),
        400000,
        &TimelineOptions {
            clips: vec![ClipAnimation {
                fade_in_us: 200000,
                ..Default::default()
            }],
        },
        &mut |_| {
            delivered += 1;
            cancel.store(true, Ordering::Relaxed);
            Ok(())
        },
        &|_, _| {},
        &cancel,
    );
    assert!(matches!(result, Err(ConvertError::Cancelled)));
    assert_eq!(delivered, 1);
    let result = timeline::render_frames_with_options(
        &c,
        &k,
        vec![],
        &settings(),
        400000,
        &TimelineOptions::default(),
        &mut |_| Err(ConvertError::Sink("sink".into())),
        &|_, _| {},
        &AtomicBool::new(false),
    );
    assert!(matches!(result,Err(ConvertError::Sink(s)) if s=="sink"));
}

#[cfg(all(feature = "mp4", feature = "h264", feature = "aac"))]
mod common;

#[cfg(all(feature = "mp4", feature = "h264", feature = "aac"))]
#[test]
fn animated_output_encodes_with_exact_duration_and_decodes_externally() {
    use std::{fs::File, process::Command};
    let Some(ffmpeg) = common::ffmpeg_exe() else {
        return;
    };
    let (mut containers, mut codecs) = registries();
    ye::container::mp4::register_mp4(&mut containers);
    ye::codec_h264::register_h264_encoder(&mut codecs);
    ye::codec_aac::register_aac_encoder(&mut codecs);
    let path = std::env::temp_dir().join(format!("ye-animated-export-{}.mp4", std::process::id()));
    let mut s = settings();
    s.width = 32;
    s.height = 32;
    s.audio = AudioEncodeSettings::Aac {
        bitrate_bps: 128000,
    };
    let transform = Transform {
        scale: 8.0,
        ..Transform::default()
    };
    let clips = vec![
        clip(1, 0, 400000, 0, Layer::Video(transform)),
        clip(
            2,
            0,
            400000,
            0,
            Layer::Video(Transform { z: 1, ..transform }),
        ),
        clip(4, 0, 400000, 0, Layer::Audio(AudioMix::default())),
    ];
    let options = TimelineOptions {
        clips: vec![
            ClipAnimation::default(),
            ClipAnimation {
                fade_in_us: 200000,
                ..Default::default()
            },
            ClipAnimation {
                fade_in_us: 100000,
                fade_out_us: 100000,
                ..Default::default()
            },
        ],
    };
    let stats = timeline::render_with_options(
        &containers,
        &codecs,
        clips,
        "mp4",
        Box::new(File::create(&path).unwrap()),
        &s,
        400000,
        &options,
        &|_, _| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(stats.video_frames, 4);
    let header = ye::media::inspect(Box::new(File::open(&path).unwrap())).unwrap();
    assert_eq!(header.streams.len(), 2);
    for stream in header.streams {
        assert_eq!(
            stream
                .duration
                .unwrap()
                .rescale(Rational::new(1, 1_000_000))
                .unwrap()
                .ticks(),
            400000
        );
    }
    let decoded = Command::new(&ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(&path)
        .args(["-an", "-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .output()
        .unwrap();
    assert!(
        decoded.status.success(),
        "{}",
        String::from_utf8_lossy(&decoded.stderr)
    );
    assert_eq!(decoded.stdout.len(), 4 * 32 * 32 * 4);
    for (index, expected) in [[255, 0, 0], [128, 128, 0], [0, 255, 0], [0, 255, 0]]
        .into_iter()
        .enumerate()
    {
        let pixel = &decoded.stdout[index * 32 * 32 * 4 + (16 * 32 + 16) * 4..][..3];
        for ch in 0..3 {
            assert!(
                pixel[ch].abs_diff(expected[ch]) <= 8,
                "frame {index} channel {ch}: {} != {}",
                pixel[ch],
                expected[ch]
            );
        }
    }
    let decoded_audio = Command::new(ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(&path)
        .args(["-vn", "-f", "f32le", "-"])
        .output()
        .unwrap();
    assert!(
        decoded_audio.status.success(),
        "{}",
        String::from_utf8_lossy(&decoded_audio.stderr)
    );
    assert!(!decoded_audio.stdout.is_empty());
    std::fs::remove_file(path).unwrap();
}

#[test]
fn fractional_video_tick_does_not_advance_a_hold_keyframe_early() {
    let mut s = settings();
    s.frame_rate = Rational::new(30000, 1001);
    let animation = ClipAnimation {
        opacity: track(&[(0, 0.0), (33367, 1.0)], Interpolation::Hold),
        ..Default::default()
    };
    let (video, _) = frames(
        vec![clip(
            1,
            0,
            100100,
            1000000,
            Layer::Video(Transform::default()),
        )],
        &s,
        100100,
        TimelineOptions {
            clips: vec![animation],
        },
    );
    assert_eq!(video.len(), 3);
    assert_eq!(pixel(&video[0], 1, 1), [0; 3]);
    assert_eq!(pixel(&video[1], 1, 1), [0; 3]); // exact 33366.666... µs, before the key
    assert_eq!(pixel(&video[2], 1, 1), [255, 0, 0]);
}

#[test]
fn finite_extreme_pcm_mixes_before_clipping_without_nan() {
    let mut s = settings();
    s.audio = AudioEncodeSettings::Aac {
        bitrate_bps: 128000,
    };
    for tags in [[5, 6], [7, 8]] {
        let clips = tags
            .into_iter()
            .map(|tag| {
                clip(
                    tag,
                    0,
                    100000,
                    200000,
                    Layer::Audio(AudioMix {
                        gain: 2.0,
                        pan: 0.0,
                    }),
                )
            })
            .collect();
        let (_, pcm) = frames(clips, &s, 100000, TimelineOptions::default());
        for (i, pair) in pcm.iter().enumerate() {
            assert_eq!(
                *pair, [0.0; 2],
                "opposite extreme tracks must cancel, sample {i}"
            );
        }
    }
    let (_, pcm) = frames(
        vec![clip(
            5,
            0,
            100000,
            200000,
            Layer::Audio(AudioMix {
                gain: 2.0,
                pan: 1.0,
            }),
        )],
        &s,
        100000,
        TimelineOptions::default(),
    );
    for pair in pcm {
        assert_eq!(pair, [0.0, 1.0]);
    }
}
