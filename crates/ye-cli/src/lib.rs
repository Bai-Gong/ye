//! `ye` 命令行的可测试主体：参数解析、`probe` 报告、`remux` 直通与
//! `convert` 转码。
//!
//! 对标 ffprobe 的最小子集（探查素材信息）、`ffmpeg -c copy` 的最小
//! 子集（重封装直通）；`convert`（M3 集成阶段落地）走门面
//! [`ye::convert::convert`]：视频链 解码 → 处理链（缩放/像素格式）→
//! H.264 编码 → MP4 封装，音频包直通（`ffmpeg -c:v libx264 -c:a copy`
//! 的对位）。参数解析手写，不引第三方库——命令面还很小，选项成体系
//! 后再评估 clap。
//!
//! probe/remux 全部走 [`ContainerRegistry`]（显式构造、注册 MP4 工厂，
//! 输入 auto-probe ftyp/box 嗅探），不再直连具体容器类型——M2 接线后
//! 的命令面与未来新容器（MOV/MKV…）天然解耦。
//!
//! 二进制入口在 `src/main.rs`，只做 argv 分发与输出；
//! 逻辑放本 lib 供 `tests/` 集成测试直接调用。

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use ye::codec::CodecRegistry;
use ye::container::mp4::register_mp4;
use ye::container::{ContainerRegistry, MediaHeader};

/// 用法说明（`--help` 与用法错误共用，输出方负责加换行）。
pub const USAGE: &str = "\
ye — 百工·冶命令行（媒体探查 / 转码）

用法:
  ye probe <FILE>        探查媒体文件：每路流的编解码、宽高/采样率、时基、时长、帧率、码率
  ye remux <IN> <OUT>    重封装直通（解封装→封装，不转码；输入格式自动探测，输出 MP4）
  ye convert <IN> <OUT> [选项]
                         转码输出 MP4：视频 H.264 Baseline 重编码（--size 320x240 缩放、
                         --vb 800 视频码率封顶 kbps、--no-video 剥视频），
                         音频 --audio copy 直通 / --audio aac:128 AAC-LC 重编码（kbps）
  ye --help              显示本帮助
  ye --version           显示版本

当前支持: MP4 解封装与封装（progressive 与 fMP4 读写），probe/remux/convert 可用。
convert 的参数形态即锻（duan）的进程内接入契约（进度回调 + AtomicBool 取消）。";

/// `convert` 的转码选项（解析层定形，`--vb`/`--audio`/`--no-video`）。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq, Default`（纯值载荷）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConvertOptions {
    /// 目标尺寸 `--size 宽x高`（`None` = 保持原尺寸）。
    pub size: Option<(u32, u32)>,
    /// 视频码率封顶 `--vb kbps`（`None` = 纯质量模式简单 CRF 23；
    /// `Some` = A-VBR：CRF 23 基准 + 目标码率封顶）。
    pub video_bitrate_kbps: Option<u32>,
    /// 音频设置 `--audio`（缺省直通）。
    pub audio: ConvertAudio,
    /// `--no-video`：剥视频（纯音频输出）。
    pub no_video: bool,
}

/// `--audio` 的值域（copy / aac:kbps）。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq, Default`（纯值载荷）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConvertAudio {
    /// 直通（重封装不重编码）。
    #[default]
    Copy,
    /// AAC-LC 重编码（目标码率 kbps）。
    Aac {
        /// 目标码率（kbps）。
        kbps: u32,
    },
}

/// 解析后的子命令。
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// `ye probe <FILE>`。
    Probe { path: PathBuf },
    /// `ye remux <IN> <OUT>`。
    Remux { input: PathBuf, output: PathBuf },
    /// `ye convert <IN> <OUT> [选项]`。
    Convert {
        input: PathBuf,
        output: PathBuf,
        /// 转码选项（尺寸/码率/音频/剥视频）。
        options: ConvertOptions,
    },
    /// `ye --help` / `ye -h`。
    Help,
    /// `ye --version` / `ye -V`。
    Version,
}

