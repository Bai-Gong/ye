//! compose 合成泵测试（M4 首轮：顺序拼接）：以 conformance 向量驱动
//! `ye::compose::compose`，断言单段重编码（尺寸/帧率改写）、两段拼接
//! （帧数=两段窗口帧数之和、时戳连续、解码干净、音频跨段连续）、剥音频
//! 链路与拒绝面（Copy / 空窗 / 窗内无帧 / 音频参数不一致）。
//!
//! 向量缺席（未跑 `bash tests/conformance/fetch.sh`）按**失败**处理
//! ——门禁不许假绿（与 tests/convert.rs 同口径）。双流输入用自家
//! Mp4Writer 现场组装（参照 tests/convert_audio.rs 的
//! strip_video_on_dual_stream_input_keeps_audio 组装手法）；ffmpeg 侧
//! 对拍归 jian 仓的 e2e（剪的导出接线），本文件纯自家园地。

use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ye::codec::{CodecId, CodecRegistry, Decoded, Frame, MediaType, QualityMode};
use ye::compose::{ComposeSegment, ComposeSettings, compose};
use ye::container::mp4::Mp4Reader;
use ye::container::{ContainerRegistry, Metadata, ReadSeek, StreamTemplate};
use ye::convert::{AudioEncodeSettings, ConvertError, filter_ops};
use ye::time::{Rational, Timestamp};

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

/// 以只读文件句柄打开向量（compose 按值消费 `Box<dyn ReadSeek>`）。
fn open_vector(path: &Path) -> Box<dyn ReadSeek> {
    let file = std::fs::File::open(path).expect("打开向量");
    Box::new(std::io::BufReader::new(file))
}

// ---- 注册表（H.264/AAC 解码 + 编码真工厂，与 convert_audio.rs 同款） ----

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
        "ye-compose-{tag}-{}-{nanos}.mp4",
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

/// 基准设置（调用处覆盖差异字段）。
fn base_settings() -> ComposeSettings {
    ComposeSettings {
        width: 176,
        height: 144,
        frame_rate: Rational::new(15, 1),
        gop: 12,
        quality: QualityMode::Crf(23),
        bit_rate: None,
        audio: AudioEncodeSettings::Aac {
            bitrate_bps: 128_000,
        },
    }
}

/// 用自家 Mp4Writer 组装双流输入：视频轨 = h264-ip 向量直通、音频轨 =
/// aac 向量直通（平移到首包 dts=0）。返回组装文件路径（调用方负责删）。
fn assemble_dual_stream(dir_tag: &str) -> PathBuf {
    let mut containers = ContainerRegistry::new();
    ye::container::mp4::register_mp4(&mut containers);
    let (path, file) = output_file(dir_tag);
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
            media_type: MediaType::Video,
            codec: v_stream.codec.clone(),
            time_base: v_stream.time_base,
            frame_rate: None,
            metadata: Metadata::new(),
        },
        StreamTemplate {
            media_type: MediaType::Audio,
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
    path
}

// ---- ① 单段全窗重编码（尺寸/帧率改写） ----

