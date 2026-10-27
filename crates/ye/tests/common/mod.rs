//! 集成测试共享基建：ffmpeg 定位器 + 波形 AAC 向量配方。
//!
//! 定位顺序（m2-jian-plan §25 两仓统一口径，与 jian/duan 的
//! ffmpeg 路径解析同义）：
//!
//! 1. 环境变量 `BJ_FFMPEG_DIR`（目录，须 ffmpeg+ffprobe **双 exe 齐备**
//!    才认，缺一视为未配置走下一档——jian 既有语义）；
//! 2. 系统 `PATH`（ffmpeg/ffprobe 各自独立查找，Windows 补 `.exe`——
//!    jian `find_in_path` 同义）；
//! 3. 仓相对路径 `CARGO_MANIFEST_DIR/../../../duan/src-tauri/binaries`
//!    （ye 测试视角的 duan binaries，dev 机 ffmpeg 实际落点）；
//! 4. 全落空：**打印原因**，调用方按分级 skip 语义合法返回——定位器
//!    命中而向量生成失败（ffmpeg 退出非零/输出未落盘）＝测试失败，
//!    **不许 skip**（§3.1 skip 语义分级；CI 无 duan 检出恒走 ④，
//!    门禁不红）。
//!
//! 向量配方（§3.1 lavfi 确定性源，仅追加 `-v error` 静音冗余日志，
//! 不改内容口径）写死不改；T3 对拍测试**只读复用**本模块（需改动须回
//! 本文件属主，走依赖串行）。

// 供多个测试二进制按需取用（本文件的定位器/双 exe/两条向量配方不会
// 被任一二进制同时用齐，T3 亦然），未用条目按共享基建约定豁免检查。
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// exe 文件名（Windows 补 `.exe` 后缀）。
fn exe_name(tool: &str) -> String {
    if cfg!(windows) {
        format!("{tool}.exe")
    } else {
        tool.to_string()
    }
}

/// 目录内双 exe（ffmpeg+ffprobe）齐备才认（§2.5 ①档语义）。
fn dir_has_both_tools(dir: &Path) -> bool {
    dir.join(exe_name("ffmpeg")).is_file() && dir.join(exe_name("ffprobe")).is_file()
}

/// `PATH` 中单工具查找（Windows 补 `.exe`；jian `find_in_path` 同义，
/// 单独使用不要求双全——双全约束只在目录档）。
fn find_in_path(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = if cfg!(windows) {
            dir.join(format!("{tool}.exe"))
        } else {
            dir.join(tool)
        };
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 定位 ffmpeg/ffprobe 可执行文件对（§2.5 顺序；全落空打印原因返回
/// `(None, None)`）。
///
/// ①③ 目录档要求双 exe 齐备；② PATH 档两工具独立查找（找到 ffmpeg
/// 但 PATH 无 ffprobe → 仍算落空，打印原因）。
fn resolve_tool_pair() -> (Option<PathBuf>, Option<PathBuf>) {
    // ① BJ_FFMPEG_DIR：双全才认。
    if let Some(dir) = std::env::var_os("BJ_FFMPEG_DIR").map(PathBuf::from) {
        if dir_has_both_tools(&dir) {
            return (
                Some(dir.join(exe_name("ffmpeg"))),
                Some(dir.join(exe_name("ffprobe"))),
            );
        }
        eprintln!(
            "common: BJ_FFMPEG_DIR 已设但 ffmpeg/ffprobe 未双全（{}），转下一档",
            dir.display()
        );
    }
    // ② PATH：两工具独立查找。
    if let Some(ffmpeg) = find_in_path("ffmpeg") {
        return match find_in_path("ffprobe") {
            Some(ffprobe) => (Some(ffmpeg), Some(ffprobe)),
            None => {
                eprintln!(
                    "common: PATH 有 ffmpeg（{}）但无 ffprobe，转下一档",
                    ffmpeg.display()
                );
                (None, None)
            }
        };
    }
    // ③ 仓相对：duan binaries（dev 机 ffmpeg 实际落点）。
    let repo_relative =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../duan/src-tauri/binaries");
    if dir_has_both_tools(&repo_relative) {
        return (
            Some(repo_relative.join(exe_name("ffmpeg"))),
            Some(repo_relative.join(exe_name("ffprobe"))),
        );
    }
    // ④ 全落空：打印原因（调用方按分级 skip 语义合法返回）。
    eprintln!(
        "common: ffmpeg 定位三档全落空（BJ_FFMPEG_DIR 未设或未双全；PATH 无 ffmpeg+ffprobe；\
         仓相对 {} 不存在）——依赖 ffmpeg 的测试按分级 skip 语义跳过",
        repo_relative.display()
    );
    (None, None)
}

/// 已解析的工具对（进程内缓存，原因只打印一次）。
fn tool_pair() -> &'static (Option<PathBuf>, Option<PathBuf>) {
    static PAIR: OnceLock<(Option<PathBuf>, Option<PathBuf>)> = OnceLock::new();
    PAIR.get_or_init(resolve_tool_pair)
}

