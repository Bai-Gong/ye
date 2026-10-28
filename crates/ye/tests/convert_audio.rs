//! convert 泵测试（M3 补齐）：音频三态（直通/AAC-LC 重编码/剥轨）与
//! 剥视频、视频码率设置的集成行为——以 conformance 向量驱动
//! `ye::convert::convert`，断言输出容器形态、自家解码器回环与剥轨
//! 守卫。
//!
//! 向量缺席（未跑 `bash tests/conformance/fetch.sh`）按**失败**处理
//! ——门禁不许假绿（与 tests/convert.rs 同口径）。ffmpeg 侧对拍归
//! `ye-codec-aac` 的 oracle 测试与 ye-cli 的 e2e（含双轨剥视频），
//! 本文件纯自家园地。

use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use ye::codec::{CodecId, CodecRegistry, Decoded, Frame, MediaType};
use ye::container::ContainerRegistry;
use ye::container::mp4::Mp4Reader;
use ye::convert::{AudioEncodeSettings, ConvertError, VideoEncodeSettings, convert, filter_ops};

// ---- 向量定位（与 tests/convert.rs 同约定） ----

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

fn vector(name: &str) -> PathBuf {
    let path = cache_dir().join(name);
    assert!(
        path.exists(),
        "向量缺席：{}（先跑 `bash tests/conformance/fetch.sh`）",
        path.display()
    );
    path
}

/// 以只读文件句柄打开向量（`convert` 按值消费 `Box<dyn ReadSeek>`）。
fn open_vector(path: &Path) -> Box<dyn ye::container::ReadSeek> {
    let file = std::fs::File::open(path).expect("打开向量");
    Box::new(std::io::BufReader::new(file))
}

// ---- 注册表（AAC 解码 + 编码 + H.264 解码/编码真工厂） ----

fn registries() -> (ContainerRegistry, CodecRegistry) {
    let mut containers = ContainerRegistry::new();
    ye::container::mp4::register_mp4(&mut containers);
    let mut codecs = CodecRegistry::new();
    codecs.register_decoder(CodecId::H264, ye_codec_h264::decoder_factory);
    codecs.register_encoder(CodecId::H264, ye_codec_h264::encoder_factory);
    codecs.register_decoder(CodecId::Aac, ye_codec_aac::decoder_factory);
    codecs.register_encoder(CodecId::Aac, ye_codec_aac::encoder_factory);
    (containers, codecs)
}

/// 输出临时文件（`CARGO_TARGET_TMPDIR`，用后即删——仓库约定）。
fn output_file(tag: &str) -> (PathBuf, std::fs::File) {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&dir).expect("tmpdir");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!(
        "ye-convert-audio-{tag}-{}-{nanos}.mp4",
        std::process::id()
    ));
    let file = std::fs::File::create(&path).expect("创建输出临时文件");
    (path, file)
}

/// 读完即删，返回全部字节（调用方保证文件句柄已随泵丢弃关闭）。
fn read_and_remove(path: &Path) -> Vec<u8> {
    let bytes = std::fs::read(path).expect("读回输出");
    let _ = std::fs::remove_file(path);
    bytes
}

// ---- 音频重编码（纯音频输入 + video = None，一并覆盖剥视频） ----

