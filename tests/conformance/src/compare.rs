//! 对拍器：解码输出的 YUV（planar）与 PCM 逐样本比较。
//!
//! 施工图①交付**比较器本体**（M2 解码器落地后由对拍 runner 消费）：
//! 输出逐平面的 max-abs 与 PSNR（YUV）、逐样本 max-abs（PCM），
//! 是否放行由 [`crate::tolerance`] 的口径判定——比较器只量尺子，
//! 不做裁决。
//!
//! 施工图②增补**逐帧对拍模式**（[`diff_yuv_frames`]）：首差帧/首差
//! 宏块位置/max-abs 的定位报告（失败归档四元组的载体，归档行见
//! [`FrameDiffReport::archive_lines`]），容差裁决在
//! [`crate::tolerance::Tolerance::check_yuv_frames`]。

/// 色度采样（决定平面尺寸）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChromaSampling {
    /// 单色（仅 Y 平面）。
    Monochrome,
    /// 4:2:0。
    Yuv420,
    /// 4:2:2。
    Yuv422,
    /// 4:4:4。
    Yuv444,
}

impl ChromaSampling {
    /// 各平面字节数（8 bit 每样本）：`[Y, Cb, Cr]`（单色仅 `[Y]`）。
    pub fn plane_sizes(self, width: usize, height: usize) -> Vec<usize> {
        match self {
            ChromaSampling::Monochrome => vec![width * height],
            ChromaSampling::Yuv420 => vec![width * height, width * height / 4, width * height / 4],
            ChromaSampling::Yuv422 => vec![width * height, width * height / 2, width * height / 2],
            ChromaSampling::Yuv444 => vec![width * height; 3],
        }
    }
}

/// 逐帧对拍的单个差异样本定位（失败归档用）。
///
/// 坐标为**所在平面**的采样网格坐标（420 色度平面即 w/2×h/2 网格）；
/// 亮度宏块换算见 [`FrameDiffReport::first_diff_mb`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleSpot {
    /// 帧号（0 起）。
    pub frame: usize,
    /// 平面（0=Y 1=Cb 2=Cr）。
    pub plane: usize,
    /// 平面内列。
    pub x: usize,
    /// 平面内行。
    pub y: usize,
    /// 该样本的 |Δ|。
    pub abs_diff: u32,
}

/// 逐帧对拍报告：只量尺子（哪里差、差多少），裁决在
/// [`crate::tolerance::Tolerance::check_yuv_frames`]。
///
/// 施工图②失败归档四元组「向量名/首差帧/首差宏块位置/max-abs」由
/// 本报告 + [`FrameDiffReport::archive_lines`] 提供（向量名由 runner
/// 前置）。
#[derive(Debug, Clone, PartialEq)]
pub struct FrameDiffReport {
    /// 帧数（两侧较短侧的整帧数；长度不整除 → `diff_yuv_frames` Err）。
    pub frames: usize,
    /// 显示宽（像素）。
    pub width: usize,
    /// 显示高（像素）。
    pub height: usize,
    /// 每帧样本数（全平面合计）。
    pub frame_samples: usize,
    /// 首个含差异样本的帧号（`None` = 逐位一致）。
    pub first_diff_frame: Option<usize>,
    /// 首个差异样本（与 `first_diff_frame` 同帧）。
    pub first_diff: Option<SampleSpot>,
    /// 全程最大绝对差（0 = 逐位一致）。
    pub max_abs: u32,
    /// `max_abs` 的首个出现位置。
    pub max_abs_at: Option<SampleSpot>,
    /// 每帧差异样本数（Δ≠0 的样本个数，按帧索引）。
    pub frame_diff_counts: Vec<u64>,
}

