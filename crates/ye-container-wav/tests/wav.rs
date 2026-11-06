use std::{
    fs::File,
    io::Cursor,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};
use ye_codec::{Decoded, Decoder, Frame, Packet, PacketFlags};
use ye_codec_pcm::{PcmDecoder, PcmFormat, parameters};
use ye_container::{ContainerRegistry, Demuxer, Metadata, Muxer, StreamTemplate};
use ye_container_wav::{WavDemuxer, WavMuxer, register_wav};
use ye_pixfmt::ChannelLayout;
use ye_time::{Rational, Timestamp};
fn fixture(data: &[u8], tag: u16, bits: u16, channels: u16) -> Vec<u8> {
    let align = channels * (bits / 8);
    let mut v = b"RIFF\0\0\0\0WAVEfmt \x10\0\0\0".to_vec();
    v.extend_from_slice(&tag.to_le_bytes());
    v.extend_from_slice(&channels.to_le_bytes());
    v.extend_from_slice(&48000u32.to_le_bytes());
    v.extend_from_slice(&(48000u32 * u32::from(align)).to_le_bytes());
    v.extend_from_slice(&align.to_le_bytes());
    v.extend_from_slice(&bits.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(data);
    if !data.len().is_multiple_of(2) {
        v.push(0);
    }
    let len = (v.len() - 8) as u32;
    v[4..8].copy_from_slice(&len.to_le_bytes());
    v
}
fn read(bytes: Vec<u8>) -> Result<WavDemuxer, ye_container::ContainerError> {
    WavDemuxer::new(Box::new(Cursor::new(bytes)))
}
#[test]
fn packets_seek_and_eof_are_sample_exact() {
    let bytes: Vec<u8> = (0..5000i16).flat_map(i16::to_le_bytes).collect();
    let mut d = read(fixture(&bytes, 1, 16, 1)).unwrap();
    let h = d.read_header().unwrap();
    assert_eq!(h.streams[0].nb_frames, Some(5000));
    assert_eq!(h.streams[0].time_base, Rational::new(1, 48000));
    let first = d.read_packet().unwrap().unwrap();
    assert_eq!(first.data.len(), 2048);
    assert_eq!(first.pts.unwrap().ticks(), 0);
    let at = d
        .seek(0, Timestamp::new(1, Rational::new(1, 1000)))
        .unwrap();
    assert_eq!(at.ticks(), 48);
    let p = d.read_packet().unwrap().unwrap();
    assert_eq!(p.pts.unwrap().ticks(), 48);
    assert_eq!(&p.data[..2], &48i16.to_le_bytes());
    assert!(d.seek(1, at).is_err());
    d.seek(0, Timestamp::new(100, Rational::new(1, 1))).unwrap();
    assert!(d.read_packet().unwrap().is_none());
    assert!(d.read_packet().unwrap().is_none());
    d.seek(0, Timestamp::new(-1, Rational::new(1, 1))).unwrap();
    assert_eq!(d.read_packet().unwrap().unwrap().pts.unwrap().ticks(), 0);
}
#[test]
fn riff_padding_unknown_chunks_and_extensible() {
    let mut v = fixture(&[128, 255, 0], 1, 8, 1);
    let mut junk = b"JUNK\x03\0\0\0abc\0".to_vec();
    v.splice(12..12, junk.drain(..));
    let len = v.len() as u32 - 8;
    v[4..8].copy_from_slice(&len.to_le_bytes());
    assert_eq!(
        read(v).unwrap().read_packet().unwrap().unwrap().data.len(),
        3
    );
    let mut v = fixture(&[0; 12], 1, 24, 2);
    v[16..20].copy_from_slice(&40u32.to_le_bytes());
    v[20..22].copy_from_slice(&0xfffeu16.to_le_bytes());
    let extra = [
        22, 0, 24, 0, 3, 0, 0, 0, 1, 0, 0, 0, 0, 0, 16, 0, 128, 0, 0, 170, 0, 56, 155, 113,
    ];
    v.splice(36..36, extra);
    let len = v.len() as u32 - 8;
    v[4..8].copy_from_slice(&len.to_le_bytes());
    assert_eq!(
        read(v.clone()).unwrap().read_header().unwrap().streams[0].nb_frames,
        Some(2)
    );
    v[38] = 20;
    assert!(read(v).is_err());
}
#[test]
fn malformed_headers_and_truncated_samples_are_rejected() {
    let valid = fixture(&[0; 8], 1, 16, 2);
    for end in 0..valid.len() {
        assert!(read(valid[..end].to_vec()).is_err(), "truncation {end}");
    }
    for (at, bytes) in [
        (0, b"RF64".to_vec()),
        (8, b"AVI ".to_vec()),
        (20, 6u16.to_le_bytes().to_vec()),
        (22, 3u16.to_le_bytes().to_vec()),
        (24, 0u32.to_le_bytes().to_vec()),
        (28, 1u32.to_le_bytes().to_vec()),
        (32, 1u16.to_le_bytes().to_vec()),
        (34, 20u16.to_le_bytes().to_vec()),
        (40, 7u32.to_le_bytes().to_vec()),
    ] {
        let mut v = valid.clone();
        v[at..at + bytes.len()].copy_from_slice(&bytes);
        assert!(read(v).is_err(), "field {at}");
    }
    let mut v = valid.clone();
    v[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(read(v).is_err());
    let mut v = valid.clone();
    v.extend_from_slice(&valid[12..36]);
    let n = v.len() as u32 - 8;
    v[4..8].copy_from_slice(&n.to_le_bytes());
    assert!(read(v).is_err());
    let mut registry = ContainerRegistry::new();
    register_wav(&mut registry);
    assert_eq!(
        registry
            .open_demuxer(Box::new(Cursor::new(valid)))
            .unwrap()
            .0,
        "wav"
    );
}
fn temp() -> std::path::PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ye-wav-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
#[test]
fn mux_enforces_state_and_preserves_odd_data() {
    let dir = temp();
    let path = dir.join("odd.wav");
    let mut w = WavMuxer::new(Box::new(File::create(&path).unwrap()));
    let tb = Rational::new(1, 48000);
    let mut p = Packet {
        data: vec![0, 128, 255].into(),
        stream_index: 0,
        pts: Some(Timestamp::new(0, tb)),
        dts: Some(Timestamp::new(0, tb)),
        duration: Some(Timestamp::new(3, tb)),
        flags: PacketFlags::KEYFRAME,
    };
    assert!(w.write_packet(&p).is_err());
    assert!(w.write_trailer().is_err());
    let s = StreamTemplate {
        media_type: ye_codec::MediaType::Audio,
        codec: parameters(PcmFormat::U8, 48000, ChannelLayout::MONO).unwrap(),
        time_base: tb,
        frame_rate: None,
        metadata: Metadata::new(),
    };
    assert!(w.write_header(&[]).is_err());
    w.write_header(std::slice::from_ref(&s)).unwrap();
    assert!(w.write_header(&[s]).is_err());
    p.pts = Some(Timestamp::new(1, tb));
    assert!(w.write_packet(&p).is_err());
    p.pts = Some(Timestamp::new(0, tb));
    w.write_packet(&p).unwrap();
    w.write_trailer().unwrap();
    assert!(w.write_packet(&p).is_err());
    assert!(w.write_trailer().is_err());
    drop(w);
    let bytes = std::fs::read(path).unwrap();
    assert_eq!(bytes.len(), 48);
    assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 3);
    assert_eq!(
        &*read(bytes).unwrap().read_packet().unwrap().unwrap().data,
        [0, 128, 255]
    );
    std::fs::remove_dir_all(dir).unwrap();
}
fn command(cmd: &mut Command) -> Vec<u8> {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}
#[test]
fn ffmpeg_all_pcm_formats_decode_and_mux_roundtrip() {
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("skip: FFmpeg missing for WAV oracle");
        return;
    }
    let dir = temp();
    for (fmt, name) in [
        (PcmFormat::U8, "pcm_u8"),
        (PcmFormat::S16, "pcm_s16le"),
        (PcmFormat::S24, "pcm_s24le"),
        (PcmFormat::S32, "pcm_s32le"),
        (PcmFormat::F32, "pcm_f32le"),
    ] {
        for channels in [1, 2] {
            let rate = if channels == 1 { 44100 } else { 48000 };
            let source = dir.join(format!("{name}-{channels}.wav"));
            command(
                Command::new("ffmpeg")
                    .args([
                        "-v",
                        "error",
                        "-f",
                        "lavfi",
                        "-i",
                        &format!("sine=frequency=997:sample_rate={rate}:duration=0.073"),
                        "-ac",
                        &channels.to_string(),
                        "-c:a",
                        name,
                        "-y",
                    ])
                    .arg(&source),
            );
            let expected = command(
                Command::new("ffmpeg")
                    .args(["-v", "error", "-i"])
                    .arg(&source)
                    .args(["-f", "f32le", "-c:a", "pcm_f32le", "-"]),
            );
            let mut d = WavDemuxer::new(Box::new(File::open(&source).unwrap())).unwrap();
            let h = d.read_header().unwrap();
            assert_eq!(
                PcmFormat::from_parameters(&h.streams[0].codec).unwrap(),
                fmt
            );
            let mut decoder = PcmDecoder::new(&h.streams[0].codec).unwrap();
            let output = dir.join(format!("output-{name}-{channels}.wav"));
            let mut mux = WavMuxer::new(Box::new(File::create(&output).unwrap()));
            mux.write_header(&[StreamTemplate {
                media_type: ye_codec::MediaType::Audio,
                codec: h.streams[0].codec.clone(),
                time_base: h.streams[0].time_base,
                frame_rate: None,
                metadata: Metadata::new(),
            }])
            .unwrap();
            let mut actual = Vec::new();
            while let Some(p) = d.read_packet().unwrap() {
                mux.write_packet(&p).unwrap();
                decoder.feed(&p).unwrap();
                let Decoded::Frame(Frame::Audio(a)) = decoder.pull().unwrap() else {
                    panic!()
                };
                actual.extend_from_slice(a.data());
            }
            assert_eq!(actual, expected, "decoder {name}/{channels}");
            mux.write_trailer().unwrap();
            drop(mux);
            let decoded = command(
                Command::new("ffmpeg")
                    .args(["-v", "error", "-i"])
                    .arg(&output)
                    .args(["-f", "f32le", "-c:a", "pcm_f32le", "-"]),
            );
            assert_eq!(decoded, expected, "mux {name}/{channels}");
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn mux_rejects_existing_cursor_and_file_without_writing() {
    use std::{
        io::{Seek, SeekFrom, Write},
        sync::{Arc, Mutex},
    };
    struct SharedCursor(Arc<Mutex<Cursor<Vec<u8>>>>);
    impl Write for SharedCursor {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(bytes)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Seek for SharedCursor {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.0.lock().unwrap().seek(pos)
        }
    }
    let bytes = b"existing output must remain unchanged".to_vec();
    let s = StreamTemplate {
        media_type: ye_codec::MediaType::Audio,
        codec: parameters(PcmFormat::F32, 48000, ChannelLayout::STEREO).unwrap(),
        time_base: Rational::new(1, 48000),
        frame_rate: None,
        metadata: Metadata::new(),
    };
    let shared = Arc::new(Mutex::new(Cursor::new(bytes.clone())));
    let mut mux = WavMuxer::new(Box::new(SharedCursor(shared.clone())));
    assert!(mux.write_header(std::slice::from_ref(&s)).is_err());
    assert_eq!(*shared.lock().unwrap().get_ref(), bytes);
    let dir = temp();
    let path = dir.join("existing.wav");
    std::fs::write(&path, &bytes).unwrap();
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    let mut mux = WavMuxer::new(Box::new(file));
    assert!(mux.write_header(&[s]).is_err());
    drop(mux);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    std::fs::remove_dir_all(dir).unwrap();
}
