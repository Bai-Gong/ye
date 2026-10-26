//! 施工图③ 过界门禁：AAC-LC 向量集（清单 codec=aac）逐帧对拍全过。
//!
//! 流程（每条向量）：
//! 1. 按容器入口（ADTS / LOAS / MP4-esds）解码为交错样本；
//! 2. f32 → s16 写出 `<cache>/<name>.our.pcm`
//!    （runner 主口径的接线文件，写完即被 `cargo run -p ye-conformance`
//!    自动纳入对拍）；
//! 3. 本测试内以 ffmpeg `-f s16le` 为 oracle（MP4 条带
//!    `-ignore_editlist 1`，对齐我方裸码流语义）、用 ye-conformance 的
//!    比较器与施工图③容差（100% |Δ|≤4 且每帧 |Δ|≤1 占比 ≥99.9%，
//!    逐帧判定）独立裁决。
//!
//! 三分支门禁语义：PATH 无 ffmpeg → 整组打印跳过、不计失败（oracle
//! 缺位时对拍无从谈起，先于向量检查短路）；ffmpeg 在而向量缓存缺席
//! （未跑 `bash tests/conformance/fetch.sh`）→ 仍按**失败**处理——
//! 工具就绪却没拉向量是维护疏漏，跳过即是假绿；两者齐备 → 逐条对拍。

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use ye_codec::{Decoded, Decoder, Packet};
use ye_codec_aac::AacDecoder;
use ye_conformance::compare;
use ye_conformance::manifest::Manifest;
use ye_conformance::tolerance::{self, Tolerance};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root")
        .to_path_buf()
}

