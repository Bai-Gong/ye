//! CABAC 真流对拍（M2 遗留收口，2026-10-26）：ffmpeg 造确定性 CABAC
//! 向量 → ye 解码 vs ffmpeg 解码逐帧灰度对拍。
//!
//! 阶梯（从简到繁，逐级收口）：
//! 1. 全 I 帧 CABAC（High）——无帧间、无重排，纯 intra 链路；
//! 2. I/P 30fps 长链（45 帧）——运动向量/参考帧/残差链路；
//! 3. 带 B 帧（bf=3 默认金字塔）——重排缓冲与双向预测。
//!
//! 工具缺席＝分级 skip 合法（打印跳过信息返回）；工具在场而编/解码
//! 失败＝测试失败不许 skip（与 ye-codec-aac oracle 同款约定）。
//! 容差：起点逐像素 100% |Δ|≤1（CAVLC 收口后同口径；解码语义正确时
//! ffmpeg 与本实现只差去块边角 ±1），达到全等即收紧为 0——只准收紧。

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
        for dir in path.split(';') {
            let p = Path::new(dir).join(exe_name(tool));
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
/// 调试口：YE_LAVFI 覆写源滤镜（默认 testsrc2）、YE_FRAMES 覆写帧数。
fn gen_vector(dir: &Path, ffmpeg: &Path, tag: &str, x264: &str) -> PathBuf {
    let lavfi =
        std::env::var("YE_LAVFI").unwrap_or_else(|_| "testsrc2=duration=3:size=176x144:rate=15".into());
    let out = dir.join(format!("cabac-{tag}.mp4"));
    let st = Command::new(ffmpeg)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i"])
        .arg(&lavfi)
        .args(["-c:v", "libx264", "-profile:v", "high", "-pix_fmt", "yuv420p", "-x264-params"])
        .arg(x264)
        .arg(&out)
        .status()
        .expect("ffmpeg 应可执行（tools 已确认）");
    assert!(st.success(), "ffmpeg 造 {tag} 向量失败：{}", out.display());
    out
}

// ---- ye 解码 → 每帧紧致 luma ----

fn decode_luma_frames(path: &Path) -> Vec<(u32, u32, Vec<u8>)> {
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
                    let stride = f.layout().plane(0).stride;
                    let luma = f.plane(0)[..h as usize * stride]
                        .chunks_exact(stride)
                        .flat_map(|row| row[..w as usize].to_vec())
                        .collect();
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
                let stride = f.layout().plane(0).stride;
                let luma: Vec<u8> = f.plane(0)[..h as usize * stride]
                    .chunks_exact(stride)
                    .flat_map(|row| row[..w as usize].to_vec())
                    .collect();
                frames.push((w, h, luma));
            }
            Decoded::Frame(Frame::Audio(_)) => {}
            Decoded::NeedInput | Decoded::Finished => break,
        }
    }
    frames
}

// ---- ffmpeg 参照解码（yuv420p rawvideo 取 luma；帧序 = 解码输出序） ----
// 口径注意：不用 -pix_fmt gray——swscale 的 yuv420p→gray 转换会整体
// 抬值（平色实测 126 → 128），与解码正确性无关；yuv420p 参照的 luma
// 平面才是解码原值（M2 像素对拍同口径）。

