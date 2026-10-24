//! 对拍 runner（bin）：编排缓存对账与 FFmpeg oracle 对拍，打印摘要行。
//!
//! 用法：
//!
//! ```text
//! cargo run -p ye-conformance [--manifest PATH] [--cache DIR]
//! ```
//!
//! 默认清单 = 本 crate 目录的 `manifest.toml`；默认缓存 =
//! `$YE_CONFORMANCE_CACHE` 或本 crate 目录的 `.cache/`（git 忽略）。
//! 退出码：0 = 无失败（skip 合法）；1 = 有对拍失败；2 = 用法/清单错误。
//!
//! 施工图①的 h264 口径：
//! - NAL 序列：`ffmpeg -i <vec> -map 0:v:0 -c:v copy -bsf:v
//!   h264_mp4toannexb -f h264 ref.264` 提取的 NAL 序列与我方
//!   （ye-container-mp4 解 AVCC + ye-codec-h264 拆包）一致；
//! - SPS 字段：avcC 内 SPS 解析的 profile/level/width/height 与
//!   `ffprobe -show_entries stream=profile,level,width,height` 逐项相等。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ye_codec_h264::{AvcConfigRecord, Sps, split_annexb, split_avcc};
use ye_conformance::manifest::Manifest;
use ye_conformance::report::{Outcome, Report};
use ye_conformance::sha256::sha256_hex;
use ye_conformance::tolerance;

fn main() {
    let (manifest_path, cache_dir) = match parse_args() {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("ye: {msg}");
            std::process::exit(2);
        }
    };
    let manifest_text = match std::fs::read_to_string(&manifest_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("ye: 读清单 {} 失败：{e}", manifest_path.display());
            std::process::exit(2);
        }
    };
    let manifest = match Manifest::parse(&manifest_text) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("ye: 清单解析失败：{e}");
            std::process::exit(2);
        }
    };
    println!(
        "conformance: manifest={} vectors={} cache={}",
        manifest_path.display(),
        manifest.vectors.len(),
        cache_dir.display()
    );
    println!("tolerance ({})", tolerance::POLICY_NOTE);
    for codec in unique_codecs(&manifest) {
        for t in tolerance::tolerances(&codec) {
            let tolerance::Tolerance::BitExact { dimension } = t;
            println!("  {codec}: bit-exact [{dimension}]");
        }
    }

    let ffmpeg = command_available("ffmpeg");
    let ffprobe = command_available("ffprobe");
    if !ffmpeg || !ffprobe {
        println!(
            "oracle: ffmpeg={} ffprobe={}（缺席则相关向量 skip）",
            ffmpeg, ffprobe
        );
    }
    for p in &manifest.pending {
        println!(
            "[{}] PENDING 待网络（{}）{}",
            p.name,
            p.source,
            p.note.as_deref().unwrap_or("")
        );
    }

    let mut report = Report {
        outcomes: Vec::new(),
        pending: manifest.pending.len(),
    };
    for entry in &manifest.vectors {
        let vec_path = Path::new(&cache_dir).join(format!("{}.{}", entry.name, entry.container));
        // 1) 缓存与 sha 对账（裁决：sha 不匹配**不当 skip**——文件在就照跑，
        //    sha_ok=false 落摘要行。sha 漂移（如换 ffmpeg 重生成）是信号，
        //    不是可静默跳过的常态）
        let data = match std::fs::read(&vec_path) {
            Ok(d) => d,
            Err(_) => {
                println!(
                    "[{}] SKIP not_fetched（{}）",
                    entry.name,
                    vec_path.display()
                );
                report.outcomes.push(Outcome::Skipped {
                    name: entry.name.clone(),
                    reason: "not_fetched".into(),
                    sha_ok: false,
                });
                continue;
            }
        };
        let sha_ok = sha256_hex(&data) == entry.sha256;
        if !sha_ok {
            println!(
                "[{}] WARN sha_mismatch（期望 {}，实得 {}）——照跑不 skip，见摘要行 sha_ok",
                entry.name,
                entry.sha256,
                sha256_hex(&data)
            );
        }
        // 2) 容差口径必须已声明
        if tolerance::tolerances(&entry.codec).is_empty() {
            println!(
                "[{}] SKIP unsupported_codec（{} 无对拍口径）",
                entry.name, entry.codec
            );
            report.outcomes.push(Outcome::Skipped {
                name: entry.name.clone(),
                reason: "unsupported_codec".into(),
                sha_ok,
            });
            continue;
        }
        // 3) oracle 可用性（sha 已对账，skip 仍计 sha_ok）
        if !ffmpeg || !ffprobe {
            println!(
                "[{}] SKIP oracle_missing（ffmpeg={ffmpeg} ffprobe={ffprobe}）",
                entry.name
            );
            report.outcomes.push(Outcome::Skipped {
                name: entry.name.clone(),
                reason: "oracle_missing".into(),
                sha_ok,
            });
            continue;
        }
        // 4) 分派对拍
        let result = match entry.codec.as_str() {
            "h264" => h264_run(&entry.name, &vec_path, &data),
            other => Err(format!("codec {other} 已声明容差但 runner 未接线")),
        };
        match result {
            Ok(detail) => {
                println!(
                    "[{}] PASS{} {detail}",
                    entry.name,
                    if sha_ok { "" } else { "（sha_mismatch）" }
                );
                report.outcomes.push(Outcome::Ran {
                    name: entry.name.clone(),
                    passed: true,
                    sha_ok,
                    detail,
                });
            }
            Err(detail) => {
                println!(
                    "[{}] FAIL{} {detail}",
                    entry.name,
                    if sha_ok { "" } else { "（sha_mismatch）" }
                );
                report.outcomes.push(Outcome::Ran {
                    name: entry.name.clone(),
                    passed: false,
                    sha_ok,
                    detail,
                });
            }
        }
    }

    println!("{}", report.summary_line());
    println!("{}", report.pending_line());
    let (_, _, failed, _) = report.counts();
    if failed > 0 {
        std::process::exit(1);
    }
}

