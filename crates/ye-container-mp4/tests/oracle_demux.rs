//! ffmpeg/ffprobe oracle 对拍：PATH 中找不到 ffmpeg/ffprobe 时打印原因并跳过
//! （非 CI 环境的本机验证测试；本机实测 C:\ffmpeg\bin 在 PATH）。
//!
//! 对拍内容：
//! ① ffprobe -show_streams 的 codec/尺寸/声道采样率/time_base 与 ye streams()；
//! ② ffprobe -show_packets 逐包 dts/pts/duration/size 与 ye read_packet——
//!    时戳即流时基原生单位（ticks 直读，无换算无舍入）：dts/pts 允许整轨
//!    常数偏移（ffmpeg 按 elist 平移时间轴、ye 读原始表，实测 prog.mp4
//!    视频偏 2 帧、音频偏 1024 样本），但偏移必须恒定且 pts-dts 差逐包
//!    严格相等；duration 严格相等；
//! ③ keyframe 对拍 ffprobe flags 字段的 K；
//! ④ data 字节数=size、读全包不越界（逐包 read_packet 本身即验证）。
//!
//! 注：m1 版⑤「ye probe 冒烟」（构建并运行 ye-cli 真二进制）随合流移出
//! 本组——ye-cli 为 m1 版、develop 上暂不编译（超出本 crate 移植范围），
//! 待其按新契约重写后回归（见 docs/contracts.md 移植裁决节）。
//!
//! 素材由 ffmpeg lavfi 生成到 CARGO_TARGET_TMPDIR（命令固定，保证 ≥2 个
//! moof——实测 duration=1 -g 10 只产单 moof）：
//! - fmp4_default.mp4：testsrc2=duration=2:rate=12 + libx264 -g 12 +
//!   frag_keyframe+empty_moov+default_base_moof；
//! - fmp4_explicit.mp4：同上但去掉 default_base_moof（tfhd 显式 base_data_offset）；
//! - progressive_av.mp4：加 sine 音轨的普通渐进式 MP4。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use ye_container_mp4::{CodecId, MediaType, Mp4Reader, Rational};

// —— 工具查找与素材生成 ——

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

/// 三个素材文件与 ffprobe 路径（OnceLock 保证并行测试下只生成一次）。
struct Fixture {
    ffprobe: PathBuf,
    fmp4_default: PathBuf,
    fmp4_explicit: PathBuf,
    progressive: PathBuf,
}

fn fixture() -> Option<&'static Fixture> {
    static FX: OnceLock<Option<Fixture>> = OnceLock::new();
    FX.get_or_init(|| {
        let (Some(ffmpeg), Some(ffprobe)) = (find_tool("ffmpeg"), find_tool("ffprobe")) else {
            eprintln!("跳过 oracle_demux：PATH 中未找到 ffmpeg/ffprobe");
            return None;
        };
        // CARGO_TARGET_TMPDIR 是编译期注入的环境（运行时进程看不到），用 env! 取。
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
        let dir = dir.join("oracle_demux");
        std::fs::create_dir_all(&dir).expect("建临时目录失败");

        let fmp4_default = dir.join("fmp4_default.mp4");
        let fmp4_explicit = dir.join("fmp4_explicit.mp4");
        let progressive = dir.join("progressive_av.mp4");
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
                .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=2"])
                .args(["-c:v", "libx264", "-g", "12", "-c:a", "aac", "-shortest"])
                .arg(&progressive),
            "ffmpeg 生成渐进式 A/V",
        );
        Some(Fixture {
            ffprobe,
            fmp4_default,
            fmp4_explicit,
            progressive,
        })
    })
    .as_ref()
}

// —— ffprobe compact 输出解析（零第三方依赖）——

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
    index: usize,
    codec_name: String,
    codec_type: String,
    width: Option<u32>,
    height: Option<u32>,
    channels: Option<u32>,
    sample_rate: Option<u32>,
    time_base: (i32, i32),
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
            .args([
                "-show_entries",
                "stream=index,codec_name,codec_type,width,height,channels,sample_rate,time_base",
            ])
            .args(["-of", "compact"])
            .arg(file),
        "ffprobe -show_streams",
    );
    out.lines()
        .filter(|l| l.starts_with("stream|"))
        .map(|l| {
            let kv = parse_compact(l);
            let get = |k: &str| kv.iter().find(|(a, _)| a == k).map(|(_, v)| v.clone());
            let get_i64 = |k: &str| get(k).and_then(|v| v.parse::<i64>().ok());
            let tb = get("time_base").expect("stream 必有 time_base");
            let (num, den) = tb.split_once('/').expect("time_base 形如 1/12288");
            FfStream {
                index: get_i64("index").expect("index") as usize,
                codec_name: get("codec_name").unwrap(),
                codec_type: get("codec_type").unwrap(),
                width: get_i64("width").map(|v| v as u32),
                height: get_i64("height").map(|v| v as u32),
                channels: get_i64("channels").map(|v| v as u32),
                sample_rate: get_i64("sample_rate").map(|v| v as u32),
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
            // "N/A"（如个别容器缺 dts）按缺失处理，跳过该字段对拍。
            pts: get("pts").and_then(|v| v.parse().ok()),
            dts: get("dts").and_then(|v| v.parse().ok()),
            duration: get("duration").and_then(|v| v.parse().ok()),
            size: get("size").unwrap().parse().unwrap(),
            keyframe: get("flags").map(|f| f.contains('K')).unwrap_or(false),
        });
    }
    streams
}

/// ffprobe codec_name → [`CodecId`]（本测试生成的素材只含 h264/aac）。
fn codec_id_of(codec: &str) -> CodecId {
    match codec {
        "h264" => CodecId::H264,
        "aac" => CodecId::Aac,
        other => panic!("未映射的 codec_name={other}（请在测试里补映射）"),
    }
}

