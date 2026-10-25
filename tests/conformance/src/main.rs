//! 对拍 runner（bin）：编排缓存对账与 FFmpeg oracle 对拍，打印摘要行。
//!
//! 用法：
//!
//! ```text
//! cargo run -p ye-conformance [--manifest PATH] [--cache DIR] [--set NAME]
//! ```
//!
//! 默认清单 = 本 crate 目录的 `manifest.toml`；默认缓存 =
//! `$YE_CONFORMANCE_CACHE` 或本 crate 目录的 `.cache/`（git 忽略）。
//! `--set intra|ip|all` 按清单 `set` 标签选通子阶段集（施工图②：
//! ②a 纯 I 集 / ②b I/P 混合集；不带 `--set` 或 `all` = 全集 = ②c）。
//! 退出码：0 = 无失败（skip 合法）；1 = 有对拍失败；2 = 用法/清单错误。
//!
//! h264 口径（施工图①码流层 + 施工图②逐帧）：
//! - NAL 序列：`ffmpeg -i <vec> -map 0:v:0 -c:v copy -bsf:v
//!   h264_mp4toannexb -f h264 ref.264` 提取的 NAL 序列与我方
//!   （ye-container-mp4 解 AVCC + ye-codec-h264 拆包）一致；
//! - SPS 字段：avcC 内 SPS 解析的 profile/level/width/height 与
//!   `ffprobe -show_entries stream=profile,level,width,height` 逐项相等；
//! - 逐帧 YUV（施工图②）：`ffmpeg -i <vec> -pix_fmt yuv420p -f rawvideo
//!   ref.yuv` 的参考帧序列 vs 我方解码输出 `<cache>/<name>.our.yuv`
//!   （显示尺寸 yuv420p 平面布局）。我方文件**缺席时本口径不参与
//!   pass/fail**（打 NOTE，②a/②b 解码接线后生成该文件即自动加入）；
//!   在场则按 [`ye_conformance::tolerance`] 的施工图②容差裁决，失败
//!   落归档 `<cache>/failures/<name>.fail.txt`（向量名/首差帧/首差
//!   宏块位置/max-abs），通过时清除旧归档。

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ye_codec_h264::{AvcConfigRecord, Sps, split_annexb, split_avcc};
use ye_conformance::compare::{self, ChromaSampling, FrameDiffReport};
use ye_conformance::manifest::{Manifest, VectorEntry};
use ye_conformance::nalalign::{
    align_parameter_sets, first_nal_diff, nal_type, trim_trailing_zeros,
};
use ye_conformance::report::{Outcome, Report};
use ye_conformance::sha256::sha256_hex;
use ye_conformance::tolerance::{self, Tolerance};

