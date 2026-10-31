#![cfg(all(feature = "mp4", feature = "aac", feature = "h264"))]
mod common;
use std::{fs::File, process::Command, sync::atomic::AtomicBool};
use ye::{
    codec::{CodecRegistry, Frame, MediaType},
    container::ContainerRegistry,
    convert::AudioEncodeSettings,
    timeline::*,
};
#[test]
fn explicit_audio_streams_remain_distinct_in_pcm_and_waveforms() {
    let Some(input) = common::ensure_vector(
        "selected-audio.mp4",
        &[
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=red:size=32x32:rate=10:duration=0.3",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=0.3",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000:duration=0.3",
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-map",
            "2:a",
            "-c:v",
            "libx264",
            "-c:a",
            "aac",
            "-aac_pns",
            "0",
            "-filter:a:1",
            "volume=0.2",
        ],
    ) else {
        return;
    };
    let mut c = ContainerRegistry::new();
    ye::container::mp4::register_mp4(&mut c);
    let mut k = CodecRegistry::new();
    ye::codec_aac::register_aac(&mut k);
    let h = ye::media::inspect(Box::new(File::open(&input).unwrap())).unwrap();
    assert!(select_stream(&h, MediaType::Audio, None).is_err());
    assert!(select_stream(&h, MediaType::Audio, Some(0)).is_err());
    assert!(select_stream(&h, MediaType::Audio, Some(99)).is_err());
    let settings = ye::compose::ComposeSettings {
        width: 32,
        height: 32,
        frame_rate: ye::time::Rational::new(10, 1),
        gop: 25,
        quality: ye::codec::QualityMode::Crf(23),
        bit_rate: None,
        audio: AudioEncodeSettings::Aac {
            bitrate_bps: 128000,
        },
    };
    let cancel = AtomicBool::new(false);
    let mut energies = Vec::new();
    for index in [1, 2] {
        let clip = TimelineClip {
            input: Box::new(File::open(&input).unwrap()),
            stream_index: Some(index),
            source_in_us: 0,
            source_out_us: 300000,
            timeline_start_us: 0,
            timeline_end_us: 300000,
            layer: Layer::Audio(AudioMix::default()),
        };
        let mut pcm = Vec::new();
        render_frames(
            &c,
            &k,
            vec![clip],
            &settings,
            300000,
            &mut |f| {
                if let Frame::Audio(a) = f {
                    pcm.extend_from_slice(a.data());
                }
                Ok(())
            },
            &|_, _| {},
            &cancel,
        )
        .unwrap();
        let reference = Command::new(common::ffmpeg_exe().unwrap())
            .args(["-v", "error", "-i"])
            .arg(&input)
            .args([
                "-map",
                &format!("0:{index}"),
                "-t",
                "0.3",
                "-af",
                "pan=stereo|c0=c0|c1=c0",
                "-ac",
                "2",
                "-ar",
                "48000",
                "-f",
                "f32le",
                "-",
            ])
            .output()
            .unwrap();
        assert!(reference.status.success());
        assert_eq!(pcm.len(), reference.stdout.len());
        let max = pcm
            .chunks_exact(4)
            .zip(reference.stdout.chunks_exact(4))
            .map(|(a, b)| {
                (f32::from_le_bytes(a.try_into().unwrap())
                    - f32::from_le_bytes(b.try_into().unwrap()))
                .abs()
            })
            .fold(0.0f32, f32::max);
        assert!(max < 0.00015, "stream {index}: {max}");
        let peaks = ye::waveform::audio_peaks_aligned_stream_with_count(
            Box::new(File::open(&input).unwrap()),
            Some(index),
            64,
            &cancel,
        )
        .unwrap();
        energies.push(peaks.peaks[0].iter().map(|p| p.rms * p.rms).sum::<f32>());
    }
    assert!(
        energies[0] > energies[1] * 10.0,
        "wrong stream or mixed waveforms: {energies:?}"
    );
    std::fs::remove_file(input).unwrap();
}
