//! `ye convert` 端到端集成测试（M3 集成阶段）：PATH 上的 ffmpeg 造
//! 约 3 秒 176x144 的 H.264+AAC 双轨源（Baseline——对齐自家解码器的
//! 对拍口径）→ 真二进制 `ye convert --size 320x240`（视频重编码 +
//! 音频直通）→ 三重验收：
//!
//! 1. 输出能被 ffprobe/ffmpeg 正常读取（`-v error` 解码无错）；
//! 2. 输出视频为 h264 320x240、音频 aac 参数与源一致（直通）；
//! 3. 时长与帧数和输入一致（ffprobe duration / nb_read_frames 对拍，
//!    ±0.1s / 逐帧相等——视频时间线按帧率重排，帧数不变是硬约束）。
//!
//! PATH 上找不到 ffmpeg/ffprobe 时打印跳过信息直接返回（不 fail），
//! 与 `remux_e2e.rs` 同规则；临时文件全部落在 `CARGO_TARGET_TMPDIR`
//! （target/ 下，不入库）。

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

// —— 工具探测与临时目录 ——

/// ffmpeg/ffprobe 是否可用（与 remux_e2e 同规则）。
fn tools_ok() -> bool {
    ["ffmpeg", "ffprobe"].iter().all(|tool| {
        Command::new(tool)
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
    })
}

fn tmp(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

/// 造源：3s 176x144@15fps testsrc2（libx264 Baseline——对齐自家解码
/// 口径）+ 3s 44.1kHz stereo sine（aac）。流序：0=视频，1=音频。
fn gen_source(path: &Path) {
    let st = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=3:size=176x144:rate=15",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:sample_rate=44100:duration=3",
            "-c:v",
            "libx264",
            "-profile:v",
            "baseline",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            // AAC 解码当前明确拒绝 PNS，造源须与 conformance 的支持范围一致。
            "-aac_pns",
            "0",
            "-b:a",
            "64k",
            "-shortest",
        ])
        .arg(path)
        .status()
        .expect("ffmpeg 应可执行（tools_ok 已确认）");
    assert!(st.success(), "ffmpeg 造源失败：{}", path.display());
}

fn run_stdout(cmd: &mut Command) -> String {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "命令失败：{:?}\n{}",
        cmd,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn ffprobe_streams_json(path: &Path) -> String {
    run_stdout(
        Command::new("ffprobe")
            .args(["-v", "error", "-print_format", "json", "-show_streams"])
            .arg(path),
    )
}

/// 逐个抽取 `"key": value` 出现值（与 remux_e2e 同款朴素抽取）。
fn json_fields(json: &str, key: &str) -> Vec<String> {
    let pat = format!("\"{key}\":");
    let mut vals = Vec::new();
    let mut rest = json;
    while let Some(i) = rest.find(&pat) {
        let tail = rest[i + pat.len()..].trim_start();
        if let Some(s) = tail.strip_prefix('"') {
            let end = s.find('"').unwrap_or(s.len());
            vals.push(s[..end].to_string());
            rest = &s[end..];
        } else {
            let end = tail.find([',', '\n', '}']).unwrap_or(tail.len());
            vals.push(tail[..end].trim().to_string());
            rest = &tail[end..];
        }
    }
    vals
}

/// 视频流实读帧数（`-count_frames`）。
fn video_frame_count(path: &Path) -> u64 {
    let out = run_stdout(
        Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-count_frames",
                "-show_entries",
                "stream=nb_read_frames",
                "-of",
                // CSV 会把流附带的 side data 输出成额外列；这里只读取帧数。
                "default=noprint_wrappers=1:nokey=1",
            ])
            .arg(path),
    );
    out.trim().parse().expect("nb_read_frames 应为整数")
}

/// 容器时长（秒，首个 duration 字段）。
fn container_duration(path: &Path) -> f64 {
    let out = run_stdout(
        Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "csv=p=0",
            ])
            .arg(path),
    );
    out.trim().parse().expect("duration 应为数值")
}

// —— 端到端主用例 ——

