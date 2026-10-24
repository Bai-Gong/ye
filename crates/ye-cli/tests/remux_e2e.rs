//! `ye remux` 端到端集成测试（M1 收尾，M2 接线后随新 API 更新）：
//! PATH 上的 ffmpeg 造小双轨源（视频 + 音频各一路）→ 真二进制
//! `ye remux` 直通（注册表 auto-probe + 门面 remux 助手）→ 三重验收：
//!
//! 1. 输出能被 ffprobe 正常读取（`-v error` 退出码 0）；
//! 2. 流参数与输入一致（ffprobe codec/尺寸/声道/采样率/时基逐字段）；
//! 3. ye 再解封装输出与首次解封装输入逐包对拍——数据字节严格一致，
//!    dts/pts/duration 在流时基原生单位严格相等（读取端无换算无舍入；
//!    remux 同容器直通、模板时基 = 源流时基 ⇒ muxer 重标度恒等），
//!    关键帧 flags 逐包一致。
//!
//! PATH 上找不到 ffmpeg/ffprobe（或不可执行）时打印跳过信息直接返回
//! （不 fail）。临时文件全部落在 `CARGO_TARGET_TMPDIR`（target/ 下，
//! 不入库）。本文件同时充当门面 re-export 的回归：全部类型经
//! `ye::container::mp4` 门面路径引用。

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use ye::container::mp4::Mp4Reader;
use ye_cli::{Command as CliCommand, parse_args};

// —— 工具探测与临时目录 ——

/// ffmpeg/ffprobe 是否可用（与 ye-container-mp4 oracle 测试同规则）。
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

/// lavfi 造源：128x96@10fps 1s testsrc2（libx264 含 2 个 B 帧 ⇒ ctts）+
/// 44.1kHz 1s sine（aac）。流序：0=视频（avc1），1=音频（mp4a）。
/// 与 ye-container-mp4/tests/oracle_mux.rs 的造源命令一致。
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
            "testsrc2=duration=1:size=128x96:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:sample_rate=44100:duration=1",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-bf",
            "2",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "64k",
            "-shortest",
        ])
        .arg(path)
        .status()
        .expect("ffmpeg 应可执行（tools_ok 已确认）");
    assert!(st.success(), "ffmpeg 造源失败：{}", path.display());
}

// —— 朴素 JSON 字段抽取（ffprobe 输出扁平，与 oracle_mux 同款）——

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

/// 逐个抽取 `"key": value` / `"key": "value"` 出现值（按出现序）。
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

// —— 端到端主用例 ——

