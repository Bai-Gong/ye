#![cfg(all(feature = "mp4", feature = "h264", feature = "aac"))]
mod common;
use std::{fs::File, path::Path, process::Command, sync::atomic::AtomicBool};
use ye::{
    codec::{CodecRegistry, Frame, QualityMode, VideoFrame},
    compose::ComposeSettings,
    container::ContainerRegistry,
    convert::AudioEncodeSettings,
    pixfmt::{ColorDesc, PixelFormat},
    time::Rational,
    timeline::*,
};

fn registries() -> (ContainerRegistry, CodecRegistry) {
    let mut c = ContainerRegistry::new();
    ye::container::mp4::register_mp4(&mut c);
    let mut k = CodecRegistry::new();
    ye::codec_h264::register_h264(&mut k);
    ye::codec_h264::register_h264_encoder(&mut k);
    ye::codec_aac::register_aac(&mut k);
    ye::codec_aac::register_aac_encoder(&mut k);
    (c, k)
}
fn settings() -> ComposeSettings {
    ComposeSettings {
        width: 32,
        height: 32,
        frame_rate: Rational::new(10, 1),
        gop: 25,
        quality: QualityMode::Qp(18),
        bit_rate: None,
        audio: AudioEncodeSettings::Aac {
            bitrate_bps: 128000,
        },
    }
}
fn clip(path: &Path, start: i64, end: i64, layer: Layer) -> TimelineClip {
    TimelineClip {
        stream_index: None,
        input: Box::new(File::open(path).unwrap()),
        source_in_us: 0,
        source_out_us: end - start,
        timeline_start_us: start,
        timeline_end_us: end,
        layer,
    }
}
fn fixture(name: &str, color: &str, rate: &str) -> Option<std::path::PathBuf> {
    common::ensure_vector(
        name,
        &[
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            color,
            "-f",
            "lavfi",
            "-i",
            rate,
            "-c:v",
            "libx264",
            "-profile:v",
            "high",
            "-bf",
            "2",
            "-c:a",
            "aac",
            "-ac",
            "1",
            "-y",
        ],
    )
}
fn layers(red: &Path, green: &Path) -> Vec<TimelineClip> {
    vec![
        clip(red, 100000, 900000, Layer::Video(Transform::default())),
        clip(
            green,
            300000,
            700000,
            Layer::Video(Transform {
                x: 8.0,
                scale: 0.5,
                opacity: 0.5,
                z: 1,
                ..Transform::default()
            }),
        ),
        clip(
            red,
            0,
            1000000,
            Layer::Audio(AudioMix {
                gain: 0.5,
                pan: -1.0,
            }),
        ),
        clip(
            green,
            0,
            1000000,
            Layer::Audio(AudioMix {
                gain: 0.25,
                pan: 1.0,
            }),
        ),
    ]
}