/// h264-ip 向量（45 帧 @15fps、3s）→ 320x240 @10fps、剥音频 → 30 tick
/// 重编码，回读断言流参数/时戳/解码回环。
#[test]
fn single_segment_reencodes_with_size_and_fps_rewrite() {
    let (containers, codecs) = registries();
    let (out_path, out_file) = output_file("single");
    let settings = ComposeSettings {
        width: 320,
        height: 240,
        frame_rate: Rational::new(10, 1),
        audio: AudioEncodeSettings::Drop,
        ..base_settings()
    };
    let segments = vec![ComposeSegment {
        input: open_vector(&vector("h264-ip-testsrc2-176x144.mp4")),
        trim_in_us: 0,
        trim_out_us: 3_000_000,
    }];
    let stats = compose(
        &containers,
        &codecs,
        segments,
        "mp4",
        Box::new(BufWriter::new(out_file)),
        &settings,
        filter_ops(),
        &|_, _| {},
        &AtomicBool::new(false),
    )
    .expect("单段重编码应成功");
    // 30 tick（⌈3s × 10fps⌉）、单视频流、无音频包。
    assert_eq!(stats.streams, 1);
    assert_eq!(stats.video_frames, 30, "3s @10fps = 30 tick");
    assert_eq!(stats.audio_frames, 0);
    assert_eq!(stats.total_us, 3_000_000, "total = 窗口时长");
    assert_eq!(stats.processed_us, 2_900_000, "末 tick 29/10s → µs");

    // 回读：320x240、时基 1/10、30 样例、时戳按帧号连续。
    let bytes = read_and_remove(&out_path);
    let mut mp4 = Mp4Reader::from_vec(bytes).expect("输出应可解封装");
    assert_eq!(mp4.streams().len(), 1);
    let stream = mp4.stream(0).unwrap();
    assert_eq!(stream.codec.codec, CodecId::H264);
    assert_eq!(stream.time_base, Rational::new(1, 10), "时基 = 1/帧率分子");
    let video = stream.codec.video.as_ref().expect("视频参数");
    assert_eq!((video.width, video.height), (320, 240));
    let avcc = stream
        .codec
        .extradata
        .clone()
        .expect("视频轨应带 avcC extradata");
    let count = mp4.sample_count(0).unwrap();
    assert_eq!(count, 30);
    for i in 0..count {
        let pkt = mp4.read_packet(0, i).unwrap();
        assert_eq!(
            pkt.dts.unwrap().ticks(),
            i as i64,
            "样例 {i} dts = 帧号 tick"
        );
        assert_eq!(pkt.pts.unwrap().ticks(), i as i64);
        assert_eq!(pkt.duration.unwrap().ticks(), 1);
    }

    // 解码回环：自家解码器吃 avcC + 输出包 → 30 帧 320x240 yuv420p。
    decode_all_frames_h264(&mut mp4, &avcc, 30, (320, 240));
}

// ---- ② 两段拼接：帧数=两段窗口帧数之和、时戳连续、解码干净 ----

