//! yuv420p ↔ rgba/rgb24 像素格式转换内核（M2 像素核，纯函数、无状态）。
//!
//! ## 确定性颜色转换
//!
//! 色度按最近邻 2×2 复制；BT.601/709 矩阵使用 13 位定点系数，
//! 亮度与色度相加后统一四舍五入、截幅，避免逐项 floor 的系统性偏暗。
//! 黑白端点保持 0/255；输出不随 CPU 架构变化。
//! oracle 显式使用 swscale 的 `neighbor+full_chroma_int+accurate_rnd`
//! 与 `-cpuflags 0`，避免默认 x86/NEON/旧表路径采用不同量化规则。
//!
//! rgb→yuv 方向按 BT.601 规格实现（先逐像素换算再对 2×2 块取均值得到色度），
//! 无 oracle 对拍（M2 施工图对拍口径只覆盖 yuv→rgb 方向），以已知色 +
//! roundtrip 单测兜底。
//!
//! ## 有限/全程范围语义
//!
//! - [`YuvRange::Limited`]（MPEG/studio swing）：Y∈[16,235]、C∈[16,240]，
//!   对应 `ye_pixfmt::ColorRange::Mpeg`（H.264 码流缺省）；
//! - [`YuvRange::Full`]（JPEG/full swing）：[0,255] 全量程，
//!   对应 `ye_pixfmt::ColorRange::Jpeg`。
//!
//! 调用方（convert 泵）从流参数读 color_range，缺省按 Limited 处理
//! （H.264 SPS 未标注时 FFmpeg 同样按有限范围解释）。

use crate::error::FilterError;

/// yuv420p 的量化范围（映射 `ye_pixfmt::ColorRange`：Mpeg→Limited、Jpeg→Full）。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YuvRange {
    /// 有限范围（MPEG）：Y∈[16,235]、C∈[16,240]。
    Limited,
    /// 全程范围（JPEG）：[0,255]。
    Full,
}

/// yuv420p 三个平面的只读视图（跨距逐平面独立，泵可直接传解码帧平面）。
///
/// 不变量（构造后由 [`Self::validate`] 核验）：每个平面切片至少覆盖
/// `(rows−1)·stride + cols` 字节。
#[derive(Debug, Clone, Copy)]
pub struct Yuv420pView<'a> {
    /// Y 平面。
    pub y: &'a [u8],
    /// Y 平面跨距（字节）。
    pub y_stride: usize,
    /// U（Cb）平面。
    pub u: &'a [u8],
    /// U 平面跨距。
    pub u_stride: usize,
    /// V（Cr）平面。
    pub v: &'a [u8],
    /// V 平面跨距。
    pub v_stride: usize,
}

impl Yuv420pView<'_> {
    /// 核验三个平面切片长度覆盖 `width×height`（色度按 ceil 半分辨率）。
    ///
    /// # Errors
    /// 任一平面过短 → [`FilterError::InvalidArguments`]（固定文案，不 panic）。
    pub fn validate(&self, width: usize, height: usize) -> Result<(), FilterError> {
        let cw = width.div_ceil(2);
        let ch = height.div_ceil(2);
        let need = |stride: usize, rows: usize, cols: usize| {
            if rows == 0 {
                0
            } else {
                (rows - 1) * stride + cols
            }
        };
        if self.y.len() < need(self.y_stride, height, width)
            || self.u.len() < need(self.u_stride, ch, cw)
            || self.v.len() < need(self.v_stride, ch, cw)
        {
            return Err(FilterError::InvalidArguments(
                "yuv420p plane slice too short for width/height/stride",
            ));
        }
        Ok(())
    }

    fn sample(&self, r: usize, c: usize) -> (u8, u8, u8) {
        let y = self.y[r * self.y_stride + c];
        let u = self.u[(r / 2) * self.u_stride + c / 2];
        let v = self.v[(r / 2) * self.v_stride + c / 2];
        (y, u, v)
    }
}

