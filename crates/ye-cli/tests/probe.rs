//! `ye probe` 集成测试：手工构造 MP4 字节（零外部文件零网络），
//! 走真实的 `Mp4Reader::open`（临时文件）路径，断言 M1 验收口径——
//! 能列出每路流的编解码、宽高/采样率、时基、时长。
//!
//! 构造器是 ye-container-mp4/tests/helpers 的裁剪副本
//! （测试辅助不跨 crate 导出，重复是接受的成本）。

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use ye_cli::{Command, parse_args, probe};

// —— MP4 构造（与 ye-container-mp4 黄金路径同布局）——

fn be16(v: u16) -> [u8; 2] {
    v.to_be_bytes()
}

fn be32(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

fn push(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend_from_slice(bytes);
}

fn kind4(kind: &str) -> [u8; 4] {
    kind.as_bytes().try_into().unwrap()
}

/// 8 字节头（size u32 + type）+ 载荷。
fn r#box(kind: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    push(&mut out, &be32(8 + payload.len() as u32));
    push(&mut out, &kind4(kind));
    push(&mut out, payload);
    out
}

fn wrap(kind: &str, children: &[Vec<u8>]) -> Vec<u8> {
    let mut payload = Vec::new();
    for c in children {
        push(&mut payload, c);
    }
    r#box(kind, &payload)
}

fn ftyp() -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, b"isom");
    push(&mut p, &be32(0x200));
    push(&mut p, b"isomiso2avc1mp4a");
    r#box("ftyp", &p)
}

/// mvhd v0（timescale=1000, duration=30, next_track_ID=8）。
fn mvhd() -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be32(0)); // version 0 + flags
    push(&mut p, &be32(0)); // creation_time
    push(&mut p, &be32(0)); // modification_time
    push(&mut p, &be32(1000)); // timescale
    push(&mut p, &be32(30)); // duration
    push(&mut p, &be32(0x0001_0000)); // rate
    push(&mut p, &be16(0x0100)); // volume
    push(&mut p, &be16(0)); // reserved
    push(&mut p, &be32(0));
    push(&mut p, &be32(0)); // reserved
    for v in [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        push(&mut p, &be32(v)); // matrix
    }
    for _ in 0..6 {
        push(&mut p, &be32(0)); // pre_defined
    }
    push(&mut p, &be32(8)); // next_track_ID（解析端不校验）
    r#box("mvhd", &p)
}

fn tkhd(track_id: u32, duration: u32) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be32(0x0000_0003)); // version 0 + flags(enabled|in_movie)
    push(&mut p, &be32(0));
    push(&mut p, &be32(0)); // creation/modification
    push(&mut p, &be32(track_id));
    push(&mut p, &be32(0)); // reserved
    push(&mut p, &be32(duration));
    push(&mut p, &be32(0));
    push(&mut p, &be32(0)); // reserved
    push(&mut p, &be16(0)); // layer
    push(&mut p, &be16(0)); // alternate_group
    push(&mut p, &be16(0)); // volume
    push(&mut p, &be16(0)); // reserved
    for v in [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000] {
        push(&mut p, &be32(v)); // matrix
    }
    push(&mut p, &be32(0)); // width 16.16（读取端不用）
    push(&mut p, &be32(0)); // height 16.16
    r#box("tkhd", &p)
}

fn mdhd(timescale: u32, duration: u32) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be32(0)); // version 0 + flags
    push(&mut p, &be32(0));
    push(&mut p, &be32(0)); // creation/modification
    push(&mut p, &be32(timescale));
    push(&mut p, &be32(duration));
    push(&mut p, &be16(0x55C4)); // language=und
    push(&mut p, &be16(0)); // pre_defined
    r#box("mdhd", &p)
}

fn hdlr(handler: &str) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be32(0)); // version + flags
    push(&mut p, &kind4(handler));
    push(&mut p, &[0; 12]); // reserved
    push(&mut p, b"Handler\0");
    r#box("hdlr", &p)
}

