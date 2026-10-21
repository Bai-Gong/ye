//! oracle 层封装对拍：ffmpeg lavfi 造 avc1+mp4a 源 → Mp4Reader 读 →
//! Mp4Writer(Progressive) 重封装 → ffprobe/ffmpeg 对拍。
//!
//! skip 规则同模块一：PATH 上找不到 ffmpeg/ffprobe（或不可执行）时
//! 打印一行跳过信息直接返回（不 fail）。临时文件全部落在
//! `CARGO_TARGET_TMPDIR`。
//!
//! 时戳对拍口径：原文件与重封装文件都用自有 Mp4Reader 读（均为 stts/
//! ctts 原始值，不含 ffmpeg demuxer 的 edit-list 平移），统一在流时基
//! 单位域严格相等——timescale ≤ 7,056,000 时 tick→单位→tick 两次
//! half-away 舍入互相抵消（数学保证，见 ye-time
//! units_round_trip_common_timescales 测试）。tick 域严格相等仅当
//! timescale 整除 7,056,000（如 44100/48000；视频 10240 之类非整除
//! 时基只在单位域断言），另作一组断言。

use std::path::{Path, PathBuf};
use std::process::Command;
use ye_container_mp4::{Layout, Mp4Reader, Mp4Writer, Mp4WriterOptions, Packet, TrackConfig};
use ye_time::TICKS_PER_SECOND;

/// ffmpeg/ffprobe 是否可用。
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

/// lavfi 造源：128x96@10fps 1s testsrc2（libx264，2 个 B 帧 ⇒ ctts）+
/// 44.1kHz 1s sine（aac）。流序：0=视频（avc1），1=音频（mp4a）。
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

/// 读源 → Mp4Writer(Progressive) 重封装（按 dts 交错写）。
fn remux(src: &Path, dst: &Path, force_co64: bool) {
    let mut rd = Mp4Reader::open(src).unwrap();
    let mut w = Mp4Writer::create(
        dst,
        Mp4WriterOptions {
            layout: Layout::Progressive,
            force_co64,
            ..Default::default()
        },
    )
    .unwrap();
    for s in rd.streams() {
        w.add_stream(TrackConfig::from_stream(s)).unwrap();
    }
    let mut per_stream: Vec<Vec<Packet>> = Vec::new();
    for si in 0..rd.streams().len() {
        let mut v = Vec::new();
        for ii in 0..rd.sample_count(si).unwrap() {
            v.push(rd.read_packet(si, ii).unwrap());
        }
        per_stream.push(v);
    }
    // 合并写：按 (dts, stream_index) 全局排序；同轨子序列仍保持轨内序。
    let mut all: Vec<&Packet> = per_stream.iter().flatten().collect();
    all.sort_by_key(|p| (p.dts, p.stream_index));
    for p in all {
        w.write_packet(p).unwrap();
    }
    w.finalize().unwrap();
}

// —— 朴素 JSON 字段抽取（ffprobe 输出扁平，足够用）——

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

fn ffprobe_packets_json(path: &Path) -> String {
    run_stdout(
        Command::new("ffprobe")
            .args(["-v", "error", "-print_format", "json", "-show_packets"])
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
            let end = s_find(tail, &[',', '\n', '}']).unwrap_or(tail.len());
            vals.push(tail[..end].trim().to_string());
            rest = &tail[end..];
        }
    }
    vals
}

fn s_find(s: &str, chars: &[char]) -> Option<usize> {
    s.find(|c| chars.contains(&c))
}

/// 指定流（ffprobe packets 的 stream_index）逐包 flags 是否含 K。
fn ffprobe_keyframe_flags(packets_json: &str, stream_index: usize) -> Vec<bool> {
    let mut out = Vec::new();
    // 按包对象切：每个对象含一个 "stream_index": N。
    for chunk in packets_json.split("\"stream_index\":") {
        let num: String = chunk
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if num.is_empty() {
            continue; // 首段前导 JSON 文本
        }
        if num.parse::<usize>().ok() != Some(stream_index) {
            continue;
        }
        let is_k = chunk.find("\"flags\":").is_some_and(|i| {
            let s = chunk[i + "\"flags\":".len()..].trim_start();
            s.starts_with("\"K")
        });
        out.push(is_k);
    }
    out
}

