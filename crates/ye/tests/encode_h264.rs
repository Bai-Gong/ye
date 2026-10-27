//! H.264 编码收口对拍（M3 施工图 T5 验收①②，docs/m3-plan.md §3
//! 冻结口径）。
//!
//! ## 对拍口径（§3，容差/门槛写死为常量、只准收紧免评审）
//!
//! 1. **回环位级一致**：编码器本地解码环重建（`H264Encoder::recon_frames`）
//!    vs 自家 `H264Decoder` 解同一 AVCC+avcC 码流的输出——逐像素相等，
//!    0 容差；
//! 2. **合法性双解码**：(a) 自家解码器帧数与送入一致；(b) ffmpeg：
//!    `avcc_to_annexb` 转 Annex B 裸流后 `ffmpeg -v error -f null` 无错
//!    退出 + `ffprobe -count_frames` 帧数一致。PATH 无 ffmpeg/ffprobe
//!    时打印跳过信息返回（不 fail）；
//! 3. **Y-PSNR 门槛**（仅平滑三类向量族）：源帧 vs 解码帧的 luma PSNR，
//!    **逐帧**下限 QP20 ≥ 36 dB、QP32 ≥ 28 dB；LCG 噪声向量只走口径
//!    1/2、不设 PSNR 门槛（噪声在高 QP 下天然不可比，§3.3）。
//!
//! ## 向量（§3.4：确定性合成，LCG/整数算术生成）
//!
//! 平滑三类：纯色（静止恒值）、渐变（全局增亮，逐帧小残差）、运动
//! 方块（平滑底 + 实心方块平移，I+P 运动路径）；另加 LCG 噪声一组
//! （位级回环/合法性专用）。每组 32 帧（≥30）、GOP 12（帧 0/12/24 为
//! IDR，其余 P），QP 20/32 两档。
//!
//! ## 门面全链（验收②）
//!
//! 注册面走真工厂：`ye_codec_h264::encoder_factory` 经
//! `CodecRegistry::create_encoder` 全链可用（dev 依赖直连——门面默认
//! feature 集不含 `h264`，`ye::codec_h264::register_h264_encoder` 路径
//! 在 `#[cfg(feature = "h264")]` 组覆盖，集成阶段开 feature 后同入
//! 默认门禁）。AAC 桩行为回归在 `tests/stubs.rs`（桩收窄语义）。

use std::path::PathBuf;
use std::process::Command;

use ye::codec::{
    CodecId, CodecParameters, Decoded, Decoder, Encoded, Encoder, EncoderConfig, MediaType, Packet,
    PacketFlags, QualityMode, VideoEncoderConfig,
};
use ye::pixfmt::{ColorDesc, PixelFormat};
use ye::time::{Rational, Timestamp};
use ye_codec_h264::{H264Decoder, H264Encoder, avcc_to_annexb};

// ---- 冻结口径常量（只准收紧，放宽须回施工图重新定口径） ----

/// 平滑向量族 QP20 的逐帧 Y-PSNR 下限（dB，§3.3 冻结：36）。
const PSNR_MIN_QP20_DB: f64 = 36.0;
/// 平滑向量族 QP32 的逐帧 Y-PSNR 下限（dB，§3.3 冻结：28）。
const PSNR_MIN_QP32_DB: f64 = 28.0;
/// 每组帧数（§3.1/验收①：≥30）。
const FRAMES: usize = 32;
/// GOP（IDR 间隔）：帧 0/12/24 为 IDR，其余 P。
const GOP: u32 = 12;
/// 测试帧尺寸（偶数、宏块对齐）。
const W: u32 = 96;
const H: u32 = 64;

// 口径注记（对齐 ye-filter conformance 的 POLICY_NOTE 先例）：
// 上述门槛为 2026-10-26 冻结值，实测后只准收紧；放宽须回施工图
// （m3-plan §3）重新定口径。

// ---- 确定性向量生成（§3.4：整数算术 + LCG，无浮点无随机库） ----

/// 确定性 LCG（先例：ye-codec-h264 encoder 测试、ye-filter testutil）。
struct Lcg(u32);

impl Lcg {
    fn next_u8(&mut self) -> u8 {
        self.0 = self.0.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        (self.0 >> 16) as u8
    }
}

/// 向量族标识。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Family {
    /// 纯色（静止恒值；P 帧全 skip 的退化端）。
    Solid,
    /// 渐变（全局逐帧增亮；P 帧小残差）。
    Gradient,
    /// 运动方块（平滑底 + 实心方块斜向平移；I+P 运动路径）。
    MovingBlock,
    /// LCG 噪声（位级回环/合法性专用，不进 PSNR 门槛集）。
    Noise,
}