/// avc1 visual entry（固定字段 78 字节）+ avcC 子 box。
fn stsd_video(w: u16, h: u16, avcc: &[u8]) -> Vec<u8> {
    let mut e = Vec::with_capacity(78);
    push(&mut e, &[0; 6]); // reserved
    push(&mut e, &be16(1)); // data_reference_index
    push(&mut e, &be16(0)); // pre_defined
    push(&mut e, &be16(0)); // reserved
    push(&mut e, &[0; 12]); // pre_defined[3]
    push(&mut e, &be16(w));
    push(&mut e, &be16(h));
    push(&mut e, &be32(0x0048_0000)); // horizresolution 72dpi
    push(&mut e, &be32(0x0048_0000)); // vertresolution
    push(&mut e, &be32(0)); // reserved
    push(&mut e, &be16(1)); // frame_count
    push(&mut e, &be32(0));
    push(&mut e, &[0; 28]); // compressorname 32 字节
    push(&mut e, &be16(24)); // depth
    push(&mut e, &be16(0xFFFF)); // pre_defined = -1
    assert_eq!(e.len(), 78);

    let mut entry = Vec::new();
    let avcc_box = r#box("avcC", avcc);
    push(&mut entry, &be32(8 + 78 + avcc_box.len() as u32));
    push(&mut entry, b"avc1");
    push(&mut entry, &e);
    push(&mut entry, &avcc_box);

    let mut p = Vec::new();
    push(&mut p, &be32(0)); // version + flags
    push(&mut p, &be32(1)); // entry_count
    push(&mut p, &entry);
    r#box("stsd", &p)
}

/// mp4a audio entry（固定字段 20 字节，samplerate 16.16）+ esds 子 box。
fn stsd_audio(ch: u16, rate_hz: u32, esds: &[u8]) -> Vec<u8> {
    let mut e = Vec::with_capacity(20);
    push(&mut e, &[0; 6]); // reserved
    push(&mut e, &be16(1)); // data_reference_index
    push(&mut e, &be16(ch)); // channelcount
    push(&mut e, &be16(16)); // samplesize
    push(&mut e, &be16(0)); // pre_defined
    push(&mut e, &be16(0)); // reserved
    push(&mut e, &be32(rate_hz << 16)); // samplerate 16.16
    assert_eq!(e.len(), 20);

    let mut entry = Vec::new();
    let esds_box = r#box("esds", esds);
    push(&mut entry, &be32(8 + 20 + esds_box.len() as u32));
    push(&mut entry, b"mp4a");
    push(&mut entry, &e);
    push(&mut entry, &esds_box);

    let mut p = Vec::new();
    push(&mut p, &be32(0)); // version + flags
    push(&mut p, &be32(1)); // entry_count
    push(&mut p, &entry);
    r#box("stsd", &p)
}

fn stts(runs: &[(u32, u32)]) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be32(0));
    push(&mut p, &be32(runs.len() as u32));
    for &(count, delta) in runs {
        push(&mut p, &be32(count));
        push(&mut p, &be32(delta));
    }
    r#box("stts", &p)
}

fn stsc(runs: &[(u32, u32, u32)]) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be32(0));
    push(&mut p, &be32(runs.len() as u32));
    for &(first_chunk, spc, desc) in runs {
        push(&mut p, &be32(first_chunk));
        push(&mut p, &be32(spc));
        push(&mut p, &be32(desc));
    }
    r#box("stsc", &p)
}

fn stsz_var(sizes: &[u32]) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be32(0));
    push(&mut p, &be32(0)); // sample_size = 0
    push(&mut p, &be32(sizes.len() as u32));
    for &s in sizes {
        push(&mut p, &be32(s));
    }
    r#box("stsz", &p)
}

fn stco(offsets: &[u32]) -> Vec<u8> {
    let mut p = Vec::new();
    push(&mut p, &be32(0));
    push(&mut p, &be32(offsets.len() as u32));
    for &o in offsets {
        push(&mut p, &be32(o));
    }
    r#box("stco", &p)
}

fn trak(track_id: u32, handler: &str, timescale: u32, duration: u32, stbl: Vec<u8>) -> Vec<u8> {
    wrap(
        "trak",
        &[
            tkhd(track_id, duration),
            wrap(
                "mdia",
                &[
                    mdhd(timescale, duration),
                    hdlr(handler),
                    wrap("minf", &[stbl]),
                ],
            ),
        ],
    )
}

/// avcC 载荷（内容任意，定长便于构造）。
const AVCC: [u8; 14] = [
    0x01, 0x64, 0x00, 0x1E, 0xFF, 0xE1, 0x00, 0x08, 0x1A, 0x2B, 0x3C, 0x4D, 0x5E, 0x6F,
];