/// 组装双流向量开两份输入（窗口各 [0, 2s)）→ 60 tick、视频时戳 0..60
/// 连续、音频跨段连续累积出包；进度 total=Σ窗口 且单调。
#[test]
fn two_segment_concat_full_chain() {
    let dual = assemble_dual_stream("dual-src");
    let (containers, codecs) = registries();
    let (out_path, out_file) = output_file("concat");
    let segments = vec![
        ComposeSegment {
            input: open_vector(&dual),
            trim_in_us: 0,
            trim_out_us: 2_000_000,
        },
        ComposeSegment {
            input: open_vector(&dual),
            trim_in_us: 0,
            trim_out_us: 2_000_000,
        },
    ];
    let progress_log = std::sync::Mutex::new(Vec::new());
    let progress = &|processed, total| progress_log.lock().unwrap().push((processed, total));
    let stats = compose(
        &containers,
        &codecs,
        segments,
        "mp4",
        Box::new(BufWriter::new(out_file)),
        &base_settings(),
        filter_ops(),
        progress,
        &AtomicBool::new(false),
    )
    .expect("两段拼接应成功");

    // 帧数 = 30 + 30（两段窗口帧数之和）；双输出流。
    assert_eq!(stats.streams, 2);
    assert_eq!(stats.video_frames, 60, "两段各 ⌈2s×15fps⌉=30 tick");
    assert!(stats.audio_frames > 0, "应重编码出音频包");
    assert_eq!(stats.total_us, 4_000_000, "total = Σ窗口 = 4s");

    // 进度：单调不减、total 恒 4s。
    let progress_log = progress_log.into_inner().unwrap();
    assert!(progress_log.len() >= 60, "至少逐视频包一报");
    for pair in progress_log.windows(2) {
        assert!(pair[0].0 <= pair[1].0, "processed_us 单调不减：{pair:?}");
    }
    assert!(progress_log.iter().all(|&(_, t)| t == 4_000_000));

    // 回读：视频轨时戳连续（dts=帧号、duration=1、pts=dts）。
    let bytes = read_and_remove(&out_path);
    let mut mp4 = Mp4Reader::from_vec(bytes).expect("输出应可解封装");
    assert_eq!(mp4.streams().len(), 2);
    let v_stream = mp4.stream(0).unwrap();
    assert_eq!(v_stream.codec.codec, CodecId::H264);
    assert_eq!(v_stream.time_base, Rational::new(1, 15));
    let avcc = v_stream
        .codec
        .extradata
        .clone()
        .expect("视频轨应带 avcC extradata");
    let v_count = mp4.sample_count(0).unwrap();
    assert_eq!(v_count, 60);
    for i in 0..v_count {
        let pkt = mp4.read_packet(0, i).unwrap();
        assert_eq!(
            pkt.dts.unwrap().ticks(),
            i as i64,
            "样例 {i}：跨段时戳连续（全局帧号）"
        );
        assert_eq!(pkt.pts.unwrap().ticks(), i as i64);
        assert_eq!(pkt.duration.unwrap().ticks(), 1);
    }
    decode_all_frames_h264(&mut mp4, &avcc, 60, (176, 144));

    // 音频轨：AAC、44.1kHz、样例数 = 输出包数、自家解码非静音。
    let a_stream = mp4.stream(1).expect("音频输出轨");
    assert_eq!(a_stream.codec.codec, CodecId::Aac);
    let audio = a_stream.codec.audio.as_ref().expect("音频参数");
    assert_eq!(audio.sample_rate, 44_100);
    let a_count = mp4.sample_count(1).unwrap();
    assert_eq!(a_count as u64, stats.audio_frames, "逐帧一包");
    // 跨段连续性的粗口径：两段各 2s 窗口 → 样本 ≈ 2×88200 → 包数 ≈ 173。
    assert!(
        (170..=176).contains(&a_count),
        "两段 2s 音频应有 ~173 个 1024 帧包，实际 {a_count}"
    );
    let mut params = ye::codec::CodecParameters::new(CodecId::Aac, MediaType::Audio);
    params.extradata = a_stream.codec.extradata.clone();
    params.audio = a_stream.codec.audio.clone();
    let mut dec = ye_codec_aac::decoder_factory(&params).expect("AAC 解码器构造");
    let mut peak = 0.0f32;
    for i in 0..a_count {
        let pkt = mp4.read_packet(1, i).unwrap();
        let packet = ye::codec::Packet {
            data: pkt.data.clone(),
            stream_index: 1,
            pts: None,
            dts: None,
            duration: None,
            flags: ye::codec::PacketFlags::empty(),
        };
        dec.feed(&packet).expect("feed");
    }
    dec.flush();
    while let Decoded::Frame(frame) = dec.pull().expect("pull") {
        let Frame::Audio(audio) = frame else {
            panic!("AAC 解码器应出音频帧");
        };
        for c in audio.data().as_chunks::<4>().0 {
            peak = peak.max(f32::from_le_bytes(*c).abs());
        }
    }
    assert!(peak > 0.05, "跨段拼接音频应非静音（峰值 {peak}）");

    let _ = std::fs::remove_file(&dual);
}

// ---- ③ 剥音频（Drop）链路：双流输入 → 单视频轨 ----

