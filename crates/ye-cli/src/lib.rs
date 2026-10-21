//! `ye` 命令行的可测试主体：参数解析、`probe` 报告与 `remux` 直通。
//!
//! 对标 ffprobe 的最小子集（M1 验收演示：能列出素材信息）与
//! `ffmpeg -c copy` 的最小子集（M1 收尾：MP4 重封装直通）。
//! 参数解析手写，不引第三方库——命令面还很小，等 M3 `convert`
//! 带来成体系的选项后再评估 clap。
//!
//! 二进制入口在 `src/main.rs`，只做 argv 分发与输出；
//! 逻辑放本 lib 供 `tests/` 集成测试直接调用。

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

use ye::container::mp4::{
    Layout, MediaKind, Mp4Reader, Mp4Writer, Mp4WriterOptions, Packet, TrackConfig,
};
use ye::time::{TICKS_PER_SECOND, Timestamp};

/// 用法说明（`--help` 与用法错误共用，输出方负责加换行）。
pub const USAGE: &str = "\
ye — 百工·冶命令行（媒体探查 / 转码）

用法:
  ye probe <FILE>        探查媒体文件：列出每路流的编解码、宽高/采样率、时基、时长
  ye remux <IN> <OUT>    MP4 重封装直通（解封装→封装，不转码）
  ye --help              显示本帮助
  ye --version           显示版本

当前支持: MP4 解封装与封装（progressive 与 fMP4 读写），probe/remux 可用。
转码（convert）在 M3 落地。";

/// 解析后的子命令。
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// `ye probe <FILE>`。
    Probe { path: PathBuf },
    /// `ye remux <IN> <OUT>`。
    Remux { input: PathBuf, output: PathBuf },
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
        Some("remux") => parse_remux(&args[1..]),
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

fn parse_remux(rest: &[OsString]) -> Result<Command, String> {
    match rest {
        [input, output] => Ok(Command::Remux {
            input: PathBuf::from(input),
            output: PathBuf::from(output),
        }),
        [] => Err(format!("`remux` 需要输入与输出两个文件参数。\n\n{USAGE}")),
        [_only] => Err(format!(
            "`remux` 需要输入与输出两个文件参数（缺输出）。\n\n{USAGE}"
        )),
        _ => Err(format!("`remux` 只接受输入与输出两个文件参数。\n\n{USAGE}")),
    }
}

/// 探查 `path` 指向的媒体文件，返回完整报告文本（含末尾换行）。
///
/// Err 载荷为已含路径前缀的用户向错误信息。
pub fn probe(path: &Path) -> Result<String, String> {
    let reader = Mp4Reader::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(report(&reader, path))
}

/// 由已打开的读取器生成报告（generic 便于测试传入 from_vec 的读取器）。
fn report<R: Read + Seek>(reader: &Mp4Reader<R>, path: &Path) -> String {
    let streams = reader.streams();
    let mut out = format!("{}: MP4, {} 路流\n", path.display(), streams.len());
    for s in streams {
        // open 成功后样例表已通过校验，sample_count 实际不会失败；
        // 仍走防御分支，报告里如实显示而不是崩溃。
        let count = reader
            .sample_count(s.index)
            .map(|n| n.to_string())
            .unwrap_or_else(|e| format!("读取失败: {e}"));
        // 浮点仅用于显示（ye-time 契约允许），时长精确值以上面 tick 数为准。
        let secs = s.duration.as_ticks() as f64 / TICKS_PER_SECOND as f64;
        out.push_str(&format!(
            "流 #{} [{}] codec={} 时基={} 时长={:.3}s ({} tick) 样例数={}",
            s.index,
            kind_label(s.kind),
            s.codec,
            s.timebase,
            secs,
            s.duration.as_ticks(),
            count,
        ));
        match (s.kind, s.video, s.audio) {
            (MediaKind::Video, Some(v), _) => {
                out.push_str(&format!(" 尺寸={}x{}\n", v.width, v.height));
            }
            (MediaKind::Audio, _, Some(a)) => {
                out.push_str(&format!(
                    " 声道={} 采样率={}Hz\n",
                    a.channel_count, a.sample_rate
                ));
            }
            _ => out.push('\n'),
        }
    }
    out
}

fn kind_label(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Video => "视频",
        MediaKind::Audio => "音频",
        MediaKind::Other => "其他",
    }
}

// —— remux ——

