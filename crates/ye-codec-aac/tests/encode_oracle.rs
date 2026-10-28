//! AAC-LC 编码器 ffmpeg 对拍门禁（分级 skip 语义）。
//!
//! 流程（立体声 128k / 单声道 64k 两用例，44.1 kHz 3 s AM 正弦）：
//!
//! 1. `ffmpeg -f lavfi`（`aevalsrc` 确定性 AM 正弦）落 f32le PCM 源；
//! 2. 本编码器按 1024 样本帧编码成 ADTS（`.aac`）；
//! 3. ffmpeg 解码我们的 ADTS：退出码 0 且 stderr 无输出；ffprobe 帧数
//!    = ceil(样本/1024)；
//! 4. 自家解码器与 ffmpeg 各解同一 ADTS，逐样本 |Δ| 对拍（容差见各
//!    断言注释——实测收紧的诚实值，主判据 99.9% 样本 |Δ| ≤ 上限）；
//! 5. 质量门：ffmpeg 解码 vs 源 PCM 的 SNR ≥ 18 dB（128 k 立体声，
//!    跳过首 1024 样本——TDAC 首帧前半窗从零爬升的 priming 固有段）；
//! 6. 码率门：ADTS 总字节 × 8 / 时长在目标 ±20% 内。
//!
//! 工具缺席（定位器三档全落空）＝分级 skip 合法；工具在场但任何
//! ffmpeg 调用失败/产物缺失＝测试失败，不许 skip。

mod common;

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use ye_codec::{AudioFrame, CodecId, Decoded, Decoder, Encoder, EncoderConfig, Frame, Packet};
use ye_codec_aac::{AacDecoder, AacEncoder};
use ye_pixfmt::{ChannelLayout, SampleFormat};
use ye_time::Rational;

/// 测试时长（秒）与采样率。
const DURATION_S: f64 = 3.0;
const SAMPLE_RATE: u32 = 44_100;

/// AM 正弦 lavfi 配方（440 Hz 载波 × 0.25 Hz 幅度调制，峰值 0.4）。
const LAVFI_ARGS: &[&str] = &[
    "-v",
    "error",
    "-f",
    "lavfi",
    "-i",
    "aevalsrc=0.4*sin(2*PI*440*t)*0.5*(1+sin(2*PI*0.25*t)):s=44100:d=3",
];