#[test]
fn drop_audio_on_dual_stream_input() {
    let dual = assemble_dual_stream("dual-drop");
    let (containers, codecs) = registries();
    let (out_path, out_file) = output_file("drop");
    let settings = ComposeSettings {
        audio: AudioEncodeSettings::Drop,
        ..base_settings()
    };
    let segments = vec![
        ComposeSegment {
            input: open_vector(&dual),
            trim_in_us: 0,
            trim_out_us: 2_000_000,
        },
        ComposeSegment {
            input: open_vector(&dual),
            trim_in_us: 0,
            trim_out_us: 2_000_000,
        },
    ];
    let stats = compose(
        &containers,
        &codecs,
        segments,
        "mp4",
        Box::new(BufWriter::new(out_file)),
        &settings,
        filter_ops(),
        &|_, _| {},
        &AtomicBool::new(false),
    )
    .expect("剥音频拼接应成功");
    assert_eq!(stats.streams, 1, "只剩单视频轨");
    assert_eq!(stats.video_frames, 60);
    assert_eq!(stats.audio_frames, 0);
    let bytes = read_and_remove(&out_path);
    let mp4 = Mp4Reader::from_vec(bytes).expect("输出应可解封装");
    assert_eq!(mp4.streams().len(), 1);
    assert_eq!(mp4.stream(0).unwrap().codec.codec, CodecId::H264);
    let _ = std::fs::remove_file(&dual);
}

// ---- ④ 拒绝面 ----

/// 音频 Copy 显式拒绝（跨段参数一致性不保）。
#[test]
fn rejects_copy_setting() {
    let (containers, codecs) = registries();
    let (out_path, out_file) = output_file("copy");
    let settings = ComposeSettings {
        audio: AudioEncodeSettings::Copy,
        ..base_settings()
    };
    let result = compose(
        &containers,
        &codecs,
        vec![ComposeSegment {
            input: open_vector(&vector("h264-ip-testsrc2-176x144.mp4")),
            trim_in_us: 0,
            trim_out_us: 3_000_000,
        }],
        "mp4",
        Box::new(BufWriter::new(out_file)),
        &settings,
        filter_ops(),
        &|_, _| {},
        &AtomicBool::new(false),
    );
    assert!(
        matches!(&result, Err(ConvertError::InvalidArguments(msg)) if msg.contains("Copy")),
        "Copy 应明确拒绝，实际：{result:?}"
    );
    let _ = std::fs::remove_file(&out_path);
}

/// 空窗参数（终点 ≤ 起点）在入口拒绝。
#[test]
fn rejects_empty_window_param() {
    let (containers, codecs) = registries();
    let (out_path, out_file) = output_file("empty-window");
    let result = compose(
        &containers,
        &codecs,
        vec![ComposeSegment {
            input: open_vector(&vector("h264-ip-testsrc2-176x144.mp4")),
            trim_in_us: 2_000_000,
            trim_out_us: 2_000_000,
        }],
        "mp4",
        Box::new(BufWriter::new(out_file)),
        &base_settings(),
        filter_ops(),
        &|_, _| {},
        &AtomicBool::new(false),
    );
    assert!(
        matches!(&result, Err(ConvertError::InvalidArguments(msg)) if msg.contains("第 1 段")),
        "空窗应指明段号拒绝，实际：{result:?}"
    );
    let _ = std::fs::remove_file(&out_path);
}

/// 窗内无帧（窗口落在文件时长之外）：逐段处理到桶采样收尾时显式拒绝。
#[test]
fn rejects_window_without_frames() {
    let (containers, codecs) = registries();
    let (out_path, out_file) = output_file("no-frames");
    let result = compose(
        &containers,
        &codecs,
        vec![ComposeSegment {
            input: open_vector(&vector("h264-ip-testsrc2-176x144.mp4")),
            trim_in_us: 10_000_000,
            trim_out_us: 10_500_000,
        }],
        "mp4",
        Box::new(BufWriter::new(out_file)),
        &ComposeSettings {
            // 向量无音轨：剥音频（被测路径是视频窗校验，与音频无关）。
            audio: AudioEncodeSettings::Drop,
            ..base_settings()
        },
        filter_ops(),
        &|_, _| {},
        &AtomicBool::new(false),
    );
    assert!(
        matches!(&result, Err(ConvertError::InvalidArguments(msg)) if msg.contains("没有任何视频帧")),
        "窗内无帧应显式拒绝，实际：{result:?}"
    );
    let _ = std::fs::remove_file(&out_path);
}