// —— 用例 ——

/// ①⑤ ffprobe 可读且字段一致 ② roundtrip 逐包对拍 ③ flags=K 对拍
/// ⑤ 解码冒烟。合一个测试共用一次造源/重封装。
#[test]
fn remux_roundtrip_and_probes() {
    if !tools_ok() {
        eprintln!("跳过：PATH 无 ffmpeg/ffprobe（oracle_mux）");
        return;
    }
    let dir = tmp("oracle_mux");
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("src.mp4");
    let out = dir.join("out.mp4");
    gen_source(&src);
    remux(&src, &out, false);

    // —— ② roundtrip：自有 reader 两侧逐包对拍 ——
    let mut so = Mp4Reader::open(&src).unwrap();
    let mut rt = Mp4Reader::open(&out).unwrap();
    assert_eq!(rt.streams().len(), so.streams().len());
    for si in 0..so.streams().len() {
        let s_src = so.stream(si).unwrap();
        let s_rt = rt.stream(si).unwrap();
        // 流元数据一致（extra_data 原样回写）。
        assert_eq!(s_src.codec.as_str(), s_rt.codec.as_str(), "流 {si} codec");
        assert_eq!(s_src.timebase, s_rt.timebase, "流 {si} 时基");
        assert_eq!(s_src.extra_data, s_rt.extra_data, "流 {si} extra_data");
        assert_eq!(s_src.video, s_rt.video, "流 {si} 视频参数");
        assert_eq!(s_src.audio, s_rt.audio, "流 {si} 音频参数");
        assert_eq!(
            so.sample_count(si).unwrap(),
            rt.sample_count(si).unwrap(),
            "流 {si} 样例数"
        );
        let tb = s_src.timebase;
        // tick 域严格相等仅当 timescale 整除 7,056,000。
        let tick_strict = tb.den != 0 && TICKS_PER_SECOND % tb.den == 0;
        for ii in 0..rt.sample_count(si).unwrap() {
            let po = so.read_packet(si, ii).unwrap();
            let pr = rt.read_packet(si, ii).unwrap();
            assert_eq!(po.data, pr.data, "流 {si} 样例 {ii} 数据");
            // 单位域严格相等（timescale ≤ 7,056,000 的数学保证，见文件头注）。
            assert_eq!(
                po.dts.to_units(tb),
                pr.dts.to_units(tb),
                "流 {si} 样例 {ii} dts（单位域）"
            );
            // pts 缺省语义 = dts（None 与 Some(dts) 等价处理）。
            let po_pts = po.pts.unwrap_or(po.dts);
            let pr_pts = pr.pts.unwrap_or(pr.dts);
            assert_eq!(
                po_pts.to_units(tb),
                pr_pts.to_units(tb),
                "流 {si} 样例 {ii} pts（单位域）"
            );
            assert_eq!(
                po.duration.to_units(tb),
                pr.duration.to_units(tb),
                "流 {si} 样例 {ii} duration（单位域）"
            );
            assert_eq!(po.keyframe, pr.keyframe, "流 {si} 样例 {ii} keyframe");
            if tick_strict {
                assert_eq!(po.dts, pr.dts, "流 {si} 样例 {ii} dts（tick 域，整除时基）");
                assert_eq!(po_pts, pr_pts, "流 {si} 样例 {ii} pts（tick 域）");
                assert_eq!(
                    po.duration, pr.duration,
                    "流 {si} 样例 {ii} duration（tick 域）"
                );
            }
        }
    }

    // —— ① ffprobe -show_streams：重封装可读且 codec/尺寸/采样率/time_base 一致 ——
    let ja = ffprobe_streams_json(&src);
    let jb = ffprobe_streams_json(&out);
    assert_eq!(json_fields(&ja, "codec_name").len(), 2, "源应有 2 路流");
    for key in [
        "codec_name",
        "width",
        "height",
        "sample_rate",
        "time_base",
        "channels",
    ] {
        assert_eq!(
            json_fields(&ja, key),
            json_fields(&jb, key),
            "ffprobe 字段 {key} 应与源一致"
        );
    }

    // —— ③ ffprobe -show_packets flags=K 与读回 keyframe 逐包对拍 ——
    let pk_json = ffprobe_packets_json(&out);
    for si in 0..rt.streams().len() {
        let flags = ffprobe_keyframe_flags(&pk_json, si);
        let ours: Vec<bool> = (0..rt.sample_count(si).unwrap())
            .map(|ii| rt.read_packet(si, ii).unwrap().keyframe)
            .collect();
        assert_eq!(
            flags, ours,
            "流 {si} ffprobe flags=K 应与读回 keyframe 一致"
        );
    }

    // —— ⑤ 解码冒烟：-v error 下退出码 0 且无错误输出 ——
    let smoke = Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&out)
        .args(["-f", "null", "-"])
        .output()
        .unwrap();
    assert!(
        smoke.status.success(),
        "ffmpeg 解码冒烟失败：{}",
        String::from_utf8_lossy(&smoke.stderr)
    );
    assert!(
        smoke.stderr.is_empty(),
        "解码冒烟应无错误输出：{}",
        String::from_utf8_lossy(&smoke.stderr)
    );
}