/// esds 载荷（含 4 字节版本/标志，内容任意）。
const ESDS: [u8; 29] = [
    0x00, 0x00, 0x00, 0x00, // version/flags
    0x03, 0x19, 0x00, 0x01, 0x00, // ES_Descriptor 头
    0x04, 0x11, 0x40, 0x15, // DecoderConfigDescriptor 头
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // avgBitrate 等
    0x05, 0x02, 0x10, 0x88, // DecspecificInfo
    0x06, 0x01, 0x02, // SLConfigDescriptor
];

/// 视频样例数据 16 字节 + 音频样例数据 4 字节。
const MDAT: [u8; 20] = [
    0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xAB, 0xAC, 0xAD, 0xAE, 0xAF,
    0xB0, 0xB1, 0xB2, 0xB3,
];

/// 双轨 MP4：视频 64x96 avc1（timescale 1000，3 样例，时长 30 单位），
/// 音频 2ch 48000Hz mp4a（timescale 48000，1 样例，时长 48 单位）。
/// stco 偏移先以 0 占位测布局（u32 定宽，回填不影响布局），再回填真值。
fn two_track_mp4() -> Vec<u8> {
    let build = |video_stco: [u32; 2], audio_stco: [u32; 1]| -> Vec<u8> {
        let video_stbl = wrap(
            "stbl",
            &[
                stsd_video(64, 96, &AVCC),
                stts(&[(3, 10)]),
                stsc(&[(1, 2, 1), (2, 1, 1)]),
                stsz_var(&[4, 5, 6]),
                stco(&video_stco),
            ],
        );
        let audio_stbl = wrap(
            "stbl",
            &[
                stsd_audio(2, 48000, &ESDS),
                stts(&[(1, 48)]),
                stsc(&[(1, 1, 1)]),
                stsz_var(&[4]),
                stco(&audio_stco),
            ],
        );
        let moov = wrap(
            "moov",
            &[
                mvhd(),
                trak(7, "vide", 1000, 30, video_stbl),
                trak(3, "soun", 48000, 48, audio_stbl),
            ],
        );
        let mut f = Vec::new();
        push(&mut f, &ftyp());
        push(&mut f, &moov);
        push(&mut f, &r#box("mdat", &MDAT));
        f
    };
    let probe_file = build([0, 0], [0]);
    let mdat_payload = (probe_file.len() - MDAT.len()) as u32;
    build(
        [mdat_payload, mdat_payload + 9], // chunk1=样例0/1(4+5)，chunk2=样例2(6)
        [mdat_payload + 16],              // 音频样例 4 字节
    )
}

fn temp_mp4(bytes: &[u8]) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path =
        std::env::temp_dir().join(format!("ye-cli-probe-{}-{unique}.mp4", std::process::id()));
    std::fs::write(&path, bytes).expect("写临时 MP4 失败");
    path
}

// —— 断言 —-

/// M2 接线后验收口径：`ye probe` 经注册表 auto-probe 识别格式，列出
/// 每路流的编解码、宽高/采样率、时基、时长，另补帧率与码率。
/// 视频时长 30 单位 @ 1/1000（流时基原生 tick）；
/// 帧率 3÷0.030s = 100.000；码率 (4+5+6)×8÷0.030s = 4000bps。
/// 音频时长 48 单位 @ 1/48000；码率 4×8÷0.001s = 32000bps。
#[test]
fn probe_lists_every_stream() {
    let path = temp_mp4(&two_track_mp4());
    let _guard = scopeguard(&path); // 无论断言成败都清理临时文件
    let out = probe(&path).expect("probe 双轨 MP4 应成功");

    assert!(
        out.contains(": mp4, 2 路流"),
        "报告头应声明容器（注册表识别）与流数，实际:\n{out}"
    );
    assert!(
        out.contains(
            "流 #0 [视频] codec=h264 时基=1/1000 时长=0.030s (30 tick) 样例数=3 帧率≈100.000（样例数÷时长近似，VFR 文件有偏差） 尺寸=64x96 码率=4000bps"
        ),
        "视频流行应完整（含帧率近似与 VFR 偏差注明、码率），实际:\n{out}"
    );
    assert!(
        out.contains(
            "流 #1 [音频] codec=aac 时基=1/48000 时长=0.001s (48 tick) 样例数=1 声道=2 采样率=48000Hz 码率=32000bps"
        ),
        "音频流行应完整（含码率），实际:\n{out}"
    );
}