impl FrameDiffReport {
    /// 平面名（Y/Cb/Cr；单色只有 Y）。
    fn plane_name(plane: usize) -> &'static str {
        match plane {
            0 => "Y",
            1 => "Cb",
            2 => "Cr",
            _ => "?",
        }
    }

    /// 样本位置的人类可读串（含 |Δ|）。
    pub fn spot_str(spot: &SampleSpot) -> String {
        format!(
            "frame={} plane={} x={} y={} |Δ|={}",
            spot.frame,
            Self::plane_name(spot.plane),
            spot.x,
            spot.y,
            spot.abs_diff
        )
    }

    /// 首差样本的宏块位置（16×16 亮度网格，施工图②归档字段）：色度
    /// 平面样本按采样比换算回亮度坐标后取整除 16（420：×2；422：x×2；
    /// 444/单色：×1）。
    ///
    /// # Errors
    /// 尚无差异（`first_diff` 为 None）→ `Err`。
    pub fn first_diff_mb(&self, chroma: ChromaSampling) -> Result<(usize, usize), String> {
        let Some(spot) = self.first_diff else {
            return Err("无差异样本，无宏块位置".into());
        };
        let (sx, sy) = plane_sample_scale(chroma, spot.plane);
        Ok(((spot.x * sx) / 16, (spot.y * sy) / 16))
    }

    /// 失败归档行（施工图②格式：首差帧/首差宏块位置/max-abs；向量名
    /// 与容差判据行由 runner 拼入）。
    #[must_use]
    pub fn archive_lines(&self, chroma: ChromaSampling) -> Vec<String> {
        let mut lines = Vec::new();
        match (self.first_diff_frame, self.first_diff) {
            (Some(f), Some(spot)) => {
                lines.push(format!(
                    "first_diff_frame={} ({}，首差样本)",
                    f,
                    Self::spot_str(&spot)
                ));
                if let Ok((mbx, mby)) = self.first_diff_mb(chroma) {
                    lines.push(format!("first_diff_mb=({mbx},{mby}) (16×16 亮度宏块网格)"));
                }
            }
            _ => lines.push("first_diff_frame=none (逐位一致)".into()),
        }
        lines.push(format!(
            "max_abs={} ({})",
            self.max_abs,
            self.max_abs_at
                .as_ref()
                .map(Self::spot_str)
                .unwrap_or_else(|| "无差异".into())
        ));
        lines.push(format!(
            "frames={} ({}x{} yuv420p 口径, 每帧 {} 样本)",
            self.frames, self.width, self.height, self.frame_samples
        ));
        lines
    }
}

/// 平面样本坐标 → 亮度坐标的倍率（平面, 采样）→ (x 倍率, y 倍率)。
fn plane_sample_scale(chroma: ChromaSampling, plane: usize) -> (usize, usize) {
    match (chroma, plane) {
        (ChromaSampling::Yuv420, 1 | 2) => (2, 2),
        (ChromaSampling::Yuv422, 1 | 2) => (2, 1),
        _ => (1, 1),
    }
}

/// 逐平面网格 `[(宽, 高); n]`（行紧排无 padding，与 rawvideo 一致）。
fn plane_grids(chroma: ChromaSampling, width: usize, height: usize) -> Vec<(usize, usize)> {
    match chroma {
        ChromaSampling::Monochrome => vec![(width, height)],
        ChromaSampling::Yuv420 => {
            vec![
                (width, height),
                (width / 2, height / 2),
                (width / 2, height / 2),
            ]
        }
        ChromaSampling::Yuv422 => vec![(width, height), (width / 2, height), (width / 2, height)],
        ChromaSampling::Yuv444 => vec![(width, height); 3],
    }
}