impl Family {
    fn name(self) -> &'static str {
        match self {
            Family::Solid => "solid",
            Family::Gradient => "gradient",
            Family::MovingBlock => "moving-block",
            Family::Noise => "noise",
        }
    }

    /// 平滑族（进 PSNR 门槛集）。
    fn smooth(self) -> bool {
        !matches!(self, Family::Noise)
    }
}

/// 生成第 `t` 帧的紧凑 yuv420p 三平面（luma W×H、chroma 各 W/2×H/2）。
fn gen_frame(family: Family, t: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let (w, h) = (W as usize, H as usize);
    let (cw, ch) = (w / 2, h / 2);
    let mut luma = vec![0u8; w * h];
    let mut cb = vec![0u8; cw * ch];
    let mut cr = vec![0u8; cw * ch];
    match family {
        Family::Solid => {
            luma.fill(120);
            cb.fill(72);
            cr.fill(148);
        }
        Family::Gradient => {
            // 全局增亮：v = 32 + x + y + 2t（32 帧内无上溢：max 32+95+63+62=252）
            for y in 0..h {
                for x in 0..w {
                    luma[y * w + x] = (32 + x + y + 2 * t) as u8;
                }
            }
            for y in 0..ch {
                for x in 0..cw {
                    cb[y * cw + x] = (48 + x + y + t) as u8;
                    cr[y * cw + x] = (96 + 2 * x + y + t) as u8;
                }
            }
        }
        Family::MovingBlock => {
            // 平滑底（静态渐变，40+x+y max 198——全程不回卷）+
            // 16×16 实心方块：斜向平移（x +2/帧、y +1/帧）
            for y in 0..h {
                for x in 0..w {
                    luma[y * w + x] = (40 + x + y) as u8;
                }
            }
            let (bx, by, bs) = (8 + 2 * t, 6 + t, 16usize);
            for y in 0..bs {
                for x in 0..bs {
                    let (px, py) = (bx + x, by + y);
                    if px < w && py < h {
                        luma[py * w + px] = 220;
                    }
                }
            }
            for y in 0..ch {
                for x in 0..cw {
                    cb[y * cw + x] = (64 + x + y) as u8;
                    cr[y * cw + x] = (128 + x + 2 * y) as u8;
                }
            }
            // 方块 chroma（半分辨率同位置）
            let (cx, cy, cs) = (bx / 2, by / 2, bs / 2);
            for y in 0..cs {
                for x in 0..cs {
                    let (px, py) = (cx + x, cy + y);
                    if px < cw && py < ch {
                        cb[py * cw + px] = 40;
                        cr[py * cw + px] = 200;
                    }
                }
            }
        }
        Family::Noise => {
            let mut rng = Lcg(0x5EED_0000u32.wrapping_add(t as u32));
            for v in luma.iter_mut() {
                *v = rng.next_u8();
            }
            for v in cb.iter_mut() {
                *v = rng.next_u8();
            }
            for v in cr.iter_mut() {
                *v = rng.next_u8();
            }
        }
    }
    (luma, cb, cr)
}

/// 生成第 `t` 帧的 `VideoFrame`（紧凑平面 → 帧对象；pts 留空——编码器
/// 按帧号打戳，输入 pts 被忽略是契约的一部分）。
fn gen_video_frame(family: Family, t: usize) -> ye::codec::VideoFrame {
    let (luma, cb, cr) = gen_frame(family, t);
    let mut f = ye::codec::VideoFrame::new(
        PixelFormat::Yuv420p,
        W,
        H,
        ColorDesc::UNSPECIFIED,
        None,
        None,
    )
    .expect("frame alloc");
    let ls = f.layout().plane(0).stride;
    let cs = f.layout().plane(1).stride;
    for y in 0..H as usize {
        f.plane_mut(0)[y * ls..y * ls + W as usize]
            .copy_from_slice(&luma[y * W as usize..][..W as usize]);
    }
    for y in 0..(H / 2) as usize {
        let (src, w2) = (y * (W as usize / 2), W as usize / 2);
        f.plane_mut(1)[y * cs..y * cs + w2].copy_from_slice(&cb[src..][..w2]);
        f.plane_mut(2)[y * cs..y * cs + w2].copy_from_slice(&cr[src..][..w2]);
    }
    f
}

