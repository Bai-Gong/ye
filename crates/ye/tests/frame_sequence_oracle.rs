//! Filmstrip cursor regression: reordered fractional-rate frames and explicit streams.
#![cfg(all(feature = "mp4", feature = "h264", feature = "png"))]
mod common;
use std::{fs::File, path::Path, process::Command, sync::atomic::AtomicBool};
use ye::{
    convert::ConvertError, frame_grab::decode_frame_rgba, frame_sequence::decode_frames_rgba,
};

fn open(path: &Path) -> Box<dyn ye::container::ReadSeek> {
    Box::new(File::open(path).unwrap())
}

#[test]
fn high_b_fractional_rate_cross_gop_matches_independent_grabs() {
    let Some(input) = common::ensure_vector(
        "sequence-high-b-fractional.mp4",
        &[
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=96x64:rate=30000/1001:duration=2.1",
            "-an",
            "-c:v",
            "libx264",
            "-profile:v",
            "high",
            "-preset",
            "medium",
            "-crf",
            "20",
            "-x264-params",
            "keyint=15:min-keyint=15:scenecut=0:bframes=3:b-adapt=0",
            "-y",
        ],
    ) else {
        return;
    };
    let probe = Command::new(common::ffprobe_exe().unwrap())
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "frame=pict_type",
            "-of",
            "csv=p=0",
        ])
        .arg(&input)
        .output()
        .unwrap();
    assert!(probe.status.success());
    assert!(
        String::from_utf8_lossy(&probe.stdout)
            .lines()
            .any(|line| line.trim() == "B"),
        "fixture must actually reorder B frames"
    );
    // 30000/1001 timestamps straddle microsecond boundaries; 500500 is a GOP boundary.
    let times = [
        -9, 0, 1, 33_366, 33_367, 500_499, 500_500, 500_501, 1_000_999, 1_001_000, 1_001_001,
        2_035_366, 2_035_367, 2_068_733,
    ];
    for first in [0, 5, 9] {
        let requested = &times[first..];
        let mut delivered = 0;
        decode_frames_rgba(
            open(&input),
            requested,
            None,
            &AtomicBool::new(false),
            |index, got| {
                assert_eq!(index, delivered);
                let expected = decode_frame_rgba(open(&input), requested[index]).unwrap();
                assert_eq!(
                    *got, expected,
                    "request {} after seek {}",
                    requested[index], requested[0]
                );
                delivered += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(delivered, requested.len());
    }
    std::fs::remove_file(input).unwrap();
}

#[test]
fn explicit_video_stream_uses_container_index_and_rejects_ambiguity() {
    let Some(input) = common::ensure_vector(
        "sequence-multi-video.mp4",
        &[
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=duration=0.5",
            "-f",
            "lavfi",
            "-i",
            "color=red:size=32x32:rate=25:duration=0.5",
            "-f",
            "lavfi",
            "-i",
            "color=blue:size=32x32:rate=25:duration=0.5",
            "-map",
            "0:a",
            "-map",
            "1:v",
            "-map",
            "2:v",
            "-c:a",
            "aac",
            "-c:v",
            "libx264",
            "-y",
        ],
    ) else {
        return;
    };
    let cancel = AtomicBool::new(false);
    for index in [None, Some(0), Some(999)] {
        assert!(matches!(
            decode_frames_rgba(open(&input), &[0], index, &cancel, |_, _| panic!(
                "invalid stream must not deliver"
            )),
            Err(ConvertError::InvalidArguments(_))
        ));
    }
    let mut selected = Vec::new();
    for stream in [1, 2] {
        let single = input.with_extension(format!("stream{stream}.mp4"));
        let status = Command::new(common::ffmpeg_exe().unwrap())
            .args(["-v", "error", "-i"])
            .arg(&input)
            .args(["-map", &format!("0:{stream}"), "-c", "copy", "-y"])
            .arg(&single)
            .status()
            .unwrap();
        assert!(status.success());
        decode_frames_rgba(
            open(&input),
            &[0, 40_001],
            Some(stream),
            &cancel,
            |index, got| {
                assert_eq!(
                    *got,
                    decode_frame_rgba(open(&single), [0, 40_001][index]).unwrap()
                );
                if index == 0 {
                    selected.push(got.rgba.clone());
                }
                Ok(())
            },
        )
        .unwrap();
        std::fs::remove_file(single).unwrap();
    }
    assert_ne!(
        selected[0], selected[1],
        "selected streams must be distinct"
    );
    std::fs::remove_file(input).unwrap();
}