/// 纯音频向量 → `video = None` + `audio = Aac{128k}` → 单 aac 轨、
/// 样例数与输入一致（1024 对齐）、自家解码器回环干净且非静音。
#[test]
fn audio_reencode_from_audio_only_input() {
    let (containers, codecs) = registries();
    let (out_path, out_file) = output_file("reencode");
    let stats = convert(
        &containers,
        &codecs,
        open_vector(&vector("aac-44k1-st-m4a.mp4")),
        "mp4",
        Box::new(BufWriter::new(out_file)),
        None,
        &AudioEncodeSettings::Aac {
            bitrate_bps: 128_000,
        },
        filter_ops(),
        &|_, _| {},
        &AtomicBool::new(false),
    )
    .expect("纯音频重编码应成功");
    assert_eq!(stats.streams, 1, "单 aac 输出轨");
    assert_eq!(stats.video_frames, 0, "无视频帧");
    assert!(stats.audio_frames > 0, "应有重编码输出包");

    // 回读：单 aac 轨、44.1kHz、样例数与输入一致。
    let bytes = read_and_remove(&out_path);
    let mut mp4 = Mp4Reader::from_vec(bytes).expect("输出应可解封装");
    assert_eq!(mp4.streams().len(), 1);
    let stream = mp4.stream(0).unwrap();
    assert_eq!(stream.codec.codec, CodecId::Aac);
    let audio = stream.codec.audio.as_ref().expect("音频参数");
    assert_eq!(audio.sample_rate, 44_100);
    let out_count = mp4.sample_count(0).unwrap();
    let src = Mp4Reader::open(vector("aac-44k1-st-m4a.mp4")).unwrap();
    let in_count = src.sample_count(0).unwrap();
    assert_eq!(out_count, in_count, "样例数应与输入一致（1024 对齐）");
    assert_eq!(stats.audio_frames, out_count as u64);

    // 自家解码器回环：extradata（esds）构造 → 逐包解码干净、帧数
    // 一致、输出非静音（正弦向量重编码后仍有能量）。
    let mut params = ye::codec::CodecParameters::new(CodecId::Aac, MediaType::Audio);
    params.extradata = stream.codec.extradata.clone();
    params.audio = stream.codec.audio.clone();
    let mut dec = ye_codec_aac::decoder_factory(&params).expect("解码器构造");
    for i in 0..out_count {
        let pkt = mp4.read_packet(0, i).unwrap();
        let packet = ye::codec::Packet {
            data: pkt.data.clone(),
            stream_index: 0,
            pts: None,
            dts: None,
            duration: None,
            flags: ye::codec::PacketFlags::empty(),
        };
        dec.feed(&packet).expect("feed");
    }
    dec.flush();
    let mut frames = 0u64;
    let mut peak = 0.0f32;
    while let Decoded::Frame(frame) = dec.pull().expect("pull") {
        let Frame::Audio(audio) = frame else {
            panic!("音频解码器应出音频帧");
        };
        let samples: Vec<f32> = audio
            .data()
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect();
        // packed f32 交错：峰值取全声道。
        peak = peak.max(samples.iter().fold(0.0f32, |m, s| m.max(s.abs())));
        frames += 1;
    }
    assert_eq!(frames, out_count as u64, "逐包一帧");
    assert!(peak > 0.05, "重编码输出应非静音（峰值 {peak}）");
}

// ---- 剥视频回归（双流输入） ----

/// 剥视频 + AAC 重编码在**双流输入**（视频流 0 + 音频流 1）下不丢包：
/// 编码包自带 stream_index=0（Encoder 契约），冲出循环按输入轨位回查
/// ——音轨在索引 1 时命中被剥轨即静默丢包（2026-10-26 M3 补齐 e2e
/// 实测回归的向量级锁定；单流向量的音轨恰在 0 号位测不出）。
///
/// 双流向量用自家 Mp4Writer 现场组装：视频/音频各取既有向量直通，
/// 音频轨平移到首包 dts=0（progressive 写侧约束）。
#[test]
fn strip_video_on_dual_stream_input_keeps_audio() {
    use ye::container::mp4::register_mp4;
    use ye::container::{Metadata, StreamTemplate};

    // —— 组双流输入 ——
    let mut containers = ContainerRegistry::new();
    register_mp4(&mut containers);
    let (composed, file) = output_file("dual");
    let mut muxer = containers
        .create_muxer("mp4", Box::new(BufWriter::new(file)))
        .expect("组装 muxer");

    let mut video_src = Mp4Reader::open(vector("h264-ip-testsrc2-176x144.mp4")).unwrap();
    let mut audio_src = Mp4Reader::open(vector("aac-44k1-st-m4a.mp4")).unwrap();
    let (v_stream, a_stream) = (
        video_src.stream(0).unwrap().clone(),
        audio_src.stream(0).unwrap().clone(),
    );
    let (v_count, a_count) = (
        video_src.sample_count(0).unwrap(),
        audio_src.sample_count(0).unwrap(),
    );
    let templates = vec![
        StreamTemplate {
            media_type: ye::codec::MediaType::Video,
            codec: v_stream.codec.clone(),
            time_base: v_stream.time_base,
            frame_rate: None,
            metadata: Metadata::new(),
        },
        StreamTemplate {
            media_type: ye::codec::MediaType::Audio,
            codec: a_stream.codec.clone(),
            time_base: a_stream.time_base,
            frame_rate: None,
            metadata: Metadata::new(),
        },
    ];
    muxer.write_header(&templates).expect("写头");
    for i in 0..v_count {
        let mut pkt = video_src.read_packet(0, i).unwrap();
        pkt.stream_index = 0;
        muxer.write_packet(&pkt).expect("写视频包");
    }
    let shift = audio_src
        .read_packet(0, 0)
        .unwrap()
        .dts
        .expect("音频首包 dts");
    for i in 0..a_count {
        let mut pkt = audio_src.read_packet(0, i).unwrap();
        pkt.dts = Some(pkt.dts.expect("音频包 dts").checked_sub(shift).unwrap());
        if let Some(pts) = pkt.pts {
            pkt.pts = Some(pts.checked_sub(shift).unwrap());
        }
        pkt.stream_index = 1;
        muxer.write_packet(&pkt).expect("写音频包");
    }
    muxer.write_trailer().expect("写尾");
    drop(muxer);

    // —— 被测：剥视频 + AAC 重编码 ——
    let (out_path, out_file) = output_file("dual-out");
    let (containers, codecs) = registries();
    let stats = convert(
        &containers,
        &codecs,
        open_vector(&composed),
        "mp4",
        Box::new(BufWriter::new(out_file)),
        None,
        &AudioEncodeSettings::Aac {
            bitrate_bps: 128_000,
        },
        filter_ops(),
        &|_, _| {},
        &AtomicBool::new(false),
    )
    .expect("双流剥视频重编码应成功");
    assert_eq!(stats.streams, 1, "只剩单 aac 轨");
    assert_eq!(stats.video_frames, 0);
    assert_eq!(stats.audio_frames, a_count as u64, "重编码包数应等于源包数");

    // 回读：单 aac 轨、样例数与源一致（丢包回归的硬断言）。
    let bytes = read_and_remove(&out_path);
    let mp4 = Mp4Reader::from_vec(bytes).expect("输出应可解封装");
    assert_eq!(mp4.streams().len(), 1);
    assert_eq!(mp4.stream(0).unwrap().codec.codec, CodecId::Aac);
    assert_eq!(mp4.sample_count(0).unwrap(), a_count, "样例数不得缩水");
    let _ = std::fs::remove_file(&composed);
}