/// 13-bit fixed-point matrix. Coefficients are rounded once, and channel
/// contributions are summed before rounding the resulting sample.
/// The limited-range matrix is scaled by 255/224 (chroma), 255/219 (luma).
fn coefficients(range: YuvRange, matrix: YuvMatrix) -> [i32; 6] {
    match (matrix, range) {
        (YuvMatrix::Bt601, YuvRange::Limited) => [9539, 16, 13075, -3209, -6660, 16525],
        (YuvMatrix::Bt601, YuvRange::Full) => [8192, 0, 11485, -2819, -5850, 14516],
        (YuvMatrix::Bt709, YuvRange::Limited) => [9539, 16, 14686, -1747, -4366, 17305],
        (YuvMatrix::Bt709, YuvRange::Full) => [8192, 0, 12901, -1534, -3835, 15201],
    }
}

fn rgb_sample(y: u8, u: u8, v: u8, c: &[i32; 6]) -> [u8; 3] {
    let y = (i32::from(y) - c[1]) * c[0] + 4096;
    let u = i32::from(u) - 128;
    let v = i32::from(v) - 128;
    [
        clip8((y + c[2] * v) >> 13),
        clip8((y + c[3] * u + c[4] * v) >> 13),
        clip8((y + c[5] * u) >> 13),
    ]
}

fn clip8(x: i32) -> u8 {
    x.clamp(0, 255) as u8
}

/// yuv420p → rgba（紧密排布，每像素 4 字节 R,G,B,A；A 恒 255）。
///
/// # Errors
/// 平面切片长度不足 → [`FilterError::InvalidArguments`]。
pub fn yuv420p_to_rgba(
    src: &Yuv420pView<'_>,
    width: usize,
    height: usize,
    range: YuvRange,
) -> Result<Vec<u8>, FilterError> {
    src.validate(width, height)?;
    let coefficients = coefficients(range, YuvMatrix::Bt601);
    let mut out = vec![0u8; width * height * 4];
    for r in 0..height {
        for c in 0..width {
            let (y, u, v) = src.sample(r, c);
            let o = (r * width + c) * 4;
            out[o..o + 3].copy_from_slice(&rgb_sample(y, u, v, &coefficients));
            out[o + 3] = 255;
        }
    }
    Ok(out)
}

/// yuv420p → rgb24（紧密排布，每像素 3 字节 R,G,B）。
///
/// # Errors
/// 同 [`yuv420p_to_rgba`]。
pub fn yuv420p_to_rgb24(
    src: &Yuv420pView<'_>,
    width: usize,
    height: usize,
    range: YuvRange,
) -> Result<Vec<u8>, FilterError> {
    src.validate(width, height)?;
    let coefficients = coefficients(range, YuvMatrix::Bt601);
    let mut out = vec![0u8; width * height * 3];
    for r in 0..height {
        for c in 0..width {
            let (y, u, v) = src.sample(r, c);
            let o = (r * width + c) * 3;
            out[o..o + 3].copy_from_slice(&rgb_sample(y, u, v, &coefficients));
        }
    }
    Ok(out)
}

/// 紧密排布的 yuv420p 帧缓冲（rgb→yuv 的输出；也可直接喂回 [`Yuv420pView`]）。
///
/// 平面按 Y/U/V 顺序紧密拼接（无跨距填充）；`view()` 产出跨距视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Yuv420pBuffer {
    data: Vec<u8>,
    width: usize,
    height: usize,
}

impl Yuv420pBuffer {
    /// 由紧密平面数据组装（crate 内部：缩放内核的输出通道）。
    pub(super) fn from_planes(data: Vec<u8>, width: usize, height: usize) -> Self {
        Yuv420pBuffer {
            data,
            width,
            height,
        }
    }

    /// 帧宽（像素）。
    pub fn width(&self) -> usize {
        self.width
    }

    /// 帧高（像素）。
    pub fn height(&self) -> usize {
        self.height
    }

    /// Y 平面（`height` 行 × `width` 字节，紧密）。
    pub fn y(&self) -> &[u8] {
        &self.data[..self.width * self.height]
    }

    /// U 平面（ceil(h/2) × ceil(w/2)，紧密）。
    pub fn u(&self) -> &[u8] {
        let ylen = self.width * self.height;
        &self.data[ylen..ylen + self.plane_len(1)]
    }

