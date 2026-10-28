//! 编码对拍共享基建：ffmpeg/ffprobe 定位器（ye 仓 `tests/common` 同款
//! 三档口径的复制件——本 crate 独立测试二进制不跨仓引依赖）。
//!
//! 定位顺序：
//!
//! 1. 环境变量 `BJ_FFMPEG_DIR`（目录，须 ffmpeg+ffprobe **双 exe 齐备**
//!    才认，缺一视为未配置走下一档）；
//! 2. 系统 `PATH`（ffmpeg/ffprobe 各自独立查找，Windows 补 `.exe`）；
//! 3. 仓相对路径 `CARGO_MANIFEST_DIR/../../../duan/src-tauri/binaries`
//!    （dev 机 ffmpeg 实际落点）；
//! 4. 全落空：**打印原因**，调用方按分级 skip 语义合法返回——定位器
//!    命中而生成/解码失败（ffmpeg 退出非零/输出未落盘）＝测试失败，
//!    **不许 skip**。

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// exe 文件名（Windows 补 `.exe` 后缀）。
fn exe_name(tool: &str) -> String {
    if cfg!(windows) {
        format!("{tool}.exe")
    } else {
        tool.to_string()
    }
}

/// 目录内双 exe（ffmpeg+ffprobe）齐备才认。
fn dir_has_both_tools(dir: &Path) -> bool {
    dir.join(exe_name("ffmpeg")).is_file() && dir.join(exe_name("ffprobe")).is_file()
}

/// `PATH` 中单工具查找（Windows 补 `.exe`）。
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

/// 定位 ffmpeg/ffprobe 可执行文件对（全落空打印原因返回 `(None, None)`）。
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
