#![cfg(all(feature = "mp4", feature = "aac"))]
mod common;
use std::{fs::File, process::Command, sync::atomic::AtomicBool};

#[test]
fn audio_edit_offset_and_priming_match_the_visible_timeline() {
    let Some(input) = common::ensure_vector(
        "waveform-aligned.m4a",
        &[
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=44100:duration=2",
            "-af",
            "asetpts=PTS+0.25/TB",
            "-c:a",
            "aac",
            "-aac_pns",
            "0",
            "-y",
        ],
    ) else {
        return;
    };
    let result = ye::waveform::audio_peaks_aligned_with_count(
        Box::new(File::open(&input).unwrap()),
        90,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(result.channels, 1);
    assert_eq!(result.buckets, 90);
    assert!(result.peaks[0][..8].iter().all(|p| p.rms == 0.0));
    let reference = Command::new(common::ffmpeg_exe().unwrap())
        .args(["-v", "error", "-copyts", "-i"])
        .arg(&input)
        .args(["-af", "aresample=async=1:first_pts=0", "-f", "f32le", "-"])
        .output()
        .unwrap();
    assert!(reference.status.success());
    let samples: Vec<_> = reference
        .stdout
        .chunks_exact(4)
        .map(|s| f32::from_le_bytes(s.try_into().unwrap()))
        .collect();
    let expected = ye::waveform::peaks_by_bucket_size(&samples, 1, result.bucket_samples).unwrap();
    for (i, expected_bucket) in expected[0].iter().enumerate().take(60).skip(20) {
        assert!(
            (result.peaks[0][i].rms - expected_bucket.rms).abs() < 1.5e-4,
            "bucket {i}: {:?} vs {:?}",
            result.peaks[0][i],
            expected_bucket
        );
    }
    std::fs::remove_file(input).unwrap();
}
