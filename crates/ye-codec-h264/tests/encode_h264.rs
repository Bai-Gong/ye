//! H.264 单程码控收口对拍（M3 后续批次「真码控」：m3-plan §5 裁决 5
//! 登记 → §7.2 施工——CBR / A-VBR，2-pass 与完整 CRF 仍归后续）。
//!
//! ## 验收口径（容差常量见下，依实测定值、只准收紧）
//!
//! 1. **CBR 收敛**：混合复杂度（渐变/强纹理交替）90 帧 320×240@25、
//!    `bit_rate = 300k` → 实测平均码率落在目标容差内；
//! 2. **单调性**：同内容 800k vs 300k → 平均码率严格更高；
//! 3. **A-VBR 封顶**：`Crf(18)` + 300k 封顶、强纹理内容 → 平均码率
//!    ≤ 300k × 1.2，且明显低于无封顶 `Crf(18)`；
//! 4. **回归**：`bit_rate = None` 与收口前行为一致——现有测试
//!    （`crates/ye/tests/encode_h264.rs` 位级回环/PSNR/ffmpeg 合法性）
//!    全绿为准 + 同一配置编两次输出**逐字节相同**（确定性）。
//!
//! 码控样本一律经自家 `H264Decoder` 解码干净并位级回环（对齐
//! `crates/ye/tests/encode_h264.rs` 的既有模式：解码帧 vs 本地重建
//! 逐像素相等——码控只改逐帧 QP，不得触碰回环口径）。

use ye_codec::{
    CodecId, CodecParameters, Decoded, Decoder, Encoded, Encoder, EncoderConfig, Frame, MediaType,
    Packet, PacketFlags, QualityMode, VideoEncoderConfig, VideoFrame,
};
use ye_codec_h264::{H264Decoder, H264Encoder};
use ye_pixfmt::{ColorDesc, PixelFormat};
use ye_time::{Rational, Timestamp};

// ---- 冻结口径常量（只准收紧，放宽须回批次重新定口径）----

/// 测试帧尺寸（宏块对齐）。
const W: u32 = 320;
const H: u32 = 240;
/// 帧率（25fps：帧预算 = bps/25）。
const FPS: i64 = 25;
/// 每组帧数（GOP 12 → 8 个 IDR）。
const FRAMES: usize = 90;
/// GOP（IDR 间隔）。
const GOP: u32 = 12;

/// CBR 300k 容差下界（bps）＝ 300k × 75%。
///
/// 依据（2026-10-26 实测）：单程无前向 lookahead 的积分式反馈，控制
/// 变量 QP 为整数量子（每档 ≈ 11% 码率，`2^(-1/6)`），GOP 12 的 IDR
/// 脉冲周期性激励水位——±25% 是该类控制器的诚实带。实测中心：
/// 300k → **327,775**（+9.3%）、800k → **794,671**（−0.7%），均落在
/// 带内中部、不贴边；定 QP 参照曲线 QP46 → 350,533 / QP51 → 260,624
/// （300k 的均衡点在 QP≈48–49，控制权充分）。
const CBR_300K_MIN_BPS: i64 = 225_000;
/// CBR 300k 容差上界（bps）＝ 300k × 125%（依据见 [`CBR_300K_MIN_BPS`]）。
const CBR_300K_MAX_BPS: i64 = 375_000;
/// A-VBR 封顶上界（bps）＝ 300k × 1.2（口径给定；实测 338,506）。
const AVBR_CAP_BPS: i64 = 360_000;

// ---- 确定性向量生成（整数算术，无随机库；对齐既有 encode 对拍风格）----

/// 合成第 `t` 帧（混合复杂度）：
/// - 全程静态渐变底 `16 + x/4 + y/6`（max = 16+79+39 = 134 无回卷；随
///   `t` 整体 +1 增亮——亚量化步长，背景区 P 帧大面积 P_Skip，廉价
///   底座）；
/// - 偶数帧 = **纯渐变**（渐变帧，P 帧近全 skip——廉价档）；
/// - 奇数帧 = **强纹理**：160×112 的 8×8 棋盘补丁（对比度 32/224，
///   约 1/4 宏块）叠在渐变底上、逐帧斜向平移 (2,1)——补丁区相对参考
///   帧残差大（昂贵档），强纹理主导全序列码率。
///
/// 补丁只占少数宏块 → 高 QP 下补丁残差量化归零、全帧近 skip（码率
/// 下限低：QP51 → 260k），低 QP 下补丁昂贵（码率上限高：QP46 →
/// 350k、QP18 → ~1M）——300k/800k 目标都落在可控区间，两档交替驱动
/// 码控在「预算富余 ↔ 超支」间持续反馈。全程确定性。
fn gen_mixed(t: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let (w, h) = (W as usize, H as usize);
    let (cw, ch) = (w / 2, h / 2);
    let mut luma = vec![0u8; w * h];
    let mut cb = vec![0u8; cw * ch];
    let mut cr = vec![0u8; cw * ch];
    // 渐变底（随 t 缓慢增亮）
    for y in 0..h {
        for x in 0..w {
            luma[y * w + x] = (16 + x / 4 + y / 6 + t) as u8;
        }
    }
    // 强纹理补丁（仅奇数帧）
    if t % 2 == 1 {
        let (pw, ph) = (160usize, 112usize);
        let (bx, by) = (2 * t, 4 + t);
        for y in 0..ph {
            let py = by + y;
            if py >= h {
                break;
            }
            for x in 0..pw {
                let px = bx + x;
                if px >= w {
                    break;
                }
                luma[py * w + px] = if (x / 8 + y / 8) % 2 == 0 { 32 } else { 224 };
            }
        }
    }
    // chroma 静态平滑（成本聚焦 luma）
    for y in 0..ch {
        for x in 0..cw {
            cb[y * cw + x] = (64 + x / 4) as u8;
            cr[y * cw + x] = (128 + y / 6) as u8;
        }
    }
    (luma, cb, cr)
}