/// 解析 argv（不含程序名）。
///
/// Err 载荷为已可直接打印的用户向错误信息（含用法提示）；
/// 调用方约定以退出码 2 表示用法错误。
pub fn parse_args(args: &[OsString]) -> Result<Command, String> {
    let Some(first) = args.first() else {
        return Err(format!("缺少子命令。\n\n{USAGE}"));
    };
    match first.to_str() {
        Some("probe") => parse_probe(&args[1..]),
        Some("remux") => parse_two_paths("remux", &args[1..])
            .map(|(input, output)| Command::Remux { input, output }),
        Some("convert") => parse_convert(&args[1..]),
        Some("--help") | Some("-h") => Ok(Command::Help),
        Some("--version") | Some("-V") => Ok(Command::Version),
        other => Err(format!(
            "未知命令 {:?}。\n\n{USAGE}",
            other
                .map(str::to_owned)
                .unwrap_or_else(|| first.to_string_lossy().into_owned())
        )),
    }
}

fn parse_probe(rest: &[OsString]) -> Result<Command, String> {
    match rest {
        [path] => Ok(Command::Probe {
            path: PathBuf::from(path),
        }),
        [] => Err(format!("`probe` 需要一个文件参数。\n\n{USAGE}")),
        _ => Err(format!("`probe` 只接受一个文件参数。\n\n{USAGE}")),
    }
}

/// `<IN> <OUT>` 双路径子命令的共用解析。
fn parse_two_paths(name: &str, rest: &[OsString]) -> Result<(PathBuf, PathBuf), String> {
    match rest {
        [input, output] => Ok((PathBuf::from(input), PathBuf::from(output))),
        [] => Err(format!("`{name}` 需要输入与输出两个文件参数。\n\n{USAGE}")),
        [_only] => Err(format!(
            "`{name}` 需要输入与输出两个文件参数（缺输出）。\n\n{USAGE}"
        )),
        _ => Err(format!(
            "`{name}` 只接受输入与输出两个文件参数。\n\n{USAGE}"
        )),
    }
}

/// `ye convert <IN> <OUT> [选项]`：两个位置路径 + 可选
/// `--size`/`--vb`/`--audio`/`--no-video`。
fn parse_convert(rest: &[OsString]) -> Result<Command, String> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut options = ConvertOptions::default();
    let mut audio_seen = false;
    let mut no_video_seen = false;
    let mut i = 0usize;
    while i < rest.len() {
        if rest[i].to_str() == Some("--size") {
            let Some(value) = rest.get(i + 1) else {
                return Err(format!(
                    "`--size` 需要一个 `宽x高` 值（如 320x240）。\n\n{USAGE}"
                ));
            };
            if options.size.is_some() {
                return Err(format!("`--size` 只能出现一次。\n\n{USAGE}"));
            }
            options.size = Some(parse_size(value)?);
            i += 2;
            continue;
        }
        if rest[i].to_str() == Some("--vb") {
            let Some(value) = rest.get(i + 1) else {
                return Err(format!("`--vb` 需要一个 kbps 数值（如 800）。\n\n{USAGE}"));
            };
            if options.video_bitrate_kbps.is_some() {
                return Err(format!("`--vb` 只能出现一次。\n\n{USAGE}"));
            }
            options.video_bitrate_kbps = Some(parse_kbps(&value.to_string_lossy(), "--vb")?);
            i += 2;
            continue;
        }
        if rest[i].to_str() == Some("--audio") {
            let Some(value) = rest.get(i + 1) else {
                return Err(format!(
                    "`--audio` 需要一个值：copy 或 aac:kbps（如 aac:128）。\n\n{USAGE}"
                ));
            };
            if audio_seen {
                return Err(format!("`--audio` 只能出现一次。\n\n{USAGE}"));
            }
            options.audio = parse_audio(value)?;
            audio_seen = true;
            i += 2;
            continue;
        }
        if rest[i].to_str() == Some("--no-video") {
            if no_video_seen {
                return Err(format!("`--no-video` 只能出现一次。\n\n{USAGE}"));
            }
            options.no_video = true;
            no_video_seen = true;
            i += 1;
            continue;
        }
        if paths.len() == 2 {
            return Err(format!(
                "`convert` 只接受输入与输出两个文件参数。\n\n{USAGE}"
            ));
        }
        paths.push(PathBuf::from(&rest[i]));
        i += 1;
    }
    match paths.len() {
        2 => Ok(Command::Convert {
            input: paths[0].clone(),
            output: paths[1].clone(),
            options,
        }),
        0 => Err(format!("`convert` 需要输入与输出两个文件参数。\n\n{USAGE}")),
        _ => Err(format!(
            "`convert` 需要输入与输出两个文件参数（缺输出）。\n\n{USAGE}"
        )),
    }
}