#[test]
fn probe_reports_open_failure_with_path() {
    let missing = std::env::temp_dir().join("ye-cli-probe-no-such-file.mp4");
    let err = probe(&missing).expect_err("打开不存在的文件应失败");
    assert!(
        err.contains(missing.display().to_string().as_str()),
        "错误信息应含路径，实际: {err}"
    );
}

fn scopeguard(path: &std::path::Path) -> impl Drop + '_ {
    struct Remove<'a>(&'a std::path::Path);
    impl Drop for Remove<'_> {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(self.0);
        }
    }
    Remove(path)
}

// —— 真二进制端到端（cargo 提供的 CARGO_BIN_EXE_ye 指向已编译的 ye.exe）——

#[test]
fn binary_probe_end_to_end() {
    let path = temp_mp4(&two_track_mp4());
    let _guard = scopeguard(&path);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ye"))
        .arg("probe")
        .arg(&path)
        .output()
        .expect("启动 ye 二进制失败");

    assert!(out.status.success(), "probe 应以 0 退出");
    let stdout = String::from_utf8(out.stdout).expect("stdout 应为 UTF-8");
    println!("---- ye probe 实际输出 ----\n{stdout}---------------------------");
    assert!(
        stdout.contains("流 #0 [视频] codec=h264") && stdout.contains("尺寸=64x96"),
        "二进制 stdout 应含视频流信息，实际:\n{stdout}"
    );
    assert!(
        stdout.contains("流 #1 [音频] codec=aac") && stdout.contains("采样率=48000Hz"),
        "二进制 stdout 应含音频流信息，实际:\n{stdout}"
    );
}

#[test]
fn binary_exit_codes() {
    let bin = env!("CARGO_BIN_EXE_ye");

    let version = std::process::Command::new(bin)
        .arg("--version")
        .output()
        .expect("启动 ye 二进制失败");
    assert!(version.status.success());
    assert_eq!(
        String::from_utf8(version.stdout).unwrap(),
        format!("ye {}\n", ye::VERSION)
    );

    let help = std::process::Command::new(bin)
        .arg("-h")
        .output()
        .expect("启动 ye 二进制失败");
    assert!(help.status.success());
    assert!(String::from_utf8(help.stdout).unwrap().contains("ye probe"));

    let bad = std::process::Command::new(bin)
        .arg("frobnicate")
        .output()
        .expect("启动 ye 二进制失败");
    assert_eq!(
        bad.status.code(),
        Some(2),
        "未知命令应以退出码 2 报用法错误"
    );
    assert!(String::from_utf8(bad.stderr).unwrap().contains("未知命令"));
}

// —— convert（M3 集成：真转码；e2e 对拍在 tests/convert_e2e.rs）——

/// lib 层直调：输入打不开时报错含路径前缀，进度回调不触发、不产出
/// 输出文件（真转码的失败面；ffmpeg oracle 侧的端到端在
/// `convert_e2e.rs`）。
#[test]
fn convert_reports_open_failure_with_path() {
    use std::sync::atomic::AtomicBool;

    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&dir).unwrap();
    let missing = dir.join("no-such-convert-input.mp4");
    let out = dir.join("no-such-convert-output.mp4");
    let cancel = AtomicBool::new(false);
    let called = std::cell::Cell::new(false);
    let progress = |_, _| called.set(true);
    let err = ye_cli::convert(&missing, &out, None, &progress, &cancel)
        .expect_err("打开不存在的文件应失败");
    assert!(
        err.contains(missing.display().to_string().as_str()),
        "错误信息应含输入路径，实际: {err}"
    );
    assert!(!called.get(), "打不开输入不得触发进度回调");
    assert!(!out.exists(), "失败不得产出输出文件");
}

/// 真二进制调用 `ye convert`：坏尺寸参数报用法错误（退出码 2）。
#[test]
fn binary_convert_rejects_bad_size() {
    let src = temp_mp4(&two_track_mp4());
    let _guard = scopeguard(&src);
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dst = std::env::temp_dir().join(format!(
        "ye-cli-convert-badsize-{}-{unique}.mp4",
        std::process::id()
    ));
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ye"))
        .arg("convert")
        .arg(&src)
        .arg(&dst)
        .arg("--size")
        .arg("321x240") // 奇数宽：yuv420p 平面几何拒绝
        .output()
        .expect("启动 ye 二进制失败");
    assert_eq!(
        out.status.code(),
        Some(2),
        "坏 --size 应以退出码 2 报用法错误"
    );
    assert!(!dst.exists(), "用法错误不产出输出文件");
}