/// ④ force_co64 输出 ffprobe 仍可读（流数与 codec/尺寸一致）。
#[test]
fn force_co64_output_still_ffprobe_readable() {
    if !tools_ok() {
        eprintln!("跳过：PATH 无 ffmpeg/ffprobe（oracle_mux co64）");
        return;
    }
    let dir = tmp("oracle_mux_co64");
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("src.mp4");
    let out = dir.join("out64.mp4");
    gen_source(&src);
    remux(&src, &out, true);

    let ja = ffprobe_streams_json(&src);
    let jb = ffprobe_streams_json(&out);
    assert_eq!(
        json_fields(&ja, "codec_name"),
        json_fields(&jb, "codec_name")
    );
    assert_eq!(json_fields(&ja, "width"), json_fields(&jb, "width"));
    assert_eq!(json_fields(&ja, "height"), json_fields(&jb, "height"));
    assert_eq!(json_fields(&ja, "time_base"), json_fields(&jb, "time_base"));
    // 自有 reader 也能读（co64 路径）。
    let r = Mp4Reader::open(&out).unwrap();
    assert_eq!(r.streams().len(), 2);
}

/// 挑一个非整除时基（视频轨，如 1/10240）证明只有单位域保证：
/// 7,056,000 % 10240 ≠ 0，tick 域不作断言——由 remux_roundtrip 的
/// tick_strict 分支天然覆盖（44100 整除走 tick 断言、10240 只走单位域）。
#[test]
fn source_video_timescale_is_non_dividing() {
    if !tools_ok() {
        eprintln!("跳过：PATH 无 ffmpeg/ffprobe（oracle_mux 时基分类）");
        return;
    }
    let dir = tmp("oracle_mux_ts");
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("src.mp4");
    gen_source(&src);
    let r = Mp4Reader::open(&src).unwrap();
    for s in r.streams() {
        let divides = TICKS_PER_SECOND % s.timebase.den == 0;
        // 记录分类即可：44100 整除（tick 域组），视频 timescale（ffmpeg
        // 常给 10240/12800）非整除（单位域组）。断言至少一轨非整除，
        // 保证两组断言路径都被真正执行。
        eprintln!(
            "流 {} timescale={} 整除={}",
            s.index, s.timebase.den, divides
        );
    }
    assert!(
        r.streams()
            .iter()
            .any(|s| TICKS_PER_SECOND % s.timebase.den != 0),
        "源文件应含非整除时基轨（视频），否则分组断言形同虚设"
    );
}