#[test]
fn convert_end_to_end() {
    if !tools_ok() {
        eprintln!("跳过：PATH 无 ffmpeg/ffprobe（convert_end_to_end）");
        return;
    }
    let dir = tmp("convert_e2e");
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("src.mp4");
    let dst = dir.join("out-320x240.mp4");
    gen_source(&src);

    // —— 真二进制跑 `ye convert --size 320x240` ——
    let run = Command::new(env!("CARGO_BIN_EXE_ye"))
        .arg("convert")
        .arg(&src)
        .arg(&dst)
        .arg("--size")
        .arg("320x240")
        .output()
        .expect("启动 ye 二进制失败");
    let stdout = String::from_utf8_lossy(&run.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
    assert!(
        run.status.success(),
        "`ye convert` 应以 0 退出，stderr:\n{stderr}"
    );
    assert!(
        stdout.contains("2 路流") && stdout.contains("重编码") && stdout.contains("直通"),
        "stdout 应含转码摘要，实际:\n{stdout}"
    );
    assert!(
        stdout.contains("320x240"),
        "stdout 应含目标尺寸，实际:\n{stdout}"
    );
    eprintln!("---- ye convert 实际输出 ----\n{stdout}---------------------------");

    // —— ① ffmpeg 解码无错 ——
    let decode = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&dst)
        .args(["-f", "null", "-"])
        .output()
        .expect("启动 ffmpeg 失败");
    assert!(
        decode.status.success() && decode.stderr.is_empty(),
        "输出应可被 ffmpeg 无错解码：{}",
        String::from_utf8_lossy(&decode.stderr)
    );

    // —— ② 流参数：视频 h264 320x240；音频直通与源一致 ——
    let ja = ffprobe_streams_json(&src);
    let jb = ffprobe_streams_json(&dst);
    assert_eq!(
        json_fields(&ja, "codec_name").len(),
        2,
        "源应为视频+音频双轨"
    );
    assert_eq!(json_fields(&jb, "codec_name"), vec!["h264", "aac"]);
    assert_eq!(json_fields(&jb, "width"), vec!["320"]);
    assert_eq!(json_fields(&jb, "height"), vec!["240"]);
    for key in ["sample_rate", "channels", "channel_layout"] {
        assert_eq!(
            json_fields(&ja, key),
            json_fields(&jb, key),
            "音频字段 {key} 应与源一致（直通）"
        );
    }

    // —— ③ 时长与帧数和输入一致 ——
    let (da, db) = (container_duration(&src), container_duration(&dst));
    assert!((da - db).abs() <= 0.1, "时长应一致：源 {da}s vs 输出 {db}s");
    let (fa, fb) = (video_frame_count(&src), video_frame_count(&dst));
    assert_eq!(fa, fb, "视频帧数应一致（源 {fa} vs 输出 {fb}）");
    assert_eq!(fa, 45, "3s@15fps 应为 45 帧");

    // 清理临时产物。
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);
}

