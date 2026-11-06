use std::{fs, path::PathBuf, process::Command, sync::atomic::AtomicBool};
struct Temp(PathBuf);
impl Temp {
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "ye-cli-wav-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&p).unwrap();
        Self(p)
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn fixture() -> Vec<u8> {
    let mut b = b"RIFF".to_vec();
    b.extend(9636u32.to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(48000u32.to_le_bytes());
    b.extend(96000u32.to_le_bytes());
    b.extend(2u16.to_le_bytes());
    b.extend(16u16.to_le_bytes());
    b.extend(b"data");
    b.extend(9600u32.to_le_bytes());
    for i in 0..4800 {
        b.extend(((i % 100) * 200 - 10000i16).to_le_bytes());
    }
    b
}
#[test]
fn cli_probes_and_exports_wav_and_converts_pcm_to_aac() {
    let dir = Temp::new();
    let input = dir.0.join("source.wav");
    let wav = dir.0.join("float.wav");
    let mp4 = dir.0.join("audio.mp4");
    fs::write(&input, fixture()).unwrap();
    let report = ye_cli::probe(&input).unwrap();
    assert!(report.contains(": wav,"));
    assert!(report.contains("codec=pcm"));
    let output = Command::new(env!("CARGO_BIN_EXE_ye"))
        .env("PATH", "")
        .arg("export-wav")
        .arg(&input)
        .arg(&wav)
        .args(["--format", "f32", "--stream", "0"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let h = ye::media::inspect(Box::new(fs::File::open(&wav).unwrap())).unwrap();
    assert_eq!(h.streams[0].nb_frames, Some(4800));
    let options = ye_cli::ConvertOptions {
        audio: ye_cli::ConvertAudio::Aac { kbps: 128 },
        no_video: true,
        ..Default::default()
    };
    ye_cli::convert(&input, &mp4, &options, &|_, _| {}, &AtomicBool::new(false)).unwrap();
    let h = ye::media::inspect(Box::new(fs::File::open(&mp4).unwrap())).unwrap();
    assert_eq!(h.streams.len(), 1);
    assert_eq!(h.streams[0].codec.codec, ye::codec::CodecId::Aac);
    assert!((h.duration.unwrap().to_seconds() - 0.1).abs() < 0.000001);
    let original = fs::read(&mp4).unwrap();
    assert!(ye_cli::convert(&input, &mp4, &options, &|_, _| {}, &AtomicBool::new(false)).is_err());
    assert_eq!(fs::read(&mp4).unwrap(), original);
    assert!(ye_cli::remux(&input, &mp4).is_err());
    assert_eq!(fs::read(&mp4).unwrap(), original);
    let cancelled = dir.0.join("cancelled.mp4");
    assert!(
        ye_cli::convert(
            &input,
            &cancelled,
            &options,
            &|_, _| {},
            &AtomicBool::new(true)
        )
        .is_err()
    );
    assert!(!cancelled.exists());
    let badcopy = dir.0.join("bad-copy.mp4");
    assert!(
        ye_cli::convert(
            &input,
            &badcopy,
            &ye_cli::ConvertOptions {
                no_video: true,
                ..Default::default()
            },
            &|_, _| {},
            &AtomicBool::new(false)
        )
        .is_err()
    );
    assert!(!badcopy.exists());
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 3);
}
#[test]
fn wav_cli_rejects_ambiguous_or_malformed_arguments() {
    let parse =
        |args: &[&str]| ye_cli::parse_args(&args.iter().map(|s| (*s).into()).collect::<Vec<_>>());
    for args in [
        vec!["export-wav", "in", "out", "--format", "mp3"],
        vec!["export-wav", "in", "out", "--stream", "-1"],
        vec![
            "export-wav",
            "in",
            "out",
            "--format",
            "s16",
            "--format",
            "f32",
        ],
        vec!["export-wav", "in", "out", "--stream", "0", "--stream", "1"],
        vec!["export-wav", "in"],
    ] {
        assert!(parse(&args).is_err());
    }
    assert!(matches!(
        parse(&["export-wav", "in", "out"]),
        Ok(ye_cli::Command::ExportWav { .. })
    ));
}