/// `--size` 值解析：`宽x高`（十进制、非零偶数——yuv420p 平面几何与
/// H.264 编码器的共同要求）。
fn parse_size(value: &OsString) -> Result<(u32, u32), String> {
    let text = value.to_string_lossy();
    let bad = || {
        format!(
            "`--size` 的值须为 `宽x高`（十进制、非零偶数，如 320x240），收到 {text}。\n\n{USAGE}"
        )
    };
    let (w, h) = text.split_once('x').ok_or_else(bad)?;
    let (w, h) = (
        w.parse::<u32>().map_err(|_| bad())?,
        h.parse::<u32>().map_err(|_| bad())?,
    );
    if w == 0 || h == 0 || w % 2 != 0 || h % 2 != 0 {
        return Err(bad());
    }
    Ok((w, h))
}

/// `--vb`/`--audio aac:` 的 kbps 值解析（十进制、1..=4_000_000）。
fn parse_kbps(text: &str, flag: &str) -> Result<u32, String> {
    let bad =
        |range: String| format!("`{flag}` 的值须为 kbps 数值（{range}），收到 {text}。\n\n{USAGE}");
    let Ok(kbps) = text.parse::<u32>() else {
        return Err(bad("十进制".into()));
    };
    if kbps == 0 || kbps > 4_000_000 {
        return Err(bad("1..=4000000".into()));
    }
    Ok(kbps)
}

/// `--audio` 值解析：`copy` 直通 / `aac:kbps` 重编码。
fn parse_audio(value: &OsString) -> Result<ConvertAudio, String> {
    let text = value.to_string_lossy();
    if text == "copy" {
        return Ok(ConvertAudio::Copy);
    }
    if let Some(kbps) = text.strip_prefix("aac:") {
        return Ok(ConvertAudio::Aac {
            kbps: parse_kbps(kbps, "--audio aac:")?,
        });
    }
    Err(format!(
        "`--audio` 的值须为 copy 或 aac:kbps（如 aac:128），收到 {text}。\n\n{USAGE}"
    ))
}

/// 命令行自用的容器注册表：MP4 demuxer/muxer 已注册（显式构造、
/// 显式传递——注册表无全局单例，见 ye-container 契约）。
pub fn registry() -> ContainerRegistry {
    let mut reg = ContainerRegistry::new();
    register_mp4(&mut reg);
    reg
}

/// 命令行自用的编解码注册表：H.264 解码/编码 + AAC 解码/编码真工厂
/// 已注册（直通路径用不到 AAC，`--audio aac` 重编码需要；显式构造、
/// 显式传递）。
pub fn codec_registry() -> CodecRegistry {
    let mut reg = CodecRegistry::new();
    ye::codec_h264::register_h264(&mut reg);
    ye::codec_h264::register_h264_encoder(&mut reg);
    ye::codec_aac::register_aac(&mut reg);
    ye::codec_aac::register_aac_encoder(&mut reg);
    reg
}

