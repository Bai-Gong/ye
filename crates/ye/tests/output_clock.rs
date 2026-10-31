#![cfg(all(feature = "mp4", feature = "aac", feature = "h264"))]
mod common;
use std::{fs::File, process::Command, sync::atomic::AtomicBool};
use ye::{codec::CodecRegistry, container::ContainerRegistry, convert::*, time::Rational};

#[test]
fn rational_cfr_and_resampled_mono_audio_have_one_origin() {
    let Some(ffmpeg) = common::ffmpeg_exe() else {
        return;
    };
    let Some(input) = common::ensure_vector(
        "clock.mp4",
        &[
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=32x32:rate=30000/1001:duration=1.001",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:sample_rate=44100:duration=1.001",
            "-c:v",
            "libx264",
            "-profile:v",
            "high",
            "-bf",
            "2",
            "-c:a",
            "aac",
            "-y",
        ],
    ) else {
        return;
    };
    let output = input.with_extension("converted.mp4");
    let mut containers = ContainerRegistry::new();
    ye::container::mp4::register_mp4(&mut containers);
    let mut codecs = CodecRegistry::new();
    ye::codec_h264::register_h264(&mut codecs);
    ye::codec_h264::register_h264_encoder(&mut codecs);
    ye::codec_aac::register_aac(&mut codecs);
    ye::codec_aac::register_aac_encoder(&mut codecs);
    let input_header = ye::media::inspect(Box::new(File::open(&input).unwrap())).unwrap();
    assert!(
        check_convert(
            &input_header,
            &codecs,
            Some(&VideoEncodeSettings::default()),
            &AudioEncodeSettings::Aac {
                bitrate_bps: 128000
            },
            &ConvertOptions {
                frame_rate: Some(Rational::new(24, 1)),
                audio_output: Some(AudioOutput {
                    sample_rate: 48000,
                    channels: 2
                })
            }
        )
        .is_empty()
    );
    assert!(
        !check_convert(
            &input_header,
            &codecs,
            Some(&VideoEncodeSettings::default()),
            &AudioEncodeSettings::Drop,
            &ConvertOptions {
                frame_rate: Some(Rational::ZERO),
                audio_output: None
            }
        )
        .is_empty()
    );
    let stats = convert_with_options(
        &containers,
        &codecs,
        Box::new(File::open(&input).unwrap()),
        "mp4",
        Box::new(File::create(&output).unwrap()),
        Some(&VideoEncodeSettings::default()),
        &AudioEncodeSettings::Aac {
            bitrate_bps: 128000,
        },
        &ConvertOptions {
            frame_rate: Some(Rational::new(24, 1)),
            audio_output: Some(AudioOutput {
                sample_rate: 48000,
                channels: 2,
            }),
        },
        filter_ops(),
        &|_, _| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(stats.video_frames, 25); // ceil(1.001 * 24)
    let header = ye::media::inspect(Box::new(File::open(&output).unwrap())).unwrap();
    let audio = header
        .streams
        .iter()
        .find_map(|s| s.codec.audio.as_ref())
        .unwrap();
    assert_eq!(audio.sample_rate, 48000);
    let video = header
        .streams
        .iter()
        .find_map(|s| s.codec.video.as_ref())
        .unwrap();
    assert_eq!(video.frame_rate, Some(Rational::new(24, 1)));
    assert_eq!(audio.channel_layout.unwrap().channels(), 2);
    // ffmpeg must decode the complete produced file without packet or timestamp errors.
    let result = Command::new(ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(&output)
        .args(["-f", "null", "-"])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    std::fs::remove_file(input).unwrap();
    std::fs::remove_file(output).unwrap();
}
