//! CABAC progressive 8-bit 4:2:0 真流回归：确定性 x264 素材逐帧比较 Y/Cb/Cr。
//! 全 I、I/P、B 金字塔均要求 bit-exact；不放宽容差，不跳过解码失败。
//! ffmpeg/ffprobe 缺席允许打印 skip，工具在场则所有错误均失败。

use std::path::{Path, PathBuf};
use std::process::Command;

use ye_codec::{Decoded, Decoder, Frame};
use ye_codec_h264::H264Decoder;
use ye_container::Demuxer;
use ye_container_mp4::Mp4Demuxer;

// ---- 工具定位（三档：BJ_FFMPEG_DIR → PATH → 仓相对 duan binaries） ----

fn exe_name(tool: &str) -> String {
    if cfg!(windows) {
        format!("{tool}.exe")
    } else {
        tool.to_string()
    }
}

fn find_tool(tool: &str) -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("BJ_FFMPEG_DIR") {
        let p = Path::new(&dir).join(exe_name(tool));
        if p.is_file() {
            return Some(p);
        }
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let sibling = manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|root| root.join("duan/src-tauri/binaries"))
        .unwrap_or_else(|| manifest.join("../../duan/src-tauri/binaries"));
    let p = sibling.join(exe_name(tool));
    if p.is_file() {
        return Some(p);
    }
    // 系统 PATH
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let p = dir.join(exe_name(tool));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

fn tools() -> Option<(PathBuf, PathBuf)> {
    let ffmpeg = find_tool("ffmpeg")?;
    let ffprobe = find_tool("ffprobe")?;
    Some((ffmpeg, ffprobe))
}

// ---- 向量生成（lavfi 确定性配方） ----

/// 造一个 CABAC MP4：testsrc2 3s 176x144@15 + 指定 x264 参数。
fn gen_vector(dir: &Path, ffmpeg: &Path, tag: &str, x264: &str) -> PathBuf {
    let lavfi = std::env::var("YE_LAVFI")
        .unwrap_or_else(|_| "testsrc2=duration=3:size=176x144:rate=15".into());
    let out = dir.join(format!("cabac-{tag}.mp4"));
    let st = Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg(lavfi)
        .args([
            "-c:v",
            "libx264",
            "-profile:v",
            "high",
            "-pix_fmt",
            "yuv420p",
            "-x264-params",
        ])
        .arg(x264)
        .arg(&out)
        .status()
        .expect("ffmpeg 应可执行（tools 已确认）");
    assert!(st.success(), "ffmpeg 造 {tag} 向量失败：{}", out.display());
    out
}

// ---- ye 解码 → 每帧紧致 YUV420P ----

fn decode_yuv_frames(path: &Path) -> Vec<(u32, u32, Vec<u8>)> {
    let file = std::fs::File::open(path).expect("打开向量");
    let mut demux = Mp4Demuxer::new(Box::new(std::io::BufReader::new(file))).expect("解封装");
    let header = demux.read_header().expect("读头");
    let video = header
        .streams
        .iter()
        .find(|s| s.media_type == ye_codec::MediaType::Video)
        .expect("应有视频流")
        .clone();
    let mut dec = H264Decoder::new(&video.codec).expect("解码器构造");
    let mut frames = Vec::new();
    let mut pkt_index = 0usize;
    while let Some(pkt) = demux.read_packet().expect("读包") {
        let mut packet = ye_codec::Packet::empty(0);
        packet.data = pkt.data;
        if let Err(e) = dec.feed(&packet) {
            panic!("feed 第 {pkt_index} 包失败：{e:?}");
        }
        pkt_index += 1;
        loop {
            match dec.pull().expect("pull") {
                Decoded::Frame(Frame::Video(f)) => {
                    let (w, h) = (f.width(), f.height());
                    let mut luma = Vec::new();
                    for plane in 0..3 {
                        let stride = f.layout().plane(plane).stride;
                        let (pw, ph) = if plane == 0 {
                            (w as usize, h as usize)
                        } else {
                            (w as usize / 2, h as usize / 2)
                        };
                        for row in f.plane(plane)[..ph * stride].chunks_exact(stride) {
                            luma.extend_from_slice(&row[..pw]);
                        }
                    }
                    frames.push((w, h, luma));
                }
                Decoded::Frame(Frame::Audio(_)) => {}
                Decoded::NeedInput | Decoded::Finished => break,
            }
        }
    }
    dec.flush();
    loop {
        match dec.pull().expect("pull 排空") {
            Decoded::Frame(Frame::Video(f)) => {
                let (w, h) = (f.width(), f.height());
                let mut luma = Vec::new();
                for plane in 0..3 {
                    let stride = f.layout().plane(plane).stride;
                    let (pw, ph) = if plane == 0 {
                        (w as usize, h as usize)
                    } else {
                        (w as usize / 2, h as usize / 2)
                    };
                    for row in f.plane(plane)[..ph * stride].chunks_exact(stride) {
                        luma.extend_from_slice(&row[..pw]);
                    }
                }
                frames.push((w, h, luma));
            }
            Decoded::Frame(Frame::Audio(_)) => {}
            Decoded::NeedInput | Decoded::Finished => break,
        }
    }
    frames
}