// —— 对拍断言 ——

/// ① 流元数据对拍。
fn compare_streams(name: &str, file: &Path) -> Vec<Rational> {
    let r = Mp4Reader::open(file).unwrap();
    let ff = ff_streams(file);
    assert_eq!(r.streams().len(), ff.len(), "{name}：流数");
    ff.iter().zip(r.streams()).for_each(|(f, s)| {
        assert_eq!(f.index, s.index as usize, "{name}：流序号");
        assert_eq!(s.codec.codec, codec_id_of(&f.codec_name), "{name}：codec");
        match f.codec_type.as_str() {
            "video" => {
                assert_eq!(s.media_type, MediaType::Video, "{name}：kind");
                let v = s.codec.video.as_ref().unwrap();
                assert_eq!(
                    (v.width, v.height),
                    (f.width.unwrap(), f.height.unwrap()),
                    "{name}：尺寸"
                );
            }
            "audio" => {
                assert_eq!(s.media_type, MediaType::Audio, "{name}：kind");
                let a = s.codec.audio.as_ref().unwrap();
                assert_eq!(
                    a.channel_layout.map(|l| l.channels()).unwrap_or(0),
                    f.channels.unwrap(),
                    "{name}：声道"
                );
                assert_eq!(a.sample_rate, f.sample_rate.unwrap(), "{name}：采样率");
            }
            other => panic!("{name}：未预期的 codec_type={other}"),
        }
        // time_base "1/12288" 形式与 ye 的 1/mdhd:timescale 应严格相等。
        assert_eq!(
            (s.time_base.num(), s.time_base.den()),
            f.time_base,
            "{name}：流 {} 时基",
            s.index
        );
    });
    r.streams().iter().map(|s| s.time_base).collect()
}

/// ②③④ 逐包对拍。`zero_dts_offset`：该文件是否要求 dts 常数偏移为 0
/// （fMP4 无 elist、实测偏移为 0；渐进式文件 ffmpeg 写 elist 平移时间轴——
/// 视频 B 帧延迟偏 2 帧、AAC priming 偏 1024 样本——只断言偏移恒定）。
fn compare_packets(name: &str, file: &Path, tbs: &[Rational], zero_dts_offset: bool) {
    let mut r = Mp4Reader::open(file).unwrap();
    let ff = ff_packets(file);
    assert_eq!(ff.len(), tbs.len(), "{name}：有包流数");
    for (si, ff_pk) in ff.iter().enumerate() {
        let tb = tbs[si];
        let count = r.sample_count(si).unwrap();
        assert_eq!(count, ff_pk.len(), "{name}：流 {si} 包数");
        let mut dts_off: Option<i64> = None;
        let mut pts_off: Option<i64> = None;
        for (i, f) in ff_pk.iter().enumerate() {
            let p = r.read_packet(si, i).unwrap();
            // 时戳已按流时基打戳：ticks 即轨道单位，直读对拍。
            let ydts = p.dts.unwrap().ticks();
            assert_eq!(p.dts.unwrap().time_base(), tb, "{name}：流 {si} 打戳时基");
            if let Some(fd) = f.dts {
                let off = ydts - fd;
                if let Some(prev) = dts_off.replace(off) {
                    assert_eq!(
                        prev, off,
                        "{name}：流 {si} 包 {i} dts 偏移须恒定（ye={ydts}, ff={fd}）"
                    );
                }
            }
            let ypts = p.pts.unwrap().ticks();
            if let (Some(fp), Some(fd)) = (f.pts, f.dts) {
                let off = ypts - fp;
                if let Some(prev) = pts_off.replace(off) {
                    assert_eq!(prev, off, "{name}：流 {si} 包 {i} pts 偏移须恒定");
                }
                // pts-dts 差（B 帧重排）不受 elist 平移影响，逐包严格相等。
                assert_eq!(ypts - ydts, fp - fd, "{name}：流 {si} 包 {i} pts-dts");
            }
            if let Some(fdur) = f.duration {
                assert_eq!(
                    p.duration.unwrap().ticks(),
                    fdur,
                    "{name}：流 {si} 包 {i} 时长"
                );
            }
            // ④ data 字节数 = ffprobe size（读全包不越界由 read_packet 保证）。
            assert_eq!(p.data.len() as i64, f.size, "{name}：流 {si} 包 {i} 大小");
            let key = p.flags.contains(ye_container_mp4::PacketFlags::KEYFRAME);
            assert_eq!(key, f.keyframe, "{name}：流 {si} 包 {i} 关键帧");
        }
        if let Some(off) = dts_off {
            assert!(
                !zero_dts_offset || off == 0,
                "{name}：流 {si} dts 偏移应为 0，实为 {off}"
            );
        }
    }
}

fn demux_and_compare(name: &str, file: &Path, zero_dts_offset: bool) {
    let tbs = compare_streams(name, file);
    compare_packets(name, file, &tbs, zero_dts_offset);
}

// —— 测试入口 ——

#[test]
fn oracle_fmp4_default_base_moof() {
    let Some(fx) = fixture() else { return };
    // 实测形态：tfdt v1、tfhd 0x020038、trun 0x00000a05、2 个 moof、顶层 mfra。
    demux_and_compare("fmp4_default", &fx.fmp4_default, true);
}

#[test]
fn oracle_fmp4_explicit_base() {
    let Some(fx) = fixture() else { return };
    demux_and_compare("fmp4_explicit", &fx.fmp4_explicit, true);
}

#[test]
fn oracle_progressive_with_audio() {
    let Some(fx) = fixture() else { return };
    demux_and_compare("progressive_av", &fx.progressive, false);
}