/// 唯一临时文件（用后即删）。
struct TempFile(PathBuf);
impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// 就位一个临时文件路径。
fn temp_path(name: &str) -> TempFile {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&dir).expect("CARGO_TARGET_TMPDIR 建目录");
    TempFile(dir.join(format!(
        "ye-aac-enc-{}-{}-{name}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )))
}

/// 运行 ffmpeg，断言退出码 0 且 stderr 无输出（`-v error` 下 stderr
/// 非空即有错误）。
fn run_ffmpeg_quiet(args: &[String]) {
    let exe = common::ffmpeg_exe().expect("定位器已命中");
    let out = Command::new(&exe)
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("启动 ffmpeg 失败（{}）：{err}", exe.display()));
    assert!(
        out.status.success(),
        "ffmpeg 退出码 {:?}（stderr: {}）",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.trim().is_empty(),
        "ffmpeg stderr 应无错误输出：{stderr}"
    );
}

/// 生成源 PCM（f32le，`channels` 声道）→ (样本交织 f32, 临时文件)。
fn make_source_pcm(channels: u32) -> (Vec<f32>, TempFile) {
    let pcm = temp_path(&format!("src-{}.pcm", channels));
    let mut args: Vec<String> = LAVFI_ARGS.iter().map(|s| (*s).to_string()).collect();
    args.extend(
        [
            "-ac",
            if channels == 1 { "1" } else { "2" },
            "-f",
            "f32le",
            "-c:a",
            "pcm_f32le",
            "-y",
        ]
        .iter()
        .map(|s| (*s).to_string()),
    );
    args.push(pcm.0.to_str().expect("临时路径 UTF-8").to_string());
    run_ffmpeg_quiet(&args);
    let bytes =
        std::fs::read(&pcm.0).unwrap_or_else(|e| panic!("读源 PCM {}: {e}", pcm.0.display()));
    let samples: Vec<f32> = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect();
    assert!(
        samples.len() > SAMPLE_RATE as usize * 2,
        "源 PCM 过短（{} 样本）",
        samples.len()
    );
    (samples, pcm)
}

/// 编码 PCM → ADTS 字节流（逐 1024 样本帧，尾帧 flush 补零）。
fn encode_adts(samples: &[f32], channels: u32, bit_rate: u64) -> Vec<u8> {
    let layout = if channels == 1 {
        ChannelLayout::MONO
    } else {
        ChannelLayout::STEREO
    };
    let cfg = EncoderConfig {
        codec: CodecId::Aac,
        time_base: Rational::new(1, SAMPLE_RATE as i32),
        bit_rate: Some(bit_rate),
        video: None,
        audio: Some(ye_codec::AudioEncoderConfig {
            sample_rate: SAMPLE_RATE,
            channel_layout: layout,
            format: SampleFormat::F32,
        }),
    };
    let mut enc = AacEncoder::new(&cfg).expect("编码器构造");
    let frames_total = samples.len() / channels as usize;
    let mut pos = 0usize;
    while pos < frames_total {
        let n = (frames_total - pos).min(1024);
        let mut frame =
            AudioFrame::new(SampleFormat::F32, layout, SAMPLE_RATE, n, None).expect("帧构造");
        let bytes: Vec<u8> = samples[pos * channels as usize..(pos + n) * channels as usize]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        frame.data_mut().copy_from_slice(&bytes);
        enc.feed(&Frame::Audio(frame)).expect("编码 feed");
        pos += n;
    }
    enc.flush();
    let mut out = Vec::new();
    while let Ok(ye_codec::Encoded::Packet(p)) = enc.pull() {
        out.extend_from_slice(&p.data);
    }
    out
}

/// 自家解码器解 ADTS → 交织 f32。
fn decode_with_ours(adts: &[u8]) -> Vec<f32> {
    let params = ye_codec::CodecParameters::new(CodecId::Aac, ye_codec::MediaType::Audio);
    let mut dec = AacDecoder::new(&params).expect("解码器构造");
    let mut off = 0usize;
    let mut samples = Vec::new();
    while off < adts.len() {
        let len = ((adts[off + 3] as usize & 0x03) << 11)
            | ((adts[off + 4] as usize) << 3)
            | ((adts[off + 5] as usize >> 5) & 0x07);
        assert!(
            len >= 7 && off + len <= adts.len(),
            "ADTS 帧长 {len} @{off}"
        );
        let mut p = Packet::empty(0);
        p.data = Arc::from(adts[off..off + len].to_vec());
        dec.feed(&p).expect("解码 feed");
        while let Ok(Decoded::Frame(Frame::Audio(f))) = dec.pull() {
            for c in f.data().as_chunks::<4>().0 {
                samples.push(f32::from_le_bytes(*c));
            }
        }
        off += len;
    }
    dec.flush();
    samples
}

/// ffmpeg 解 ADTS → 交织 f32（退出码 0 + stderr 无输出即门禁③）。
fn decode_with_ffmpeg(adts_path: &std::path::Path) -> Vec<f32> {
    let pcm = temp_path("ffdec.pcm");
    let args: Vec<String> = [
        "-v",
        "error",
        "-i",
        adts_path.to_str().expect("路径 UTF-8"),
        "-f",
        "f32le",
        "-c:a",
        "pcm_f32le",
        "-y",
        pcm.0.to_str().expect("路径 UTF-8"),
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    run_ffmpeg_quiet(&args);
    let bytes = std::fs::read(&pcm.0).unwrap_or_else(|e| panic!("读 ffmpeg 解码: {e}"));
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| f32::from_le_bytes(*c))
        .collect()
}

/// ffprobe 统计 ADTS 帧数。
fn count_frames_ffprobe(adts_path: &std::path::Path) -> usize {
    let exe = common::ffprobe_exe().expect("定位器已命中");
    let out = Command::new(&exe)
        .args([
            "-v",
            "error",
            "-count_packets",
            "-select_streams",
            "a:0",
            "-show_entries",
            "stream=nb_read_packets",
            "-of",
            "csv=p=0",
        ])
        .arg(adts_path)
        .output()
        .unwrap_or_else(|err| panic!("启动 ffprobe 失败（{}）：{err}", exe.display()));
    assert!(
        out.status.success(),
        "ffprobe 退出码 {:?}（stderr: {}）",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<usize>()
        .unwrap_or_else(|e| panic!("ffprobe 帧数解析失败：{e}"))
}

/// 一个用例的全链路：源 → 编码 → ffmpeg/自家解码对拍 → 质量门/码率门。
/// `snr_gate` = None 时跳过 SNR 门（单声道 64k 用例按任务面只做对拍与
/// 码率门）。
fn run_case(channels: u32, bit_rate: u64, snr_gate: Option<f64>) {
    let (src, _src_file) = make_source_pcm(channels);
    let adts = encode_adts(&src, channels, bit_rate);
    let aac = temp_path(&format!("ours-{}.aac", channels));
    std::fs::write(&aac.0, &adts).expect("写 ADTS");

    let frames_total_src = src.len() / channels as usize;
    let expect_frames = frames_total_src.div_ceil(1024);

    // 门禁③：ffmpeg 解码 exit-clean + 帧数
    let ff = decode_with_ffmpeg(&aac.0);
    let n_frames = count_frames_ffprobe(&aac.0);
    assert_eq!(n_frames, expect_frames, "ADTS 帧数 = ceil(样本/1024)");
    assert_eq!(
        ff.len(),
        expect_frames * 1024 * channels as usize,
        "ffmpeg 解码样本数 = 帧数×1024×声道"
    );

    // 门禁④：自家解码 vs ffmpeg 解码逐样本对拍。
    // 容差依据（实测后收紧的诚实值，见 run 输出）：两解码器同为
    // requant + IMDCT 的 f32 实现，差异仅来自浮点求和次序——本机实测
    // 3 s 立体声/单声道 max|Δ| ≤ 1.5e-7（远优于任务给的 5e-3/5e-2
    // 起步口径），断言收紧为 100% |Δ| ≤ 1e-5（约 66 倍实测余量）。
    let ours = decode_with_ours(&adts);
    assert_eq!(ours.len(), ff.len(), "两解码器输出样本数一致");
    let mut max_d = 0.0f32;
    for (a, b) in ours.iter().zip(ff.iter()) {
        let d = (a - b).abs();
        max_d = max_d.max(d);
        assert!(
            d <= 1.0e-5,
            "两解码器逐样本 |Δ| {d} 超 1e-5 上限（实测 ~1.5e-7 量级）"
        );
    }
    println!("aac-enc oracle ch={channels}: max|Δ|={max_d:.3e}");

    // 门禁⑤：SNR（跳过首 1024 帧 priming 段；源与解码逐样本对齐）
    if let Some(gate) = snr_gate {
        let skip = 1024 * channels as usize;
        let mut sig = 0.0f64;
        let mut err = 0.0f64;
        let n = (src.len().min(ff.len()) - skip).min(src.len() - skip);
        for i in 0..n {
            let s = f64::from(src[skip + i]);
            let d = f64::from(ff[skip + i]);
            sig += s * s;
            err += (s - d) * (s - d);
        }
        let snr = 10.0 * (sig / err.max(1e-30)).log10();
        println!("aac-enc oracle ch={channels}: SNR = {snr:.2} dB");
        assert!(snr >= gate, "SNR {snr:.2} dB < 门限 {gate} dB");
    }

    // 门禁⑥：码率 ±20%
    let bitrate = adts.len() as f64 * 8.0 / DURATION_S;
    let target = bit_rate as f64;
    println!(
        "aac-enc oracle ch={channels}: 码率 {bitrate:.0} bps（目标 {target}，{} 帧）",
        n_frames
    );
    assert!(
        (bitrate - target).abs() / target <= 0.20,
        "码率 {bitrate:.0} 偏离目标 {target} 超 ±20%"
    );
}

/// 立体声 128 kbps：全链路门禁（对拍 + SNR ≥ 18 dB + 码率门）。
#[test]
fn oracle_stereo_128k() {
    if common::ffmpeg_exe().is_none() {
        eprintln!("跳过：ffmpeg 定位三档全落空（分级 skip 语义）");
        return;
    }
    run_case(2, 128_000, Some(18.0));
}

/// 单声道 64 kbps：对拍 + 码率门（SNR 门按任务面只约束 128 k 立体声）。
#[test]
fn oracle_mono_64k() {
    if common::ffmpeg_exe().is_none() {
        eprintln!("跳过：ffmpeg 定位三档全落空（分级 skip 语义）");
        return;
    }
    run_case(1, 64_000, None);
}