// ---- FFmpeg 参照解码（显示序、完整 YUV420P） ----
// 口径注意：不用 -pix_fmt gray——swscale 的 yuv420p→gray 转换会整体
// 抬值（平色实测 126 → 128），与解码正确性无关；yuv420p 参照的 luma
// 平面才是解码原值（M2 像素对拍同口径）。

fn ffmpeg_yuv_frames(ffmpeg: &Path, path: &Path, count: usize, w: usize, h: usize) -> Vec<u8> {
    let out = Command::new(ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-pix_fmt", "yuv420p", "-f", "rawvideo", "-"])
        .output()
        .expect("启动 ffmpeg 失败");
    assert!(
        out.status.success() && out.stderr.is_empty(),
        "ffmpeg 参照解码失败：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout.len(), count * w * h * 3 / 2, "参照帧数与尺寸");
    out.stdout
}

fn probe_frame_count(ffprobe: &Path, path: &Path) -> usize {
    let out = Command::new(ffprobe)
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-count_frames",
            "-show_entries",
            "stream=nb_read_frames",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
        ])
        .arg(path)
        .output()
        .expect("启动 ffprobe 失败");
    assert!(out.status.success(), "ffprobe 失败");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("nb_read_frames 应为整数")
}

// ---- 对拍 ----