// —— 参数解析 ——

#[test]
fn parse_args_accepts_probe_help_version() {
    let probe_case = parse_args(&[OsString::from("probe"), OsString::from("a.mp4")]);
    assert_eq!(
        probe_case,
        Ok(Command::Probe {
            path: PathBuf::from("a.mp4")
        })
    );
    assert_eq!(parse_args(&[OsString::from("--help")]), Ok(Command::Help));
    assert_eq!(parse_args(&[OsString::from("-h")]), Ok(Command::Help));
    assert_eq!(
        parse_args(&[OsString::from("--version")]),
        Ok(Command::Version)
    );
    assert_eq!(parse_args(&[OsString::from("-V")]), Ok(Command::Version));
}

#[test]
fn parse_args_rejects_bad_usage() {
    assert!(parse_args(&[]).is_err(), "无参数应报用法错误");
    assert!(
        parse_args(&[OsString::from("probe")]).is_err(),
        "probe 缺文件参数应报用法错误"
    );
    assert!(
        parse_args(&[
            OsString::from("probe"),
            OsString::from("a"),
            OsString::from("b")
        ])
        .is_err(),
        "probe 多余参数应报用法错误"
    );
    assert!(
        parse_args(&[OsString::from("convert")]).is_err(),
        "convert 缺参数应报用法错误"
    );
    assert!(
        parse_args(&[OsString::from("frobnicate")]).is_err(),
        "未知命令应报用法错误"
    );
}

#[test]
fn parse_args_accepts_convert() {
    // 裸双路径（size = None）。
    let case = parse_args(&[
        OsString::from("convert"),
        OsString::from("in.mp4"),
        OsString::from("out.mp4"),
    ]);
    assert_eq!(
        case,
        Ok(Command::Convert {
            input: PathBuf::from("in.mp4"),
            output: PathBuf::from("out.mp4"),
            size: None
        })
    );
    // --size 在路径后。
    let case = parse_args(&[
        OsString::from("convert"),
        OsString::from("in.mp4"),
        OsString::from("out.mp4"),
        OsString::from("--size"),
        OsString::from("320x240"),
    ]);
    assert_eq!(
        case,
        Ok(Command::Convert {
            input: PathBuf::from("in.mp4"),
            output: PathBuf::from("out.mp4"),
            size: Some((320, 240))
        })
    );
    // --size 在路径间（选项与位置参数可混排）。
    let case = parse_args(&[
        OsString::from("convert"),
        OsString::from("--size"),
        OsString::from("320x240"),
        OsString::from("in.mp4"),
        OsString::from("out.mp4"),
    ]);
    assert!(matches!(
        case,
        Ok(Command::Convert {
            size: Some((320, 240)),
            ..
        })
    ));
    // 坏形态：奇数尺寸 / 缺值 / 重复。
    assert!(
        parse_args(&[
            OsString::from("convert"),
            OsString::from("in.mp4"),
            OsString::from("out.mp4"),
            OsString::from("--size"),
            OsString::from("321x240")
        ])
        .is_err(),
        "奇数宽应报用法错误"
    );
    assert!(
        parse_args(&[
            OsString::from("convert"),
            OsString::from("in.mp4"),
            OsString::from("out.mp4"),
            OsString::from("--size")
        ])
        .is_err(),
        "--size 缺值应报用法错误"
    );
    assert!(
        parse_args(&[
            OsString::from("convert"),
            OsString::from("in.mp4"),
            OsString::from("out.mp4"),
            OsString::from("--size"),
            OsString::from("320x240"),
            OsString::from("--size"),
            OsString::from("320x240"),
        ])
        .is_err(),
        "重复 --size 应报用法错误"
    );
    assert!(
        parse_args(&[
            OsString::from("convert"),
            OsString::from("in.mp4"),
            OsString::from("out.mp4"),
            OsString::from("extra.mp4"),
        ])
        .is_err(),
        "多余路径应报用法错误"
    );
}