/// 逐帧比较两份 planar YUV（长度须相等且为整帧的整数倍），产出定位
/// 报告（不做容差裁决）。布局不符 → `Err`。
///
/// 与 [`compare_yuv`] 的关系：后者是整缓冲的总量统计（PSNR 等），
/// 本函数面向**逐帧对拍模式**——首差帧/首差宏块定位是失败归档的
/// 硬需求，整缓冲统计给不出来。
pub fn diff_yuv_frames(
    ours: &[u8],
    reference: &[u8],
    width: usize,
    height: usize,
    chroma: ChromaSampling,
) -> Result<FrameDiffReport, String> {
    if width == 0 || height == 0 {
        return Err("diff_yuv_frames: 零尺寸".into());
    }
    let grids = plane_grids(chroma, width, height);
    let frame_len: usize = grids.iter().map(|&(w, h)| w * h).sum();
    if frame_len == 0
        || !ours.len().is_multiple_of(frame_len)
        || !reference.len().is_multiple_of(frame_len)
    {
        return Err(format!(
            "diff_yuv_frames: 缓冲长度 {}/{} 与帧长 {frame_len} 不符（显示尺寸 {width}x{height}）",
            ours.len(),
            reference.len()
        ));
    }
    if ours.len() != reference.len() {
        return Err(format!(
            "diff_yuv_frames: 长度不等（我方 {} 字节 = {} 帧，参考 {} 字节 = {} 帧）",
            ours.len(),
            ours.len() / frame_len,
            reference.len(),
            reference.len() / frame_len
        ));
    }
    let frames = ours.len() / frame_len;
    let frame_samples = frame_len;
    let mut report = FrameDiffReport {
        frames,
        width,
        height,
        frame_samples,
        first_diff_frame: None,
        first_diff: None,
        max_abs: 0,
        max_abs_at: None,
        frame_diff_counts: vec![0; frames],
    };
    for f in 0..frames {
        let base = f * frame_len;
        let mut off = 0usize;
        for (p, &(gw, gh)) in grids.iter().enumerate() {
            for y in 0..gh {
                let row = base + off + y * gw;
                for x in 0..gw {
                    let d = u32::from(ours[row + x].abs_diff(reference[row + x]));
                    if d == 0 {
                        continue;
                    }
                    report.frame_diff_counts[f] += 1;
                    if report.first_diff.is_none() {
                        report.first_diff_frame = Some(f);
                        report.first_diff = Some(SampleSpot {
                            frame: f,
                            plane: p,
                            x,
                            y,
                            abs_diff: d,
                        });
                    }
                    if d > report.max_abs {
                        report.max_abs = d;
                        report.max_abs_at = Some(SampleSpot {
                            frame: f,
                            plane: p,
                            x,
                            y,
                            abs_diff: d,
                        });
                    }
                }
            }
            off += gw * gh;
        }
    }
    Ok(report)
}

/// YUV 对拍结果（逐平面）。
#[derive(Debug, Clone, PartialEq)]
pub struct YuvDiff {
    /// 每平面最大绝对差（字节域 0..=255）。
    pub plane_max_abs: Vec<u32>,
    /// 每平面 PSNR（dB）；完全一致为 `f64::INFINITY`。
    pub plane_psnr_db: Vec<f64>,
    /// 全平面逐字节一致。
    pub bit_exact: bool,
}