// ---- 编码配置 ----

/// 基准配置：96×64@25、GOP 12、时基 1/90000（3600 tick/帧，打戳精确）。
fn encoder_config(qp: u8) -> EncoderConfig {
    EncoderConfig {
        codec: CodecId::H264,
        time_base: Rational::new(1, 90_000),
        bit_rate: None,
        video: Some(VideoEncoderConfig {
            width: W,
            height: H,
            format: PixelFormat::Yuv420p,
            frame_rate: Rational::RATE_25,
            gop: GOP,
            quality: QualityMode::Qp(qp),
        }),
        audio: None,
    }
}

// ---- 编码一轮：feed ×N → pull ×N（经 Encoder trait） ----

/// 编码 `FRAMES` 帧并取回全部包（trait 推拉：逐帧 feed——编码器内部
/// 逐帧同步产出；全部送入后 flush 排空收集，末次 pull 幂等断言）。
fn encode_all(encoder: &mut dyn Encoder, family: Family) -> Vec<Packet> {
    for t in 0..FRAMES {
        encoder
            .feed(&ye::codec::Frame::Video(gen_video_frame(family, t)))
            .expect("feed");
    }
    encoder.flush();
    let mut out = Vec::with_capacity(FRAMES);
    while let Encoded::Packet(p) = encoder.pull().expect("pull") {
        out.push(p);
    }
    assert!(
        matches!(encoder.pull(), Ok(Encoded::Finished)),
        "排空后 pull 幂等 Finished"
    );
    out
}

// ---- 解码与比对 ----

/// 用自家 `H264Decoder`（avcC extradata + AVCC 包）解码全部包。
fn decode_all(extradata: &[u8], packets: &[Packet]) -> Vec<ye::codec::VideoFrame> {
    let mut params = CodecParameters::new(CodecId::H264, MediaType::Video);
    params.extradata = Some(extradata.to_vec());
    let mut dec = H264Decoder::new(&params).expect("decoder new");
    for p in packets {
        dec.feed(p).expect("feed");
    }
    dec.flush();
    let mut out = Vec::with_capacity(packets.len());
    while let Decoded::Frame(ye::codec::Frame::Video(f)) = dec.pull().expect("pull") {
        out.push(f);
    }
    assert!(matches!(dec.pull(), Ok(Decoded::Finished)));
    out
}

/// luma 平面 Y-PSNR（dB；mse=0 → 无穷，用 f64 上限表达）。
fn y_psnr_db(a: &[u8], b: &[u8]) -> f64 {
    let mut mse: f64 = 0.0;
    for (x, y) in a.iter().zip(b) {
        let d = i32::from(*x) - i32::from(*y);
        mse += f64::from(d * d);
    }
    mse /= a.len() as f64;
    if mse == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (255.0 * 255.0 / mse).log10()
    }
}

/// 紧凑 luma 视图（VideoFrame plane → 紧凑缓冲）。
fn tight_luma(frame: &ye::codec::VideoFrame) -> Vec<u8> {
    let (w, h) = (frame.width() as usize, frame.height() as usize);
    let stride = frame.layout().plane(0).stride;
    let mut out = Vec::with_capacity(w * h);
    for y in 0..h {
        out.extend_from_slice(&frame.plane(0)[y * stride..y * stride + w]);
    }
    out
}

// ---- ffmpeg oracle（PATH 无工具整组跳过，§3.2/§3.6 分支语义） ----

/// ffmpeg 与 ffprobe 是否都在 PATH（缺一即视为 oracle 缺席——帧数
/// 断言两者都需要，只做半套没有意义）。
fn ffmpeg_oracle_available() -> bool {
    let probe = |tool: &str| {
        Command::new(tool)
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    probe("ffmpeg") && probe("ffprobe")
}

/// 唯一临时文件路径（`CARGO_TARGET_TMPDIR`，target/ 下不入库）。
fn tmp_path(tag: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&dir).expect("tmpdir");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dir.join(format!(
        "ye-encode-oracle-{tag}-{}-{nanos}.h264",
        std::process::id()
    ))
}

