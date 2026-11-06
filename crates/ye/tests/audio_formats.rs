#![cfg(feature = "wav")]
mod common;
use std::{
    fs,
    fs::File,
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};
use ye::{audio_export::*, codec_pcm::PcmFormat, convert::ConvertError};

struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ye-audio-formats-{}-{}-{}",
            NEXT.fetch_add(1, Ordering::Relaxed),
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn wav_s16(samples: &[i16], rate: u32) -> Vec<u8> {
    let data = samples.len() as u32 * 2;
    let mut out = b"RIFF".to_vec();
    out.extend((36 + data).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend(16u32.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(1u16.to_le_bytes());
    out.extend(rate.to_le_bytes());
    out.extend((rate * 2).to_le_bytes());
    out.extend(2u16.to_le_bytes());
    out.extend(16u16.to_le_bytes());
    out.extend(b"data");
    out.extend(data.to_le_bytes());
    for sample in samples {
        out.extend(sample.to_le_bytes());
    }
    out
}
fn decode(path: &std::path::Path) -> Vec<f32> {
    use ye::codec::{Decoded, Frame};
    let mut containers = ye::container::ContainerRegistry::new();
    ye::container::register_enabled(&mut containers);
    let (_, mut d) = containers
        .open_demuxer(Box::new(File::open(path).unwrap()))
        .unwrap();
    let h = d.read_header().unwrap();
    let mut codecs = ye::codec::CodecRegistry::new();
    ye::register_decoders(&mut codecs);
    let mut decoder = codecs.create_decoder(&h.streams[0].codec).unwrap();
    let mut out = Vec::new();
    while let Some(p) = d.read_packet().unwrap() {
        decoder.feed(&p).unwrap();
        while let Decoded::Frame(Frame::Audio(f)) = decoder.pull().unwrap() {
            out.extend(
                f.data()
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes(b.try_into().unwrap())),
            );
        }
    }
    decoder.flush();
    while let Decoded::Frame(Frame::Audio(f)) = decoder.pull().unwrap() {
        out.extend(
            f.data()
                .chunks_exact(4)
                .map(|b| f32::from_le_bytes(b.try_into().unwrap())),
        );
    }
    out
}

#[test]
fn wav_import_waveform_and_all_pcm_export_formats() {
    let dir = Temp::new();
    let input = dir.path("source.bin");
    let samples: Vec<i16> = (0..4800)
        .map(|i| ((i * 127) % 65536 - 32768) as i16)
        .collect();
    fs::write(&input, wav_s16(&samples, 48000)).unwrap();
    let h = ye::media::inspect(Box::new(File::open(&input).unwrap())).unwrap();
    assert_eq!(h.format_name, "wav");
    assert_eq!(h.streams[0].codec.codec, ye::codec::CodecId::Pcm);
    let peaks = ye::waveform::audio_peaks_aligned_with_count(
        Box::new(File::open(&input).unwrap()),
        8,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        (peaks.sample_rate, peaks.channels, peaks.buckets),
        (48000, 1, 8)
    );
    assert!(peaks.peaks[0][0].max > 0.9);
    for format in [
        PcmFormat::U8,
        PcmFormat::S16,
        PcmFormat::S24,
        PcmFormat::S32,
        PcmFormat::F32,
    ] {
        let output = dir.path(&format!("{format:?}.wav"));
        let last = std::cell::Cell::new(0);
        let stats = export_wav(
            &input,
            &output,
            &WavExportOptions {
                format,
                stream_index: None,
            },
            &AtomicBool::new(false),
            &|done, total| {
                assert!(done >= last.get() && done <= total);
                last.set(done);
            },
        )
        .unwrap();
        assert_eq!(stats.sample_frames, 4800);
        assert_eq!(last.get(), 100000);
        let actual = decode(&output);
        assert_eq!(actual.len(), samples.len());
        for (&got, &source) in actual.iter().zip(&samples) {
            let wanted = f32::from(source) / 32768.;
            if format == PcmFormat::U8 {
                assert!((got - wanted).abs() <= 1. / 128.);
            } else {
                assert_eq!(got, wanted);
            }
        }
    }
}

#[test]
fn export_preserves_existing_output_and_cleans_cancelled_or_invalid_output() {
    let dir = Temp::new();
    let input = dir.path("in.wav");
    let output = dir.path("out.wav");
    fs::write(&input, wav_s16(&vec![100; 4800], 48000)).unwrap();
    fs::write(&output, b"keep me").unwrap();
    assert!(
        export_wav(
            &input,
            &output,
            &WavExportOptions::default(),
            &AtomicBool::new(false),
            &|_, _| {}
        )
        .is_err()
    );
    assert_eq!(fs::read(&output).unwrap(), b"keep me");
    fs::remove_file(&output).unwrap();
    let cancel = AtomicBool::new(false);
    assert!(matches!(
        export_wav(
            &input,
            &output,
            &WavExportOptions::default(),
            &cancel,
            &|_, _| {
                cancel.store(true, Ordering::Relaxed);
            }
        ),
        Err(ConvertError::Cancelled)
    ));
    assert!(!output.exists());
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
    assert!(
        export_wav(
            &input,
            &output,
            &WavExportOptions {
                stream_index: Some(4),
                ..Default::default()
            },
            &AtomicBool::new(false),
            &|_, _| {}
        )
        .is_err()
    );
    fs::write(&input, b"bad media").unwrap();
    assert!(
        export_wav(
            &input,
            &output,
            &WavExportOptions::default(),
            &AtomicBool::new(false),
            &|_, _| {}
        )
        .is_err()
    );
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
}

#[cfg(all(feature = "mp4", feature = "aac"))]
#[test]
fn aac_to_wav_removes_priming_and_padding_like_presentation_reference() {
    use std::process::Command;
    let Some(input) = common::ensure_vector(
        "audio-export-priming.m4a",
        &[
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=997:sample_rate=44100:duration=0.123",
            "-c:a",
            "aac",
            "-aac_pns",
            "0",
            "-y",
        ],
    ) else {
        return;
    };
    let dir = Temp::new();
    let output = dir.path("audio.wav");
    let stats = export_wav(
        &input,
        &output,
        &WavExportOptions {
            format: PcmFormat::F32,
            stream_index: None,
        },
        &AtomicBool::new(false),
        &|_, _| {},
    )
    .unwrap();
    let reference = Command::new(common::ffmpeg_exe().unwrap())
        .args(["-v", "error", "-i"])
        .arg(&input)
        .args(["-f", "f32le", "-c:a", "pcm_f32le", "-"])
        .output()
        .unwrap();
    assert!(reference.status.success());
    let reference: Vec<f32> = reference
        .stdout
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
        .collect();
    let actual = decode(&output);
    assert_eq!(actual.len(), stats.sample_frames as usize);
    assert_eq!(actual.len(), reference.len());
    assert!(
        actual
            .iter()
            .zip(reference)
            .all(|(a, b)| (a - b).abs() < 0.00015)
    );
    fs::remove_file(input).unwrap();
}

#[cfg(all(feature = "mp4", feature = "aac", feature = "h264", feature = "png"))]
#[test]
fn quicktime_h264_aac_reaches_frame_waveform_and_wav_export_facades() {
    use std::process::Command;
    let Some(input) = common::ensure_vector(
        "mov-facade.mov",
        &[
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=48x32:rate=30:duration=0.4",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=997:sample_rate=48000:duration=0.4",
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
            "-y",
        ],
    ) else {
        return;
    };
    let dir = Temp::new();
    let mp4 = dir.path("same-streams.mp4");
    assert!(
        Command::new(common::ffmpeg_exe().unwrap())
            .args(["-v", "error", "-i"])
            .arg(&input)
            .args(["-c", "copy"])
            .arg(&mp4)
            .status()
            .unwrap()
            .success()
    );
    let header = ye::media::inspect(Box::new(File::open(&input).unwrap())).unwrap();
    assert_eq!(header.format_name, "mov");
    for at in [0, 100000, 233333] {
        let mov =
            ye::frame_grab::decode_frame_rgba(Box::new(File::open(&input).unwrap()), at).unwrap();
        let expected =
            ye::frame_grab::decode_frame_rgba(Box::new(File::open(&mp4).unwrap()), at).unwrap();
        assert_eq!(mov, expected);
    }
    let cancel = AtomicBool::new(false);
    let peaks = ye::waveform::audio_peaks_aligned_with_count(
        Box::new(File::open(&input).unwrap()),
        8,
        &cancel,
    )
    .unwrap();
    assert_eq!(peaks.sample_rate, 48000);
    assert!(peaks.peaks[0].iter().any(|p| p.rms > 0.05));
    let stats = export_wav(
        &input,
        &dir.path("mov-audio.wav"),
        &WavExportOptions::default(),
        &cancel,
        &|_, _| {},
    )
    .unwrap();
    assert_eq!(stats.sample_frames, 19200);
    fs::remove_file(input).unwrap();
}

#[cfg(all(feature = "mp4", feature = "aac"))]
#[test]
fn selected_audio_export_keeps_leading_silence_and_rejects_ambiguity() {
    use ye::time::{Rational, Rounding, Timestamp};
    let Some(input) = common::ensure_vector(
        "export-selected-offset.mp4",
        &[
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=0.2",
            "-itsoffset",
            "0.1",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000:duration=0.2",
            "-map",
            "0:a",
            "-map",
            "1:a",
            "-c:a",
            "aac",
            "-aac_pns",
            "0",
            "-y",
        ],
    ) else {
        return;
    };
    let dir = Temp::new();
    let cancel = AtomicBool::new(false);
    assert!(
        export_wav(
            &input,
            &dir.path("ambiguous.wav"),
            &WavExportOptions::default(),
            &cancel,
            &|_, _| {}
        )
        .is_err()
    );
    assert!(!dir.path("ambiguous.wav").exists());
    let header = ye::media::inspect(Box::new(File::open(&input).unwrap())).unwrap();
    let stream = &header.streams[1];
    let tb = Rational::new(1, 48000);
    let start = stream.start_time.unwrap_or(Timestamp::new(0, tb));
    let lead = start.rescale_with(tb, Rounding::Up).unwrap().ticks() as usize;
    assert!(lead >= 3000);
    let total = start
        .checked_add(stream.duration.unwrap())
        .unwrap()
        .rescale_with(tb, Rounding::Up)
        .unwrap()
        .ticks() as usize;
    let output = dir.path("selected.wav");
    let stats = export_wav(
        &input,
        &output,
        &WavExportOptions {
            stream_index: Some(1),
            format: PcmFormat::F32,
        },
        &cancel,
        &|_, _| {},
    )
    .unwrap();
    assert_eq!(stats.sample_frames, total as u64);
    let actual = decode(&output);
    assert_eq!(actual.len(), total);
    assert!(actual[..lead].iter().all(|v| *v == 0.));
    let voiced = &actual[lead + 1024..];
    let crossings = voiced
        .windows(2)
        .filter(|p| p[0] <= 0. && p[1] > 0.)
        .count();
    let hz = crossings as f64 * 48000. / voiced.len() as f64;
    assert!((hz - 880.).abs() < 10., "selected stream frequency={hz}");
    fs::remove_file(input).unwrap();
}

#[test]
fn pcm_wav_is_a_real_timeline_audio_source() {
    use ye::{
        codec::{Frame, QualityMode},
        compose::ComposeSettings,
        convert::AudioEncodeSettings,
        time::Rational,
        timeline::{AudioMix, Layer, TimelineClip},
    };
    let dir = Temp::new();
    let input = dir.path("timeline.wav");
    fs::write(&input, wav_s16(&vec![8192; 4800], 48000)).unwrap();
    let mut containers = ye::container::ContainerRegistry::new();
    ye::container::register_enabled(&mut containers);
    let mut codecs = ye::codec::CodecRegistry::new();
    ye::register_decoders(&mut codecs);
    let settings = ComposeSettings {
        width: 16,
        height: 16,
        frame_rate: Rational::new(10, 1),
        gop: 1,
        quality: QualityMode::Qp(20),
        bit_rate: None,
        audio: AudioEncodeSettings::Aac {
            bitrate_bps: 128000,
        },
    };
    let clip = TimelineClip {
        input: Box::new(File::open(&input).unwrap()),
        stream_index: None,
        source_in_us: 0,
        source_out_us: 100000,
        timeline_start_us: 0,
        timeline_end_us: 100000,
        layer: Layer::Audio(AudioMix { gain: 0.5, pan: 0. }),
    };
    let mut samples = 0;
    ye::timeline::render_frames(
        &containers,
        &codecs,
        vec![clip],
        &settings,
        100000,
        &mut |frame| {
            if let Frame::Audio(a) = frame {
                samples += a.frames();
                for b in a.data().chunks_exact(4) {
                    assert!((f32::from_le_bytes(b.try_into().unwrap()) - 0.125).abs() < 0.000001);
                }
            }
            Ok(())
        },
        &|_, _| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(samples, 4800);
}