/// `ye remux <IN> <OUT>`：MP4 解封装 → 封装直通（不转码）。
///
/// - 输出为 progressive 布局（`ftyp → mdat → moov`）；包数据、关键帧、
///   时长与 pts−dts 偏移原样透传（H.264 保持 AVCC，不转 Annex B）。
/// - 流按 reader 流序一一对应写入（`TrackConfig::from_stream`）。
/// - 交错：各轨按样例序游标推进，全局按 `(dts tick, 流号)` 归并写入
///   （轨内序天然保持，同轨连续样例由 writer 自动合一 chunk）。
/// - 时戳：progressive 的 stts 从 0 起算（无 elst），故每轨时间线整体
///   平移到首包 dts = 0（平移量 = 该轨首包 dts；常见文件本就是 0，
///   平移为 no-op）。轨内相对时序不变；跨轨起始差不保真。
///
/// Err 载荷为已含路径前缀的用户向错误信息。
pub fn remux(input: &Path, output: &Path) -> Result<String, String> {
    let mut reader = Mp4Reader::open(input).map_err(|e| format!("{}: {e}", input.display()))?;
    if reader.streams().is_empty() {
        return Err(format!("{}: 输入无流，无可直通内容", input.display()));
    }
    let mut writer = Mp4Writer::create(
        output,
        Mp4WriterOptions {
            layout: Layout::Progressive,
            ..Default::default()
        },
    )
    .map_err(|e| format!("{}: {e}", output.display()))?;
    for s in reader.streams() {
        writer
            .add_stream(TrackConfig::from_stream(s))
            .map_err(|e| format!("{}: 流 {}: {e}", input.display(), s.index))?;
    }

    // 每轨游标 + 头包缓冲的 k 路归并（内存里每轨至多挂一个包）。
    let stream_count = reader.streams().len();
    let err_open =
        |si: usize, e: ye::container::mp4::Mp4Error| format!("{}: 流 {si}: {e}", input.display());
    let mut cursors = vec![0usize; stream_count];
    let mut heads: Vec<Option<Packet>> = Vec::with_capacity(stream_count);
    for si in 0..stream_count {
        let count = reader.sample_count(si).map_err(|e| err_open(si, e))?;
        heads.push(if count > 0 {
            Some(reader.read_packet(si, 0).map_err(|e| err_open(si, e))?)
        } else {
            None
        });
    }

    // 各轨首包 dts（tick），作为该轨的整体平移量；None = 未定（未遇首包）。
    let mut shifts: Vec<Option<i64>> = vec![None; stream_count];
    let mut written = 0usize;
    loop {
        // 挑当前头包中 dts 最小者（并列取流号小者，输出确定）。
        let mut pick: Option<usize> = None;
        for si in 0..stream_count {
            let Some(pkt) = &heads[si] else { continue };
            pick = match pick {
                None => Some(si),
                Some(cur) => {
                    let cur_dts = heads[cur].as_ref().expect("已选流应有头包").dts.as_ticks();
                    if (pkt.dts.as_ticks(), si) < (cur_dts, cur) {
                        Some(si)
                    } else {
                        Some(cur)
                    }
                }
            };
        }
        let Some(si) = pick else { break };
        let pkt = heads[si].take().expect("选中流的头包应存在");

        // 首包定平移量；后续包按同值平移（轨内相对时序不变）。
        let shift_ticks = *shifts[si].get_or_insert(pkt.dts.as_ticks());
        let shift = Timestamp::from_ticks(shift_ticks);
        let dts = pkt
            .dts
            .checked_sub(shift)
            .ok_or_else(|| format!("{}: 流 {si}: dts 平移溢出", input.display()))?;
        let pts = match pkt.pts {
            None => None,
            Some(p) => Some(
                p.checked_sub(shift)
                    .ok_or_else(|| format!("{}: 流 {si}: pts 平移溢出", input.display()))?,
            ),
        };
        writer
            .write_packet(&Packet { dts, pts, ..pkt })
            .map_err(|e| format!("{}: 流 {si}: {e}", input.display()))?;
        written += 1;

        // 推进该轨游标，读入下一头包。
        cursors[si] += 1;
        let count = reader.sample_count(si).map_err(|e| err_open(si, e))?;
        heads[si] = if cursors[si] < count {
            Some(
                reader
                    .read_packet(si, cursors[si])
                    .map_err(|e| err_open(si, e))?,
            )
        } else {
            None
        };
    }
    writer
        .finalize()
        .map_err(|e| format!("{}: {e}", output.display()))?;
    Ok(format!(
        "{} → {}: MP4, {} 路流, {} 样例直通（不转码）\n",
        input.display(),
        output.display(),
        stream_count,
        written
    ))
}