/// 口径 2(b)：AVCC 包序列 → Annex B 裸流 → ffmpeg 解码无错 +
/// ffprobe 帧数一致。工具缺席打印跳过信息返回（不 fail）。
fn ffmpeg_decode_oracle(packets: &[Packet], expect_frames: usize, ctx: &str) {
    if !ffmpeg_oracle_available() {
        eprintln!(
            "[skip] ffmpeg/ffprobe 不在 PATH，编码对拍口径 2(b) 跳过（{ctx}；\
             PATH 加回后重跑即对拍）"
        );
        return;
    }
    let mut annexb = Vec::new();
    for p in packets {
        annexb.extend_from_slice(&avcc_to_annexb(&p.data, 4).expect("avcc→annexb"));
    }
    let path = tmp_path(ctx);
    std::fs::write(&path, &annexb).expect("write annexb");
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&path)
        .args(["-f", "null", "-"])
        .output()
        .expect("spawn ffmpeg");
    assert!(
        out.status.success(),
        "{ctx}: ffmpeg 解码失败：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stderr.is_empty(),
        "{ctx}: ffmpeg -v error 有输出：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let probe = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-count_frames",
            "-show_entries",
            "stream=nb_read_frames",
            "-of",
            "csv=p=0",
        ])
        .arg(&path)
        .output()
        .expect("spawn ffprobe");
    let count: String = String::from_utf8_lossy(&probe.stdout).trim().to_string();
    let _ = std::fs::remove_file(&path);
    assert!(
        probe.status.success(),
        "{ctx}: ffprobe 失败：{}",
        String::from_utf8_lossy(&probe.stderr)
    );
    assert_eq!(
        count,
        expect_frames.to_string(),
        "{ctx}: ffmpeg 侧帧数须与送入一致"
    );
}

// ---- 主对拍：一族 × 一 QP 的全链（§3 口径 1/2/3） ----

/// 单族单 QP 全链：trait 编码 → 包形态/GOP/打戳断言 → 自家解码 →
/// 位级回环 + PSNR（平滑族）→ ffmpeg oracle。
fn roundtrip_family(family: Family, qp: u8) {
    let ctx = format!("{} qp{qp}", family.name());
    let mut encoder = H264Encoder::new(&encoder_config(qp)).expect(&ctx);
    let packets = encode_all(&mut encoder, family);

    // ---- 包形态 / GOP / 打戳 ----
    assert_eq!(packets.len(), FRAMES, "{ctx}: 帧数须与送入一致");
    for (i, p) in packets.iter().enumerate() {
        let key = p.flags.contains(PacketFlags::KEYFRAME);
        assert_eq!(key, i % GOP as usize == 0, "{ctx}: frame {i} IDR 判定");
        let want = Timestamp::new((i * 3600) as i64, Rational::new(1, 90_000));
        assert_eq!(p.pts, Some(want), "{ctx}: frame {i} pts（帧号×帧率→时基）");
        assert_eq!(p.dts, Some(want), "{ctx}: 无 B 帧 → dts == pts");
        assert_eq!(
            p.duration,
            Some(Timestamp::new(3600, Rational::new(1, 90_000))),
            "{ctx}: frame {i} duration"
        );
        assert_eq!(p.stream_index, 0);
        // AVCC：4 字节大端长度前缀恰好覆盖全包
        let mut pos = 0usize;
        while pos < p.data.len() {
            let len = u32::from_be_bytes([
                p.data[pos],
                p.data[pos + 1],
                p.data[pos + 2],
                p.data[pos + 3],
            ]) as usize;
            pos += 4 + len;
        }
        assert_eq!(pos, p.data.len(), "{ctx}: frame {i} AVCC 前缀覆盖全包");
    }

    // ---- 口径 1：自家解码 → 位级回环（0 容差） ----
    let frames = decode_all(encoder.extradata(), &packets);
    assert_eq!(frames.len(), FRAMES, "{ctx}: 解码帧数须与送入一致");
    let recon = encoder.recon_frames();
    assert_eq!(recon.len(), FRAMES);
    for (i, (frame, rec)) in frames.iter().zip(recon).enumerate() {
        let got = tight_luma(frame);
        assert_eq!(got, rec.luma, "{ctx}: frame {i} luma 位级回环");
        // chroma 位级
        let cs = frame.layout().plane(1).stride;
        let (cw, ch) = ((W / 2) as usize, (H / 2) as usize);
        for y in 0..ch {
            for x in 0..cw {
                assert_eq!(
                    frame.plane(1)[y * cs + x],
                    rec.cb[y * cw + x],
                    "{ctx}: frame {i} cb ({x},{y})"
                );
                assert_eq!(
                    frame.plane(2)[y * cs + x],
                    rec.cr[y * cw + x],
                    "{ctx}: frame {i} cr ({x},{y})"
                );
            }
        }
        // 解码帧时戳 = 包 pts 透传（编码器打戳的消费侧一致性）
        assert_eq!(
            frame.pts(),
            Some(Timestamp::new((i * 3600) as i64, Rational::new(1, 90_000))),
            "{ctx}: frame {i} pts 透传"
        );
    }

    // ---- 口径 3：Y-PSNR（仅平滑族；逐帧下限） ----
    if family.smooth() {
        let floor_db = if qp <= 20 {
            PSNR_MIN_QP20_DB
        } else {
            PSNR_MIN_QP32_DB
        };
        for (i, frame) in frames.iter().enumerate() {
            let (src, _, _) = gen_frame(family, i);
            let psnr = y_psnr_db(&src, &tight_luma(frame));
            assert!(
                psnr >= floor_db,
                "{ctx}: frame {i} Y-PSNR {psnr:.2} dB < 门槛 {floor_db} dB"
            );
        }
    }

    // ---- 口径 2(b)：ffmpeg 双解码（平滑与噪声族都做合法性） ----
    ffmpeg_decode_oracle(&packets, FRAMES, &ctx);
}