fn cache_dir() -> PathBuf {
    std::env::var_os("YE_CONFORMANCE_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace_root().join("tests/conformance/.cache"))
}

/// 解一条向量（按容器入口）→ 交错 f32 样本 + 声道数。
fn decode_vector(vec_path: &PathBuf, container: &str) -> (Vec<f32>, usize) {
    let mut channels = 0usize;
    let mut samples: Vec<f32> = Vec::new();
    let mut drain = |dec: &mut AacDecoder| -> Result<(), String> {
        loop {
            match dec.pull().map_err(|e| format!("pull: {e}"))? {
                Decoded::Frame(f) => {
                    let ye_codec::Frame::Audio(a) = f else {
                        return Err("非音频帧".into());
                    };
                    if channels == 0 {
                        channels = a.channel_layout().channels() as usize;
                    }
                    for c in a.data().as_chunks::<4>().0 {
                        samples.push(f32::from_le_bytes(*c));
                    }
                }
                Decoded::NeedInput | Decoded::Finished => return Ok(()),
            }
        }
    };
    match container {
        "adts" => {
            let params =
                ye_codec::CodecParameters::new(ye_codec::CodecId::Aac, ye_codec::MediaType::Audio);
            let mut dec = AacDecoder::new(&params).expect("构造");
            let data = std::fs::read(vec_path).expect("读向量");
            let mut off = 0usize;
            while off + 7 <= data.len() {
                let len = ((data[off + 3] as usize & 0x03) << 11)
                    | ((data[off + 4] as usize) << 3)
                    | ((data[off + 5] as usize >> 5) & 0x07);
                assert!(
                    len >= 7 && off + len <= data.len(),
                    "ADTS 帧长 {len} @{off}"
                );
                let mut p = Packet::empty(0);
                p.data = Arc::from(data[off..off + len].to_vec());
                dec.feed(&p).expect("feed");
                drain(&mut dec).expect("drain");
                off += len;
            }
        }
        "loas" => {
            let params =
                ye_codec::CodecParameters::new(ye_codec::CodecId::Aac, ye_codec::MediaType::Audio);
            let mut dec = AacDecoder::new(&params).expect("构造");
            let data = std::fs::read(vec_path).expect("读向量");
            let mut off = 0usize;
            while off + 3 <= data.len() {
                if data[off] != 0x56 || data[off + 1] & 0xE0 != 0xE0 {
                    off += 1;
                    continue;
                }
                let len = (((data[off + 1] & 0x1F) as usize) << 8 | data[off + 2] as usize) + 3;
                if off + len > data.len() {
                    break;
                }
                let mut p = Packet::empty(0);
                p.data = Arc::from(data[off..off + len].to_vec());
                dec.feed(&p).expect("feed");
                drain(&mut dec).expect("drain");
                off += len;
            }
        }
        "mp4" => {
            use ye_container::Demuxer;
            let file = std::fs::File::open(vec_path).expect("打开向量");
            let mut demux = ye_container_mp4::Mp4Demuxer::new(Box::new(file)).expect("解封装");
            let header = demux.read_header().expect("读头");
            let audio = header
                .streams
                .iter()
                .find(|s| s.media_type == ye_codec::MediaType::Audio)
                .expect("音频流");
            let mut dec = AacDecoder::new(&audio.codec).expect("构造（esds 消费）");
            while let Some(pkt) = demux.read_packet().expect("读包") {
                dec.feed(&pkt).expect("feed");
                drain(&mut dec).expect("drain");
            }
        }
        other => panic!("容器 {other} 未接线"),
    }
    assert!(!samples.is_empty() && channels > 0, "解码输出为空");
    (samples, channels)
}

/// PATH 上是否有可用 ffmpeg（探测式启动 `-version`，不依赖 shell）。
fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[test]
fn aac_vectors_pass_conformance() {
    let manifest_path = workspace_root().join("tests/conformance/manifest.toml");
    let manifest = Manifest::parse(
        &std::fs::read_to_string(&manifest_path)
            .unwrap_or_else(|e| panic!("读清单 {}: {e}", manifest_path.display())),
    )
    .expect("清单解析");
    let cache = cache_dir();
    let aac: Vec<_> = manifest
        .vectors
        .iter()
        .filter(|v| v.codec == "aac")
        .collect();
    assert!(!aac.is_empty(), "清单无 aac 向量（施工图③要求 ≥3 条）");
    assert!(aac.len() >= 3, "aac 向量不足 3 条（44.1k/48k × 单/立体声）");

    let tol = tolerance::tolerances("aac")[0];
    let Tolerance::PcmTwoLevel { .. } = tol else {
        panic!("aac 容差非 PCM 双层口径")
    };
    // 三分支守卫①：ffmpeg 缺位 → 整组跳过（先于向量缓存检查，不计失败；
    // 清单/容差表的库内完整性检查不受影响）。
    if !ffmpeg_available() {
        eprintln!(
            "跳过 aac 对拍：PATH 无 ffmpeg（oracle 缺位；\
             装回 ffmpeg 并跑 `bash tests/conformance/fetch.sh` 后复测）"
        );
        return;
    }
    let mut passed = 0usize;
    for entry in &aac {
        let vec_path = cache.join(format!("{}.{}", entry.name, entry.container));
        let (samples, channels) = decode_vector(&vec_path, &entry.container);

        // 写接线文件（runner 主口径自动纳入）
        let to_s16 = |v: f32| (v * 32768.0).clamp(-32768.0, 32767.0).round() as i16;
        let our_pcm: Vec<u8> = samples
            .iter()
            .flat_map(|&v| to_s16(v).to_le_bytes())
            .collect();
        std::fs::write(cache.join(format!("{}.our.pcm", entry.name)), &our_pcm)
            .expect("写 our.pcm");

        // oracle：ffmpeg -f s16le（mp4 加 -ignore_editlist 1）
        let ref_path = std::env::temp_dir().join(format!(
            "ye-aac-{}-{}.ref.pcm",
            entry.name,
            std::process::id()
        ));
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-y", "-v", "error"]);
        if entry.container == "mp4" {
            cmd.arg("-ignore_editlist").arg("1");
        }
        cmd.arg("-i")
            .arg(&vec_path)
            .args(["-map", "0:a:0", "-f", "s16le", "-c:a", "pcm_s16le"]);
        // 守卫语义：入口已查 ffmpeg 在位（缺席即整组跳过），此处失联
        // 属环境竞态，按失败暴露。
        let status = cmd
            .arg(&ref_path)
            .status()
            .expect("启动 ffmpeg（入口守卫已确认在位）");
        assert!(status.success(), "ffmpeg 退出码 {status}");
        let ref_pcm = std::fs::read(&ref_path).expect("读 ref");
        let _ = std::fs::remove_file(&ref_path);

        let report = compare::diff_pcm_frames(&our_pcm, &ref_pcm, 1024 * channels)
            .unwrap_or_else(|e| panic!("{}: {e}", entry.name));
        tol.check_pcm_frames(&report)
            .unwrap_or_else(|e| panic!("{}: {e}", entry.name));
        passed += 1;
    }
    println!("aac conformance: {passed}/{} 全过", aac.len());
}