// —— probe ——

/// 探查 `path` 指向的媒体文件，返回完整报告文本（含末尾换行）。
///
/// 输入经注册表 auto-probe（ftyp/box 嗅探）识别格式。报告在头信息
/// （流表）之外补两列：
/// - **帧率**（仅视频）：`样例数 ÷ 时长` 近似——恒帧率（CFR）文件
///   准确，可变帧率（VFR）文件有偏差（逐帧时长不均时平均帧率不等于
///   瞬时帧率），报告里注明；
/// - **码率**：`样例字节总和 ÷ 时长`——为拿到样例字节总和，探查会
///   把解封装排空一遍（读全文件一次；M2 取精确值优先于快）。
///
/// Err 载荷为已含路径前缀的用户向错误信息。
pub fn probe(path: &Path) -> Result<String, String> {
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let (format, mut demuxer) = registry()
        .open_demuxer(Box::new(BufReader::new(file)))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let header = demuxer
        .read_header()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    // 码率的样例字节总和：排空解封装逐包累加（每路 + 总量）。
    let mut stream_bytes = vec![0u64; header.streams.len()];
    while let Some(pkt) = demuxer
        .read_packet()
        .map_err(|e| format!("{}: {e}", path.display()))?
    {
        let si = usize::try_from(pkt.stream_index).unwrap_or(usize::MAX);
        if si < stream_bytes.len() {
            stream_bytes[si] += pkt.data.len() as u64;
        }
    }
    Ok(report(&header, &format, &stream_bytes, path))
}

/// 由头信息与每路样例字节数生成报告。
fn report(header: &MediaHeader, format: &str, stream_bytes: &[u64], path: &Path) -> String {
    let mut out = format!(
        "{}: {}, {} 路流\n",
        path.display(),
        format,
        header.streams.len()
    );
    for s in &header.streams {
        let secs = s.duration.map(|d| d.to_seconds());
        let count = s
            .nb_frames
            .map(|n| n.to_string())
            .unwrap_or_else(|| "未知".to_string());
        out.push_str(&format!(
            "流 #{} [{}] codec={} 时基={}/{} 时长={} 样例数={}",
            s.index,
            kind_label(s.media_type),
            codec_label(s.codec.codec),
            s.time_base.num(),
            s.time_base.den(),
            duration_label(s.duration),
            count,
        ));
        // 帧率（仅视频）：样例数÷时长近似，VFR 有偏差。
        if s.media_type == ye::container::mp4::MediaType::Video
            && let Some(fps) = approx_fps(s.nb_frames, secs)
        {
            out.push_str(&format!(
                " 帧率≈{fps:.3}（样例数÷时长近似，VFR 文件有偏差）"
            ));
        }
        // 媒体参数。
        match (&s.codec.video, &s.codec.audio) {
            (Some(v), _) => out.push_str(&format!(" 尺寸={}x{}", v.width, v.height)),
            (None, Some(a)) => {
                let ch = a
                    .channel_layout
                    .map(|l| l.channels().to_string())
                    .unwrap_or_else(|| "未知".to_string());
                out.push_str(&format!(" 声道={ch} 采样率={}Hz", a.sample_rate));
            }
            (None, None) => {}
        }
        // 码率：样例字节总和×8 ÷ 时长。
        if let Some(bps) = approx_bit_rate(stream_bytes.get(s.index as usize).copied(), secs) {
            out.push_str(&format!(" 码率={bps}bps"));
        }
        out.push('\n');
    }
    out
}

/// 时长字段：`x.xxxs (n tick)`；缺失 → `未知`。tick 即流时基的
/// 原生单位数（精确值以它为准，秒数只是显示值）。
fn duration_label(d: Option<ye::time::Timestamp>) -> String {
    match d {
        Some(d) => format!("{:.3}s ({} tick)", d.to_seconds(), d.ticks()),
        None => "未知".to_string(),
    }
}