#[test]
fn roundtrip_solid_qp20() {
    roundtrip_family(Family::Solid, 20);
}

#[test]
fn roundtrip_solid_qp32() {
    roundtrip_family(Family::Solid, 32);
}

#[test]
fn roundtrip_gradient_qp20() {
    roundtrip_family(Family::Gradient, 20);
}

#[test]
fn roundtrip_gradient_qp32() {
    roundtrip_family(Family::Gradient, 32);
}

#[test]
fn roundtrip_moving_block_qp20() {
    roundtrip_family(Family::MovingBlock, 20);
}

#[test]
fn roundtrip_moving_block_qp32() {
    roundtrip_family(Family::MovingBlock, 32);
}

/// LCG 噪声：仅口径 1（位级回环）+ 口径 2（双解码合法性），无 PSNR
/// 门槛（§3.3：噪声在高 QP 下天然不可比）。
#[test]
fn roundtrip_noise_bitexact_and_legality() {
    roundtrip_family(Family::Noise, 32);
}

// ---- 验收②：注册面全链 ----

/// 真工厂经 `CodecRegistry` 全链：register → create_encoder →
/// 编码/拉包可用（dev 依赖直连注册；门面 `register_h264_encoder`
/// 路径在下方 `h264` feature 组）。
#[test]
fn registry_full_chain_with_real_factory() {
    let mut reg = ye::codec::CodecRegistry::new();
    assert!(!reg.supports_encoder(CodecId::H264));
    let old = reg.register_encoder(CodecId::H264, ye_codec_h264::encoder_factory);
    assert!(old.is_none());
    assert!(reg.supports_encoder(CodecId::H264));

    let mut enc = reg.create_encoder(&encoder_config(26)).expect("全链创建");
    assert_eq!(enc.codec_id(), CodecId::H264);
    let packets = encode_all(enc.as_mut(), Family::Gradient);
    assert_eq!(packets.len(), FRAMES);
    // 参数校验经注册表同样生效（QP 0 拒收）
    let mut bad = encoder_config(26);
    bad.video.as_mut().unwrap().quality = QualityMode::Qp(0);
    assert!(matches!(
        reg.create_encoder(&bad),
        Err(ye::codec::CodecError::InvalidArguments(_))
    ));
}

/// avcC extradata 的容器消费形态：`AvcConfigRecord::parse` 回环 +
/// 解码器经 avcC 而非带内参数集初始化（构造即 extradata 可用性）。
#[test]
fn extradata_is_ready_before_first_frame() {
    let enc = H264Encoder::new(&encoder_config(24)).unwrap();
    let rec = ye_codec_h264::AvcConfigRecord::parse(enc.extradata()).unwrap();
    assert_eq!(rec.profile_indication, 66, "Baseline");
    assert_eq!(
        rec.level_indication, 10,
        "96x64@25 查表 → level 1.0（24MB、600MB/s）"
    );
    assert_eq!(rec.length_size, 4);
    assert_eq!(rec.sps_nals.len(), 1);
    assert_eq!(rec.pps_nals.len(), 1);
    // SPS 解析回环（定形口径：写侧参数集经自家解析器还原）
    let sps = ye_codec_h264::Sps::parse(&rec.sps_nals[0]).unwrap();
    assert_eq!(sps.profile_idc, 66);
    assert!(sps.is_baseline());
    assert_eq!(sps.level_idc, 10);
    assert_eq!((sps.width(), sps.height()), (W, H));
}