fn ffmpeg_yuv_luma_frames(
    ffmpeg: &Path,
    path: &Path,
    count: usize,
    w: usize,
    h: usize,
) -> Vec<u8> {
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
    let luma_len = w * h;
    let frame_len = luma_len * 3 / 2;
    let mut luma = Vec::with_capacity(count * luma_len);
    for i in 0..count {
        let start = i * frame_len;
        luma.extend_from_slice(&out.stdout[start..start + luma_len]);
    }
    luma
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
            "csv=p=0",
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
    assert!(count >= 1, "应有至少 1 帧（{tag}）");

    let ye_frames = decode_luma_frames(&src);
    if std::env::var_os("YE_CABAC_SALVAGE").is_none() {
        assert_eq!(ye_frames.len(), count, "ye 解码帧数应与 ffprobe 一致（{tag}）");
    }
    let (vw, vh) = (ye_frames[0].0 as usize, ye_frames[0].1 as usize);
    let frame_len = vw * vh;
    let reference = ffmpeg_yuv_luma_frames(&ffmpeg, &src, count, vw, vh);
    assert_eq!(
        reference.len(),
        count * frame_len,
        "ffmpeg 参照帧总长（{tag}）"
    );

    if std::env::var_os("YE_CABAC_SALVAGE").is_some() {
        // 诊断：逐帧找首个 |Δ|>8 的 16px 宏块行（分歧起始带）。
        for (i, (w, h, luma)) in ye_frames.iter().enumerate() {
            let (w, h) = (*w as usize, *h as usize);
            let ref_frame = &reference[i * w * h..(i + 1) * w * h];
            let mut first_bad: Option<usize> = None;
            for band in 0..h.div_ceil(16) {
                let y0 = band * 16;
                let y1 = (y0 + 16).min(h);
                let mut bad = false;
                for y in y0..y1 {
                    for x in 0..w {
                        if luma[y * w + x].abs_diff(ref_frame[y * w + x]) > 8 {
                            bad = true;
                            break;
                        }
                    }
                    if bad { break; }
                }
                if bad {
                    first_bad = Some(band);
                    break;
                }
            }
            if let Some(band) = first_bad {
                // 细化：该行内逐 16px 宏块列找首坏块。
                let y0 = band * 16;
                let y1 = (y0 + 16).min(h);
                let mut bad_cols = Vec::new();
                for mbc in 0..w.div_ceil(16) {
                    let x0 = mbc * 16;
                    let x1 = (x0 + 16).min(w);
                    let mut bad = false;
                    for y in y0..y1 {
                        for x in x0..x1 {
                            if luma[y * w + x].abs_diff(ref_frame[y * w + x]) > 8 {
                                bad = true;
                                break;
                            }
                        }
                        if bad { break; }
                    }
                    bad_cols.push((mbc, bad));
                }
                eprintln!("{tag} 帧 {i}: 首个损坏宏块行 = {band:?}，逐块坏 = {bad_cols:?}");
                // 细化：坏块内逐 4x4 块（b = 块行×4+块列）的 max|Δ|，
                // 并把双方 luma 落盘（前 2 个坏帧）供离线对照。
                let mut dumped = 0;
                for (mbc, bad) in bad_cols {
                    if !bad { continue; }
                    let y0 = band * 16;
                    for by in 0..4usize {
                        let mut row = Vec::with_capacity(4);
                        for bx in 0..4usize {
                            let (x0, yy0) = (mbc * 16 + bx * 4, y0 + by * 4);
                            let mut mx = 0u8;
                            for y in yy0..(yy0 + 4).min(h) {
                                for x in x0..(x0 + 4).min(w) {
                                    mx = mx.max(luma[y * w + x].abs_diff(ref_frame[y * w + x]));
                                }
                            }
                            row.push(format!("{mx}"));
                        }
                        eprintln!("  帧{i} MB({mbc},{band}) 4x4行{by}: {}", row.join(","));
                    }
                    if dumped < 2 {
                        let out_dir = std::env::temp_dir().join("ye-cabac-dbg");
                        std::fs::create_dir_all(&out_dir).unwrap();
                        std::fs::write(
                            out_dir.join(format!("{tag}-f{i}-mb{mbc}_{band}-ours.bin")),
                            &luma[..w * h],
                        )
                        .unwrap();
                        std::fs::write(
                            out_dir.join(format!("{tag}-f{i}-mb{mbc}_{band}-ref.bin")),
                            &ref_frame[..w * h],
                        )
                        .unwrap();
                        eprintln!("  已落盘 {} （向量 {src:?}）", out_dir.display());
                        dumped += 1;
                    }
                }
            } else {
                eprintln!("{tag} 帧 {i}: 首个损坏宏块行 = None");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    let mut worst = 0usize;
    for (i, (w, h, luma)) in ye_frames.iter().enumerate() {
        assert_eq!((*w as usize, *h as usize), (vw, vh), "帧 {i} 尺寸（{tag}）");
        let ref_frame = &reference[i * frame_len..(i + 1) * frame_len];
        let mut bad = 0usize;
        let mut max_diff = 0u8;
        let mut hist = [0usize; 8];
        for (a, b) in luma.iter().zip(ref_frame) {
            let d = a.abs_diff(*b);
            if d > 1 {
                bad += 1;
            }
            if d < 8 {
                hist[d as usize] += 1;
            }
            max_diff = max_diff.max(d);
        }
        if bad > 0 && i == 0 && std::env::var_os("YE_DUMP_DIFF").is_some() {
            eprintln!(
                "{tag} 帧 0 直方图 |Δ|0..7 = {hist:?}；我方前 16: {:?}；参照前 16: {:?}",
                &luma[..16],
                &ref_frame[..16]
            );
        }
        assert!(
            bad == 0,
            "{tag} 帧 {i} 有 {bad} 像素 |Δ|>1（max |Δ|={max_diff}）——CABAC 解码与 ffmpeg 分歧"
        );
        worst = worst.max(max_diff as usize);
    }
    eprintln!("cabac {tag}: 45 帧全对拍通过，实测最大 |Δ|={worst}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn cabac_全_i_帧对拍() {
    let x264 = std::env::var("YE_X264_OVERRIDE").unwrap_or_else(|_| {
        "keyint=1:min-keyint=1:scenecut=0:bf=0".to_string()
    });
    run_case("all-i", &x264);
}

#[test]
fn cabac_i_p_长链对拍() {
    run_case("ip", "keyint=30:min-keyint=30:scenecut=0:bf=0:refs=1");
}

#[test]
fn cabac_带_b_帧对拍() {
    run_case("b", "keyint=30:min-keyint=30:scenecut=0:bf=3");
}