/// 音频参数不一致拒绝：第 1 段 44.1kHz、第 2 段（stsd 声明 48kHz 的
/// 组装向量）→ InvalidArguments 指明段号与两侧参数。音频能力面校验在
/// 解码前（第 2 段音频包永不被解码，内容可为任意合法 AAC 字节）。
#[test]
fn rejects_audio_param_mismatch() {
    let dual_44k = assemble_dual_stream("mismatch-a");
    let dual_48k = assemble_dual_stream_declared_48k("mismatch-b");
    let (containers, codecs) = registries();
    let (out_path, out_file) = output_file("mismatch");
    let result = compose(
        &containers,
        &codecs,
        vec![
            ComposeSegment {
                input: open_vector(&dual_44k),
                trim_in_us: 0,
                trim_out_us: 2_000_000,
            },
            ComposeSegment {
                input: open_vector(&dual_48k),
                trim_in_us: 0,
                trim_out_us: 2_000_000,
            },
        ],
        "mp4",
        Box::new(BufWriter::new(out_file)),
        &base_settings(),
        filter_ops(),
        &|_, _| {},
        &AtomicBool::new(false),
    );
    assert!(
        matches!(&result, Err(ConvertError::InvalidArguments(msg))
            if msg.contains("第 2 段") && msg.contains("不一致")),
        "音频参数不一致应指明段号拒绝，实际：{result:?}"
    );
    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_file(&dual_44k);
    let _ = std::fs::remove_file(&dual_48k);
}

/// 取消检查点：入口取消不触发进度。
#[test]
fn cancel_before_start_short_circuits() {
    let (containers, codecs) = registries();
    let (out_path, out_file) = output_file("cancel");
    let reported = AtomicUsize::new(0);
    let progress = &|_, _| {
        reported.fetch_add(1, Ordering::SeqCst);
    };
    let result = compose(
        &containers,
        &codecs,
        vec![ComposeSegment {
            input: open_vector(&vector("h264-ip-testsrc2-176x144.mp4")),
            trim_in_us: 0,
            trim_out_us: 3_000_000,
        }],
        "mp4",
        Box::new(BufWriter::new(out_file)),
        &ComposeSettings {
            audio: AudioEncodeSettings::Drop,
            ..base_settings()
        },
        filter_ops(),
        progress,
        &AtomicBool::new(true),
    );
    assert!(matches!(result, Err(ConvertError::Cancelled)));
    assert_eq!(
        reported.load(Ordering::SeqCst),
        0,
        "入口取消不得触发进度回调"
    );
    let _ = std::fs::remove_file(&out_path);
}

/// 中途取消：第 3 次进度上报后置位 → 下一个输入包检查点返回 Cancelled。
#[test]
fn cancel_at_package_checkpoint() {
    let (containers, codecs) = registries();
    let (out_path, out_file) = output_file("cancel-mid");
    let cancel = AtomicBool::new(false);
    let seen = AtomicUsize::new(0);
    let progress = &|_, _| {
        if seen.fetch_add(1, Ordering::SeqCst) == 2 {
            cancel.store(true, Ordering::SeqCst);
        }
    };
    let result = compose(
        &containers,
        &codecs,
        vec![ComposeSegment {
            input: open_vector(&vector("h264-ip-testsrc2-176x144.mp4")),
            trim_in_us: 0,
            trim_out_us: 3_000_000,
        }],
        "mp4",
        Box::new(BufWriter::new(out_file)),
        &ComposeSettings {
            audio: AudioEncodeSettings::Drop,
            ..base_settings()
        },
        filter_ops(),
        progress,
        &cancel,
    );
    assert!(
        matches!(result, Err(ConvertError::Cancelled)),
        "应返回取消：{result:?}"
    );
    assert!(
        seen.load(Ordering::SeqCst) >= 3,
        "取消前应有进度上报（每包检查点）"
    );
    let _ = std::fs::remove_file(&out_path);
}

// ---- 辅助 ----

