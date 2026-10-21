//! oracle 层 fMP4 封装对拍（模块三）：ffmpeg 造两种形态 fMP4 源
//! （default_base_moof 与显式 base_data_offset，生成命令同模块一
//! oracle_demux，duration=2 -g 12 保证多 moof）→ Mp4Reader 读入 →
//! Mp4Writer(Layout::Fragmented) 重封装 → 对拍。
//!
//! skip 规则同前：PATH 中找不到 ffmpeg/ffprobe 时打印原因并跳过（非 CI
//! 环境的本机验证测试；本机实测 C:\ffmpeg\bin 在 PATH）。临时文件全在
//! `CARGO_TARGET_TMPDIR`。
//!
//! 对拍内容（①④，两种源形态各跑一遍——显式 base_data_offset 源覆盖
//! 读取端两条基值路径）：
//! ① ffprobe -show_streams 字段一致 + -show_packets 逐包 dts/pts/duration
//!    （流时基单位域严格相等；fMP4 无 elist 平移，偏移恒 0 即严格相等）、
//!    size、keyframe（flags=K）；
//! ② roundtrip：自有 reader 源/出两侧逐包 data/dts/pts/duration/keyframe
//!    严格相等（单位域；整除时基另作 tick 域断言，规则同模块二）；
//! ③ ffmpeg -v error -i out.mp4 -f null - 退出码 0（解码冒烟）；
//! ④ 重封装产物 moof ≥ 2（fragment_duration 自动切在 oracle 侧真实生效）。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use ye_container_mp4::{Layout, Mp4Reader, Mp4Writer, Mp4WriterOptions, Packet, TrackConfig};
use ye_time::{TICKS_PER_SECOND, Timestamp};

// —— 工具查找与素材生成（同模块一）——

/// 在 PATH 里手写扫描找可执行文件（Windows 带 .exe，其他平台裸名）。
fn find_tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidates = if cfg!(windows) {
            [dir.join(format!("{name}.exe")), dir.join(name)]
        } else {
            [dir.join(name), dir.join(format!("{name}.exe"))]
        };
        for c in candidates {
            if c.is_file() {
                return Some(c);
            }
        }
    }
    None
}

fn run(cmd: &mut Command, what: &str) -> String {
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("{what} 启动失败：{e}"));
    assert!(
        out.status.success(),
        "{what} 失败：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// 两种形态的 fMP4 源与 ffmpeg/ffprobe 路径（OnceLock 只生成一次）。
struct Fixture {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    fmp4_default: PathBuf,
    fmp4_explicit: PathBuf,
}

fn fixture() -> Option<&'static Fixture> {
    static FX: OnceLock<Option<Fixture>> = OnceLock::new();
    FX.get_or_init(|| {
        let (Some(ffmpeg), Some(ffprobe)) = (find_tool("ffmpeg"), find_tool("ffprobe")) else {
            eprintln!("跳过 oracle_fmp4：PATH 中未找到 ffmpeg/ffprobe");
            return None;
        };
        // CARGO_TARGET_TMPDIR 是编译期注入的环境（运行时进程看不到），用 env! 取。
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("oracle_fmp4");
        std::fs::create_dir_all(&dir).expect("建临时目录失败");
        let fmp4_default = dir.join("fmp4_default.mp4");
        let fmp4_explicit = dir.join("fmp4_explicit.mp4");
        // 命令同模块一 oracle_demux：duration=2 -g 12 ⇒ 24 帧、关键帧每 12
        // 帧 ⇒ ≥2 个 moof。
        for (dst, movflags) in [
            (&fmp4_default, "frag_keyframe+empty_moov+default_base_moof"),
            (&fmp4_explicit, "frag_keyframe+empty_moov"),
        ] {
            run(
                Command::new(&ffmpeg)
                    .args([
                        "-y",
                        "-v",
                        "error",
                        "-f",
                        "lavfi",
                        "-i",
                        "testsrc2=duration=2:rate=12",
                    ])
                    .args(["-c:v", "libx264", "-g", "12", "-movflags", movflags])
                    .arg(dst),
                "ffmpeg 生成 fMP4",
            );
        }
        Some(Fixture {
            ffmpeg,
            ffprobe,
            fmp4_default,
            fmp4_explicit,
        })
    })
    .as_ref()
}

// —— ffprobe compact 输出解析（零第三方依赖，同模块一）——