/// 比较两份 planar YUV 缓冲（帧数 = 总长 ÷ 单帧长，须整除）。
/// 布局不符（长度、宽高）→ `Err`。
pub fn compare_yuv(
    a: &[u8],
    b: &[u8],
    width: usize,
    height: usize,
    chroma: ChromaSampling,
) -> Result<YuvDiff, String> {
    if width == 0 || height == 0 {
        return Err("compare_yuv: 零尺寸".into());
    }
    let plane_sizes = chroma.plane_sizes(width, height);
    let frame_len: usize = plane_sizes.iter().sum();
    if frame_len == 0 || !a.len().is_multiple_of(frame_len) || !b.len().is_multiple_of(frame_len) {
        return Err(format!(
            "compare_yuv: 缓冲长度 {}/{} 与帧长 {frame_len} 不符",
            a.len(),
            b.len()
        ));
    }
    if a.len() != b.len() {
        return Err(format!("compare_yuv: 长度不等 {}/{}", a.len(), b.len()));
    }
    let frames = a.len() / frame_len;
    let mut plane_max_abs = vec![0u32; plane_sizes.len()];
    let mut plane_sq_err = vec![0u64; plane_sizes.len()];
    let mut bit_exact = true;
    for f in 0..frames {
        let base = f * frame_len;
        let mut off = 0usize;
        for (p, psize) in plane_sizes.iter().enumerate() {
            for i in 0..*psize {
                let x = a[base + off + i];
                let y = b[base + off + i];
                let d = x.abs_diff(y);
                plane_max_abs[p] = plane_max_abs[p].max(u32::from(d));
                plane_sq_err[p] += u64::from(d) * u64::from(d);
                if x != y {
                    bit_exact = false;
                }
            }
            off += psize;
        }
    }
    let plane_psnr_db = plane_sq_err
        .iter()
        .zip(plane_sizes.iter())
        .map(|(&sq, &psize)| {
            if sq == 0 {
                f64::INFINITY
            } else {
                let mse = sq as f64 / (psize as f64 * frames as f64);
                10.0 * (255.0f64 * 255.0 / mse).log10()
            }
        })
        .collect();
    Ok(YuvDiff {
        plane_max_abs,
        plane_psnr_db,
        bit_exact,
    })
}

/// PCM 对拍结果。
#[derive(Debug, Clone, PartialEq)]
pub struct PcmDiff {
    /// 最大绝对样本差。
    pub max_abs: i64,
    /// 逐样本一致。
    pub bit_exact: bool,
}

/// 比较两份交错 PCM 缓冲（小端有符号，样本宽 1..=4 字节）。
pub fn compare_pcm(a: &[u8], b: &[u8], sample_width: usize) -> Result<PcmDiff, String> {
    if sample_width == 0 || sample_width > 4 {
        return Err("compare_pcm: sample_width 须 1..=4".into());
    }
    if a.len() != b.len() {
        return Err(format!("compare_pcm: 长度不等 {}/{}", a.len(), b.len()));
    }
    if !a.len().is_multiple_of(sample_width) {
        return Err(format!(
            "compare_pcm: 长度 {} 不是样本宽 {sample_width} 的整数倍",
            a.len()
        ));
    }
    let mut max_abs = 0i64;
    let mut bit_exact = true;
    for (chunk_a, chunk_b) in a
        .chunks_exact(sample_width)
        .zip(b.chunks_exact(sample_width))
    {
        let sa = le_signed(chunk_a);
        let sb = le_signed(chunk_b);
        max_abs = max_abs.max((sa - sb).abs());
        if sa != sb {
            bit_exact = false;
        }
    }
    Ok(PcmDiff { max_abs, bit_exact })
}