/// 简单 CRF 模式的收口路径：可编码、位级回环成立（QP 推导不影响
/// 回环口径——重建与码流同源）、ffmpeg 合法性（同口径 2）。
#[test]
fn crf_mode_roundtrip_and_legality() {
    let mut cfg = encoder_config(28);
    cfg.video.as_mut().unwrap().quality = QualityMode::Crf(28);
    let mut encoder = H264Encoder::new(&cfg).unwrap();
    let packets = encode_all(&mut encoder, Family::MovingBlock);
    assert_eq!(packets.len(), FRAMES);
    let frames = decode_all(encoder.extradata(), &packets);
    assert_eq!(frames.len(), FRAMES);
    for (i, (frame, rec)) in frames.iter().zip(encoder.recon_frames()).enumerate() {
        assert_eq!(&tight_luma(frame), &rec.luma, "crf frame {i} 位级回环");
    }
    ffmpeg_decode_oracle(&packets, FRAMES, "crf");
}

// ---- 门面路径（`h264` feature；默认门禁不含，集成阶段并入） ----

#[cfg(feature = "h264")]
mod facade_path {
    use super::*;

    /// 验收②的门面路径：`ye::codec_h264::register_h264_encoder` 注册后
    /// `create_encoder` 全链可用（与 dev 依赖直连同工厂）。
    #[test]
    fn facade_register_h264_encoder_full_chain() {
        let mut reg = ye::codec::CodecRegistry::new();
        ye::codec_h264::register_h264_encoder(&mut reg);
        assert!(reg.supports_encoder(CodecId::H264));
        assert_eq!(reg.encoder_ids().collect::<Vec<_>>(), vec![CodecId::H264]);
        let mut enc = reg
            .create_encoder(&encoder_config(30))
            .expect("门面全链创建");
        assert_eq!(enc.codec_id(), CodecId::H264);
        let packets = encode_all(enc.as_mut(), Family::Solid);
        assert_eq!(packets.len(), FRAMES);
        assert!(packets[0].flags.contains(PacketFlags::KEYFRAME));
        // 解码器同门面注册面（register_h264）——读写两侧走同一聚合层
        let mut reg2 = ye::codec::CodecRegistry::new();
        ye::codec_h264::register_h264(&mut reg2);
        let params = {
            let mut p = CodecParameters::new(CodecId::H264, MediaType::Video);
            p.extradata = Some(enc_extradata_of(&packets));
            p
        };
        let mut dec = reg2.create_decoder(&params).expect("解码全链");
        for p in &packets {
            dec.feed(p).unwrap();
        }
        dec.flush();
        let mut n = 0usize;
        while let Decoded::Frame(_) = dec.pull().unwrap() {
            n += 1;
        }
        assert_eq!(n, FRAMES);
    }

    /// 门面全链组没有具体类型可拿 extradata——取首包（IDR）带内的
    /// SPS/PPS 两个 NAL 手工拼 avcC 记录（与 `ye-codec-h264` 的
    /// build_avcc 同构：profile/compat/level 取自 SPS 头三字节、
    /// lengthSizeMinusOne=3；片 NAL 不进 extradata）——门面组没有
    /// 具体类型可拿 `H264Encoder::extradata`。
    fn enc_extradata_of(packets: &[Packet]) -> Vec<u8> {
        let mut sps = None;
        let mut pps = None;
        for nal in split_packet_nals(&packets[0]) {
            match nal[0] & 0x1F {
                7 => sps = Some(nal.clone()),
                8 => pps = Some(nal.clone()),
                _ => {}
            }
        }
        let (sps, pps) = (
            sps.expect("idr carries in-band sps"),
            pps.expect("idr carries in-band pps"),
        );
        let mut avcc = vec![1, sps[1], sps[2], sps[3], 0xFF, 0xE1];
        avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(&sps);
        avcc.push(1);
        avcc.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(&pps);
        avcc
    }

    /// 拆一个 AVCC 包的全部 NAL。
    fn split_packet_nals(p: &Packet) -> Vec<Vec<u8>> {
        let mut nals = Vec::new();
        let mut pos = 0usize;
        while pos < p.data.len() {
            let len = u32::from_be_bytes([
                p.data[pos],
                p.data[pos + 1],
                p.data[pos + 2],
                p.data[pos + 3],
            ]) as usize;
            nals.push(p.data[pos + 4..pos + 4 + len].to_vec());
            pos += 4 + len;
        }
        nals
    }
}