#[test]
fn two_video_and_two_audio_layers_match_unencoded_and_final_output() {
    let Some(red) = fixture(
        "timeline-red.mp4",
        "color=c=red:size=32x32:rate=10:duration=1",
        "sine=frequency=440:sample_rate=44100:duration=1",
    ) else {
        return;
    };
    let green = fixture(
        "timeline-green.mp4",
        "color=c=lime:size=32x32:rate=10:duration=1",
        "sine=frequency=880:sample_rate=48000:duration=1",
    )
    .unwrap();
    let (c, k) = registries();
    let s = settings();
    let cancel = AtomicBool::new(false);
    let mut images = Vec::new();
    let mut pcm = Vec::new();
    let stats = render_frames(
        &c,
        &k,
        layers(&red, &green),
        &s,
        1000000,
        &mut |f| {
            match f {
                Frame::Video(v) => images.push(v),
                Frame::Audio(a) => pcm.extend(
                    a.data()
                        .chunks_exact(4)
                        .map(|p| f32::from_le_bytes(p.try_into().unwrap())),
                ),
            }
            Ok(())
        },
        &|_, _| {},
        &cancel,
    )
    .unwrap();
    assert_eq!(stats.video_frames, 10);
    assert_eq!(pcm.len(), 48000 * 2);
    let pixel = |f: &VideoFrame, x: usize, y: usize| {
        let stride = f.layout().plane(0).stride;
        f.plane(0)[y * stride + x * 4..y * stride + x * 4 + 3].to_vec()
    };
    assert_eq!(pixel(&images[0], 16, 16), [0, 0, 0]);
    assert_eq!(pixel(&images[9], 16, 16), [0, 0, 0]);
    assert!(pixel(&images[2], 24, 16)[0] > 245); // red base
    let overlay = pixel(&images[4], 24, 16);
    assert!(
        (120..=135).contains(&overlay[0]) && (120..=135).contains(&overlay[1]),
        "{overlay:?}"
    );
    assert!(pixel(&images[4], 4, 16)[0] > 245); // upper layer has bounded extent
    let left_energy: f64 = pcm.chunks_exact(2).map(|p| f64::from(p[0]).powi(2)).sum();
    let right_energy: f64 = pcm.chunks_exact(2).map(|p| f64::from(p[1]).powi(2)).sum();
    assert!((3.5..4.5).contains(&(left_energy / right_energy))); // gain .5 vs .25, pans separate
    let output = red.with_extension("composed.mp4");
    render(
        &c,
        &k,
        layers(&red, &green),
        "mp4",
        Box::new(File::create(&output).unwrap()),
        &s,
        1000000,
        &|_, _| {},
        &cancel,
    )
    .unwrap();
    let ffmpeg = common::ffmpeg_exe().unwrap();
    let decoded = Command::new(&ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(&output)
        .args(["-an", "-pix_fmt", "rgba", "-f", "rawvideo", "-"])
        .output()
        .unwrap();
    assert!(
        decoded.status.success(),
        "{}",
        String::from_utf8_lossy(&decoded.stderr)
    );
    assert_eq!(decoded.stdout.len(), 10 * 32 * 32 * 4);
    for (i, f) in images.iter().enumerate() {
        for (x, y) in [(4, 16), (24, 16)] {
            let at = i * 32 * 32 * 4 + (y * 32 + x) * 4;
            let want = pixel(f, x, y);
            for (channel, expected) in want.iter().enumerate() {
                assert!(
                    decoded.stdout[at + channel].abs_diff(*expected) <= 8,
                    "frame {i}, channel {channel}"
                );
            }
        }
    }
    let probe = Command::new(common::ffprobe_exe().unwrap())
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=color_space,color_transfer,color_primaries,color_range",
            "-of",
            "default=nw=1",
        ])
        .arg(&output)
        .output()
        .unwrap();
    let tags = String::from_utf8_lossy(&probe.stdout);
    assert!(
        tags.contains("color_space=bt709") && tags.contains("color_range=tv"),
        "{tags}"
    );
    let header = ye::media::inspect(Box::new(File::open(&output).unwrap())).unwrap();
    for stream in &header.streams {
        assert_eq!(
            stream
                .duration
                .unwrap()
                .rescale(Rational::new(1, 1000000))
                .unwrap()
                .ticks(),
            1000000
        );
    }
    for p in [red, green, output] {
        std::fs::remove_file(p).unwrap();
    }
}

#[test]
fn center_rotation_and_opacity_have_deterministic_pixels() {
    let mut dst =
        VideoFrame::new(PixelFormat::Rgba8, 4, 4, ColorDesc::UNSPECIFIED, None, None).unwrap();
    let mut src =
        VideoFrame::new(PixelFormat::Rgba8, 2, 2, ColorDesc::UNSPECIFIED, None, None).unwrap();
    let stride = src.layout().plane(0).stride;
    for (x, y, c) in [
        (0, 0, [255, 0, 0, 255]),
        (1, 0, [0, 255, 0, 255]),
        (0, 1, [0, 0, 255, 255]),
        (1, 1, [255, 255, 255, 255]),
    ] {
        src.plane_mut(0)[y * stride + x * 4..y * stride + x * 4 + 4].copy_from_slice(&c);
    }
    blend(
        &mut dst,
        &src,
        Transform {
            rotation: 90.0,
            ..Transform::default()
        },
    )
    .unwrap();
    let d = dst.layout().plane(0).stride;
    assert_eq!(&dst.plane(0)[d + 4..d + 8], &[0, 0, 255, 255]);
    assert_eq!(&dst.plane(0)[d + 8..d + 12], &[255, 0, 0, 255]);
}

