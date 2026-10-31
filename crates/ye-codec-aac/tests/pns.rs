//! PNS 噪声重建采用统计验收，非 PNS 的逐样本门禁保持不变。
use std::process::Command;
use ye_codec::{CodecId, CodecParameters, Decoded, Decoder, Frame, MediaType, Packet};
use ye_codec_aac::AacDecoder;

fn decode(data: &[u8]) -> Vec<f32> {
    let mut dec = AacDecoder::new(&CodecParameters::new(CodecId::Aac, MediaType::Audio)).unwrap();
    let mut samples = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        assert!(offset + 7 <= data.len());
        let len = (((data[offset + 3] & 3) as usize) << 11)
            | ((data[offset + 4] as usize) << 3)
            | ((data[offset + 5] as usize) >> 5);
        assert!(len >= 7 && offset + len <= data.len());
        let mut packet = Packet::empty(0);
        packet.data = data[offset..offset + len].into();
        dec.feed(&packet).unwrap();
        loop {
            match dec.pull().unwrap() {
                Decoded::Frame(Frame::Audio(f)) => {
                    for b in f.data().chunks_exact(4) {
                        samples.push(f32::from_le_bytes(b.try_into().unwrap()));
                    }
                }
                Decoded::NeedInput | Decoded::Finished => break,
                _ => panic!("unexpected video"),
            }
        }
        offset += len;
    }
    samples
}

#[test]
fn pns_sine_and_noise_energy_and_repeatability() {
    if !Command::new("ffmpeg")
        .arg("-version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        eprintln!("SKIP oracle_missing: PNS integration");
        return;
    }
    for (rate, channels, source) in [
        (44100, 1, "sine=frequency=997:sample_rate=44100:duration=2"),
        (
            48000,
            2,
            "anoisesrc=color=white:amplitude=0.2:duration=2:seed=739:sample_rate=48000",
        ),
        (
            48000,
            2,
            "aevalsrc=0.2*sin(2*PI*997*t)*abs(sin(2*PI*8*t)):s=48000:d=2",
        ),
    ] {
        let encoded = Command::new("ffmpeg")
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                source,
                "-ac",
                &channels.to_string(),
                "-c:a",
                "aac",
                "-b:a",
                "48k",
                "-aac_pns",
                "1",
                "-f",
                "adts",
                "-",
            ])
            .output()
            .unwrap();
        assert!(
            encoded.status.success(),
            "{}",
            String::from_utf8_lossy(&encoded.stderr)
        );
        let ours = decode(&encoded.stdout);
        assert_eq!(ours, decode(&encoded.stdout), "per-decoder RNG must reset");
        assert!(ours.iter().all(|x| x.is_finite()));
        let dir =
            std::env::temp_dir().join(format!("ye-pns-{}-{rate}-{channels}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("source.aac");
        std::fs::write(&path, &encoded.stdout).unwrap();
        let reference = Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&path)
            .args(["-f", "f32le", "-"])
            .output()
            .unwrap();
        assert!(reference.status.success());
        let reference: Vec<f32> = reference
            .stdout
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        assert_eq!(ours.len(), reference.len());
        let energy = |s: &[f32]| s.iter().map(|&x| f64::from(x).powi(2)).sum::<f64>();
        let db = 10.0 * (energy(&ours) / energy(&reference)).log10();
        assert!(db.abs() <= 1.0, "{rate}/{channels}: energy Δ={db}dB");
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