/// 帧率近似：样例数 ÷ 秒；时长缺失/为 0 → None。
/// 仅显示用（f64），见 ye-time 契约。
fn approx_fps(nb_frames: Option<u64>, secs: Option<f64>) -> Option<f64> {
    let n = nb_frames?;
    let s = secs?;
    (s > 0.0).then_some(n as f64 / s)
}

/// 码率：样例字节总和 × 8 ÷ 秒；字节数/时长缺失或时长为 0 → None。
fn approx_bit_rate(bytes: Option<u64>, secs: Option<f64>) -> Option<u64> {
    let b = bytes?;
    let s = secs?;
    (s > 0.0).then_some((b as f64 * 8.0 / s).round() as u64)
}

fn kind_label(kind: ye::container::mp4::MediaType) -> &'static str {
    match kind {
        ye::container::mp4::MediaType::Video => "视频",
        ye::container::mp4::MediaType::Audio => "音频",
        ye::container::mp4::MediaType::Subtitle => "字幕",
    }
}

/// codec 标签：`CodecId` 的 Debug 形式转小写（对齐 ffprobe 命名，
/// 如 `h264`/`aac`；Unknown → `unknown`）。
fn codec_label(codec: ye::container::mp4::CodecId) -> String {
    format!("{codec:?}").to_lowercase()
}

// —— remux ——

/// `ye remux <IN> <OUT>`：重封装直通（不转码，对标 `ffmpeg -c copy`）。
///
/// 归并循环上提到门面助手 [`ye::container::remux`]（见其文档）：
/// 输入 auto-probe 打开 → 每轨平移到首包 dts=0 → 逐包直通 MP4
/// （progressive）。包数据、关键帧、时长与 pts−dts 偏移原样透传
/// （H.264 保持 AVCC，不转 Annex B）。
///
/// Err 载荷为已含路径前缀的用户向错误信息。
pub fn remux(input: &Path, output: &Path) -> Result<String, String> {
    let in_file = File::open(input).map_err(|e| format!("{}: {e}", input.display()))?;
    let out_file = File::create(output).map_err(|e| format!("{}: {e}", output.display()))?;
    let stats = ye::container::remux(
        &registry(),
        Box::new(BufReader::new(in_file)),
        "mp4",
        Box::new(BufWriter::new(out_file)),
    )
    .map_err(|e| format!("{} → {}: {e}", input.display(), output.display()))?;
    Ok(format!(
        "{} → {}: MP4, {} 路流, {} 样例直通（不转码）\n",
        input.display(),
        output.display(),
        stats.streams,
        stats.packets
    ))
}

// —— convert（M3 集成：视频重编码 + 音频直通）——

/// 转码进度回调的参数形态（锻 D2 接入约定）：
/// `(processed_us, total_us)` —— 已处理/总时长，微秒（参数名以文档
/// 说明为准，`Fn(i64, i64)` 位置参数：第一个已处理、第二个总时长）。
///
/// 与锻现有 `transcode-progress` 事件负载 `{jobId, timeUs, totalUs}`
/// 同源（timeUs/totalUs 直传）；jobId 路由与 ≥100ms 节流归锻侧
/// （Tauri 事件层），ye 只提供进程内的每任务回调。
pub type ConvertProgress<'a> = &'a dyn Fn(i64, i64);