/// 小端有符号定宽整数（符号扩展）。
fn le_signed(bytes: &[u8]) -> i64 {
    let mut v: u64 = 0;
    for (i, &b) in bytes.iter().enumerate() {
        v |= u64::from(b) << (8 * i);
    }
    // 符号扩展：把最高有效位扩到 i64
    let bits = bytes.len() * 8;
    if bits == 64 {
        v as i64
    } else {
        let sign = 1u64 << (bits - 1);
        ((v ^ sign).wrapping_sub(sign)) as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yuv_bit_exact_and_diff() {
        // 4×4 的 420 单帧长 24（Y=16, Cb=4, Cr=4），两帧共 48 字节
        let frame_len = 16 + 4 + 4;
        let a = vec![100u8; frame_len * 2];
        let mut b = a.clone();
        b[3] = 90; // 帧 0 的 Y 平面（0..15）：差 10
        b[17] = 200; // 帧 0 的 Cb 平面（16..19）：差 100
        let diff = compare_yuv(&a, &b, 4, 4, ChromaSampling::Yuv420).unwrap();
        assert!(!diff.bit_exact);
        assert_eq!(diff.plane_max_abs, vec![10, 100, 0]);
        assert!(diff.plane_psnr_db[2].is_infinite());
        // PSNR 数量级合理性：Y 平面 16×2=32 样本、平方误差 100 →
        // mse=3.125 → 10·log10(65025/3.125) ≈ 43.18 dB
        assert!(
            (diff.plane_psnr_db[0] - 43.18).abs() < 0.1,
            "{:?}",
            diff.plane_psnr_db
        );
        // 完全一致
        let same = compare_yuv(&a, &a, 4, 4, ChromaSampling::Yuv420).unwrap();
        assert!(same.bit_exact);
        assert!(same.plane_psnr_db.iter().all(|p| p.is_infinite()));
    }

    #[test]
    fn yuv_plane_layouts() {
        assert_eq!(
            ChromaSampling::Yuv420.plane_sizes(64, 48),
            vec![64 * 48, 768, 768]
        );
        assert_eq!(
            ChromaSampling::Yuv422.plane_sizes(64, 48),
            vec![64 * 48, 1536, 1536]
        );
        assert_eq!(ChromaSampling::Yuv444.plane_sizes(64, 48), vec![64 * 48; 3]);
        assert_eq!(
            ChromaSampling::Monochrome.plane_sizes(64, 48),
            vec![64 * 48]
        );
        // 长度与帧长不符 → Err
        assert!(compare_yuv(&[0; 10], &[0; 10], 4, 4, ChromaSampling::Yuv420).is_err());
        assert!(compare_yuv(&[0; 24], &[0; 23], 4, 4, ChromaSampling::Yuv420).is_err());
    }

    #[test]
    fn pcm_bit_exact_and_diff() {
        let a: Vec<u8> = [0i16, -1, 32767, -32768]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let diff = compare_pcm(&a, &a, 2).unwrap();
        assert!(diff.bit_exact);
        assert_eq!(diff.max_abs, 0);
        let mut b = a.clone();
        b[0] = 0xF0; // 0x00F0 = 240 → 与 0 差 240
        let diff = compare_pcm(&a, &b, 2).unwrap();
        assert!(!diff.bit_exact);
        assert_eq!(diff.max_abs, 240);
        // 符号扩展路径：低字节改 0x7F → 0x807F = -32641 与 -32768 差 127
        let mut c = a.clone();
        c[6] = 0x7F;
        let diff = compare_pcm(&a, &c, 2).unwrap();
        assert_eq!(diff.max_abs, 127);
        // 极值对：0x8000 (-32768) vs 0x7FFF (32767) → 差 65535
        let mut d = a.clone();
        d[6] = 0xFF;
        d[7] = 0x7F;
        let diff = compare_pcm(&a, &d, 2).unwrap();
        assert_eq!(diff.max_abs, 65535);
    }

    #[test]
    fn pcm_rejects_bad_args() {
        assert!(compare_pcm(&[0; 4], &[0; 4], 0).is_err());
        assert!(compare_pcm(&[0; 4], &[0; 4], 5).is_err());
        assert!(compare_pcm(&[0; 4], &[0; 5], 2).is_err());
        assert!(compare_pcm(&[0; 3], &[0; 3], 2).is_err());
    }

    // —— 逐帧对拍模式（施工图②）：定位报告与归档字段 ——

    /// 4×4 420：帧长 24（Y 4×4 + Cb 2×2 + Cr 2×2），平面偏移 Y=0、
    /// Cb=16、Cr=20。
    #[test]
    fn frame_diff_spots_and_archive() {
        let frame = [50u8; 24];
        let both: Vec<u8> = frame.iter().chain(frame.iter()).cloned().collect();
        // 完全一致
        let r = diff_yuv_frames(&both, &both, 4, 4, ChromaSampling::Yuv420).unwrap();
        assert_eq!(r.frames, 2);
        assert_eq!(r.first_diff_frame, None);
        assert_eq!(r.max_abs, 0);
        assert_eq!(r.frame_diff_counts, vec![0, 0]);
        // 帧 1 的 Cb 平面 (x=1,y=1)：偏移 16 + 1*2 + 1 = 19，+3
        let mut ours = both.clone();
        ours[24 + 19] += 3;
        // 帧 1 的 Y 平面 (x=0,y=0)：偏移 0，−1（回绕按字节差 1）
        ours[24] -= 1;
        let r = diff_yuv_frames(&ours, &both, 4, 4, ChromaSampling::Yuv420).unwrap();
        assert_eq!(r.first_diff_frame, Some(1));
        assert_eq!(
            r.first_diff,
            Some(SampleSpot {
                frame: 1,
                plane: 0,
                x: 0,
                y: 0,
                abs_diff: 1
            })
        );
        assert_eq!(r.max_abs, 3);
        assert_eq!(
            r.max_abs_at,
            Some(SampleSpot {
                frame: 1,
                plane: 1,
                x: 1,
                y: 1,
                abs_diff: 3
            })
        );
        assert_eq!(r.frame_diff_counts, vec![0, 2]);
        // 归档行：首差帧/宏块/max-abs 四元组（向量名由 runner 拼）
        let lines = r.archive_lines(ChromaSampling::Yuv420);
        assert!(lines[0].contains("first_diff_frame=1"), "{lines:?}");
        // Cb(1,1) → 亮度坐标 (2,2) → 宏块 (0,0)——4×4 帧只有 1 个宏块
        assert!(lines[1].contains("first_diff_mb=(0,0)"), "{lines:?}");
        assert!(lines[2].contains("max_abs=3"), "{lines:?}");
    }

    /// 色度样本 → 亮度宏块换算：640×360 帧 Y(x=100,y=50) → mb=(6,3)；
    /// Cb(x=10,y=5) → 亮度 (20,10) → mb=(1,0)。
    #[test]
    fn mb_conversion_from_chroma() {
        let mut spot = SampleSpot {
            frame: 0,
            plane: 0,
            x: 100,
            y: 50,
            abs_diff: 1,
        };
        let r = FrameDiffReport {
            frames: 1,
            width: 640,
            height: 360,
            frame_samples: 345_600,
            first_diff_frame: Some(0),
            first_diff: Some(spot),
            max_abs: 1,
            max_abs_at: Some(spot),
            frame_diff_counts: vec![1],
        };
        assert_eq!(r.first_diff_mb(ChromaSampling::Yuv420).unwrap(), (6, 3));
        spot.plane = 1;
        spot.x = 10;
        spot.y = 5;
        let r = FrameDiffReport {
            first_diff: Some(spot),
            ..r
        };
        assert_eq!(r.first_diff_mb(ChromaSampling::Yuv420).unwrap(), (1, 0));
        assert!(r.first_diff_mb(ChromaSampling::Yuv420).is_ok());
        // 无差异 → Err
        let clean = FrameDiffReport {
            frames: 1,
            width: 4,
            height: 4,
            frame_samples: 24,
            first_diff_frame: None,
            first_diff: None,
            max_abs: 0,
            max_abs_at: None,
            frame_diff_counts: vec![0],
        };
        assert!(clean.first_diff_mb(ChromaSampling::Yuv420).is_err());
    }

    /// 布局不符（长度不整除 / 不等）→ Err；420 与整缓冲版帧长一致。
    #[test]
    fn frame_diff_rejects_bad_layout() {
        let a = vec![0u8; 48];
        assert!(diff_yuv_frames(&a, &a, 4, 4, ChromaSampling::Yuv420).is_ok());
        assert!(diff_yuv_frames(&a[..24], &a, 4, 4, ChromaSampling::Yuv420).is_err());
        assert!(diff_yuv_frames(&a[..40], &a[..40], 4, 4, ChromaSampling::Yuv420).is_err());
        assert!(diff_yuv_frames(&a, &a, 0, 4, ChromaSampling::Yuv420).is_err());
    }
}