#[test]
fn rounded_full_audio_window_does_not_reject_or_leak_encoder_padding() {
    let Some(input) = common::ensure_vector(
        "timeline-fractional-audio.m4a",
        &[
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=sample_rate=48000:duration=1.9791875",
            "-c:a",
            "aac",
            "-aac_pns",
            "0",
            "-movie_timescale",
            "48000",
            "-y",
        ],
    ) else {
        return;
    };
    let header = ye::media::inspect(Box::new(File::open(&input).unwrap())).unwrap();
    let end = header
        .duration
        .unwrap()
        .rescale(Rational::new(1, 1000000))
        .unwrap()
        .ticks();
    assert_eq!(end, 1979188);
    let (c, k) = registries();
    let mut samples = Vec::new();
    render_frames(
        &c,
        &k,
        vec![clip(&input, 0, end, Layer::Audio(AudioMix::default()))],
        &settings(),
        end,
        &mut |f| {
            if let Frame::Audio(a) = f {
                samples.extend(
                    a.data()
                        .chunks_exact(4)
                        .map(|b| f32::from_le_bytes(b.try_into().unwrap())),
                );
            }
            Ok(())
        },
        &|_, _| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(samples.len(), 95002 * 2);
    assert!(samples[samples.len() - 2..].iter().all(|s| s.abs() < 1e-10));
    std::fs::remove_file(input).unwrap();
}

#[test]
fn non_keyframe_windows_seek_and_reorder_b_frames_before_selection() {
    let Some(input) = common::ensure_vector(
        "timeline-seek.mp4",
        &[
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=64x48:rate=30:duration=2",
            "-c:v",
            "libx264",
            "-profile:v",
            "high",
            "-bf",
            "3",
            "-g",
            "30",
            "-y",
        ],
    ) else {
        return;
    };
    let (c, k) = registries();
    let mut s = settings();
    s.width = 64;
    s.height = 48;
    s.audio = AudioEncodeSettings::Drop;
    let clips = [(950000, 1150000, 0), (150000, 350000, 200000)]
        .into_iter()
        .map(|(lo, hi, start)| TimelineClip {
            stream_index: None,
            input: Box::new(File::open(&input).unwrap()),
            source_in_us: lo,
            source_out_us: hi,
            timeline_start_us: start,
            timeline_end_us: start + hi - lo,
            layer: Layer::Video(Transform::default()),
        })
        .collect();
    let mut got = Vec::new();
    render_frames(
        &c,
        &k,
        clips,
        &s,
        400000,
        &mut |f| {
            if let Frame::Video(v) = f {
                let stride = v.layout().plane(0).stride;
                let mut tight = Vec::new();
                for row in 0..48 {
                    tight.extend_from_slice(&v.plane(0)[row * stride..row * stride + 256]);
                }
                got.push(tight);
            }
            Ok(())
        },
        &|_, _| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    let reference = Command::new(common::ffmpeg_exe().unwrap())
        .args(["-v", "error", "-i"])
        .arg(&input)
        .args([
            "-vf",
            "select='eq(n,4)+eq(n,7)+eq(n,28)+eq(n,31)'",
            "-fps_mode",
            "passthrough",
            "-pix_fmt",
            "rgba",
            "-f",
            "rawvideo",
            "-",
        ])
        .output()
        .unwrap();
    assert!(
        reference.status.success(),
        "{}",
        String::from_utf8_lossy(&reference.stderr)
    );
    assert_eq!(got.len(), 4);
    let bytes = 64 * 48 * 4;
    assert_eq!(reference.stdout.len(), bytes * 4);
    for (frame, ref_index) in got.iter().zip([2, 3, 0, 1]) {
        let expected = &reference.stdout[ref_index * bytes..(ref_index + 1) * bytes];
        let maximum = frame
            .iter()
            .zip(expected)
            .map(|(a, b)| a.abs_diff(*b))
            .max()
            .unwrap();
        assert!(maximum <= 2, "seek pixel error={maximum}");
    }
    std::fs::remove_file(input).unwrap();
}