/// `--no-video --audio aac:96`：纯音频输出——单 aac 轨、参数与源一致、
/// 时长一致、ffmpeg 无错解码、实测码率落在目标容差内（M3 补齐）。
#[test]
fn convert_audio_reencode_end_to_end() {
    if !tools_ok() {
        eprintln!("跳过：PATH 无 ffmpeg/ffprobe（convert_audio_reencode_end_to_end）");
        return;
    }
    let dir = tmp("convert_e2e");
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("src-audio.mp4");
    let dst = dir.join("out-audio-96k.mp4");
    gen_source(&src);

    let run = Command::new(env!("CARGO_BIN_EXE_ye"))
        .arg("convert")
        .arg(&src)
        .arg(&dst)
        .arg("--no-video")
        .arg("--audio")
        .arg("aac:96")
        .output()
        .expect("启动 ye 二进制失败");
    let stdout = String::from_utf8_lossy(&run.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
    assert!(
        run.status.success(),
        "`ye convert --no-video --audio aac:96` 应以 0 退出，stderr:\n{stderr}"
    );
    assert!(
        stdout.contains("1 路流") && stdout.contains("剥除") && stdout.contains("96kbps"),
        "stdout 应含纯音频转码摘要，实际:\n{stdout}"
    );
    eprintln!("---- ye convert（纯音频）实际输出 ----\n{stdout}---------------------------");

    // ffmpeg 解码无错。
    let decode = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&dst)
        .args(["-f", "null", "-"])
        .output()
        .expect("启动 ffmpeg 失败");
    assert!(
        decode.status.success() && decode.stderr.is_empty(),
        "输出应可被 ffmpeg 无错解码：{}",
        String::from_utf8_lossy(&decode.stderr)
    );

    // 流参数：单 aac 轨、44.1kHz、声道随源（lavfi sine 默认单声道 → SCE）。
    let jb = ffprobe_streams_json(&dst);
    assert_eq!(json_fields(&jb, "codec_name"), vec!["aac"]);
    assert_eq!(json_fields(&jb, "sample_rate"), vec!["44100"]);
    assert_eq!(json_fields(&jb, "channels"), vec!["1"]);
    let (da, db) = (container_duration(&src), container_duration(&dst));
    assert!((da - db).abs() <= 0.1, "时长应一致：源 {da}s vs 输出 {db}s");
    let bit_rate: f64 = json_fields(&jb, "bit_rate")
        .first()
        .expect("aac 流应带 bit_rate")
        .parse()
        .expect("bit_rate 应为数值");
    // 帧内码率环从细侧向预算收敛：简单内容（正弦）常落在预算下方，
    // 容差取 [0.5, 1.4]×96k（按实测记录收紧的依据见 ye-codec-aac
    // oracle 测试——那边才是收敛性的正本）。
    assert!(
        (48_000.0..=134_400.0).contains(&bit_rate),
        "实测码率 {bit_rate} 应落在 96k×[0.5,1.4] 容差内"
    );

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&dst);
}

// —— 参数解析（不需要 ffmpeg）——

/// 坏尺寸报用法错误（lib 层直调解析）。
#[test]
fn parse_args_rejects_bad_convert_usage() {
    use std::ffi::OsString;
    assert!(
        ye_cli::parse_args(&[OsString::from("convert")]).is_err(),
        "convert 缺参数应报用法错误"
    );
    assert!(
        ye_cli::parse_args(&[
            OsString::from("convert"),
            OsString::from("a.mp4"),
            OsString::from("b.mp4"),
            OsString::from("--size"),
            OsString::from("320"),
        ])
        .is_err(),
        "--size 缺 x 应报用法错误"
    );
    assert!(
        ye_cli::parse_args(&[
            OsString::from("convert"),
            OsString::from("a.mp4"),
            OsString::from("b.mp4"),
            OsString::from("--size"),
            OsString::from("ax240"),
        ])
        .is_err(),
        "--size 非数字应报用法错误"
    );
    assert!(
        ye_cli::parse_args(&[
            OsString::from("convert"),
            OsString::from("a.mp4"),
            OsString::from("b.mp4"),
            OsString::from("--audio"),
            OsString::from("mp3:128"),
        ])
        .is_err(),
        "--audio 非 copy/aac 值应报用法错误"
    );
    assert!(
        ye_cli::parse_args(&[
            OsString::from("convert"),
            OsString::from("a.mp4"),
            OsString::from("b.mp4"),
            OsString::from("--vb"),
            OsString::from("0"),
        ])
        .is_err(),
        "--vb 0 应报用法错误"
    );
}

/// M3 补齐旗标面：--vb/--audio/--no-video 解析成功且落位。
#[test]
fn parse_args_accepts_m3_completion_flags() {
    use std::ffi::OsString;
    let cmd = ye_cli::parse_args(&[
        OsString::from("convert"),
        OsString::from("a.mp4"),
        OsString::from("b.mp4"),
        OsString::from("--size"),
        OsString::from("320x240"),
        OsString::from("--vb"),
        OsString::from("800"),
        OsString::from("--audio"),
        OsString::from("aac:128"),
        OsString::from("--no-video"),
    ])
    .expect("合法旗标组合应解析成功");
    let ye_cli::Command::Convert { options, .. } = cmd else {
        panic!("应解析为 Convert：{cmd:?}");
    };
    assert_eq!(options.size, Some((320, 240)));
    assert_eq!(options.video_bitrate_kbps, Some(800));
    assert_eq!(options.audio, ye_cli::ConvertAudio::Aac { kbps: 128 });
    assert!(options.no_video);
}