/// compact 一行 "packet|k=v|k=v" → 键值对（首段是节名，跳过）。
fn parse_compact(line: &str) -> Vec<(String, String)> {
    line.split('|')
        .skip(1)
        .filter_map(|kv| {
            kv.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect()
}

struct FfStream {
    codec_name: String,
    width: u32,
    height: u32,
    time_base: (i64, i64),
}

struct FfPacket {
    pts: Option<i64>,
    dts: Option<i64>,
    duration: Option<i64>,
    size: i64,
    keyframe: bool,
}

fn ff_streams(file: &Path) -> Vec<FfStream> {
    let out = run(
        Command::new(&fixture().unwrap().ffprobe)
            .args(["-v", "error", "-show_streams"])
            .args(["-show_entries", "stream=codec_name,width,height,time_base"])
            .args(["-of", "compact"])
            .arg(file),
        "ffprobe -show_streams",
    );
    out.lines()
        .filter(|l| l.starts_with("stream|"))
        .map(|l| {
            let kv = parse_compact(l);
            let get = |k: &str| kv.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
            let tb = get("time_base").expect("stream 必有 time_base");
            let (num, den) = tb.split_once('/').expect("time_base 形如 1/12288");
            FfStream {
                codec_name: get("codec_name").unwrap(),
                width: get("width").unwrap().parse().unwrap(),
                height: get("height").unwrap().parse().unwrap(),
                time_base: (num.parse().unwrap(), den.parse().unwrap()),
            }
        })
        .collect()
}

fn ff_packets(file: &Path) -> Vec<Vec<FfPacket>> {
    let out = run(
        Command::new(&fixture().unwrap().ffprobe)
            .args(["-v", "error", "-show_packets"])
            .args([
                "-show_entries",
                "packet=stream_index,pts,dts,duration,size,flags",
            ])
            .args(["-of", "compact"])
            .arg(file),
        "ffprobe -show_packets",
    );
    let mut streams: Vec<Vec<FfPacket>> = Vec::new();
    for line in out.lines().filter(|l| l.starts_with("packet|")) {
        let kv = parse_compact(line);
        let get = |k: &str| kv.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
        let si: usize = get("stream_index").unwrap().parse().unwrap();
        while streams.len() <= si {
            streams.push(Vec::new());
        }
        streams[si].push(FfPacket {
            // "N/A" 按缺失处理，跳过该字段对拍。
            pts: get("pts").and_then(|v| v.parse().ok()),
            dts: get("dts").and_then(|v| v.parse().ok()),
            duration: get("duration").and_then(|v| v.parse().ok()),
            size: get("size").unwrap().parse().unwrap(),
            keyframe: get("flags").map(|f| f.contains('K')).unwrap_or(false),
        });
    }
    streams
}

// —— 重封装与对拍 ——

/// 读源 → Mp4Writer(Fragmented, fragment_duration=0.5s 关键帧自动切) 重封装
/// （按 dts 交错写；ffmpeg 源为单视频轨，自动切落在 I 帧上）。
fn remux_frag(src: &Path, dst: &Path) {
    let mut rd = Mp4Reader::open(src).unwrap();
    let mut w = Mp4Writer::create(
        dst,
        Mp4WriterOptions {
            layout: Layout::Fragmented,
            fragment_duration: Some(Timestamp::from_ticks(TICKS_PER_SECOND / 2)),
            ..Default::default()
        },
    )
    .unwrap();
    for s in rd.streams() {
        w.add_stream(TrackConfig::from_stream(s)).unwrap();
    }
    let mut all: Vec<Packet> = Vec::new();
    for si in 0..rd.streams().len() {
        for ii in 0..rd.sample_count(si).unwrap() {
            all.push(rd.read_packet(si, ii).unwrap());
        }
    }
    all.sort_by_key(|p| (p.dts, p.stream_index));
    for p in &all {
        w.write_packet(p).unwrap();
    }
    w.finalize().unwrap();
}

/// ①②④ 对拍主体；返回重封装产物路径（③ 冒烟复用）。
fn remux_and_compare(name: &str, src: &Path) -> PathBuf {
    let out = src.with_file_name(format!("out_{name}.mp4"));
    remux_frag(src, &out);

    let mut so = Mp4Reader::open(src).unwrap();
    let mut rt = Mp4Reader::open(&out).unwrap();

    // ④ 多 fragment：fragment_duration 自动切在真实数据上生效。
    let moof_count = rt
        .box_tree()
        .iter()
        .filter(|n| n.info.kind.as_str() == "moof")
        .count();
    assert!(
        moof_count >= 2,
        "{name}：应产出 ≥2 个 moof，实为 {moof_count}"
    );

    // ① ffprobe -show_streams：源/出字段一致。
    let fs = ff_streams(src);
    let fo = ff_streams(&out);
    assert_eq!(fs.len(), fo.len(), "{name}：流数");
    for (a, b) in fs.iter().zip(&fo) {
        assert_eq!(a.codec_name, b.codec_name, "{name}：codec_name");
        assert_eq!((a.width, a.height), (b.width, b.height), "{name}：尺寸");
        assert_eq!(a.time_base, b.time_base, "{name}：time_base");
    }

    // ② roundtrip：自有 reader 两侧逐包严格对拍。
    assert_eq!(so.streams().len(), rt.streams().len());
    for si in 0..so.streams().len() {
        let s_src = so.stream(si).unwrap();
        let s_rt = rt.stream(si).unwrap();
        assert_eq!(
            s_src.codec.as_str(),
            s_rt.codec.as_str(),
            "{name}：流 {si} codec"
        );
        assert_eq!(s_src.timebase, s_rt.timebase, "{name}：流 {si} 时基");
        assert_eq!(
            s_src.extra_data, s_rt.extra_data,
            "{name}：流 {si} extra_data"
        );
        assert_eq!(s_src.video, s_rt.video, "{name}：流 {si} 视频参数");
        assert_eq!(
            so.sample_count(si).unwrap(),
            rt.sample_count(si).unwrap(),
            "{name}：流 {si} 样例数"
        );
        let tb = s_src.timebase;
        let tick_strict = tb.den != 0 && TICKS_PER_SECOND % tb.den == 0;
        for ii in 0..rt.sample_count(si).unwrap() {
            let po = so.read_packet(si, ii).unwrap();
            let pr = rt.read_packet(si, ii).unwrap();
            assert_eq!(po.data, pr.data, "{name}：流 {si} 样例 {ii} 数据");
            assert_eq!(
                po.dts.to_units(tb),
                pr.dts.to_units(tb),
                "{name}：流 {si} 样例 {ii} dts（单位域）"
            );
            let po_pts = po.pts.unwrap_or(po.dts);
            let pr_pts = pr.pts.unwrap_or(pr.dts);
            assert_eq!(
                po_pts.to_units(tb),
                pr_pts.to_units(tb),
                "{name}：流 {si} 样例 {ii} pts（单位域）"
            );
            assert_eq!(
                po.duration.to_units(tb),
                pr.duration.to_units(tb),
                "{name}：流 {si} 样例 {ii} duration（单位域）"
            );
            assert_eq!(
                po.keyframe, pr.keyframe,
                "{name}：流 {si} 样例 {ii} keyframe"
            );
            if tick_strict {
                assert_eq!(po.dts, pr.dts, "{name}：流 {si} 样例 {ii} dts（tick 域）");
                assert_eq!(po_pts, pr_pts, "{name}：流 {si} 样例 {ii} pts（tick 域）");
                assert_eq!(
                    po.duration, pr.duration,
                    "{name}：流 {si} 样例 {ii} duration（tick 域）"
                );
            }
        }
    }

    // ① ffprobe -show_packets 对拍重封装产物：时戳流单位域严格相等
    // （fMP4 无 elist 平移，ye 与 ffprobe 读同一组 tfdt/trun 值）、size、K。
    let ff = ff_packets(&out);
    assert_eq!(ff.len(), rt.streams().len(), "{name}：有包流数");
    for (si, ff_pk) in ff.iter().enumerate() {
        let tb = rt.stream(si).unwrap().timebase;
        let count = rt.sample_count(si).unwrap();
        assert_eq!(count, ff_pk.len(), "{name}：流 {si} 包数");
        for (i, f) in ff_pk.iter().enumerate() {
            let p = rt.read_packet(si, i).unwrap();
            assert_eq!(p.data.len() as i64, f.size, "{name}：流 {si} 包 {i} 大小");
            if let Some(fd) = f.dts {
                assert_eq!(
                    p.dts.to_units(tb).unwrap(),
                    fd,
                    "{name}：流 {si} 包 {i} dts（单位域严格相等）"
                );
            }
            if let (Some(fp), true) = (f.pts, f.dts.is_some()) {
                let ypts = p
                    .pts
                    .map(|v| v.to_units(tb).unwrap())
                    .unwrap_or_else(|| p.dts.to_units(tb).unwrap());
                assert_eq!(ypts, fp, "{name}：流 {si} 包 {i} pts（单位域严格相等）");
            }
            if let Some(fdur) = f.duration {
                assert_eq!(
                    p.duration.to_units(tb).unwrap(),
                    fdur,
                    "{name}：流 {si} 包 {i} 时长"
                );
            }
            assert_eq!(
                p.keyframe, f.keyframe,
                "{name}：流 {si} 包 {i} 关键帧（flags=K）"
            );
        }
    }
    out
}

/// ③ 解码冒烟：-v error 下退出码 0 且无错误输出。
fn decode_smoke(name: &str, out: &Path) {
    let _ = name;
    let smoke = Command::new(&fixture().unwrap().ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(out)
        .args(["-f", "null", "-"])
        .output()
        .unwrap();
    assert!(
        smoke.status.success(),
        "{name}：ffmpeg 解码冒烟失败：{}",
        String::from_utf8_lossy(&smoke.stderr)
    );
    assert!(
        smoke.stderr.is_empty(),
        "{name}：解码冒烟应无错误输出：{}",
        String::from_utf8_lossy(&smoke.stderr)
    );
}

// —— 测试入口 ——

/// ①②③④：default_base_moof 源。
#[test]
fn oracle_fmp4_default_base_remux() {
    let Some(fx) = fixture() else { return };
    let out = remux_and_compare("default", &fx.fmp4_default);
    decode_smoke("default", &out);
}

/// ④（任务书）：显式 base_data_offset 源同样过 ①②（读取端两条基值路径）。
#[test]
fn oracle_fmp4_explicit_base_remux() {
    let Some(fx) = fixture() else { return };
    let out = remux_and_compare("explicit", &fx.fmp4_explicit);
    decode_smoke("explicit", &out);
}