/// 生成第 `t` 帧的 `VideoFrame`（紧凑平面 → 帧对象）。
fn gen_video_frame(t: usize) -> VideoFrame {
    let (luma, cb, cr) = gen_mixed(t);
    let mut f = VideoFrame::new(
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

// ---- 编码配置 / 编码一轮 ----

/// 基准配置：320×240@25、GOP 12、时基 1/90000。
fn rate_config(bit_rate: Option<u64>, quality: QualityMode) -> EncoderConfig {
    EncoderConfig {
        codec: CodecId::H264,
        time_base: Rational::new(1, 90_000),
        bit_rate,
        video: Some(VideoEncoderConfig {
            width: W,
            height: H,
            format: PixelFormat::Yuv420p,
            frame_rate: Rational::RATE_25,
            gop: GOP,
            quality,
        }),
        audio: None,
    }
}

/// 编码 `frames` 帧并取回全部包（feed ×N → flush → pull 排空）。
fn encode_frames(encoder: &mut dyn Encoder, frames: usize) -> Vec<Packet> {
    for t in 0..frames {
        encoder
            .feed(&Frame::Video(gen_video_frame(t)))
            .expect("feed");
    }
    encoder.flush();
    let mut out = Vec::with_capacity(frames);
    while let Encoded::Packet(p) = encoder.pull().expect("pull") {
        out.push(p);
    }
    assert!(matches!(encoder.pull(), Ok(Encoded::Finished)));
    out
}

/// 平均码率（bps）：总包体位数 × 帧率 / 帧数（整数算术）。
fn avg_bitrate_bps(packets: &[Packet]) -> i64 {
    let total_bits: i64 = packets.iter().map(|p| p.data.len() as i64 * 8).sum();
    total_bits * FPS / packets.len() as i64
}

// ---- 解码与位级回环（对齐 crates/ye/tests/encode_h264.rs 既有模式）----

/// 码控样本统一验收：帧数/GOP/打戳 + 自家解码干净 + 解码帧 vs 本地
/// 重建逐像素相等（码控改 QP 不触碰回环口径）。
fn verify_output(encoder: &H264Encoder, packets: &[Packet]) {
    assert_eq!(packets.len(), FRAMES, "帧数须与送入一致");
    for (i, p) in packets.iter().enumerate() {
        let key = p.flags.contains(PacketFlags::KEYFRAME);
        assert_eq!(key, i % GOP as usize == 0, "frame {i} IDR 判定");
        let want = Timestamp::new((i * 3600) as i64, Rational::new(1, 90_000));
        assert_eq!(p.pts, Some(want), "frame {i} pts");
        assert_eq!(p.dts, Some(want), "frame {i} dts == pts");
    }

    let mut params = CodecParameters::new(CodecId::H264, MediaType::Video);
    params.extradata = Some(encoder.extradata().to_vec());
    let mut dec = H264Decoder::new(&params).expect("decoder new");
    for p in packets {
        dec.feed(p).expect("decode feed");
    }
    dec.flush();
    let recon = encoder.recon_frames();
    assert_eq!(recon.len(), FRAMES);
    let mut idx = 0usize;
    while let Decoded::Frame(Frame::Video(f)) = dec.pull().expect("decode pull") {
        let rec = &recon[idx];
        let (ls, cs) = (f.layout().plane(0).stride, f.layout().plane(1).stride);
        for y in 0..H as usize {
            for x in 0..W as usize {
                assert_eq!(
                    f.plane(0)[y * ls + x],
                    rec.luma[y * W as usize + x],
                    "frame {idx} luma ({x},{y}) 位级回环"
                );
            }
        }
        for y in 0..(H / 2) as usize {
            for x in 0..(W / 2) as usize {
                assert_eq!(
                    f.plane(1)[y * cs + x],
                    rec.cb[y * (W as usize / 2) + x],
                    "frame {idx} cb ({x},{y})"
                );
                assert_eq!(
                    f.plane(2)[y * cs + x],
                    rec.cr[y * (W as usize / 2) + x],
                    "frame {idx} cr ({x},{y})"
                );
            }
        }
        idx += 1;
    }
    assert_eq!(idx, FRAMES, "解码帧数须与送入一致");
    assert!(matches!(dec.pull(), Ok(Decoded::Finished)));
}

// ---- 口径 1：CBR 收敛 ----

/// CBR 300k：`Qp(24)` + `bit_rate = 300k`，90 帧混合复杂度 → 平均码率
/// 落进 ±25% 容差（实测 327,775，+9.3%；容差依据见常量注释）。
#[test]
fn cbr_converges_to_300k() {
    let mut enc = H264Encoder::new(&rate_config(Some(300_000), QualityMode::Qp(24))).unwrap();
    let packets = encode_frames(&mut enc, FRAMES);
    let avg = avg_bitrate_bps(&packets);
    eprintln!("[rate-control] cbr 300k avg = {avg} bps");
    assert!(
        (CBR_300K_MIN_BPS..=CBR_300K_MAX_BPS).contains(&avg),
        "CBR 300k 实测 {avg} 超出容差 [{CBR_300K_MIN_BPS}, {CBR_300K_MAX_BPS}]"
    );
    verify_output(&enc, &packets);
}

// ---- 口径 2：单调性 ----

/// 同一内容 800k vs 300k → 平均码率严格更高（实测 794,671 > 327,775；
/// 800k 自身收敛到目标的 −0.7%——800k 均衡点 QP 更低、补丁残差更大）。
#[test]
fn cbr_higher_target_yields_higher_rate() {
    let mut enc300 = H264Encoder::new(&rate_config(Some(300_000), QualityMode::Qp(24))).unwrap();
    let p300 = encode_frames(&mut enc300, FRAMES);
    let mut enc800 = H264Encoder::new(&rate_config(Some(800_000), QualityMode::Qp(24))).unwrap();
    let p800 = encode_frames(&mut enc800, FRAMES);
    let (a300, a800) = (avg_bitrate_bps(&p300), avg_bitrate_bps(&p800));
    eprintln!("[rate-control] monotonic: 300k -> {a300} bps, 800k -> {a800} bps");
    assert!(
        a800 > a300,
        "800k 平均码率 {a800} 须严格高于 300k 的 {a300}"
    );
    verify_output(&enc300, &p300);
    verify_output(&enc800, &p800);
}

// ---- 口径 3：A-VBR 封顶 ----

/// `Crf(18)` + 300k 封顶（强纹理主导的内容）：平均码率 ≤ 300k×1.2
/// （实测 338,506），且明显低于无封顶 `Crf(18)`（实测 949,782——「明显」
/// 取 ≤50%：封顶后为无封顶的 35.6%，留有余量的同时能捕获封顶失效）。
#[test]
fn avbr_caps_crf18_at_300k() {
    // 封顶运行
    let mut enc = H264Encoder::new(&rate_config(Some(300_000), QualityMode::Crf(18))).unwrap();
    let packets = encode_frames(&mut enc, FRAMES);
    let capped = avg_bitrate_bps(&packets);
    eprintln!("[rate-control] avbr crf18 + 300k cap avg = {capped} bps");
    assert!(
        capped <= AVBR_CAP_BPS,
        "封顶 {capped} 超出 300k×1.2 = {AVBR_CAP_BPS}"
    );
    verify_output(&enc, &packets);

    // 无封顶基线（同内容同质量）
    let mut unc = H264Encoder::new(&rate_config(None, QualityMode::Crf(18))).unwrap();
    let p_unc = encode_frames(&mut unc, FRAMES);
    let uncapped = avg_bitrate_bps(&p_unc);
    eprintln!("[rate-control] crf18 uncapped avg = {uncapped} bps");
    verify_output(&unc, &p_unc);
    assert!(
        capped * 2 <= uncapped,
        "封顶 {capped} 须明显低于无封顶 {uncapped}（≤50%）"
    );
}

// ---- 口径 4：回归（bit_rate = None 路径不变）----

/// `bit_rate = None`：码控缺席，路径与收口前一致。现有
/// `crates/ye/tests/encode_h264.rs`（位级回环/PSNR/ffmpeg 合法性）全绿
/// 覆盖「与现版本行为一致」；此处补确定性——同一配置编两次，输出包
/// **逐字节相同**（`Packet: PartialEq` 按值判等，含码流/时戳/标志），
/// avcC extradata 相同。
#[test]
fn bit_rate_none_path_is_deterministic() {
    let cfg = rate_config(None, QualityMode::Qp(24));
    let mut a = H264Encoder::new(&cfg).unwrap();
    let pa = encode_frames(&mut a, 30);
    let mut b = H264Encoder::new(&cfg).unwrap();
    let pb = encode_frames(&mut b, 30);
    assert_eq!(pa, pb, "同一配置编两次输出逐字节相同");
    assert_eq!(a.extradata(), b.extradata());
}