    /// V 平面。
    pub fn v(&self) -> &[u8] {
        let ylen = self.width * self.height;
        let clen = self.plane_len(1);
        &self.data[ylen + clen..ylen + 2 * clen]
    }

    /// 全部平面拼接的字节。
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// 产出跨距视图（跨距即平面宽，喂回转换/缩放）。
    pub fn view(&self) -> Yuv420pView<'_> {
        let cw = self.width.div_ceil(2);
        Yuv420pView {
            y: self.y(),
            y_stride: self.width,
            u: self.u(),
            u_stride: cw,
            v: self.v(),
            v_stride: cw,
        }
    }

    fn plane_len(&self, plane: usize) -> usize {
        match plane {
            0 => self.width * self.height,
            _ => self.width.div_ceil(2) * self.height.div_ceil(2),
        }
    }
}

/// 单像素 rgb→yuv（BT.601 规格值，f64 中间量）。
fn rgb_to_yuv_pixel(r: u8, g: u8, b: u8, range: YuvRange) -> (f64, f64, f64) {
    let (rf, gf, bf) = (r as f64, g as f64, b as f64);
    match range {
        YuvRange::Limited => {
            let y = 16.0 + (65.481 * rf + 128.553 * gf + 24.966 * bf) / 255.0;
            let u = 128.0 + (-37.797 * rf - 74.203 * gf + 112.0 * bf) / 255.0;
            let v = 128.0 + (112.0 * rf - 93.786 * gf - 18.214 * bf) / 255.0;
            (y, u, v)
        }
        YuvRange::Full => {
            let y = (77.0 * rf + 150.0 * gf + 29.0 * bf) / 256.0;
            let u = 128.0 + (-43.0 * rf - 85.0 * gf + 128.0 * bf) / 256.0;
            let v = 128.0 + (128.0 * rf - 107.0 * gf - 21.0 * bf) / 256.0;
            (y, u, v)
        }
    }
}

fn round_half_away(x: f64) -> u8 {
    // 值域有限，先加 0.5 再取 floor 即 half-up；负值由 clip 先行处理不进入本函数。
    (x + 0.5).floor().clamp(0.0, 255.0) as u8
}

fn rgb_to_yuv420p_impl(
    pixel: &[u8],
    channels: usize,
    width: usize,
    height: usize,
    range: YuvRange,
    matrix: YuvMatrix,
) -> Result<Yuv420pBuffer, FilterError> {
    if width == 0 || height == 0 {
        return Err(FilterError::InvalidArguments("zero frame dimension"));
    }
    if pixel.len() < width * height * channels {
        return Err(FilterError::InvalidArguments("rgb slice too short"));
    }
    let cw = width.div_ceil(2);
    let ch = height.div_ceil(2);
    let mut data = vec![0u8; width * height + 2 * cw * ch];
    let (y_plane, tail) = data.split_at_mut(width * height);
    let (u_plane, v_plane) = tail.split_at_mut(cw * ch);
    // 逐像素 Y；色度按 2×2 块对规格值取均值（BT.601「先滤波后下采样」）。
    let mut acc_u = vec![0f64; cw * ch];
    let mut acc_v = vec![0f64; cw * ch];
    let mut cnt = vec![0u32; cw * ch];
    for r in 0..height {
        for c in 0..width {
            let o = r * width + c;
            let (pr, pg, pb) = match channels {
                4 => (pixel[o * 4], pixel[o * 4 + 1], pixel[o * 4 + 2]),
                _ => (pixel[o * 3], pixel[o * 3 + 1], pixel[o * 3 + 2]),
            };
            let (y, u, v) = match matrix {
                YuvMatrix::Bt601 => rgb_to_yuv_pixel(pr, pg, pb, range),
                YuvMatrix::Bt709 => {
                    let (r, g, b) = (f64::from(pr), f64::from(pg), f64::from(pb));
                    let l = 0.2126 * r + 0.7152 * g + 0.0722 * b;
                    let (y_scale, c_scale, y_bias) = match range {
                        YuvRange::Limited => (219.0 / 255.0, 224.0 / 255.0, 16.0),
                        YuvRange::Full => (1.0, 1.0, 0.0),
                    };
                    (
                        y_bias + y_scale * l,
                        128.0 + c_scale * (b - l) / 1.8556,
                        128.0 + c_scale * (r - l) / 1.5748,
                    )
                }
            };
            y_plane[o] = round_half_away(y);
            let idx = (r / 2) * cw + c / 2;
            acc_u[idx] += u;
            acc_v[idx] += v;
            cnt[idx] += 1;
        }
    }
    for i in 0..cw * ch {
        let n = cnt[i].max(1) as f64;
        u_plane[i] = round_half_away(acc_u[i] / n);
        v_plane[i] = round_half_away(acc_v[i] / n);
    }
    Ok(Yuv420pBuffer::from_planes(data, width, height))
}