fn main() {
    let (manifest_path, cache_dir, set_filter) = match parse_args() {
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
    // 集选通（施工图②子阶段门禁）：--set intra/ip 只跑对应标签的向量，
    // 缺 set 标签的 v1 条目只在全集（不带 --set / --set all）里跑。
    // 选通不是 skip：选掉的向量不进摘要行计数，运行头行明示选择面。
    let set_name = set_filter.as_deref().unwrap_or("all");
    let selected: Vec<&VectorEntry> = match set_filter.as_deref() {
        None | Some("all") => manifest.vectors.iter().collect(),
        Some(s) => manifest
            .vectors
            .iter()
            .filter(|v| v.set.as_deref() == Some(s))
            .collect(),
    };
    let mut known_sets: Vec<&str> = manifest
        .vectors
        .iter()
        .filter_map(|v| v.set.as_deref())
        .collect();
    known_sets.sort_unstable();
    known_sets.dedup();
    println!(
        "conformance: manifest={} set={} selected={}/{} cache={}",
        manifest_path.display(),
        set_name,
        selected.len(),
        manifest.vectors.len(),
        cache_dir.display()
    );
    if !matches!(set_filter.as_deref(), None | Some("all")) && selected.is_empty() {
        // 门禁安全（2026-10-24 施工图②）：②a/②b 过界断言依赖
        // 「某集全部对拍过」——拼错集名选中 0 条若静默退出 0，门禁会
        // 假绿。按用法错误退出 2，强制把集名改对。
        let sets_list = if known_sets.is_empty() {
            "<无>".to_string()
        } else {
            known_sets.join("/")
        };
        eprintln!(
            "ye: --set {set_name} 选中 0 条（清单可用的集标签：{sets_list}；缺 set 键 = v1 全集）"
        );
        std::process::exit(2);
    }
    println!("tolerance ({})", tolerance::POLICY_NOTE);
    for codec in unique_codecs(&manifest) {
        for t in tolerance::tolerances(&codec) {
            match t {
                Tolerance::BitExact { dimension } => {
                    println!("  {codec}: bit-exact [{dimension}]");
                }
                Tolerance::MaxAbs {
                    dimension,
                    max_abs,
                    zero_ratio_permille_min,
                } => {
                    println!(
                        "  {codec}: 全样本|Δ|≤{max_abs} 且每帧 Δ=0 ≥{zero_ratio_permille_min}‰ [{dimension}]"
                    );
                }
                Tolerance::PcmTwoLevel {
                    dimension,
                    max_abs,
                    ratio1_permille_min,
                } => {
                    println!(
                        "  {codec}: 全样本|Δ|≤{max_abs} 且每帧 |Δ|≤1 ≥{ratio1_permille_min}‰ [{dimension}]"
                    );
                }
            }
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
    for entry in &selected {
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
            "h264" => h264_run(&entry.name, &vec_path, &data, &cache_dir),
            "aac" => aac_run(&entry.name, &entry.container, &vec_path, &cache_dir),
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

/// 解析命令行：`--manifest PATH` / `--cache DIR` / `--set NAME`
/// （可重复，后写覆盖）。
fn parse_args() -> Result<(PathBuf, PathBuf, Option<String>), String> {
    let manifest_default = PathBuf::from(format!("{}/manifest.toml", env!("CARGO_MANIFEST_DIR")));
    let cache_default = std::env::var("YE_CONFORMANCE_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("{}/.cache", env!("CARGO_MANIFEST_DIR"))));
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut manifest = manifest_default;
    let mut cache = cache_default;
    let mut set: Option<String> = None;
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
            "--set" => {
                let Some(v) = args.get(i + 1) else {
                    return Err("--set 需要集名参数（intra/ip/all，见 manifest set 标签）".into());
                };
                set = Some(v.clone());
                i += 2;
            }
            other => {
                return Err(format!(
                    "未知参数 {other}；用法：ye-conformance [--manifest PATH] [--cache DIR] [--set NAME]"
                ));
            }
        }
    }
    Ok((manifest, cache, set))
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

/// h264 全口径对拍：NAL 序列 + SPS 字段 + 逐帧 YUV（施工图②）。
/// 任一口径失败 → Err（明细串联）。
fn h264_run(name: &str, vec_path: &Path, data: &[u8], cache_dir: &Path) -> Result<String, String> {
    let mut failures: Vec<String> = Vec::new();
    let mut passes: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    let (mut our_nals, record) = match h264_nal_sequence(data) {
        Ok(v) => v,
        Err(e) => return Err(format!("我方码流层提取失败：{e}")),
    };
    // SPS 解析一次：口径 b（字段比对）与口径 c（帧尺寸）共用
    let sps_result: Result<Sps, String> = record
        .sps_nals
        .first()
        .ok_or_else(|| "avcC 无 SPS".to_string())
        .and_then(|nal| Sps::parse(nal).map_err(|e| format!("SPS 解析失败：{e}")));

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
                    // 参数集对齐：h264_mp4toannexb 在每个 IDR 前若无带内
                    // SPS/PPS，则把 avcC（extradata）里的参数集插到 IDR 紧前
                    // （2026-10-24 施工图②以 45-IDR 向量实测锁定：每个 IDR
                    // 前都插，首个 IDR 前有 SEI 时插在 SEI 之后；语义实现
                    // 见 lib `nalalign`）。我方序列为包内 NAL，照同一语义
                    // 以 avcC 记录补插后逐 NAL 比较。
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
            match &sps_result {
                Ok(sps) => {
                    let my_profile = ffprobe_profile_name(sps);
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

    // 口径 c：逐帧 YUV（施工图②）——我方解码输出缺席时不参与
    // pass/fail（打 NOTE）；在场则按施工图容差裁决，失败落归档。
    let yuv_tol = tolerance::tolerances("h264").iter().find_map(|t| match t {
        Tolerance::MaxAbs { .. } => Some(*t),
        _ => None,
    });
    let our_yuv_path = cache_dir.join(format!("{name}.our.yuv"));
    match (std::fs::read(&our_yuv_path), yuv_tol) {
        (Err(_), _) => notes.push(format!(
            "yuv-frames: 未接线（我方解码输出 {} 缺席；②a/②b 解码输出落此文件后本口径自动加入对拍）",
            our_yuv_path.display()
        )),
        (_, None) => notes.push("yuv-frames: 未接线（容差表无逐帧口径）".into()),
        (Ok(our_yuv), Some(tol)) => {
            let ref_path =
                std::env::temp_dir().join(format!("ye-conformance-{name}.{}.ref.yuv", std::process::id()));
            match ffmpeg_rawvideo_yuv(vec_path, &ref_path)
                .and_then(|()| std::fs::read(&ref_path).map_err(|e| format!("读 ref 失败：{e}")))
            {
                Ok(ref_yuv) => {
                    let _ = std::fs::remove_file(&ref_path);
                    match &sps_result {
                        Err(e) => failures.push(format!("yuv-frames: FAIL（{e}，帧尺寸不可得）")),
                        Ok(sps) => match chroma_sampling(sps) {
                            None => failures.push(format!(
                                "yuv-frames: FAIL（chroma_format_idc={} 非 4:2:0；逐帧口径仅支持 420 向量）",
                                sps.chroma_format_idc
                            )),
                            Some(chroma) => {
                                match compare::diff_yuv_frames(
                                    &our_yuv,
                                    &ref_yuv,
                                    sps.width() as usize,
                                    sps.height() as usize,
                                    chroma,
                                ) {
                                    Err(e) => {
                                        failures.push(format!("yuv-frames: FAIL（布局不符：{e}）"))
                                    }
                                    Ok(report) => match tol.check_yuv_frames(&report) {
                                        Ok(()) => {
                                            passes.push(format!(
                                                "yuv-frames: PASS ({} 帧, max_abs={}, 逐位一致={})",
                                                report.frames,
                                                report.max_abs,
                                                report.max_abs == 0
                                            ));
                                            // 通过即清旧归档（防止陈旧失败记录误导）
                                            let _ = std::fs::remove_file(
                                                cache_dir
                                                    .join("failures")
                                                    .join(format!("{name}.fail.txt")),
                                            );
                                        }
                                        Err(tol_msg) => {
                                            let archive =
                                                write_failure_archive(cache_dir, name, &report, chroma, &tol_msg);
                                            failures.push(format!(
                                                "yuv-frames: {tol_msg}；失败归档：{}",
                                                archive.display()
                                            ));
                                        }
                                    },
                                }
                            }
                        },
                    }
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&ref_path);
                    failures.push(format!("yuv-frames: FAIL（ffmpeg oracle 失败：{e}）"));
                }
            }
        }
    }

    let note = if notes.is_empty() {
        String::new()
    } else {
        format!(" | {}", notes.join(" | "))
    };
    if failures.is_empty() {
        Ok(format!("{}{note}", passes.join(" | ")))
    } else {
        Err(format!("{}{note}", failures.join(" | ")))
    }
}

// 施工图②口径的 ffmpeg 逐帧参考：`-pix_fmt yuv420p -f rawvideo`
// （施工图原文 `ffmpeg -i vec.mp4 -pix_fmt yuv420p -f rawvideo ref.yuv`；
// -map 0:v:0 钉死首视频流，-v error 静噪）。输出为**显示尺寸**
// （含裁剪）的 yuv420p 平面布局，与 `Sps::width()/height()` 同口径。
// ---------------------------------------------------------------------------
// AAC 对拍（施工图③）：三入口（ADTS / LOAS / MP4-esds）→ s16le 逐帧双层容差
// ---------------------------------------------------------------------------

/// AAC 全口径对拍：解码向量 → 写 `<cache>/<name>.our.pcm`，与 ffmpeg
/// `-f s16le` 参考按施工图③双层容差（100% |Δ|≤4 且每帧 |Δ|≤1 占比
/// ≥999‰，逐帧判定）裁决；失败落 `<cache>/failures/<name>.fail.txt`。
///
/// MP4 向量参考侧带 `-ignore_editlist 1`：我方解码器按裸码流语义输出
/// 全部存储帧（无容器级 priming 裁剪），ffmpeg 默认按 edit list 裁去
/// 首帧——两侧口径须一致（见 manifest/README 登记）。
fn aac_run(
    name: &str,
    container: &str,
    vec_path: &Path,
    cache_dir: &Path,
) -> Result<String, String> {
    use ye_codec::{Decoded, Decoder, Packet};
    use ye_codec_aac::AacDecoder;

    // 1) 我方解码 → 交错 s16le
    fn decode_one(
        dec: &mut ye_codec_aac::AacDecoder,
        pkt: &Packet,
        channels: &mut usize,
    ) -> Result<Vec<f32>, String> {
        dec.feed(pkt).map_err(|e| format!("feed: {e}"))?;
        let mut out = Vec::new();
        while let Decoded::Frame(f) = dec.pull().map_err(|e| format!("pull: {e}"))? {
            let ye_codec::Frame::Audio(a) = f else {
                return Err("非音频帧".into());
            };
            if *channels == 0 {
                *channels = a.channel_layout().channels() as usize;
            }
            let b = a.data();
            for c in b.chunks_exact(4) {
                out.push(f32::from_le_bytes([c[0], c[1], c[2], c[3]]));
            }
        }
        Ok(out)
    }
    let mut samples: Vec<f32> = Vec::new();
    let mut channels: usize = 0;
    match container {
        "adts" => {
            let params =
                ye_codec::CodecParameters::new(ye_codec::CodecId::Aac, ye_codec::MediaType::Audio);
            let mut dec = AacDecoder::new(&params).map_err(|e| format!("构造: {e}"))?;
            let data = std::fs::read(vec_path).map_err(|e| format!("读向量: {e}"))?;
            let mut off = 0usize;
            while off + 7 <= data.len() {
                let len = ((data[off + 3] as usize & 0x03) << 11)
                    | ((data[off + 4] as usize) << 3)
                    | ((data[off + 5] as usize >> 5) & 0x07);
                if len < 7 || off + len > data.len() {
                    return Err(format!("ADTS 帧长 {len} 异常 @{off}"));
                }
                let mut p = Packet::empty(0);
                p.data = std::sync::Arc::from(data[off..off + len].to_vec());
                samples.extend(decode_one(&mut dec, &p, &mut channels)?);
                off += len;
            }
        }
        "loas" => {
            let params =
                ye_codec::CodecParameters::new(ye_codec::CodecId::Aac, ye_codec::MediaType::Audio);
            let mut dec = AacDecoder::new(&params).map_err(|e| format!("构造: {e}"))?;
            let data = std::fs::read(vec_path).map_err(|e| format!("读向量: {e}"))?;
            let mut off = 0usize;
            while off + 3 <= data.len() {
                if data[off] != 0x56 || data[off + 1] & 0xE0 != 0xE0 {
                    off += 1;
                    continue;
                }
                let len = (((data[off + 1] & 0x1F) as usize) << 8 | data[off + 2] as usize) + 3;
                if off + len > data.len() {
                    break;
                }
                let mut p = Packet::empty(0);
                p.data = std::sync::Arc::from(data[off..off + len].to_vec());
                samples.extend(decode_one(&mut dec, &p, &mut channels)?);
                off += len;
            }
        }
        "mp4" => {
            use ye_container::Demuxer;
            let file = std::fs::File::open(vec_path).map_err(|e| format!("打开向量: {e}"))?;
            let mut demux = ye_container_mp4::Mp4Demuxer::new(Box::new(file))
                .map_err(|e| format!("mp4 解封装: {e}"))?;
            let header = demux.read_header().map_err(|e| format!("读 mp4 头: {e}"))?;
            let audio = header
                .streams
                .iter()
                .find(|st| st.media_type == ye_codec::MediaType::Audio)
                .ok_or_else(|| "向量无音频流".to_string())?;
            let mut dec = AacDecoder::new(&audio.codec).map_err(|e| format!("构造: {e}"))?;
            while let Some(pkt) = demux.read_packet().map_err(|e| format!("读包: {e}"))? {
                samples.extend(decode_one(&mut dec, &pkt, &mut channels)?);
            }
        }
        other => return Err(format!("aac 向量容器 {other} 未接线（adts/loas/mp4）")),
    }
    if samples.is_empty() || channels == 0 {
        return Err("解码输出为空".into());
    }
    if channels > 2 {
        return Err(format!("声道数 {channels} 超支持集（1..=2）"));
    }
    let to_s16 = |v: f32| (v * 32768.0).clamp(-32768.0, 32767.0).round() as i16;
    let our_pcm: Vec<u8> = samples
        .iter()
        .flat_map(|&v| to_s16(v).to_le_bytes())
        .collect();
    let our_path = cache_dir.join(format!("{name}.our.pcm"));
    std::fs::write(&our_path, &our_pcm).map_err(|e| format!("写 {}: {e}", our_path.display()))?;

    // 2) oracle：ffmpeg -f s16le（mp4 加 -ignore_editlist 1）
    let ref_path = std::env::temp_dir().join(format!(
        "ye-conformance-{name}.{}.ref.pcm",
        std::process::id()
    ));
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-y", "-v", "error"]);
    if container == "mp4" {
        cmd.arg("-ignore_editlist").arg("1");
    }
    cmd.arg("-i")
        .arg(vec_path)
        .args(["-map", "0:a:0", "-f", "s16le", "-c:a", "pcm_s16le"]);
    let status = cmd
        .arg(&ref_path)
        .status()
        .map_err(|e| format!("spawn ffmpeg: {e}"))?;
    if !status.success() {
        return Err(format!("ffmpeg 退出码 {status}"));
    }
    let ref_pcm = std::fs::read(&ref_path).map_err(|e| format!("读 ref: {e}"))?;
    let _ = std::fs::remove_file(&ref_path);

    // 3) 对拍 + 容差（帧 = 1024 样本/声道 × 声道数）
    let frame_samples = 1024 * channels;
    let tol = tolerance::tolerances("aac")[0];
    let detail = match compare::diff_pcm_frames(&our_pcm, &ref_pcm, frame_samples) {
        Err(e) => format!("pcm-frames: FAIL（{e}）"),
        Ok(report) => match tol.check_pcm_frames(&report) {
            Ok(()) => {
                let _ = std::fs::remove_file(
                    cache_dir.join("failures").join(format!("{name}.fail.txt")),
                );
                let over1: usize = report.frame_over1_counts.iter().sum();
                format!(
                    "pcm-frames: PASS ({} 帧×{} 交错样本, max_abs={}, |Δ|>1 样本 {}/{})",
                    report.frames,
                    frame_samples,
                    report.max_abs,
                    over1,
                    report.frames * frame_samples
                )
            }
            Err(tol_msg) => {
                let dir = cache_dir.join("failures");
                let _ = std::fs::create_dir_all(&dir);
                let path = dir.join(format!("{name}.fail.txt"));
                let mut text = format!(
                    "vector={name}
"
                );
                text.push_str(&format!(
                    "tolerance={tol_msg}
"
                ));
                if let Some((f, i)) = report.max_abs_at {
                    text.push_str(&format!(
                        "first-max-abs: frame={} sample={} max_abs={}
",
                        f, i, report.max_abs
                    ));
                }
                let _ = std::fs::write(&path, text);
                format!("pcm-frames: FAIL（{tol_msg}；归档 {}）", path.display())
            }
        },
    };
    if detail.starts_with("pcm-frames: PASS") {
        Ok(detail)
    } else {
        Err(detail)
    }
}

fn ffmpeg_rawvideo_yuv(input: &Path, out: &Path) -> Result<(), String> {
    let status = Command::new("ffmpeg")
        .args(["-y", "-v", "error", "-i"])
        .arg(input)
        .args(["-map", "0:v:0", "-pix_fmt", "yuv420p", "-f", "rawvideo"])
        .arg(out)
        .status()
        .map_err(|e| format!("spawn ffmpeg: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("ffmpeg 退出码 {status}"))
    }
}

/// SPS chroma_format_idc → 对拍采样布局（逐帧口径仅支持 420：
/// ffmpeg 参考侧已被 `-pix_fmt yuv420p` 规格化，非 420 码流两侧
/// 布局不可直接对齐，明确失败而非静默转换）。
fn chroma_sampling(sps: &Sps) -> Option<ChromaSampling> {
    match sps.chroma_format_idc {
        1 => Some(ChromaSampling::Yuv420),
        _ => None,
    }
}

/// 施工图②失败归档四元组：向量名 / 首差帧 / 首差宏块位置 / max-abs
/// （外加容差判据行与帧数），落 `<cache>/failures/<name>.fail.txt`；
/// 向量重新通过时由 runner 清除。返回归档路径（写失败也不阻断对拍
/// 报告——失败明细已完整打在 FAIL 行里）。
fn write_failure_archive(
    cache_dir: &Path,
    name: &str,
    report: &FrameDiffReport,
    chroma: ChromaSampling,
    tol_msg: &str,
) -> PathBuf {
    let dir = cache_dir.join("failures");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join(format!("{name}.fail.txt"));
    let mut text = format!("vector={name}\n");
    text.push_str(&format!("tolerance={tol_msg}\n"));
    for line in report.archive_lines(chroma) {
        text.push_str(&line);
        text.push('\n');
    }
    let _ = std::fs::write(&path, text);
    path
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

/// 诊断输出：逐 NAL 的 (序号, type, len)。
fn dump_nals(side: &str, nals: &[Vec<u8>]) {
    eprintln!("--- {side}: {} NALs", nals.len());
    for (i, nal) in nals.iter().enumerate() {
        eprintln!("  [{i:>3}] type={} len={}", nal_type(nal), nal.len());
    }
}

// main.rs 无单元测试：可离线单测的逻辑都在 lib（参数集对齐见
// `nalalign`，容差裁决见 `tolerance`，逐帧定位见 `compare`），
// ffmpeg/ffprobe 依赖的编排以真实运行验证（README「摘要行机制验证」）。