/// `ye convert <IN> <OUT> [选项]`：视频重编码为 H.264（可选缩放/码率
/// 封顶/剥视频），音频直通或 AAC-LC 重编码（`--audio`），输出 MP4。
///
/// 接线门面 [`ye::convert::convert`]（锻接入契约的命令行形态）：
/// - `options.size`：`--size` 目标尺寸（`None` = 保持原尺寸）；
/// - `options.video_bitrate_kbps`：`--vb` 视频码率封顶（`None` = 纯
///   质量模式简单 CRF 23）；
/// - `options.audio`：`--audio copy|aac:kbps`；
/// - `options.no_video`：`--no-video` 剥视频（纯音频输出）；
/// - `progress`：进度回调（逐输出包上报，`processed_us` 单调不减）；
/// - `cancel`：取消旗标 `&AtomicBool`——置位即请求取消，泵每包检查、
///   及时退出。
///
/// 失败即清理半成品（删除已创建的输出文件）——泵持有 `WriteSeek`
/// 抽象、无路径概念，删除归本层。
pub fn convert(
    input: &Path,
    output: &Path,
    options: &ConvertOptions,
    progress: ConvertProgress<'_>,
    cancel: &AtomicBool,
) -> Result<String, String> {
    let ctx = || format!("{} → {}", input.display(), output.display());
    let result = run_convert(input, output, options, progress, cancel);
    if result.is_err() {
        // 半成品清理：泵已随错误丢弃输出 writer（文件句柄关闭），
        // 这里删除残留文件（不存在时忽略）。
        let _ = std::fs::remove_file(output);
    }
    result
        .map(|stats| {
            let video_desc = match (options.no_video, options.size) {
                (true, _) => "，视频剥除".to_string(),
                (false, Some((w, h))) => {
                    format!("（H.264 {w}x{h}{}）", vb_desc(options))
                }
                (false, None) => format!("（H.264{}）", vb_desc(options)),
            };
            let audio_desc = match options.audio {
                ConvertAudio::Copy => "直通".to_string(),
                ConvertAudio::Aac { kbps } => {
                    format!("AAC-LC 重编码（目标 {kbps}kbps）")
                }
            };
            format!(
                "{} → {}: MP4, {} 路流, 视频 {} 帧重编码{}, 音频 {} 包{}\n",
                input.display(),
                output.display(),
                stats.streams,
                stats.video_frames,
                video_desc,
                stats.audio_frames,
                audio_desc
            )
        })
        .map_err(|e| format!("{}: {e}", ctx()))
}

/// `--vb` 的报告后缀（空 = 纯质量模式）。
fn vb_desc(options: &ConvertOptions) -> String {
    match options.video_bitrate_kbps {
        Some(kbps) => format!("，码率封顶 {kbps}kbps"),
        None => String::new(),
    }
}

/// `convert` 的主体（打开文件 → 门面泵 → 统计），错误带路径上下文
/// 由外层统一包装。
fn run_convert(
    input: &Path,
    output: &Path,
    options: &ConvertOptions,
    progress: ConvertProgress<'_>,
    cancel: &AtomicBool,
) -> Result<ye::convert::ConvertStats, String> {
    let in_file = File::open(input).map_err(|e| format!("{}: {e}", input.display()))?;
    let out_file = File::create(output).map_err(|e| format!("{}: {e}", output.display()))?;
    let settings = ye::convert::VideoEncodeSettings {
        chain: ye::convert::VideoChain {
            format: None,
            size: options.size,
        },
        bit_rate: options
            .video_bitrate_kbps
            .map(|kbps| u64::from(kbps) * 1_000),
        ..Default::default()
    };
    let video = if options.no_video {
        None
    } else {
        Some(&settings)
    };
    let audio = match options.audio {
        ConvertAudio::Copy => ye::convert::AudioEncodeSettings::Copy,
        ConvertAudio::Aac { kbps } => ye::convert::AudioEncodeSettings::Aac {
            bitrate_bps: kbps.saturating_mul(1_000),
        },
    };
    ye::convert::convert(
        &registry(),
        &codec_registry(),
        Box::new(BufReader::new(in_file)),
        "mp4",
        Box::new(BufWriter::new(out_file)),
        video,
        &audio,
        ye::convert::filter_ops(),
        progress,
        cancel,
    )
    .map_err(|e| e.to_string())
}