/// 组装「音频轨声明 48kHz」的双流向量（stsd 参数与包内容刻意不一致——
/// compose 的参数校验在解码前，48kHz 声明即为被测分歧点）。视频轨照常
/// 直通 h264-ip 向量；音频轨写一个包（任意合法 AAC 字节），模板采样率
/// 48000、时基 1/48000。
fn assemble_dual_stream_declared_48k(dir_tag: &str) -> PathBuf {
    let mut containers = ContainerRegistry::new();
    ye::container::mp4::register_mp4(&mut containers);
    let (path, file) = output_file(dir_tag);
    let mut muxer = containers
        .create_muxer("mp4", Box::new(BufWriter::new(file)))
        .expect("组装 muxer");

    let mut video_src = Mp4Reader::open(vector("h264-ip-testsrc2-176x144.mp4")).unwrap();
    let mut audio_src = Mp4Reader::open(vector("aac-44k1-st-m4a.mp4")).unwrap();
    let v_stream = video_src.stream(0).unwrap().clone();
    let a_stream = audio_src.stream(0).unwrap().clone();
    let v_count = video_src.sample_count(0).unwrap();
    let declared_rate = 48_000;
    let layout = a_stream
        .codec
        .audio
        .as_ref()
        .and_then(|a| a.channel_layout)
        .expect("向量音频声道布局");
    let mut audio_params = ye::codec::CodecParameters::new(CodecId::Aac, MediaType::Audio);
    // extradata 沿用 44.1k 向量的 esds（内容不入被测路径——参数校验先于解码）。
    audio_params.extradata = a_stream.codec.extradata.clone();
    audio_params.audio = Some(ye::codec::AudioParams {
        sample_rate: declared_rate,
        channel_layout: Some(layout),
        sample_format: None,
    });
    let templates = vec![
        StreamTemplate {
            media_type: MediaType::Video,
            codec: v_stream.codec.clone(),
            time_base: v_stream.time_base,
            frame_rate: None,
            metadata: Metadata::new(),
        },
        StreamTemplate {
            media_type: MediaType::Audio,
            codec: audio_params,
            time_base: Rational::new(1, 48_000),
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
    // 单个音频包：数据取自 44.1k 向量首包（形态合法即可，不会被解码）。
    let src_pkt = audio_src.read_packet(0, 0).unwrap();
    let packet = ye::codec::Packet {
        data: src_pkt.data.clone(),
        stream_index: 1,
        pts: Some(Timestamp::new(0, Rational::new(1, 48_000))),
        dts: Some(Timestamp::new(0, Rational::new(1, 48_000))),
        duration: Some(Timestamp::new(1024, Rational::new(1, 48_000))),
        flags: ye::codec::PacketFlags::empty(),
    };
    muxer.write_packet(&packet).expect("写音频包");
    muxer.write_trailer().expect("写尾");
    path
}

/// 自家 H.264 解码器回环：avcC + 全部输出包 → `expect_frames` 帧
/// （`size` 尺寸、yuv420p、非全零）。
fn decode_all_frames_h264(
    mp4: &mut Mp4Reader<std::io::Cursor<Vec<u8>>>,
    avcc: &[u8],
    expect_frames: u64,
    size: (u32, u32),
) {
    let mut params = ye::codec::CodecParameters::new(CodecId::H264, MediaType::Video);
    params.extradata = Some(avcc.to_vec());
    params.video = mp4.stream(0).unwrap().codec.video.clone();
    let mut dec = ye_codec_h264::decoder_factory(&params).expect("解码器构造");
    let count = mp4.sample_count(0).unwrap();
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
    while let Decoded::Frame(frame) = dec.pull().expect("pull") {
        let Frame::Video(video) = frame else {
            panic!("H.264 解码器应出视频帧");
        };
        assert_eq!((video.width(), video.height()), size);
        assert_eq!(video.format(), ye::pixfmt::PixelFormat::Yuv420p);
        assert!(video.plane(0).iter().any(|&b| b != 0), "帧应有内容");
        frames += 1;
    }
    assert_eq!(frames, expect_frames, "输出应可完整解码 {expect_frames} 帧");
}