fn run_case(tag: &str, x264: &str) {
    let Some((ffmpeg, ffprobe)) = tools() else {
        eprintln!("跳过：未定位 ffmpeg/ffprobe（cabac {tag}）");
        return;
    };
    let dir = std::env::temp_dir().join(format!("ye-cabac-oracle-{}-{tag}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = gen_vector(&dir, &ffmpeg, tag, x264);

    let count = probe_frame_count(&ffprobe, &src);
    assert_eq!(count, 45, "确定性配方应为 45 帧（{tag}）");

    let ye_frames = decode_yuv_frames(&src);
    {
        assert_eq!(
            ye_frames.len(),
            count,
            "ye 解码帧数应与 ffprobe 一致（{tag}）"
        );
    }
    let (vw, vh) = (ye_frames[0].0 as usize, ye_frames[0].1 as usize);
    let frame_len = vw * vh * 3 / 2;
    let reference = ffmpeg_yuv_frames(&ffmpeg, &src, count, vw, vh);
    assert_eq!(
        reference.len(),
        count * frame_len,
        "ffmpeg 参照帧总长（{tag}）"
    );

    let mut worst = 0usize;
    for (i, (w, h, luma)) in ye_frames.iter().enumerate() {
        assert_eq!((*w as usize, *h as usize), (vw, vh), "帧 {i} 尺寸（{tag}）");
        let ref_frame = &reference[i * frame_len..(i + 1) * frame_len];
        let mut bad = 0usize;
        let mut max_diff = 0u8;

        for (a, b) in luma.iter().zip(ref_frame) {
            let d = a.abs_diff(*b);
            if d > 0 {
                bad += 1;
            }
            max_diff = max_diff.max(d);
        }
        if bad > 0 {
            std::fs::write(dir.join(format!("ours-{i}.yuv")), luma).unwrap();
            std::fs::write(dir.join(format!("ref-{i}.yuv")), ref_frame).unwrap();
            eprintln!(
                "DIFF {dir:?}: {:?}",
                luma.iter()
                    .zip(ref_frame)
                    .enumerate()
                    .filter(|(_, (a, b))| a.abs_diff(**b) > 0)
                    .take(12)
                    .map(|(p, (a, b))| (p % vw, p / vw, *a, *b))
                    .collect::<Vec<_>>()
            );
        }
        assert!(
            bad == 0,
            "{tag} 帧 {i} 有 {bad} 样本不一致（max |Δ|={max_diff}）——CABAC 解码与 ffmpeg 分歧"
        );
        worst = worst.max(max_diff as usize);
    }
    eprintln!("cabac {tag}: {count} 帧 Y/Cb/Cr 全对拍通过，实测最大 |Δ|={worst}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cabac_全_i_帧对拍() {
    run_case("all-i", "keyint=1:min-keyint=1:scenecut=0:bframes=0");
}
#[test]
fn cabac_i_p_长链对拍() {
    run_case("ip", "keyint=30:min-keyint=30:scenecut=0:bframes=0:ref=1");
}
#[test]
fn cabac_带_b_帧对拍() {
    run_case("b", "keyint=30:min-keyint=30:scenecut=0:bframes=3");
}
#[test]
fn cabac_b_禁用去块对拍() {
    run_case(
        "b-nodeblock",
        "keyint=30:min-keyint=30:scenecut=0:bframes=3:no-deblock=1",
    );
}
#[test]
fn cabac_b_无加权对拍() {
    run_case(
        "b-unweighted",
        "keyint=30:min-keyint=30:scenecut=0:bframes=3:weightb=0",
    );
}

#[test]
fn p_skip_weighted_color_changes_match_all_three_planes() {
    let Some((ffmpeg, _)) = tools() else {
        eprintln!("跳过：未定位 ffmpeg/ffprobe（weighted P_Skip）");
        return;
    };
    for (tag, params) in [
        ("cabac", "cabac=1:bframes=3:weightp=2:keyint=90:scenecut=0"),
        // CAVLC regression targets Main-profile 4x4 transforms; High-profile
        // CAVLC 8x8 transform syntax is tracked separately in the waitlist.
        (
            "cavlc",
            "cabac=0:bframes=0:weightp=2:keyint=90:scenecut=0:8x8dct=0",
        ),
    ] {
        let dir =
            std::env::temp_dir().join(format!("ye-weighted-skip-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("colors.mp4");
        let made = Command::new(&ffmpeg)
            .args([
                "-v",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=red:s=176x144:r=15:d=1",
                "-f",
                "lavfi",
                "-i",
                "color=blue:s=176x144:r=15:d=1",
                "-f",
                "lavfi",
                "-i",
                "color=white:s=176x144:r=15:d=1",
                "-filter_complex",
                "[0:v][1:v][2:v]concat=n=3:v=1:a=0[v]",
                "-map",
                "[v]",
                "-c:v",
                "libx264",
                "-x264-params",
            ])
            .arg(params)
            .arg(&src)
            .output()
            .unwrap();
        assert!(
            made.status.success(),
            "{}",
            String::from_utf8_lossy(&made.stderr)
        );
        let frames = decode_yuv_frames(&src);
        assert_eq!(frames.len(), 45);
        let reference = ffmpeg_yuv_frames(&ffmpeg, &src, 45, 176, 144);
        for (n, (_, _, frame)) in frames.iter().enumerate() {
            assert_eq!(
                frame.as_slice(),
                &reference[n * frame.len()..(n + 1) * frame.len()],
                "{tag} frame {n}"
            );
        }
        std::fs::remove_file(src).unwrap();
        std::fs::remove_dir(dir).unwrap();
    }
}
