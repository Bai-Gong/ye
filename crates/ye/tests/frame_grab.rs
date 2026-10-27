//! 抽帧门面真解码路径测试（m2-jian-plan §5 T2 验收）。
//!
//! **测试向量程序内自产**：`H264Encoder` 编 12 帧（GOP 12 → 帧 0 为
//! I/IDR、帧 1..11 为 P，`encode_h264.rs` 门面全链先例——门面
//! `ye::codec_h264` 路径取真工厂）+ MP4 muxer 封装成内存向量
//! （时基 1/90000、25fps → 帧 `i` 的 pts 恰为 `i × 40_000` µs）。
//! **不依赖 ffmpeg、无条件跑、无 skip 分支**（评审问题 2/3）。
//!
//! rgba 断言基准：编码器本地解码环重建（`recon_frames`，与解码器输出
//! 位级一致——`encode_h264.rs` 口径 1 已锁）经同一转换槽
//! `filter_convert`（yuv420p→rgba8）逐字节对拍——解码/转换全链无损
//! 传递的锁定，宽高/alpha 同场断言。
//!
//! 音轨整体打回的行为锁定（视频+不可解音轨向量）归 T3，此处不重复。

#![cfg(all(feature = "mp4", feature = "h264", feature = "png"))]
#![forbid(unsafe_code)]

use std::io::Cursor;

use ye::codec::{
    CodecId, CodecParameters, Encoded, Encoder, EncoderConfig, Frame, MediaType, PacketFlags,
    QualityMode, VideoEncoderConfig, VideoFrame, VideoParams,
};
use ye::codec_h264::{H264Encoder, ReconFrame};
use ye::container::{ContainerRegistry, Metadata, StreamTemplate};
use ye::convert::{ConvertError, filter_convert};
use ye::frame_grab::{decode_frame_rgba, grab_frame_png};
use ye::pixfmt::{ColorDesc, PixelFormat};
use ye::time::{Rational, Timestamp};

// ---- 向量参数（确定性合成：整数算术，无随机库） ----

/// 测试帧尺寸（偶数、宏块对齐，与 encode_h264.rs 同规格）。
const W: u32 = 96;
const H: u32 = 64;
/// 帧数：12（GOP 周期整段）。
const FRAMES: usize = 12;
/// GOP（IDR 间隔）：帧 0 为 IDR，帧 1..11 为 P。
const GOP: u32 = 12;
/// 编码 QP（低 QP 高保真，帧间内容差异可靠保留）。
const QP: u8 = 20;
/// 编码时基 1/90000（3600 tick/帧 @25fps，打戳精确）。
const TB: Rational = Rational::new(1, 90_000);
/// 帧 `i` 的 pts（µs）：`i × 3600` tick @ 1/90000 = `i × 40_000`。
const FRAME_US: i64 = 40_000;

/// 生成第 `t` 帧的紧凑 yuv420p 三平面（逐帧增亮的渐变——帧间可区分，
/// 相邻帧 luma 差 3 级：`v = 32 + x + y + 3t`，12 帧内不溢出）。
fn gen_frame(t: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let (w, h) = (W as usize, H as usize);
    let (cw, ch) = (w / 2, h / 2);
    let mut luma = vec![0u8; w * h];
    let mut cb = vec![0u8; cw * ch];
    let mut cr = vec![0u8; cw * ch];
    for y in 0..h {
        for x in 0..w {
            luma[y * w + x] = (32 + x + y + 3 * t) as u8;
        }
    }
    for y in 0..ch {
        for x in 0..cw {
            cb[y * cw + x] = (48 + x + y + t) as u8;
            cr[y * cw + x] = (96 + 2 * x + y + t) as u8;
        }
    }
    (luma, cb, cr)
}

/// 生成第 `t` 帧的 `VideoFrame`（紧凑平面 → 帧对象；pts 留空——编码器
/// 按帧号打戳是输出契约的一部分）。
fn gen_video_frame(t: usize) -> VideoFrame {
    let (luma, cb, cr) = gen_frame(t);
    let mut frame = VideoFrame::new(
        PixelFormat::Yuv420p,
        W,
        H,
        ColorDesc::UNSPECIFIED,
        None,
        None,
    )
    .expect("frame alloc");
    let ls = frame.layout().plane(0).stride;
    let cs = frame.layout().plane(1).stride;
    for y in 0..H as usize {
        frame.plane_mut(0)[y * ls..y * ls + W as usize]
            .copy_from_slice(&luma[y * W as usize..][..W as usize]);
    }
    for y in 0..(H / 2) as usize {
        let (src, w2) = (y * (W as usize / 2), W as usize / 2);
        frame.plane_mut(1)[y * cs..y * cs + w2].copy_from_slice(&cb[src..][..w2]);
        frame.plane_mut(2)[y * cs..y * cs + w2].copy_from_slice(&cr[src..][..w2]);
    }
    frame
}

