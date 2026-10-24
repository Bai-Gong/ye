//! `ye` 命令行：媒体探查与转码，对标 ffmpeg / ffprobe。
//!
//! 本文件只做 argv 分发与输出；逻辑在 `ye_cli` lib（可测试主体）。

use std::ffi::OsString;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match ye_cli::parse_args(&args) {
        Ok(ye_cli::Command::Help) => {
            println!("{}", ye_cli::USAGE);
            ExitCode::SUCCESS
        }
        Ok(ye_cli::Command::Version) => {
            println!("ye {}", ye::VERSION);
            ExitCode::SUCCESS
        }
        Ok(ye_cli::Command::Probe { path }) => match ye_cli::probe(&path) {
            Ok(report) => {
                print!("{report}");
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("ye: {err}");
                ExitCode::FAILURE
            }
        },
        Ok(ye_cli::Command::Remux { input, output }) => match ye_cli::remux(&input, &output) {
            Ok(report) => {
                print!("{report}");
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("ye: {err}");
                ExitCode::FAILURE
            }
        },
        Ok(ye_cli::Command::Convert { input, output }) => {
            // CLI 形态即接入契约示范：进度回调（桩内不会触发）+ 取消旗标。
            // 命令行无交互取消入口，旗标恒 false；真转码落地后进度改打 stderr。
            let progress = |processed_us: i64, total_us: i64| {
                eprintln!("ye: 转码进度 {processed_us}/{total_us} µs");
            };
            let cancel = AtomicBool::new(false);
            match ye_cli::convert(&input, &output, &progress, &cancel) {
                Ok(report) => {
                    print!("{report}");
                    ExitCode::SUCCESS
                }
                Err(err) => {
                    eprintln!("ye: {err}");
                    ExitCode::FAILURE
                }
            }
        }
        Err(usage) => {
            eprintln!("ye: {usage}");
            ExitCode::from(2)
        }
    }
}