/// ffmpeg 可执行文件全路径（定位失败返回 `None`，原因已打印）。
pub fn ffmpeg_exe() -> Option<PathBuf> {
    tool_pair().0.clone()
}

/// ffprobe 可执行文件全路径（同目录/同档推导）。
pub fn ffprobe_exe() -> Option<PathBuf> {
    tool_pair().1.clone()
}

/// vec-audio.m4a 配方（§3.1：立体声 440Hz 正弦 48k、2s、AAC 128k，
/// `-aac_pns 0`——ye AAC 不支持 PNS，conformance 先例）。
pub const VEC_AUDIO_ARGS: &[&str] = &[
    "-v",
    "error",
    "-f",
    "lavfi",
    "-i",
    "sine=frequency=440:sample_rate=48000:duration=2",
    "-ac",
    "2",
    "-c:a",
    "aac",
    "-aac_pns",
    "0",
    "-b:a",
    "128k",
    "-y",
];

/// vec-noise.m4a 配方（§3.1：单声道白噪声 44.1k、2s、AAC 128k——rms
/// 有区分度且对样本错位敏感，能暴露 edit list 口径错误）。
pub const VEC_NOISE_ARGS: &[&str] = &[
    "-v",
    "error",
    "-f",
    "lavfi",
    "-i",
    "anoisesrc=color=white:sample_rate=44100:duration=2",
    "-ac",
    "1",
    "-c:a",
    "aac",
    "-aac_pns",
    "0",
    "-b:a",
    "128k",
    "-y",
];

/// 就位一个向量：定位器命中 → 按配方生成到 `CARGO_TARGET_TMPDIR`
/// （唯一文件名，无并发竞争；配方确定性 → 内容跨调用一致）并返回
/// 路径（**用后即删归调用方**，仓库约定）；定位器落空 → `None`
/// （原因已打印，调用方合法 skip）。
///
/// 定位器命中而生成失败（启动失败/退出非零/输出未落盘）＝ panic
/// （测试失败），**不许 skip**——§3.1 分级语义的失败侧。
#[must_use]
pub fn ensure_vector(name: &str, recipe_args: &[&str]) -> Option<PathBuf> {
    let exe = ffmpeg_exe()?;
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&dir).expect("CARGO_TARGET_TMPDIR 建目录");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // 唯一后缀放在扩展名之前（ffmpeg 按扩展名推输出格式，mangle 扩展
    // 名会报 Unable to choose an output format）。
    let plain = Path::new(name);
    let stem = plain.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = plain
        .extension()
        .map_or_else(String::new, |e| format!(".{}", e.to_string_lossy()));
    let path = dir.join(format!(
        "ye-waveform-{stem}-{}-{nanos}{ext}",
        std::process::id()
    ));
    let status = Command::new(&exe)
        .args(recipe_args)
        .arg(&path)
        .status()
        .unwrap_or_else(|err| panic!("启动 ffmpeg 失败（{}）：{err}", exe.display()));
    assert!(
        status.success(),
        "ffmpeg 生成 {name} 失败（退出码 {status:?}）——定位器已命中，生成失败不许 skip"
    );
    assert!(
        path.is_file(),
        "ffmpeg 报成功但 {name} 未落盘：{}",
        path.display()
    );
    Some(path)
}