/// 一轮自产：编码 12 帧 I/P + MP4 封装，返回（mp4 字节, 编码器——
/// 供 recon 对拍）。
///
/// 向量完整性就地锁定：包数、GOP 关键帧分布、pts/dts 打戳（时点断言
/// 的前提）。
fn produced() -> (Vec<u8>, H264Encoder) {
    let config = EncoderConfig {
        codec: CodecId::H264,
        time_base: TB,
        bit_rate: None,
        video: Some(VideoEncoderConfig {
            width: W,
            height: H,
            format: PixelFormat::Yuv420p,
            frame_rate: Rational::RATE_25,
            gop: GOP,
            quality: QualityMode::Qp(QP),
        }),
        audio: None,
    };
    let mut encoder = H264Encoder::new(&config).expect("编码器构造");
    for t in 0..FRAMES {
        encoder
            .feed(&Frame::Video(gen_video_frame(t)))
            .expect("feed");
    }
    encoder.flush();
    let mut packets = Vec::with_capacity(FRAMES);
    while let Encoded::Packet(p) = encoder.pull().expect("pull") {
        packets.push(p);
    }
    assert!(
        matches!(encoder.pull(), Ok(Encoded::Finished)),
        "排空后 pull 幂等 Finished"
    );

    // 向量完整性：12 包、帧 0 IDR 其余 P、pts = dts = 帧号 × 3600 tick。
    assert_eq!(packets.len(), FRAMES, "一帧一包");
    for (i, p) in packets.iter().enumerate() {
        assert_eq!(
            p.flags.contains(PacketFlags::KEYFRAME),
            i % GOP as usize == 0,
            "帧 {i} 的 GOP 关键帧分布"
        );
        let want = Timestamp::new((i * 3600) as i64, TB);
        assert_eq!(p.pts, Some(want), "帧 {i} pts");
        assert_eq!(p.dts, Some(want), "无 B 帧 dts == pts");
    }

    // MP4 封装：视频轨模板带 avcC extradata，时基沿用编码时基（muxer
    // 重标度恒等）。`Box<dyn WriteSeek>` 要求 'static——输出落临时文件
    // （tests/convert.rs 的 output_file 同法），用后即删。
    let mut containers = ContainerRegistry::new();
    ye::container::mp4::register_mp4(&mut containers);
    let mut params = CodecParameters::new(CodecId::H264, MediaType::Video);
    params.extradata = Some(encoder.extradata().to_vec());
    params.video = Some(VideoParams {
        width: W,
        height: H,
        pixel_format: Some(PixelFormat::Yuv420p),
        frame_rate: Some(Rational::RATE_25),
    });
    let template = StreamTemplate {
        media_type: MediaType::Video,
        codec: params,
        time_base: TB,
        frame_rate: Some(Rational::RATE_25),
        metadata: Metadata::new(),
    };
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&dir).expect("tmpdir");
    let path = dir.join(format!(
        "ye-frame-grab-vec-{}-{}.mp4",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let file = std::fs::File::create(&path).expect("创建向量临时文件");
    {
        let mut muxer = containers
            .create_muxer("mp4", Box::new(std::io::BufWriter::new(file)))
            .expect("create muxer");
        muxer.write_header(&[template]).expect("write_header");
        for p in &packets {
            muxer.write_packet(p).expect("write_packet");
        }
        muxer.write_trailer().expect("write_trailer"); // finalize 已 flush。
    }
    let mp4 = std::fs::read(&path).expect("读回向量");
    let _ = std::fs::remove_file(&path); // 用后即删（仓库约定）。
    assert!(mp4.len() > 1024, "向量应有实内容");
    (mp4, encoder)
}

/// 重建帧 → 与抽帧链同一转换槽的期望 rgba（紧致字节）。
///
/// 解码器输出与 `recon_frames` 位级一致（encode_h264.rs 口径 1），
/// `filter_convert` 即泵链 yuv420p→rgba8 的核——逐字节对拍即全链无损
/// 传递的锁定。
fn expected_rgba(encoder: &H264Encoder, index: usize) -> Vec<u8> {
    let rec: &ReconFrame = &encoder.recon_frames()[index];
    let mut frame = VideoFrame::new(
        PixelFormat::Yuv420p,
        W,
        H,
        ColorDesc::UNSPECIFIED,
        None,
        None,
    )
    .expect("frame alloc");
    let ls = frame.layout().plane(0).stride;
    let cs = frame.layout().plane(1).stride;
    for y in 0..H as usize {
        frame.plane_mut(0)[y * ls..y * ls + W as usize]
            .copy_from_slice(&rec.luma[y * W as usize..][..W as usize]);
    }
    for y in 0..(H / 2) as usize {
        let (src, w2) = (y * (W as usize / 2), W as usize / 2);
        frame.plane_mut(1)[y * cs..y * cs + w2].copy_from_slice(&rec.cb[src..][..w2]);
        frame.plane_mut(2)[y * cs..y * cs + w2].copy_from_slice(&rec.cr[src..][..w2]);
    }
    let rgba = filter_convert(&frame, PixelFormat::Rgba8).expect("转换");
    let stride = rgba.layout().plane(0).stride;
    let row = W as usize * 4;
    let mut tight = Vec::with_capacity(row * H as usize);
    for y in 0..H as usize {
        tight.extend_from_slice(&rgba.plane(0)[y * stride..y * stride + row]);
    }
    tight
}

/// 打开一份自产向量为门面输入。
fn open(mp4: &[u8]) -> Box<dyn ye::container::ReadSeek> {
    Box::new(Cursor::new(mp4.to_vec()))
}

// ---- 验收①：时点负值钳 0（与 jian ffmpeg 轨同语义） ----

/// 负时点钳 0 后取首帧：与显式 0 时点同帧，且就是帧 0（pts 0 ≥ 0）。
#[test]
fn 时点负值钳零取首帧() {
    let (mp4, encoder) = produced();
    let negative = decode_frame_rgba(open(&mp4), -123_456).expect("负时点应钳 0 取帧");
    let zero = decode_frame_rgba(open(&mp4), 0).expect("0 时点应取帧");
    assert_eq!(negative, zero, "负时点与 0 时点同帧（钳 0）");
    assert_eq!((zero.width, zero.height), (W, H));
    assert_eq!(zero.rgba.len(), (W * H * 4) as usize);
    assert_eq!(zero.rgba, expected_rgba(&encoder, 0), "取的是帧 0（IDR）");
}

// ---- 验收②：越界时点明确 Err（勿把自然 EOF 误译成功） ----

/// 时点超出最晚 pts：自然 EOF 未取到 → InvalidData 明确报错，文案述因
/// （含请求时点与最晚 pts）。
#[test]
fn 越界时点明确报错() {
    let (mp4, _) = produced();
    // 12 帧最晚 pts = 11 × 40_000 = 440_000 µs；600_000 在其外。
    let err = decode_frame_rgba(open(&mp4), 600_000).expect_err("越界时点应报错");
    assert!(
        matches!(&err, ConvertError::InvalidData(msg) if msg.contains("超出素材")
            && msg.contains("600000") && msg.contains("440000")),
        "越界文案应含请求时点与最晚 pts：{err}"
    );
}

// ---- 验收③：自产 Baseline mp4 真解码取帧（宽高/rgba 断言） ----

/// 时点判定取**首个 pts ≥ time_us** 的帧：界点含等号、从下方逼近同帧、
/// 相邻帧内容可区分；P 帧经 I→P 链真解码，rgba 与 recon 经同一转换槽
/// 逐字节一致；末帧命中走泵自然收尾路径（排空段不检 cancel），同为成功。
#[test]
fn 自产向量真解码取p帧rgba逐字节一致() {
    let (mp4, encoder) = produced();
    // 帧 5 pts = 200_000：恰等（≥ 含等号）与 199_999（从下方逼近）都取帧 5。
    for time_us in [200_000, 199_999] {
        let grabbed = decode_frame_rgba(open(&mp4), time_us).expect("取帧应成功");
        assert_eq!((grabbed.width, grabbed.height), (W, H), "宽高原尺寸");
        assert_eq!(grabbed.rgba.len(), (W * H * 4) as usize);
        assert!(
            grabbed.rgba.chunks(4).all(|px| px[3] == 255),
            "rgba8 通道序下 alpha 恒 255"
        );
        assert_eq!(
            grabbed.rgba,
            expected_rgba(&encoder, 5),
            "rgba 与帧 5 重建经同一转换槽逐字节一致（time_us={time_us}）"
        );
        assert_ne!(
            grabbed.rgba,
            expected_rgba(&encoder, 4),
            "时点判定不得早取帧 4"
        );
        assert_ne!(
            grabbed.rgba,
            expected_rgba(&encoder, 6),
            "时点判定不得跳到帧 6"
        );
    }
    // 末帧命中：cancel 置位后无下一包，泵走自然 EOF 收尾——同为成功。
    let last = decode_frame_rgba(open(&mp4), 11 * FRAME_US).expect("末帧时点应取帧");
    assert_eq!(
        last.rgba,
        expected_rgba(&encoder, 11),
        "末帧（自然收尾路径）"
    );
}

// ---- 验收④：PNG 出口与 encode_rgba 逐字节一致 ----

/// `grab_frame_png` = `decode_frame_rgba` + `ye::png::encode_rgba` 的
/// 一步组合：对同一向量同一时点，两路输出逐字节一致且为合法 PNG 签名。
#[test]
fn png出口与encode_rgba逐字节一致() {
    let (mp4, _) = produced();
    let time_us = 5 * FRAME_US;
    let frame = decode_frame_rgba(open(&mp4), time_us).expect("取帧");
    let png = grab_frame_png(open(&mp4), time_us).expect("PNG 出口");
    let manual = ye::png::encode_rgba(frame.width, frame.height, &frame.rgba).expect("encode_rgba");
    assert_eq!(png, manual, "PNG 出口与 encode_rgba 逐字节一致");
    assert_eq!(&png[0..8], &ye::png::SIGNATURE, "合法 PNG 签名开头");
}