// ---- 剥轨守卫 ----

/// 视频剥除 + 音频剥轨（纯音频向量）→ 零输出轨守卫触发。
#[test]
fn stripping_all_tracks_is_rejected() {
    let (containers, codecs) = registries();
    let (out_path, out_file) = output_file("strip-all");
    let result = convert(
        &containers,
        &codecs,
        open_vector(&vector("aac-44k1-st-m4a.mp4")),
        "mp4",
        Box::new(BufWriter::new(out_file)),
        None,
        &AudioEncodeSettings::Drop,
        filter_ops(),
        &|_, _| {},
        &AtomicBool::new(false),
    );
    assert!(
        matches!(result, Err(ConvertError::InvalidArguments(_))),
        "剥光所有轨应显式拒绝，实际：{result:?}"
    );
    let _ = std::fs::remove_file(&out_path);
}

// ---- 视频码率设置（真码控接线冒烟；收敛性归 ye-codec-h264 测试） ----

/// `bit_rate = Some(300k)` 的视频重编码：成功、45 帧、自家解码器
/// 回环干净（结构接线存在性；码率收敛容差归编码器 crate 的测试）。
#[test]
fn video_bitrate_setting_smoke() {
    let (containers, codecs) = registries();
    let settings = VideoEncodeSettings {
        bit_rate: Some(300_000),
        ..VideoEncodeSettings::default()
    };
    let (out_path, out_file) = output_file("vb");
    let stats = convert(
        &containers,
        &codecs,
        open_vector(&vector("h264-ip-testsrc2-176x144.mp4")),
        "mp4",
        Box::new(BufWriter::new(out_file)),
        Some(&settings),
        &AudioEncodeSettings::Copy,
        filter_ops(),
        &|_, _| {},
        &AtomicBool::new(false),
    )
    .expect("码率模式视频重编码应成功");
    assert_eq!(stats.streams, 1);
    assert_eq!(stats.video_frames, 45);

    // 回读 + 自家解码回环：45 样例全部解码。
    let bytes = read_and_remove(&out_path);
    let total_bits = bytes.len() as u64 * 8;
    let mut mp4 = Mp4Reader::from_vec(bytes).expect("输出应可解封装");
    assert_eq!(mp4.streams().len(), 1);
    let stream = mp4.stream(0).unwrap();
    let avcc = stream
        .codec
        .extradata
        .clone()
        .expect("视频轨应带 avcC extradata");
    let count = mp4.sample_count(0).unwrap();
    assert_eq!(count, 45);
    let mut params = ye::codec::CodecParameters::new(CodecId::H264, MediaType::Video);
    params.extradata = Some(avcc);
    let mut dec = ye_codec_h264::decoder_factory(&params).expect("解码器构造");
    for i in 0..count {
        let pkt = mp4.read_packet(0, i).unwrap();
        let packet = ye::codec::Packet {
            data: pkt.data.clone(),
            stream_index: 0,
            pts: None,
            dts: None,
            duration: None,
            flags: ye::codec::PacketFlags::empty(),
        };
        dec.feed(&packet).expect("feed");
    }
    dec.flush();
    let mut frames = 0u64;
    while let Decoded::Frame(_) = dec.pull().expect("pull") {
        frames += 1;
    }
    assert_eq!(frames, 45, "输出应可完整解码 45 帧");

    // 诊断信息：实测平均码率（不设容差断言——收敛性归编码器 crate）。
    eprintln!(
        "video_bitrate_setting_smoke: 输出 {} 字节 ≈ {} bps（目标 300k，3s 向量）",
        total_bits / 8,
        total_bits / 3
    );
}