/// 解析命令行：`--manifest PATH` / `--cache DIR`（可重复，后写覆盖）。
fn parse_args() -> Result<(PathBuf, PathBuf), String> {
    let manifest_default = PathBuf::from(format!("{}/manifest.toml", env!("CARGO_MANIFEST_DIR")));
    let cache_default = std::env::var("YE_CONFORMANCE_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("{}/.cache", env!("CARGO_MANIFEST_DIR"))));
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut manifest = manifest_default;
    let mut cache = cache_default;
    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--manifest" => {
                let Some(v) = args.get(i + 1) else {
                    return Err("--manifest 需要路径参数".into());
                };
                manifest = PathBuf::from(v);
                i += 2;
            }
            "--cache" => {
                let Some(v) = args.get(i + 1) else {
                    return Err("--cache 需要路径参数".into());
                };
                cache = PathBuf::from(v);
                i += 2;
            }
            other => {
                return Err(format!(
                    "未知参数 {other}；用法：ye-conformance [--manifest PATH] [--cache DIR]"
                ));
            }
        }
    }
    Ok((manifest, cache))
}

/// 清单里出现过的 codec（去重保序），容差打印用。
fn unique_codecs(manifest: &Manifest) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for v in &manifest.vectors {
        if !seen.contains(&v.codec) {
            seen.push(v.codec.clone());
        }
    }
    seen
}

