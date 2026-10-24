//! 对拍器：解码输出的 YUV（planar）与 PCM 逐样本比较。
//!
//! 施工图①交付**比较器本体**（M2 解码器落地后由对拍 runner 消费）：
//! 输出逐平面的 max-abs 与 PSNR（YUV）、逐样本 max-abs（PCM），
//! 是否放行由 [`crate::tolerance`] 的口径判定——比较器只量尺子，
//! 不做裁决。

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
}
