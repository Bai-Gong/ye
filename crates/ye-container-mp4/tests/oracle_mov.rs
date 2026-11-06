//! Real QuickTime H.264/AAC, classic no-ftyp MOV, seek and lossless remux.
use std::{
    fs::File,
    io::Cursor,
    path::{Path, PathBuf},
    process::Command,
};
use ye_container_mp4::{
    ContainerRegistry, Layout, MediaType, Mp4Reader, Mp4Writer, Mp4WriterOptions, PacketFlags,
    Rational, Timestamp, TrackConfig, register_mp4,
};

struct Fixture(PathBuf);
impl Fixture {
    fn create() -> Option<Self> {
        if !["ffmpeg", "ffprobe"].iter().all(|name| {
            Command::new(name)
                .arg("-version")
                .output()
                .is_ok_and(|o| o.status.success())
        }) {
            eprintln!("skip: MOV oracle requires ffmpeg and ffprobe");
            return None;
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("mov-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("source.mov");
        let status = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=96x64:rate=30000/1001:duration=1.2",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=701:sample_rate=48000:duration=1.2",
                "-c:v",
                "libx264",
                "-preset",
                "veryfast",
                "-x264-params",
                "keyint=12:min-keyint=12:scenecut=0:bframes=2",
                "-c:a",
                "aac",
                "-b:a",
                "96k",
                "-f",
                "mov",
            ])
            .arg(&input)
            .status()
            .unwrap();
        assert!(status.success(), "MOV fixture generation failed");
        Some(Self(dir))
    }
    fn source(&self) -> PathBuf {
        self.0.join("source.mov")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn registry() -> ContainerRegistry {
    let mut r = ContainerRegistry::new();
    register_mp4(&mut r);
    r
}
fn decode(path: &Path) {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-f", "null", "-"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn quicktime_aac_sound_description_seek_classic_and_remux() {
    let Some(fixture) = Fixture::create() else {
        return;
    };
    let source = fixture.source();
    let bytes = std::fs::read(&source).unwrap();
    assert_eq!(&bytes[8..12], b"qt  ");
    let (_, mut demux) = registry()
        .open_demuxer(Box::new(File::open(&source).unwrap()))
        .unwrap();
    let header = demux.read_header().unwrap();
    assert_eq!(header.format_name, "mov");
    assert_eq!(demux.format_name(), "mov");
    assert_eq!(header.streams.len(), 2);
    assert_eq!(
        header.streams[0].codec.codec,
        ye_container_mp4::CodecId::H264
    );
    assert_eq!(
        header.streams[1].codec.codec,
        ye_container_mp4::CodecId::Aac
    );
    assert_eq!(
        header.streams[1].codec.audio.as_ref().unwrap().sample_rate,
        48000
    );
    assert!(
        !header.streams[1]
            .codec
            .extradata
            .as_ref()
            .unwrap()
            .is_empty()
    );
    let mut packets = Vec::new();
    while let Some(packet) = demux.read_packet().unwrap() {
        packets.push(packet);
    }
    for us in [0, 400_400, 800_801, 1_100_000] {
        let target = Timestamp::new(us, Rational::new(1, 1_000_000));
        let landed = demux.seek(0, target).unwrap();
        let packet = loop {
            let packet = demux.read_packet().unwrap().unwrap();
            if packet.stream_index == 0 {
                break packet;
            }
        };
        assert!(packet.flags.contains(PacketFlags::KEYFRAME));
        assert_eq!(packet.dts, Some(landed));
        assert!(packet.pts.unwrap().compare(target).is_le());
        assert!(
            packets
                .iter()
                .any(|p| p.stream_index == 0 && p.data == packet.data && p.pts == packet.pts)
        );
    }
    // Preserve byte offsets while removing ftyp identity: classic MOV may start with free/wide/mdat.
    let mut classic = bytes.clone();
    classic[4..8].copy_from_slice(b"free");
    let (_, mut old) = registry()
        .open_demuxer(Box::new(Cursor::new(classic)))
        .unwrap();
    assert_eq!(old.read_header().unwrap().format_name, "mov");
    for expected in &packets {
        let actual = old.read_packet().unwrap().unwrap();
        assert_eq!(actual.data, expected.data);
        assert_eq!(actual.pts, expected.pts);
        assert_eq!(actual.dts, expected.dts);
    }
    assert!(old.read_packet().unwrap().is_none());

    // Raw sample clocks are copied exactly; no decode/re-encode in this test.
    let mut reader = Mp4Reader::open(&source).unwrap();
    let output = fixture.0.join("remux.mp4");
    let mut writer = Mp4Writer::create(
        &output,
        Mp4WriterOptions {
            layout: Layout::Progressive,
            ..Default::default()
        },
    )
    .unwrap();
    for stream in reader.streams() {
        writer.add_stream(TrackConfig::from_stream(stream)).unwrap();
    }
    for stream in 0..reader.streams().len() {
        for sample in 0..reader.sample_count(stream).unwrap() {
            writer
                .write_packet(&reader.read_packet(stream, sample).unwrap())
                .unwrap();
        }
    }
    writer.finalize().unwrap();
    let mut remuxed = Mp4Reader::open(&output).unwrap();
    for stream in 0..reader.streams().len() {
        assert_eq!(
            reader.sample_count(stream).unwrap(),
            remuxed.sample_count(stream).unwrap()
        );
        for sample in 0..reader.sample_count(stream).unwrap() {
            let a = reader.read_packet(stream, sample).unwrap();
            let b = remuxed.read_packet(stream, sample).unwrap();
            assert_eq!(a.data, b.data);
            assert_eq!(a.pts, b.pts);
            assert_eq!(a.dts, b.dts);
        }
    }
    decode(&output);
}

#[test]
fn unsupported_quicktime_codec_and_truncation_are_errors() {
    let Some(fixture) = Fixture::create() else {
        return;
    };
    let bytes = std::fs::read(fixture.source()).unwrap();
    let mut unknown = bytes.clone();
    let stsd = unknown.windows(4).position(|w| w == b"stsd").unwrap();
    assert_eq!(&unknown[stsd + 16..stsd + 20], b"avc1");
    unknown[stsd + 16..stsd + 20].copy_from_slice(b"ap4h");
    let error = match registry().open_demuxer(Box::new(Cursor::new(unknown))) {
        Ok(_) => panic!("unsupported ProRes accepted"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("unsupported QuickTime codec"),
        "{error}"
    );
    for len in [8, bytes.len() / 2, bytes.len() - 1] {
        assert!(
            registry()
                .open_demuxer(Box::new(Cursor::new(bytes[..len].to_vec())))
                .is_err()
        );
    }
    let (_, mut demux) = registry()
        .open_demuxer(Box::new(Cursor::new(bytes)))
        .unwrap();
    let header = demux.read_header().unwrap();
    assert_eq!(header.streams[0].media_type, MediaType::Video);
    assert!(
        demux
            .seek(9, Timestamp::new(0, Rational::new(1, 1000)))
            .is_err()
    );
}