/// 命令可调用探测（`-version` 退出 0 视为可用）。
fn command_available(name: &str) -> bool {
    Command::new(name)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// h264 全口径对拍：NAL 序列 + SPS 字段。任一口径失败 → Err（明细串联）。
fn h264_run(name: &str, vec_path: &Path, data: &[u8]) -> Result<String, String> {
    let mut failures: Vec<String> = Vec::new();
    let mut passes: Vec<String> = Vec::new();

    let (mut our_nals, record) = match h264_nal_sequence(data) {
        Ok(v) => v,
        Err(e) => return Err(format!("我方码流层提取失败：{e}")),
    };

    // 口径 a：NAL 序列 bit-exact
    let ref_path =
        std::env::temp_dir().join(format!("ye-conformance-{name}.{}.264", std::process::id()));
    match ffmpeg_extract_annexb(vec_path, &ref_path)
        .and_then(|()| std::fs::read(&ref_path).map_err(|e| format!("读 ref 失败：{e}")))
    {
        Ok(ref_bytes) => {
            let _ = std::fs::remove_file(&ref_path);
            match split_annexb(&ref_bytes) {
                Ok(ref_slices) => {
                    let mut ref_nals: Vec<Vec<u8>> =
                        ref_slices.iter().map(|s| s.to_vec()).collect();
                    // 参数集对齐：h264_mp4toannexb 在首个 IDR 前若无带内
                    // SPS/PPS，则把 avcC（extradata）里的参数集插到 IDR 前
                    // （实测位置在首个 IDR 紧前——包首若有 SEI，则插在
                    // SEI 之后）。我方序列为包内 NAL，照同一语义以 avcC
                    // 记录补插，保证比较的是码流本体的 NAL 序列。
                    our_nals = align_parameter_sets(&our_nals, &record);
                    // 尾 NAL 的 trailing_zero_8bits 两侧归一化（起始码切分
                    // 语义差异，非载荷差异）
                    trim_trailing_zeros(&mut our_nals);
                    trim_trailing_zeros(&mut ref_nals);
                    // 诊断明细开关：YE_CONFORMANCE_DEBUG_NALS=1 打印逐 NAL
                    if std::env::var("YE_CONFORMANCE_DEBUG_NALS").ok().as_deref() == Some("1") {
                        dump_nals("ours", &our_nals);
                        dump_nals("ffmpeg", &ref_nals);
                    }
                    if our_nals == ref_nals {
                        passes.push(format!("nal-sequence: PASS ({} NALs)", our_nals.len()));
                    } else {
                        failures.push(format!(
                            "nal-sequence: FAIL（我方 {} 个 vs ffmpeg {} 个；首个差异 {}）",
                            our_nals.len(),
                            ref_nals.len(),
                            first_nal_diff(&our_nals, &ref_nals)
                        ));
                    }
                }
                Err(e) => failures.push(format!("nal-sequence: FAIL（ref 拆包失败：{e}）")),
            }
        }
        Err(e) => {
            let _ = std::fs::remove_file(&ref_path);
            failures.push(format!("nal-sequence: FAIL（ffmpeg oracle 失败：{e}）"));
        }
    }

    // 口径 b：SPS 字段逐项相等
    match ffprobe_stream_fields(vec_path) {
        Ok(ff) => {
            let (ff_profile, ff_level, ff_width, ff_height) = ff;
            let check = record
                .sps_nals
                .first()
                .ok_or_else(|| "avcC 无 SPS".to_string())
                .and_then(|nal| Sps::parse(nal).map_err(|e| format!("SPS 解析失败：{e}")));
            match check {
                Ok(sps) => {
                    let my_profile = ffprobe_profile_name(&sps);
                    let mut sps_fail: Vec<String> = Vec::new();
                    if my_profile != ff_profile {
                        sps_fail.push(format!("profile 我方={my_profile} ffprobe={ff_profile}"));
                    }
                    if !level_equal(sps.level_idc, &ff_level) {
                        sps_fail.push(format!("level 我方={} ffprobe={ff_level}", sps.level_idc));
                    }
                    if sps.width().to_string() != ff_width {
                        sps_fail.push(format!("width 我方={} ffprobe={ff_width}", sps.width()));
                    }
                    if sps.height().to_string() != ff_height {
                        sps_fail.push(format!("height 我方={} ffprobe={ff_height}", sps.height()));
                    }
                    if sps_fail.is_empty() {
                        passes.push(format!(
                            "sps-fields: PASS (profile={my_profile} level={} {}x{})",
                            sps.level_idc,
                            sps.width(),
                            sps.height()
                        ));
                    } else {
                        failures.push(format!("sps-fields: FAIL（{}）", sps_fail.join("; ")));
                    }
                }
                Err(e) => failures.push(format!("sps-fields: FAIL（{e}）")),
            }
        }
        Err(e) => failures.push(format!("sps-fields: FAIL（ffprobe oracle 失败：{e}）")),
    }

    if failures.is_empty() {
        Ok(passes.join(" | "))
    } else {
        Err(failures.join(" | "))
    }
}

/// 我方 NAL 序列：ye-container-mp4 解 AVCC → 包内 NAL 逐包拼接
/// （参数集前置对齐在 `h264_run` 里按 ffmpeg 侧形态决定）。
fn h264_nal_sequence(data: &[u8]) -> Result<(Vec<Vec<u8>>, AvcConfigRecord), String> {
    use ye_codec::{CodecId, MediaType};
    use ye_container::ContainerRegistry;

    let mut registry = ContainerRegistry::new();
    ye_container_mp4::register_mp4(&mut registry);
    let (_, mut demuxer) = registry
        .open_demuxer(Box::new(std::io::Cursor::new(data.to_vec())))
        .map_err(|e| format!("open_demuxer: {e}"))?;
    let header = demuxer
        .read_header()
        .map_err(|e| format!("read_header: {e}"))?;
    let si = header
        .streams
        .iter()
        .position(|s| s.media_type == MediaType::Video && s.codec.codec == CodecId::H264)
        .ok_or_else(|| "无 H.264 视频流".to_string())?;
    let extradata = header.streams[si]
        .codec
        .extradata
        .clone()
        .ok_or_else(|| "流缺 extradata（avcC）".to_string())?;
    let record = AvcConfigRecord::parse(&extradata).map_err(|e| format!("avcC: {e}"))?;
    let mut nals: Vec<Vec<u8>> = Vec::new();
    while let Some(p) = demuxer
        .read_packet()
        .map_err(|e| format!("read_packet: {e}"))?
    {
        if p.stream_index as usize != si {
            continue;
        }
        for nal in split_avcc(p.data.as_ref(), record.length_size)
            .map_err(|e| format!("split_avcc: {e}"))?
        {
            nals.push(nal.to_vec());
        }
    }
    Ok((nals, record))
}

/// 施工图口径的 ffmpeg 抽取命令形态。
fn ffmpeg_extract_annexb(input: &Path, out: &Path) -> Result<(), String> {
    let status = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-i"])
        .arg(input)
        .args([
            "-map",
            "0:v:0",
            "-c:v",
            "copy",
            "-bsf:v",
            "h264_mp4toannexb",
            "-f",
            "h264",
        ])
        .arg(out)
        .status()
        .map_err(|e| format!("spawn ffmpeg: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("ffmpeg 退出码 {status}"))
    }
}

/// `ffprobe -show_entries stream=profile,level,width,height -of flat`
/// → (profile, level, width, height) 四元组（原样字符串）。
/// 用 flat 键值输出：csv 的字段顺序由 ffprobe 内部结构顺序决定，
/// 不跟随 `-show_entries` 的书写顺序（实测为 profile,width,height,level），
/// 按键取值才不依赖版本。
fn ffprobe_stream_fields(path: &Path) -> Result<(String, String, String, String), String> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=profile,level,width,height",
            "-of",
            "flat",
        ])
        .arg(path)
        .output()
        .map_err(|e| format!("spawn ffprobe: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "ffprobe 退出码 {}：{}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut fields: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for line in text.lines().map(str::trim) {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        // 形如 streams.stream.0.profile="Constrained Baseline"
        let key = k.rsplit('.').next().unwrap_or("");
        let val = v.trim().trim_matches('"').to_string();
        fields.insert(key.to_string(), val);
    }
    let get = |key: &str| -> Result<String, String> {
        fields
            .get(key)
            .cloned()
            .ok_or_else(|| format!("ffprobe 输出缺字段 {key}：{text}"))
    };
    Ok((
        get("profile")?,
        get("level")?,
        get("width")?,
        get("height")?,
    ))
}

/// profile 命名对齐 ffprobe（H.264 常见四档；其余档位以 idc 原样呈现，
/// 与 ffprobe 命名不一致会在对拍明细里暴露出来人工裁决）。
fn ffprobe_profile_name(sps: &Sps) -> String {
    match sps.profile_idc {
        66 if sps.constraint_set(1) => "Constrained Baseline".into(),
        66 => "Baseline".into(),
        77 => "Main".into(),
        100 => "High".into(),
        other => format!("profile_idc {other}"),
    }
}

/// level 等值：ffprobe 可能给 `30` 或 `3.0` 两种记法，数值上判等
/// （±0.001），不做字符串匹配。
fn level_equal(ours: u8, ffprobe: &str) -> bool {
    let Ok(ff) = ffprobe.parse::<f64>() else {
        return false;
    };
    let mine = f64::from(ours);
    (mine - ff).abs() <= 0.001 || (mine - ff * 10.0).abs() <= 0.001
}

/// NAL 首字节的 unit type（低 5 位）。
fn nal_type(nal: &[u8]) -> u8 {
    nal.first().map(|b| b & 0x1F).unwrap_or(255)
}

/// 按 `h264_mp4toannexb` 的参数集语义补插 avcC 的 SPS/PPS：首个 IDR
/// 之前若无带内参数集，把 avcC 记录里的插到 IDR 紧前（缺哪个补哪个）。
/// 流内全无 IDR 时，若全流无 SPS 则前置到流首。已知边界：后续 IDR 前
/// 的重复插入语义未对齐（当前规则只处理首个 IDR）——向量清单扩到
/// 多 IDR 码流时以诊断输出（YE_CONFORMANCE_DEBUG_NALS=1）核对。
fn align_parameter_sets(nals: &[Vec<u8>], record: &AvcConfigRecord) -> Vec<Vec<u8>> {
    let mut insert_at: Option<usize> = None; // IDR 位置
    for (i, nal) in nals.iter().enumerate() {
        if nal_type(nal) == 5 {
            insert_at = Some(i);
            break;
        }
    }
    match insert_at {
        Some(idr) => {
            let sps_seen = nals[..idr].iter().any(|n| nal_type(n) == 7);
            let pps_seen = nals[..idr].iter().any(|n| nal_type(n) == 8);
            let mut out: Vec<Vec<u8>> = nals[..idr].to_vec();
            if !sps_seen {
                out.extend(record.sps_nals.iter().cloned());
            }
            if !pps_seen {
                out.extend(record.pps_nals.iter().cloned());
            }
            out.extend(nals[idr..].iter().cloned());
            out
        }
        None => {
            if nals.iter().any(|n| nal_type(n) == 7) {
                nals.to_vec()
            } else {
                let mut out = record.sps_nals.clone();
                out.extend(record.pps_nals.iter().cloned());
                out.extend_from_slice(nals);
                out
            }
        }
    }
}

/// 尾 NAL 去尾随零字节（trailing_zero_8bits 归一化）。
fn trim_trailing_zeros(nals: &mut [Vec<u8>]) {
    if let Some(last) = nals.last_mut() {
        while last.last() == Some(&0) {
            last.pop();
        }
    }
}

/// 诊断输出：逐 NAL 的 (序号, type, len)。
fn dump_nals(side: &str, nals: &[Vec<u8>]) {
    eprintln!("--- {side}: {} NALs", nals.len());
    for (i, nal) in nals.iter().enumerate() {
        eprintln!("  [{i:>3}] type={} len={}", nal_type(nal), nal.len());
    }
}

/// 首个 NAL 差异的诊断描述。
fn first_nal_diff(ours: &[Vec<u8>], theirs: &[Vec<u8>]) -> String {
    let common = ours.len().min(theirs.len());
    for i in 0..common {
        if ours[i] != theirs[i] {
            let ty = |nal: &[u8]| nal.first().map(|b| b & 0x1F).unwrap_or(255);
            return format!(
                "第 {i} 个 NAL（type 我方={} ffprobe={}，len 我方={} ffmpeg={}）",
                ty(&ours[i]),
                ty(&theirs[i]),
                ours[i].len(),
                theirs[i].len()
            );
        }
    }
    format!("前 {common} 个一致，长度发散")
}

// main.rs 无单元测试：可离线单测的逻辑都在 lib（见 src/*.rs），
// ffmpeg/ffprobe 依赖的编排以真实运行验证（README「摘要行机制验证」）。