/// rgba（紧密，4 字节/像素）→ yuv420p。
///
/// # Errors
/// 尺寸为 0 或切片过短 → [`FilterError::InvalidArguments`]。
pub fn rgba_to_yuv420p(
    rgba: &[u8],
    width: usize,
    height: usize,
    range: YuvRange,
) -> Result<Yuv420pBuffer, FilterError> {
    rgb_to_yuv420p_impl(rgba, 4, width, height, range, YuvMatrix::Bt601)
}

/// rgb24（紧密，3 字节/像素）→ yuv420p。
///
/// # Errors
/// 同 [`rgba_to_yuv420p`]。
pub fn rgb24_to_yuv420p(
    rgb: &[u8],
    width: usize,
    height: usize,
    range: YuvRange,
) -> Result<Yuv420pBuffer, FilterError> {
    rgb_to_yuv420p_impl(rgb, 3, width, height, range, YuvMatrix::Bt601)
}

pub fn rgba_to_yuv420p_matrix(
    rgba: &[u8],
    width: usize,
    height: usize,
    range: YuvRange,
    matrix: YuvMatrix,
) -> Result<Yuv420pBuffer, FilterError> {
    rgb_to_yuv420p_impl(rgba, 4, width, height, range, matrix)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 4×4 均匀帧的平面视图（局部持有，借用传参）。
    fn uniform(y: u8, u: u8, v: u8) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        (vec![y; 16], vec![u; 4], vec![v; 4])
    }

    fn view_of(planes: &(Vec<u8>, Vec<u8>, Vec<u8>)) -> Yuv420pView<'_> {
        Yuv420pView {
            y: &planes.0,
            y_stride: 4,
            u: &planes.1,
            u_stride: 2,
            v: &planes.2,
            v_stride: 2,
        }
    }

    #[test]
    fn known_colors_limited() {
        // 数值经 ffmpeg oracle 探测校准（见模块头注）。
        let p = uniform(16, 128, 128);
        let out = yuv420p_to_rgba(&view_of(&p), 4, 4, YuvRange::Limited).unwrap();
        assert_eq!(&out[..4], &[0, 0, 0, 255]);
        let p = uniform(235, 128, 128);
        let out = yuv420p_to_rgba(&view_of(&p), 4, 4, YuvRange::Limited).unwrap();
        assert_eq!(&out[..4], &[255, 255, 255, 255]);
        // 纯红（BT.601 有限范围经典值 Y=81,U=90,V=240 → (254,0,0)）
        let p = uniform(81, 90, 240);
        let out = yuv420p_to_rgba(&view_of(&p), 4, 4, YuvRange::Limited).unwrap();
        assert_eq!(&out[..4], &[254, 0, 0, 255]);
        // Y=81,U=128,V=240 → (254,0,76)
        let p = uniform(81, 128, 240);
        let out = yuv420p_to_rgba(&view_of(&p), 4, 4, YuvRange::Limited).unwrap();
        assert_eq!(&out[..4], &[254, 0, 76, 255]);
    }

    #[test]
    fn known_colors_full() {
        // 全程范围：恒等 luma + JPEG 系数。(16,128,128) → R=G=B=16
        let p = uniform(16, 128, 128);
        let out = yuv420p_to_rgba(&view_of(&p), 4, 4, YuvRange::Full).unwrap();
        assert_eq!(&out[..4], &[16, 16, 16, 255]);
        // (255,128,128) → 255
        let p = uniform(255, 128, 128);
        let out = yuv420p_to_rgba(&view_of(&p), 4, 4, YuvRange::Full).unwrap();
        assert_eq!(&out[..4], &[255, 255, 255, 255]);
    }

    #[test]
    fn rgb24_matches_rgba_values() {
        let p = uniform(128, 90, 200);
        let a = yuv420p_to_rgba(&view_of(&p), 4, 4, YuvRange::Limited).unwrap();
        let b = yuv420p_to_rgb24(&view_of(&p), 4, 4, YuvRange::Limited).unwrap();
        for i in 0..16 {
            assert_eq!(&a[i * 4..i * 4 + 3], &b[i * 3..i * 3 + 3]);
        }
    }

    #[test]
    fn plane_too_short_is_rejected() {
        let bad = Yuv420pView {
            y: &[0; 10],
            y_stride: 4,
            u: &[0; 4],
            u_stride: 2,
            v: &[0; 4],
            v_stride: 2,
        };
        assert!(matches!(
            yuv420p_to_rgba(&bad, 4, 4, YuvRange::Limited),
            Err(FilterError::InvalidArguments(_))
        ));
    }

    #[test]
    fn odd_dimensions_round_up_chroma() {
        // 3×3 帧：色度 2×2。构造 3 宽跨距 3。
        let view = Yuv420pView {
            y: &[128u8; 9],
            y_stride: 3,
            u: &[128u8; 4],
            u_stride: 2,
            v: &[128u8; 4],
            v_stride: 2,
        };
        let out = yuv420p_to_rgba(&view, 3, 3, YuvRange::Limited).unwrap();
        assert_eq!(out.len(), 3 * 3 * 4);
        assert_eq!(&out[..4], &[130, 130, 130, 255]);
    }

    #[test]
    fn roundtrip_limited_neutral_content() {
        // 灰阶 roundtrip：rgb→yuv→rgb 在灰轴上应回到 ±2 内。
        for gray in [0u8, 32, 64, 128, 200, 255] {
            let rgba: Vec<u8> = (0..64).flat_map(|_| [gray, gray, gray, 255]).collect();
            let yuv = rgba_to_yuv420p(&rgba, 8, 8, YuvRange::Limited).unwrap();
            let back = yuv420p_to_rgba(&yuv.view(), 8, 8, YuvRange::Limited).unwrap();
            for px in back.chunks(4).take(3) {
                for k in 0..3 {
                    assert!(
                        (i16::from(px[k]) - i16::from(gray)).abs() <= 2,
                        "gray {gray}: {:?}",
                        px
                    );
                }
            }
        }
    }

    #[test]
    fn roundtrip_full_primary_colors() {
        for (r, g, b) in [
            (255u8, 0u8, 0u8),
            (0, 255, 0),
            (0, 0, 255),
            (255, 255, 255),
            (0, 0, 0),
        ] {
            let rgba: Vec<u8> = (0..64).flat_map(|_| [r, g, b, 255]).collect();
            let yuv = rgba_to_yuv420p(&rgba, 8, 8, YuvRange::Full).unwrap();
            let back = yuv420p_to_rgba(&yuv.view(), 8, 8, YuvRange::Full).unwrap();
            for px in back.chunks(4).take(3) {
                assert!(
                    (i16::from(px[0]) - i16::from(r)).abs() <= 2
                        && (i16::from(px[1]) - i16::from(g)).abs() <= 2
                        && (i16::from(px[2]) - i16::from(b)).abs() <= 2,
                    "({r},{g},{b}) -> {:?} (yuv {:?})",
                    px,
                    &yuv.as_bytes()[..3]
                );
            }
        }
    }

    #[test]
    fn rgb_to_yuv_known_limited() {
        // (255,0,0) → BT.601 有限范围经典 (81,90,240)（±1 表内舍入）。
        let rgba: Vec<u8> = (0..64).flat_map(|_| [255u8, 0, 0, 255]).collect();
        let yuv = rgba_to_yuv420p(&rgba, 8, 8, YuvRange::Limited).unwrap();
        assert!((i16::from(yuv.y()[0]) - 81).abs() <= 1);
        assert!((i16::from(yuv.u()[0]) - 90).abs() <= 1);
        assert!((i16::from(yuv.v()[0]) - 240).abs() <= 1);
    }

    #[test]
    fn zero_dimension_rejected() {
        assert!(matches!(
            rgba_to_yuv420p(&[0; 4], 0, 1, YuvRange::Limited),
            Err(FilterError::InvalidArguments(_))
        ));
    }

    // ---- ffmpeg oracle 对拍（PATH 无 ffmpeg 时打印跳过信息返回）----

    use crate::video::testutil::{
        Lcg, ORACLE_CONVERT_MAX_ABS, ffmpeg_available, max_abs_diff, run_ffmpeg_raw,
    };

    /// 随机噪声 yuv420p 帧（全立方体覆盖）。
    fn noise_frame(lcg: &mut Lcg, w: usize, h: usize) -> Vec<u8> {
        let mut d = Vec::with_capacity(w * h * 3 / 2);
        for _ in 0..w * h + 2 * (w / 2) * (h / 2) {
            d.push(lcg.next_byte());
        }
        d
    }

    fn tight_view(data: &[u8], w: usize, h: usize) -> Yuv420pView<'_> {
        let cw = w / 2;
        let ch = h / 2;
        let (y, rest) = data.split_at(w * h);
        let (u, v) = rest.split_at(cw * ch);
        Yuv420pView {
            y,
            y_stride: w,
            u,
            u_stride: cw,
            v,
            v_stride: cw,
        }
    }

    // Use the high-precision reference for the full YUV byte cube: FFmpeg 9's
    // packed 8-bit full-chroma path wraps e.g. (247,251,185) blue to zero.
    // RGB48 clips correctly; quantize only after that clipping. Keep maxAbs=1.
    // This path expands 8-bit code values by 256, not 257: neutral full-range
    // 128/200 become 32768/51200; limited white 235 becomes 65283. Thus round
    // by +128 then >>8, clipping the 65535 saturation endpoint back to 255.
    fn reference_rgb(frame: &[u8], w: usize, h: usize, format: &str, alpha: bool) -> Vec<u8> {
        let rgb48 = run_ffmpeg_raw(
            frame,
            w,
            h,
            format,
            &[
                "-cpuflags",
                "0",
                "-sws_flags",
                "neighbor+full_chroma_int+accurate_rnd",
            ],
            "rgb48le",
        );
        let mut out = Vec::with_capacity(w * h * if alpha { 4 } else { 3 });
        for pixel in rgb48.chunks_exact(6) {
            for sample in pixel.chunks_exact(2) {
                let value = u32::from(u16::from_le_bytes([sample[0], sample[1]]));
                out.push(((value + 128) >> 8).min(255) as u8);
            }
            if alpha {
                out.push(255);
            }
        }
        out
    }

    #[test]
    fn fixed_point_matrix_golden_endpoints_and_extreme_chroma() {
        for matrix in [YuvMatrix::Bt601, YuvMatrix::Bt709] {
            for (range, black, white) in [(YuvRange::Limited, 16, 235), (YuvRange::Full, 0, 255)] {
                let c = coefficients(range, matrix);
                assert_eq!(rgb_sample(black, 128, 128, &c), [0; 3]);
                assert_eq!(rgb_sample(white, 128, 128, &c), [255; 3]);
            }
        }
        assert_eq!(
            rgb_sample(
                247,
                251,
                185,
                &coefficients(YuvRange::Limited, YuvMatrix::Bt601)
            ),
            [255, 174, 255]
        );
        assert_eq!(
            rgb_sample(
                81,
                90,
                240,
                &coefficients(YuvRange::Limited, YuvMatrix::Bt601)
            ),
            [254, 0, 0]
        );
        assert_eq!(
            rgb_sample(
                81,
                90,
                240,
                &coefficients(YuvRange::Limited, YuvMatrix::Bt709)
            ),
            [255, 24, 0]
        );
    }

    /// 32×32 全随机噪声 ×4 帧与最近邻、高精度参考比较。
    /// 容差写死 ORACLE_CONVERT_MAX_ABS=1（实测有限范围 max=1）。
    #[test]
    fn oracle_convert_rgba_matches_swscale() {
        if !ffmpeg_available() {
            eprintln!("skip: PATH 无 ffmpeg，oracle_convert_rgba_matches_swscale");
            return;
        }
        let mut lcg = Lcg(0x2026_1025);
        let (w, h) = (32usize, 32usize);
        let mut worst = 0u32;
        for _ in 0..4 {
            let frame = noise_frame(&mut lcg, w, h);
            let ref_out = reference_rgb(&frame, w, h, "yuv420p", true);
            let mine =
                yuv420p_to_rgba(&tight_view(&frame, w, h), w, h, YuvRange::Limited).expect("转换");
            let d = max_abs_diff(&mine, &ref_out);
            worst = worst.max(d);
            assert!(
                d <= ORACLE_CONVERT_MAX_ABS,
                "convert rgba 超容差 {ORACLE_CONVERT_MAX_ABS}: max_abs={d}"
            );
        }
        eprintln!("oracle convert rgba: max_abs={worst} (容差 {ORACLE_CONVERT_MAX_ABS})");
    }

    /// rgb24 同口径。
    #[test]
    fn oracle_convert_rgb24_matches_swscale() {
        if !ffmpeg_available() {
            eprintln!("skip: PATH 无 ffmpeg，oracle_convert_rgb24_matches_swscale");
            return;
        }
        let mut lcg = Lcg(0x2026_1026);
        let (w, h) = (32usize, 32usize);
        let mut worst = 0u32;
        for _ in 0..3 {
            let frame = noise_frame(&mut lcg, w, h);
            let ref_out = reference_rgb(&frame, w, h, "yuv420p", false);
            let mine =
                yuv420p_to_rgb24(&tight_view(&frame, w, h), w, h, YuvRange::Limited).expect("转换");
            let d = max_abs_diff(&mine, &ref_out);
            worst = worst.max(d);
            assert!(
                d <= ORACLE_CONVERT_MAX_ABS,
                "convert rgb24 超容差 {ORACLE_CONVERT_MAX_ABS}: max_abs={d}"
            );
        }
        eprintln!("oracle convert rgb24: max_abs={worst} (容差 {ORACLE_CONVERT_MAX_ABS})");
    }

    /// 全程范围（yuvj420p 输入）沿用同一高精度参考，容差保持 1。
    #[test]
    fn oracle_convert_full_range_matches_swscale() {
        if !ffmpeg_available() {
            eprintln!("skip: PATH 无 ffmpeg，oracle_convert_full_range_matches_swscale");
            return;
        }
        let mut lcg = Lcg(0x2026_1027);
        let (w, h) = (32usize, 32usize);
        let mut worst = 0u32;
        for _ in 0..3 {
            let frame = noise_frame(&mut lcg, w, h);
            let ref_out = reference_rgb(&frame, w, h, "yuvj420p", true);
            let mine =
                yuv420p_to_rgba(&tight_view(&frame, w, h), w, h, YuvRange::Full).expect("转换");
            let d = max_abs_diff(&mine, &ref_out);
            worst = worst.max(d);
            assert!(
                d <= ORACLE_CONVERT_MAX_ABS,
                "convert full-range 超容差 {ORACLE_CONVERT_MAX_ABS}: max_abs={d}"
            );
        }
        eprintln!("oracle convert full-range: max_abs={worst} (容差 {ORACLE_CONVERT_MAX_ABS})");
    }
}

/// YUV 矩阵选择；范围独立由 YuvRange 表达。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YuvMatrix {
    Bt601,
    Bt709,
}

/// 显式矩阵转换，与 BT.601 入口共用一次舍入的定点内核。
pub fn yuv420p_to_rgba_matrix(
    src: &Yuv420pView<'_>,
    width: usize,
    height: usize,
    range: YuvRange,
    matrix: YuvMatrix,
) -> Result<Vec<u8>, FilterError> {
    src.validate(width, height)?;
    let coefficients = coefficients(range, matrix);
    let mut out = vec![0; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let (yy, uu, vv) = src.sample(y, x);
            let i = (y * width + x) * 4;
            out[i..i + 3].copy_from_slice(&rgb_sample(yy, uu, vv, &coefficients));
            out[i + 3] = 255;
        }
    }
    Ok(out)
}
