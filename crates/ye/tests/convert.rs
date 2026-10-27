//! convert 泵测试（施工图④a 门禁 + M3 集成编码模式）：以 conformance
//! 向量驱动 `ye::convert::convert_frames`（解码→转换帧消费）与
//! `ye::convert::convert`（解码→处理链→H.264 编码→MP4 封装、音频
//! 直通），断言帧数 / 时戳推进 / 取消点 / 进度单调 / 输出可回读解码。
//!
//! 向量缺席（未跑 `bash tests/conformance/fetch.sh`）按**失败**处理
//! ——门禁不许假绿（与 ye-codec-h264/tests/conformance.rs 同口径）。
//!
//! 像素核（`ye-filter/src/video/`）由施工图④(b) 并行交付，帧模式组用
//! 测试本地的最小核驱动槽位：BT.601 limited-range yuv420p→rgba8 +
//! nearest 缩放——只求确定性结构正确，像素值对拍归核侧与 jian E2E
//! （施工图④ verification）；编码模式组用门面桥接
//! `ye::convert::filter_ops()`（ye-filter 真核）。

use std::io::{Cursor, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use ye::codec::{
    CodecError, CodecId, CodecRegistry, Decoded, Decoder, Frame, MediaType, PacketFlags,
    QualityMode, VideoFrame,
};
use ye::container::ContainerRegistry;
use ye::convert::{
    ConvertError, ConvertProgress, PixelOps, VideoChain, VideoEncodeSettings, convert,
    convert_frames, filter_ops,
};
use ye::pixfmt::PixelFormat;
use ye::time::{Rational, Timestamp};

// ---- 向量定位（与 ye-codec-h264/tests/conformance.rs 同约定） ----

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

// ---- 注册表（解码经 codec registry 取 h264/aac） ----

fn registries() -> (ContainerRegistry, CodecRegistry) {
    let mut containers = ContainerRegistry::new();
    ye::container::mp4::register_mp4(&mut containers);
    let mut codecs = CodecRegistry::new();
    codecs.register_decoder(CodecId::H264, ye_codec_h264::decoder_factory);
    codecs.register_decoder(CodecId::Aac, ye_codec_aac::decoder_factory);
    (containers, codecs)
}

// ---- 测试本地像素核（结构确定性，非对拍对象） ----

/// BT.601 limited-range yuv420p → rgba8（整点系数，ffmpeg swscale 同式）。
fn convert_rgba(frame: &VideoFrame, target: PixelFormat) -> Result<VideoFrame, ConvertError> {
    if target != PixelFormat::Rgba8 {
        return Err(ConvertError::InvalidArguments(format!(
            "测试核仅支持 rgba8，收到 {}",
            target.name()
        )));
    }
    if frame.format() != PixelFormat::Yuv420p {
        return Err(ConvertError::InvalidArguments(
            "测试核仅支持 yuv420p 输入".into(),
        ));
    }
    let (w, h) = (frame.width(), frame.height());
    let mut out = VideoFrame::new(
        PixelFormat::Rgba8,
        w,
        h,
        frame.color(),
        frame.pts(),
        frame.duration(),
    )?;
    let layout = *frame.layout();
    let (y_stride, c_stride) = (layout.plane(0).stride, layout.plane(1).stride);
    let (y_plane, u_plane, v_plane) = (frame.plane(0), frame.plane(1), frame.plane(2));
    let out_stride = out.layout().plane(0).stride;
    for y in 0..h as usize {
        for x in 0..w as usize {
            let luma = y_plane[y * y_stride + x] as i32 - 16;
            let cb = u_plane[(y / 2) * c_stride + x / 2] as i32 - 128;
            let cr = v_plane[(y / 2) * c_stride + x / 2] as i32 - 128;
            let clamp = |v: i32| v.clamp(0, 255) as u8;
            let px = y * out_stride + x * 4;
            out.plane_mut(0)[px..px + 4].copy_from_slice(&[
                clamp((298 * luma + 409 * cr + 128) >> 8),
                clamp((298 * luma - 100 * cb - 208 * cr + 128) >> 8),
                clamp((298 * luma + 516 * cb + 128) >> 8),
                255,
            ]);
        }
    }
    Ok(out)
}

/// nearest 邻域缩放（仅 rgba8 单平面，测试链里恒在转换之后）。
fn scale_nearest(frame: &VideoFrame, w: u32, h: u32) -> Result<VideoFrame, ConvertError> {
    if frame.format() != PixelFormat::Rgba8 {
        return Err(ConvertError::InvalidArguments(
            "测试 scale 核仅支持 rgba8".into(),
        ));
    }
    let (sw, sh) = (frame.width() as usize, frame.height() as usize);
    let (dw, dh) = (w as usize, h as usize);
    let mut out = VideoFrame::new(
        PixelFormat::Rgba8,
        w,
        h,
        frame.color(),
        frame.pts(),
        frame.duration(),
    )?;
    let src = frame.plane(0);
    let s_stride = frame.layout().plane(0).stride;
    let d_stride = out.layout().plane(0).stride;
    for y in 0..dh {
        let sy = y * sh / dh;
        for x in 0..dw {
            let sx = x * sw / dw;
            let (s, d) = (sy * s_stride + sx * 4, y * d_stride + x * 4);
            out.plane_mut(0)[d..d + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
    Ok(out)
}

fn test_ops() -> PixelOps {
    PixelOps {
        convert: convert_rgba,
        scale: scale_nearest,
    }
}

/// 调用即失败的槽（验证 no-op 链不触核）。
fn must_not_convert(_: &VideoFrame, _: PixelFormat) -> Result<VideoFrame, ConvertError> {
    Err(ConvertError::InvalidArguments("转换槽不应被调用".into()))
}

fn must_not_scale(_: &VideoFrame, _: u32, _: u32) -> Result<VideoFrame, ConvertError> {
    Err(ConvertError::InvalidArguments("scale 槽不应被调用".into()))
}

fn no_progress() -> impl Fn(i64, i64) {
    |_, _| {}
}

fn open_vector(path: &PathBuf) -> Box<dyn ye::container::ReadSeek> {
    Box::new(std::fs::File::open(path).expect("打开向量"))
}

// ---- ① 视频全链：帧数 / 时戳推进 / 进度单调 ----

#[test]
fn video_full_chain_frames_timestamps_progress() {
    let (containers, codecs) = registries();
    let cancel = AtomicBool::new(false);

    #[allow(clippy::type_complexity)] // 测试内记录 (pts, 格式, 宽, 高, 紧致字节)
    let mut frames: Vec<(Option<Timestamp>, PixelFormat, u32, u32, Vec<u8>)> = Vec::new();
    let progress_log: std::sync::Mutex<Vec<(i64, i64)>> = std::sync::Mutex::new(Vec::new());
    let chain = VideoChain {
        format: Some(PixelFormat::Rgba8),
        size: Some((88, 72)),
    };

    let mut sink = |frame: Frame| -> Result<(), ConvertError> {
        let Frame::Video(v) = frame else {
            return Err(ConvertError::Sink("视频向量不应出音频帧".into()));
        };
        // 只取每行有效像素（plane 含行尾对齐 padding，alpha 逐像素断言
        // 须用紧致字节流）。
        let stride = v.layout().plane(0).stride;
        let row = (v.width() * 4) as usize;
        let mut tight = Vec::with_capacity(row * v.height() as usize);
        for y in 0..v.height() as usize {
            tight.extend_from_slice(&v.plane(0)[y * stride..y * stride + row]);
        }
        frames.push((v.pts(), v.format(), v.width(), v.height(), tight));
        Ok(())
    };
    let progress: ConvertProgress<'_> =
        &|processed, total| progress_log.lock().unwrap().push((processed, total));

    let stats = convert_frames(
        &containers,
        &codecs,
        open_vector(&vector("h264-intra-testsrc2-176x144.mp4")),
        chain,
        test_ops(),
        &mut sink,
        progress,
        &cancel,
    )
    .expect("全链转码应成功");

    // 帧数：45（3s × 15fps，ffprobe nb_frames=45）
    assert_eq!(frames.len(), 45, "帧数应为 45");
    assert_eq!(stats.video_frames, 45);
    assert_eq!(stats.audio_frames, 0);
    assert_eq!(stats.streams, 1);

    // 时戳推进：流时基 1/15360，pts = 0, 1024, …, 45056（严格递增）
    let tb = Rational::new(1, 15_360);
    for (i, (pts, format, w, h, _)) in frames.iter().enumerate() {
        assert_eq!(
            *pts,
            Some(Timestamp::new((i * 1024) as i64, tb)),
            "第 {i} 帧 pts"
        );
        assert_eq!(*format, PixelFormat::Rgba8, "第 {i} 帧格式");
        assert_eq!((*w, *h), (88, 72), "第 {i} 帧尺寸");
    }

    // 转换真实发生：rgba 特征（alpha 全 255）且非全零（testsrc2 有内容）
    for (i, _, _, _, data) in frames.iter().take(3) {
        let _ = i;
        assert!(data.chunks(4).all(|px| px[3] == 255), "alpha 应为 255");
        assert!(data.iter().any(|&b| b != 0), "帧不应全零");
    }

    // 进度：单调不减、total 恒 3_000_000（duration 46080 ticks @ 1/15360）、
    // 每帧一报；末值 = 末帧 pts 45056 ticks → µs（NearInf）= 2_933_333。
    let progress_log = progress_log.into_inner().unwrap();
    assert!(progress_log.len() >= 45, "进度至少逐帧一报");
    let totals: Vec<i64> = progress_log.iter().map(|(_, t)| *t).collect();
    assert!(
        totals.iter().all(|&t| t == 3_000_000),
        "total_us 恒 3s: {totals:?}"
    );
    for pair in progress_log.windows(2) {
        assert!(
            pair[0].0 <= pair[1].0,
            "processed_us 单调不减被破坏：{:?} → {:?}",
            pair[0],
            pair[1]
        );
    }
    assert_eq!(stats.total_us, 3_000_000);
    assert_eq!(stats.processed_us, 2_933_333);
    assert_eq!(progress_log.last().map(|&(p, _)| p), Some(2_933_333));
}

// ---- ② no-op 链：槽不被调用，帧原样直通 ----

#[test]
fn video_noop_chain_skips_slots() {
    let (containers, codecs) = registries();
    let cancel = AtomicBool::new(false);
    let chain = VideoChain {
        format: None,
        size: None,
    };
    let mut count = 0u64;
    let mut last: Option<(PixelFormat, u32, u32)> = None;
    let mut sink = |frame: Frame| -> Result<(), ConvertError> {
        let Frame::Video(v) = frame else {
            return Err(ConvertError::Sink("非视频帧".into()));
        };
        count += 1;
        last = Some((v.format(), v.width(), v.height()));
        Ok(())
    };
    let stats = convert_frames(
        &containers,
        &codecs,
        open_vector(&vector("h264-intra-testsrc2-176x144.mp4")),
        chain,
        PixelOps {
            convert: must_not_convert,
            scale: must_not_scale,
        },
        &mut sink,
        &no_progress(),
        &cancel,
    )
    .expect("no-op 链应成功（槽不被调用）");
    assert_eq!(count, 45);
    assert_eq!(last, Some((PixelFormat::Yuv420p, 176, 144)), "帧应原样直通");
    assert_eq!(stats.video_frames, 45);
}

// ---- ③ 取消点：每包检查点 ----

#[test]
fn cancel_at_package_checkpoint() {
    let (containers, codecs) = registries();
    let cancel = AtomicBool::new(false);
    let chain = VideoChain {
        format: None,
        size: None,
    };
    let mut seen = 0u64;
    let mut sink = |frame: Frame| -> Result<(), ConvertError> {
        let Frame::Video(_) = frame else {
            return Err(ConvertError::Sink("非视频帧".into()));
        };
        seen += 1;
        if seen == 3 {
            cancel.store(true, Ordering::SeqCst);
        }
        Ok(())
    };
    let result = convert_frames(
        &containers,
        &codecs,
        open_vector(&vector("h264-intra-testsrc2-176x144.mp4")),
        chain,
        test_ops(),
        &mut sink,
        &no_progress(),
        &cancel,
    );
    // 一包一帧（intra 向量）：第 3 帧置位 → 第 4 包检查点退出。
    assert!(
        matches!(result, Err(ConvertError::Cancelled)),
        "应返回取消：{result:?}"
    );
    assert_eq!(seen, 3, "恰消费 3 帧后取消（不吞也不多跑）");
}

#[test]
fn cancel_before_start_short_circuits() {
    let (containers, codecs) = registries();
    let cancel = AtomicBool::new(true);
    let reported = AtomicUsize::new(0);
    let mut sink = |_| Ok(());
    let progress: ConvertProgress<'_> = &|_, _| {
        reported.fetch_add(1, Ordering::SeqCst);
    };
    let result = convert_frames(
        &containers,
        &codecs,
        open_vector(&vector("h264-intra-testsrc2-176x144.mp4")),
        VideoChain {
            format: None,
            size: None,
        },
        test_ops(),
        &mut sink,
        progress,
        &cancel,
    );
    assert!(matches!(result, Err(ConvertError::Cancelled)));
    assert_eq!(
        reported.load(Ordering::SeqCst),
        0,
        "入口取消：不应触发进度回调"
    );
}

// ---- ④ 音频直通（AAC 解码，无像素链） ----

#[test]
fn audio_decodes_and_passes_through() {
    let (containers, codecs) = registries();
    let cancel = AtomicBool::new(false);
    let mut pts_log: Vec<i64> = Vec::new();
    let mut sink = |frame: Frame| -> Result<(), ConvertError> {
        let Frame::Audio(a) = frame else {
            return Err(ConvertError::Sink("音频向量不应出视频帧".into()));
        };
        let pts = a
            .pts()
            .expect("AAC 帧 pts 应透传")
            .rescale(Rational::new(1, 1_000_000))
            .expect("µs 换算")
            .ticks();
        pts_log.push(pts);
        assert_eq!(a.sample_rate(), 44_100);
        assert_eq!(a.format(), ye::pixfmt::SampleFormat::F32);
        assert!(!a.data().is_empty());
        Ok(())
    };
    let stats = convert_frames(
        &containers,
        &codecs,
        open_vector(&vector("aac-44k1-st-m4a.mp4")),
        VideoChain {
            format: None,
            size: None,
        },
        test_ops(),
        &mut sink,
        &no_progress(),
        &cancel,
    )
    .expect("音频泵应成功");
    // ffprobe nb_frames=131（3s）；priming/编辑表口径不锁死，取下界。
    assert!(
        stats.audio_frames >= 120,
        "音频帧数 { }",
        stats.audio_frames
    );
    assert_eq!(stats.video_frames, 0);
    // 时戳推进：单调不减且覆盖 >2.5s。
    for pair in pts_log.windows(2) {
        assert!(pair[0] <= pair[1], "音频 pts 单调不减被破坏：{pts_log:?}");
    }
    assert!(
        pts_log.last().is_some_and(|&us| us > 2_500_000),
        "末帧 pts 应 >2.5s：{pts_log:?}"
    );
}

// ---- ⑤ 编码模式：解码 → 处理链 → H.264 编码 → MP4 封装 ----

/// 计数读取的输入包装（取消点断言：入口取消不读一字节）。
struct CountingRead {
    inner: Cursor<Vec<u8>>,
    reads: std::sync::Arc<AtomicUsize>,
}

impl Read for CountingRead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.read(buf)
    }
}

impl Seek for CountingRead {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// 编码模式注册表：容器 MP4 + H.264 解码/编码真工厂（dev 依赖直连，
/// 与门面注册面同工厂）。
fn encode_registries() -> (ContainerRegistry, CodecRegistry) {
    let mut containers = ContainerRegistry::new();
    ye::container::mp4::register_mp4(&mut containers);
    let mut codecs = CodecRegistry::new();
    codecs.register_decoder(CodecId::H264, ye_codec_h264::decoder_factory);
    codecs.register_encoder(CodecId::H264, ye_codec_h264::encoder_factory);
    (containers, codecs)
}

/// 编码模式的输出通道：临时文件（`convert` 按值消费 `Box<dyn WriteSeek>`，
/// 文件路径可回读；落 `CARGO_TARGET_TMPDIR`、用后即删——仓库约定）。
/// 返回 `(路径, 文件句柄)`——句柄进 `BufWriter` 后由路径回读。
fn output_file(tag: &str) -> (PathBuf, std::fs::File) {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&dir).expect("tmpdir");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!(
        "ye-convert-encode-{tag}-{}-{nanos}.mp4",
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

/// 视频重编码 + 缩放全链：h264-ip 向量（45 帧 @15fps）→ 320x240 →
/// MP4 → 回读断言（流参数 / 帧数 / 时戳 / GOP / 进度 / 解码回环）。
#[test]
fn encode_mode_reencodes_and_scales() {
    let (containers, codecs) = encode_registries();
    let cancel = AtomicBool::new(false);
    let settings = VideoEncodeSettings {
        chain: VideoChain {
            format: None,
            size: Some((320, 240)),
        },
        gop: 12,
        quality: QualityMode::Qp(24),
    };
    let progress_log = std::sync::Mutex::new(Vec::new());
    let progress: ConvertProgress<'_> = &|p, t| progress_log.lock().unwrap().push((p, t));
    let (out_path, out_file) = output_file("scale");

    let stats = convert(
        &containers,
        &codecs,
        open_vector(&vector("h264-ip-testsrc2-176x144.mp4")),
        "mp4",
        Box::new(std::io::BufWriter::new(out_file)),
        &settings,
        filter_ops(),
        progress,
        &cancel,
    )
    .expect("编码模式全链应成功");

    // 统计：1 路输出流、45 帧重编码、无音频。
    assert_eq!(stats.streams, 1);
    assert_eq!(stats.video_frames, 45);
    assert_eq!(stats.audio_frames, 0);
    assert_eq!(stats.total_us, 3_000_000, "total = 流时长 3s");
    assert_eq!(stats.processed_us, 2_933_333, "末帧 pts 44/15s → µs");

    // 进度：单调不减、逐包上报。
    let progress_log = progress_log.into_inner().unwrap();
    assert!(progress_log.len() >= 45, "至少逐包一报");
    for pair in progress_log.windows(2) {
        assert!(pair[0].0 <= pair[1].0, "processed_us 单调不减：{pair:?}");
    }

    // 回读：流参数（H.264 + avcC extradata + 320x240 + 1/15 时基）。
    let bytes = read_and_remove(&out_path);
    let mut mp4 = ye::container::mp4::Mp4Reader::from_vec(bytes).expect("输出应可解封装");
    assert_eq!(mp4.streams().len(), 1);
    let stream = mp4.stream(0).unwrap();
    assert_eq!(stream.codec.codec, CodecId::H264);
    assert_eq!(stream.time_base, Rational::new(1, 15), "时基 = 1/帧率分子");
    let video = stream.codec.video.as_ref().expect("视频参数");
    assert_eq!((video.width, video.height), (320, 240));
    // avcC 可解析且为 Baseline（首包带内参数集构造，自家读侧回环）。
    let avcc = stream
        .codec
        .extradata
        .clone()
        .expect("视频轨应带 avcC extradata");
    let _ = stream;
    let rec = ye_codec_h264::AvcConfigRecord::parse(&avcc).expect("avcC 应可解析");
    assert_eq!(rec.profile_indication, 66);
    assert_eq!(rec.length_size, 4);

    // 包级：45 样例、dts=pts=帧号 tick、duration=1、GOP 12 关键帧。
    assert_eq!(mp4.sample_count(0).unwrap(), 45);
    for i in 0..45u64 {
        let pkt = mp4.read_packet(0, i as usize).unwrap();
        assert_eq!(
            pkt.dts.as_ref().unwrap().ticks(),
            i as i64,
            "样例 {i} dts = 帧号 tick"
        );
        assert_eq!(
            pkt.pts.as_ref().unwrap().ticks(),
            i as i64,
            "无 B 帧 pts=dts"
        );
        assert_eq!(pkt.duration.unwrap().ticks(), 1, "帧时长 1 tick");
        assert_eq!(
            pkt.flags.contains(PacketFlags::KEYFRAME),
            i % 12 == 0,
            "样例 {i} GOP 12 关键帧判定"
        );
    }

    // 解码回环：自家解码器吃 avcC + 输出包 → 45 帧 320x240 yuv420p。
    let mut params = ye::codec::CodecParameters::new(CodecId::H264, MediaType::Video);
    params.extradata = Some(avcc);
    let mut dec = ye_codec_h264::H264Decoder::new(&params).expect("解码器构造");
    for i in 0..45usize {
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
    while let Decoded::Frame(Frame::Video(f)) = dec.pull().expect("pull") {
        assert_eq!((f.width(), f.height()), (320, 240));
        assert_eq!(f.format(), PixelFormat::Yuv420p);
        assert!(f.plane(0).iter().any(|&b| b != 0), "帧应有内容");
        frames += 1;
    }
    assert_eq!(frames, 45, "输出应可完整解码 45 帧");
}

/// 音频直通：纯音频向量 → 编码模式 → 逐包与输入对拍（数据/时戳/
/// flags 原样，dts 平移到 0 起、平移量 = 输入首包 dts）。
#[test]
fn encode_mode_audio_passthrough() {
    let (containers, codecs) = encode_registries();
    let settings = VideoEncodeSettings::default();
    let (out_path, out_file) = output_file("audio");

    let stats = convert(
        &containers,
        &codecs,
        open_vector(&vector("aac-44k1-st-m4a.mp4")),
        "mp4",
        Box::new(std::io::BufWriter::new(out_file)),
        &settings,
        filter_ops(),
        &|_, _| {},
        &AtomicBool::new(false),
    )
    .expect("纯音频编码模式应成功");
    assert_eq!(stats.streams, 1);
    assert_eq!(stats.video_frames, 0);

    // 回读对拍：样例数一致、包数据逐字节一致、dts/duration 平移后
    // 严格相等（模板时基 = 源流时基 ⇒ muxer 重标度恒等）、flags 一致。
    let bytes = read_and_remove(&out_path);
    let mut dst = ye::container::mp4::Mp4Reader::from_vec(bytes).expect("输出应可解封装");
    let mut src = ye::container::mp4::Mp4Reader::open(vector("aac-44k1-st-m4a.mp4")).unwrap();
    assert_eq!(dst.streams().len(), 1);
    let count = dst.sample_count(0).unwrap();
    assert_eq!(src.sample_count(0).unwrap(), count);
    assert_eq!(stats.audio_frames, count as u64);
    let shift = src
        .read_packet(0, 0)
        .unwrap()
        .dts
        .expect("源首包 dts 应在场");
    for i in 0..count {
        let in_pkt = src.read_packet(0, i).unwrap();
        let out_pkt = dst.read_packet(0, i).unwrap();
        assert_eq!(in_pkt.data, out_pkt.data, "样例 {i} 数据逐字节一致");
        assert_eq!(
            in_pkt
                .dts
                .expect("源包 dts")
                .checked_sub(shift)
                .expect("平移不溢出"),
            out_pkt.dts.expect("输出包 dts"),
            "样例 {i} dts（平移后）"
        );
        assert_eq!(in_pkt.duration, out_pkt.duration, "样例 {i} duration");
        assert_eq!(in_pkt.flags, out_pkt.flags, "样例 {i} flags");
    }
}

/// 入口取消：不读输入一字节、不写输出、不触发进度。
#[test]
fn encode_mode_cancel_before_start() {
    let (containers, codecs) = encode_registries();
    let reads = std::sync::Arc::new(AtomicUsize::new(0));
    let input: Box<dyn ye::container::ReadSeek> = Box::new(CountingRead {
        inner: Cursor::new(vec![0u8; 64]),
        reads: reads.clone(),
    });
    let cancel = AtomicBool::new(true);
    let reported = AtomicUsize::new(0);
    let progress: ConvertProgress<'_> = &|_, _| {
        reported.fetch_add(1, Ordering::SeqCst);
    };
    let (out_path, out_file) = output_file("cancel");
    let result = convert(
        &containers,
        &codecs,
        input,
        "mp4",
        Box::new(std::io::BufWriter::new(out_file)),
        &VideoEncodeSettings::default(),
        filter_ops(),
        progress,
        &cancel,
    );
    assert!(matches!(result, Err(ConvertError::Cancelled)));
    assert_eq!(reads.load(Ordering::SeqCst), 0, "入口取消不得读输入");
    assert_eq!(reported.load(Ordering::SeqCst), 0, "不得触发进度回调");
    // 临时文件清理（入口取消未写一字节）。
    let _ = std::fs::remove_file(&out_path);
}

/// 编码器未注册：`create_encoder(H264)` 的 NotRegistered 透传。
#[test]
fn encode_mode_encoder_not_registered() {
    let mut containers = ContainerRegistry::new();
    ye::container::mp4::register_mp4(&mut containers);
    let mut codecs = CodecRegistry::new();
    codecs.register_decoder(CodecId::H264, ye_codec_h264::decoder_factory);
    let result = convert(
        &containers,
        &codecs,
        open_vector(&vector("h264-ip-testsrc2-176x144.mp4")),
        "mp4",
        Box::new(Cursor::new(Vec::new())),
        &VideoEncodeSettings::default(),
        filter_ops(),
        &|_, _| {},
        &AtomicBool::new(false),
    );
    assert!(
        matches!(
            &result,
            Err(ConvertError::Codec(CodecError::NotRegistered(
                CodecId::H264
            )))
        ),
        "应透传 NotRegistered(H264)：{result:?}"
    );
}

// ---- ⑥ 解码器未注册：下层错误透传 ----

#[test]
fn decoder_not_registered_propagates() {
    let mut containers = ContainerRegistry::new();
    ye::container::mp4::register_mp4(&mut containers);
    let empty_codecs = CodecRegistry::new();
    let cancel = AtomicBool::new(false);
    let mut sink = |_| Ok(());
    let result = convert_frames(
        &containers,
        &empty_codecs,
        open_vector(&vector("h264-intra-testsrc2-176x144.mp4")),
        VideoChain {
            format: None,
            size: None,
        },
        test_ops(),
        &mut sink,
        &no_progress(),
        &cancel,
    );
    assert!(
        matches!(
            &result,
            Err(ConvertError::Codec(CodecError::NotRegistered(
                CodecId::H264
            )))
        ),
        "应透传 NotRegistered(H264)：{result:?}"
    );
}

// ---- ⑦ 消费方失败：Sink 错误原样终止泵 ----

#[test]
fn sink_error_stops_pipeline() {
    let (containers, codecs) = registries();
    let cancel = AtomicBool::new(false);
    let mut seen = 0u64;
    let mut sink = |frame: Frame| -> Result<(), ConvertError> {
        let Frame::Video(_) = frame else {
            return Err(ConvertError::Sink("非视频帧".into()));
        };
        seen += 1;
        Err(ConvertError::Sink(format!("stop at {seen}")))
    };
    let result = convert_frames(
        &containers,
        &codecs,
        open_vector(&vector("h264-intra-testsrc2-176x144.mp4")),
        VideoChain {
            format: None,
            size: None,
        },
        test_ops(),
        &mut sink,
        &no_progress(),
        &cancel,
    );
    assert!(
        matches!(&result, Err(ConvertError::Sink(m)) if m == "stop at 1"),
        "应透传消费方错误：{result:?}"
    );
    assert_eq!(seen, 1, "首帧失败即终止");
}

// ---- 编译期断言：泵 trait 对象可跨线程（Tauri 多线程后端必需） ----

#[test]
fn convert_types_are_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Box<dyn ye::container::ReadSeek>>();
    assert_send_sync::<PixelOps>();
    // 空转一次确保 Decoded 导入仍被使用（编译面保持稳定）。
    let _ = Decoded::NeedInput;
}