#[test]
fn remux_end_to_end() {
    if !tools_ok() {
        eprintln!("跳过：PATH 无 ffmpeg/ffprobe（remux_end_to_end）");
        return;
    }
    let dir = tmp("remux_e2e");
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("src.mp4");
    let dst = dir.join("out.mp4");
    gen_source(&src);

    // —— 真二进制跑 `ye remux`（argv 解析 → reader → writer 全链路）——
    let run = Command::new(env!("CARGO_BIN_EXE_ye"))
        .arg("remux")
        .arg(&src)
        .arg(&dst)
        .output()
        .expect("启动 ye 二进制失败");
    let stdout = String::from_utf8_lossy(&run.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
    assert!(
        run.status.success(),
        "`ye remux` 应以 0 退出，stderr:\n{stderr}"
    );
    assert!(
        stdout.contains("2 路流") && stdout.contains("直通"),
        "stdout 应含直通摘要，实际:\n{stdout}"
    );
    eprintln!("---- ye remux 实际输出 ----\n{stdout}---------------------------");

    // —— ①② ffprobe 可读 + 流参数与源一致 ——
    let ja = ffprobe_streams_json(&src);
    let jb = ffprobe_streams_json(&dst);
    assert_eq!(
        json_fields(&ja, "codec_name").len(),
        2,
        "源应为视频+音频双轨"
    );
    for key in [
        "codec_name",
        "codec_tag_string",
        "width",
        "height",
        "sample_rate",
        "channels",
        "time_base",
    ] {
        assert_eq!(
            json_fields(&ja, key),
            json_fields(&jb, key),
            "ffprobe 字段 {key} 应与源一致"
        );
    }

    // —— ③ ye 两次解封装逐包对拍（输入 vs 重封装输出）——
    // 数据字节严格一致；时戳在流时基原生单位严格相等（存储域即真值，
    // remux 模板时基 = 源流时基 ⇒ 写入重标度恒等）；关键帧 flags 一致。
    let mut so = Mp4Reader::open(&src).unwrap();
    let mut rt = Mp4Reader::open(&dst).unwrap();
    assert_eq!(rt.streams().len(), so.streams().len());
    let mut total = 0usize;
    for si in 0..so.streams().len() {
        let s_src = so.stream(si).unwrap();
        let s_rt = rt.stream(si).unwrap();
        // 流元数据一致（时基/extra_data/视频音频参数原样回写）。
        assert_eq!(s_src.codec.codec, s_rt.codec.codec, "流 {si} codec");
        assert_eq!(s_src.time_base, s_rt.time_base, "流 {si} 时基");
        assert_eq!(
            s_src.codec.extradata, s_rt.codec.extradata,
            "流 {si} extra_data"
        );
        assert_eq!(s_src.codec.video, s_rt.codec.video, "流 {si} 视频参数");
        assert_eq!(s_src.codec.audio, s_rt.codec.audio, "流 {si} 音频参数");
        assert_eq!(
            so.sample_count(si).unwrap(),
            rt.sample_count(si).unwrap(),
            "流 {si} 样例数"
        );
        for ii in 0..rt.sample_count(si).unwrap() {
            let po = so.read_packet(si, ii).unwrap();
            let pr = rt.read_packet(si, ii).unwrap();
            assert_eq!(po.data, pr.data, "流 {si} 样例 {ii} 数据逐字节一致");
            assert_eq!(po.dts, pr.dts, "流 {si} 样例 {ii} dts（流时基原生单位）");
            assert_eq!(po.pts, pr.pts, "流 {si} 样例 {ii} pts（流时基原生单位）");
            assert_eq!(
                po.duration, pr.duration,
                "流 {si} 样例 {ii} duration（流时基原生单位）"
            );
            assert_eq!(po.flags, pr.flags, "流 {si} 样例 {ii} flags（关键帧）");
            total += 1;
        }
    }
    assert!(total > 0, "对拍应覆盖至少一个包");
}

// —— 参数解析（不需要 ffmpeg）——

#[test]
fn parse_args_accepts_remux() {
    let case = parse_args(&[
        OsString::from("remux"),
        OsString::from("in.mp4"),
        OsString::from("out.mp4"),
    ]);
    assert_eq!(
        case,
        Ok(CliCommand::Remux {
            input: PathBuf::from("in.mp4"),
            output: PathBuf::from("out.mp4")
        })
    );
}

#[test]
fn parse_args_rejects_bad_remux_usage() {
    assert!(
        parse_args(&[OsString::from("remux")]).is_err(),
        "remux 缺参数应报用法错误"
    );
    assert!(
        parse_args(&[OsString::from("remux"), OsString::from("a.mp4")]).is_err(),
        "remux 只有输入应报用法错误"
    );
    assert!(
        parse_args(&[
            OsString::from("remux"),
            OsString::from("a.mp4"),
            OsString::from("b.mp4"),
            OsString::from("c.mp4")
        ])
        .is_err(),
        "remux 多余参数应报用法错误"
    );
}

/// 打开失败的错误信息应带路径前缀（不依赖 ffmpeg）。
#[test]
fn remux_reports_open_failure_with_path() {
    let dir = tmp("remux_e2e");
    let missing = dir.join("no-such-input.mp4");
    let out = dir.join("no-such-output.mp4");
    let err = ye_cli::remux(&missing, &out).expect_err("打开不存在的文件应失败");
    assert!(
        err.contains(missing.display().to_string().as_str()),
        "错误信息应含输入路径，实际: {err}"
    );
}
