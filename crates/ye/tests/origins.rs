#![cfg(all(feature = "mp4", feature = "h264", feature = "aac"))]
mod common;
use std::{fs::File, process::Command, sync::atomic::AtomicBool};
use ye::{
    codec::{CodecRegistry, Frame, QualityMode},
    compose::ComposeSettings,
    container::ContainerRegistry,
    convert::AudioEncodeSettings,
    time::Rational,
    timeline::*,
};

#[test]
fn different_video_and_audio_origins_keep_black_and_silence_in_one_clock() {
    let Some(input) = common::ensure_vector(
        "origins.mp4",
        &[
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=32x32:rate=10:duration=1",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=1",
            "-vf",
            "setpts=PTS+0.2/TB",
            "-af",
            "asetpts=PTS+0.1/TB",
            "-fps_mode",
            "passthrough",
            "-c:v",
            "libx264",
            "-profile:v",
            "high",
            "-bf",
            "2",
            "-c:a",
            "aac",
            "-aac_pns",
            "0",
            "-b:a",
            "128k",
            "-y",
        ],
    ) else {
        return;
    };
    let mut containers = ContainerRegistry::new();
    ye::container::mp4::register_mp4(&mut containers);
    let mut codecs = CodecRegistry::new();
    ye::codec_h264::register_h264(&mut codecs);
    ye::codec_aac::register_aac(&mut codecs);
    let header = ye::media::inspect(Box::new(File::open(&input).unwrap())).unwrap();
    let mut clips = Vec::new();
    for stream in &header.streams {
        let end = stream
            .duration
            .unwrap()
            .checked_add(
                stream
                    .start_time
                    .unwrap_or(ye::time::Timestamp::new(0, stream.time_base)),
            )
            .unwrap()
            .rescale(Rational::new(1, 1000000))
            .unwrap()
            .ticks();
        clips.push(TimelineClip {
            stream_index: None,
            input: Box::new(File::open(&input).unwrap()),
            source_in_us: 0,
            source_out_us: end,
            timeline_start_us: 0,
            timeline_end_us: end,
            layer: if stream.codec.video.is_some() {
                Layer::Video(Transform::default())
            } else {
                Layer::Audio(AudioMix::default())
            },
        });
    }
    let settings = ComposeSettings {
        width: 32,
        height: 32,
        frame_rate: Rational::new(10, 1),
        gop: 25,
        quality: QualityMode::Qp(18),
        bit_rate: None,
        audio: AudioEncodeSettings::Aac {
            bitrate_bps: 128000,
        },
    };
    let (mut video, mut audio) = (Vec::new(), Vec::new());
    render_frames(
        &containers,
        &codecs,
        clips,
        &settings,
        1200000,
        &mut |frame| {
            match frame {
                Frame::Video(v) => video.push(v),
                Frame::Audio(a) => audio.extend(
                    a.data()
                        .chunks_exact(4)
                        .map(|b| f32::from_le_bytes(b.try_into().unwrap())),
                ),
            }
            Ok(())
        },
        &|_, _| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(video.len(), 12);
    assert_eq!(audio.len(), 57600 * 2);
    for frame in &video[..2] {
        assert!(frame.plane(0).chunks_exact(4).all(|p| p[..3] == [0, 0, 0]));
    }
    assert!(
        video[2]
            .plane(0)
            .chunks_exact(4)
            .any(|p| p[0] > 30 || p[1] > 30 || p[2] > 30)
    );
    assert!(audio[..3500 * 2].iter().all(|s| *s == 0.0));
    let reference = Command::new(common::ffmpeg_exe().unwrap())
        .args(["-v", "error", "-copyts", "-i"])
        .arg(&input)
        .args([
            "-vn",
            "-af",
            "aresample=async=1:first_pts=0",
            "-f",
            "f32le",
            "-",
        ])
        .output()
        .unwrap();
    assert!(reference.status.success());
    let reference: Vec<_> = reference
        .stdout
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    // Compare the audible interior, excluding container padding at the tail.
    let maximum = (12000..48000)
        .map(|i| (audio[i * 2] - reference[i]).abs())
        .fold(0.0f32, f32::max);
    assert!(maximum <= 1.5e-4, "origin/PCM error={maximum}");
    ye::codec_h264::register_h264_encoder(&mut codecs);
    ye::codec_aac::register_aac_encoder(&mut codecs);
    let output = input.with_extension("converted.mp4");
    let stats = ye::convert::convert_with_options(
        &containers,
        &codecs,
        Box::new(File::open(&input).unwrap()),
        "mp4",
        Box::new(File::create(&output).unwrap()),
        Some(&ye::convert::VideoEncodeSettings::default()),
        &AudioEncodeSettings::Aac {
            bitrate_bps: 128000,
        },
        &ye::convert::ConvertOptions {
            frame_rate: Some(Rational::new(10, 1)),
            audio_output: Some(ye::convert::AudioOutput {
                sample_rate: 48000,
                channels: 2,
            }),
        },
        ye::convert::filter_ops(),
        &|_, _| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(stats.total_us, 1200000);
    assert_eq!(stats.video_frames, 12);
    std::fs::remove_file(output).unwrap();
    std::fs::remove_file(input).unwrap();
}
