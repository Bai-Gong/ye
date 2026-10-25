//! 双线性缩放 + 裁剪 + fps 重采样 + 音量（M2 像素核，纯函数、无状态）。
//!
//! ## 缩放对齐口径（swscale oracle，2026-10-25 探测锁定）
//!
//! [`scale_yuv420p`] 是 **FFmpeg swscale `initFilter`（SWS_BILINEAR 分支）的
//! 忠实移植**，逐几何经 `-vf scale=WxH:flags=bilinear`（yuv420p 输出）冲激
//! 响应验证一致（4→8/8→4/16→4/6→4/5 帧高等升降组合、边缘重归一化、误差
//! 扩散归一化全部逐字节吻合）：
//!
//! - **核形状**：tent 核（半宽 1 源像素），下采样时滤波窗按
//!   `1 + ceil(2·srcW/dstW)` 加宽（抗混叠），窗宽上限 `srcW−2`、下限 1；
//! - **采样相位**：中心对齐（`(i+0.5)·srcW/dstW − 0.5`），定点表达为
//!   swscale 原式的「双倍单位」整数序列（`xDstInSrc += 2·xInc`）；
//! - **边界**：出界系数并入最近的有效源像素（左移归零/右移累加，即 swscale
//!   的 `filterPos` 边界修复），随后整表归一化 + 逐抽头**误差扩散**取整
//!   （水平 Q14、垂直 Q12）；
//! - **两趟定点**：水平趟 `Σ src·coef >> 7` 截断进 15 位（封顶 32767），
//!   垂直趟 `(Σ mid·coef + 2^18) >> 19` 舍入回 8 位。
//!
//! [`scale_rgba`] 为**教科书双线性**（中心对齐、边缘钳位、半进位取整），
//! 服务 convert 泵的固定链序（decode→convert→scale 的 scale 槽在 RGBA 域）。
//! 同尺寸时恒等逐字节复制。swscale 无 RGBA→RGBA 缩放路径，此函数的对拍
//! 口径为「先转换后缩放」组合对拍（见模块 `oracle` 测试：与两段式
//! ffmpeg 参考链对拍）。
//!
//! ## 已知偏离（登记，非 bug）
//!
//! ffmpeg 命令 `scale=W:H,format=rgba` 会被滤镜图**合并为单个 swscale 实例**
//! （yuv420p→rgba 直接缩放转换一体），其亮度垂直采样相位按色度网格对齐
//! （探测实测：相对平面缩放偏移 −0.5 源像素）且全程保持 >8 位精度。
//! 本模块的纯函数组合（先 [`scale_yuv420p`] 再 [`convert`][crate::video]）
//! 与该合并实例在平滑梯度上有最大 ~7/通道的**结构性**偏差；与**两段式**
//! 参考链 `scale=W:H,format=yuv420p,format=rgba`（先缩放后转换，语义与本
//! 组合一致）偏差 ≤4/通道。oracle 测试采用两段式参考并写死容差 4
//! （见 [`ORACLE_CHAIN_MAX_ABS`]）。剪（jian）E2E 双引擎 |Δ|≤2 门禁若以
//! 单段 ffmpeg 命令为对照，需在剪侧把 ffmpeg 引擎改为两段式（或后续为本
//! 模块补合并路径内核）。
//!
//! ## crop / fps / volume
//!
//! - [`crop_rgba`]：矩形裁剪（行拷贝，零运算）；
//! - [`fps_nearest_frame_indices`]：时基重采样按最近帧（输出槽时刻
//!   `t0 + k/fps`，取 |pts−t| 最小的输入帧，并列取更早帧）；
//! - [`volume_f32`]：f32 增益乘法 + 削波到 [−1,1]。

use super::convert::Yuv420pBuffer;
use super::convert::Yuv420pView;
use crate::error::FilterError;

/// 水平滤波系数量化（swscale one）。
const ONE_H: i64 = 1 << 14;
/// 垂直滤波系数量化。
const ONE_V: i64 = 1 << 12;

/// 一维双线性滤波器（swscale initFilter SWS_BILINEAR 的移植产物）。
#[derive(Debug, Clone)]
struct LineFilter {
    /// 每个输出像素的首抽头源位置。
    pos: Vec<i32>,
    /// 系数表（`filterSize * dst` 平铺，量化 `one`）。
    coefs: Vec<i32>,
    /// 抽头数。
    size: usize,
}

/// C 语义除法（向零截断；Rust 整数除法即为该语义，显式命名以示对齐）。
fn c_div(a: i64, b: i64) -> i64 {
    a / b
}

/// av_ ROUNDED_DIV（正除数；按被除数符号加/减半基数后向零截断）。
fn rounded_div(a: i64, b: i64) -> i64 {
    if a >= 0 {
        (a + (b >> 1)) / b
    } else {
        (a - (b >> 1)) / b
    }
}

/// 构建 1-D 双线性滤波器（`src`→`dst`，量化 `one`）。
///
/// 逐语句移植自 FFmpeg `libswscale/utils.c` 的 `initFilter`
/// （SWS_BILINEAR、srcPos=dstPos=128、无 srcFilter/dstFilter/减少步）；
/// 探测验证见模块头注。
fn build_line_filter(src: usize, dst: usize, one: i64) -> LineFilter {
    let src_w = src as i64;
    let dst_w = dst as i64;
    let x_inc = c_div((src_w << 16) + (dst_w >> 1), dst_w);
    // sizeFactor = 2（SWS_BILINEAR）
    let mut filter_size: i64 = if x_inc <= (1 << 16) {
        1 + 2
    } else {
        1 + c_div(2 * src_w + dst_w - 1, dst_w)
    };
    filter_size = filter_size.min(src_w - 2).max(1);
    let fs = filter_size as usize;

    // fone = 1<<(54 - min(av_log2(srcW/dstW), 8))；av_log2(0)=0
    let shift = if src >= dst {
        ((src / dst) as u32).ilog2()
    } else {
        0
    };
    let fone: i64 = 1 << (54 - shift.min(8));

    // xDstInSrc = ((dstPos·xInc)>>7) − ((srcPos·0x10000)>>7)，srcPos=dstPos=128
    let mut x_dst_in_src = ((128 * x_inc) >> 7) - ((128 * 0x10000) >> 7);

    let mut pos: Vec<i32> = Vec::with_capacity(dst);
    let mut coeff: Vec<i64> = Vec::with_capacity(dst * fs);
    for _ in 0..dst {
        let xx_start = c_div(x_dst_in_src - ((filter_size - 2) << 16), 1 << 17);
        pos.push(xx_start as i32);
        for xx in xx_start..xx_start + filter_size {
            let mut d = (xx * (1 << 17) - x_dst_in_src).unsigned_abs() as i64;
            d <<= 13;
            if x_inc > (1 << 16) {
                d = c_div(d * dst_w, src_w);
            }
            let mut c = (1 << 30) - d;
            if c < 0 {
                c = 0;
            }
            c *= fone >> 30;
            coeff.push(c);
        }
        x_dst_in_src += 2 * x_inc;
    }

    // 边界修复：左出界并入 0 号抽头；右出界累加到末个有效抽头
    for i in 0..dst {
        let p = pos[i] as i64;
        if p < 0 {
            for j in 1..fs {
                let left = (j as i64 + p).max(0) as usize;
                coeff[i * fs + left] += coeff[i * fs + j];
                coeff[i * fs + j] = 0;
            }
            pos[i] = 0;
        }
        if p + filter_size > src_w {
            let shift_amt = p + (filter_size - src_w).min(0);
            let mut acc: i64 = 0;
            for j in (0..fs).rev() {
                if p + j as i64 >= src_w {
                    acc += coeff[i * fs + j];
                    coeff[i * fs + j] = 0;
                }
            }
            for j in (0..fs).rev() {
                let k = j as i64;
                coeff[i * fs + j] = if k < shift_amt {
                    0
                } else {
                    coeff[i * fs + (k - shift_amt) as usize]
                };
            }
            pos[i] -= shift_amt as i32;
            let end = (src_w - 1 - pos[i] as i64) as usize;
            coeff[i * fs + end] += acc;
        }
    }

    // 归一化 + 误差扩散取整
    let mut coefs: Vec<i32> = vec![0; dst * fs];
    for i in 0..dst {
        let mut error: i64 = 0;
        let sum: i64 = c_div(
            coeff[i * fs..(i + 1) * fs].iter().sum::<i64>() + (one >> 1),
            one,
        );
        let sum = if sum == 0 { 1 } else { sum };
        for j in 0..fs {
            let v = coeff[i * fs + j] + error;
            let int_v = rounded_div(v, sum);
            coefs[i * fs + j] = int_v as i32;
            error = v - int_v * sum;
        }
    }
    LineFilter {
        pos,
        coefs,
        size: fs,
    }
}

/// 水平趟：8 位行 → 15 位中间行（`>>7` 截断、封顶 32767）。
fn h_pass_line(line: &[u8], f: &LineFilter, dst_w: usize) -> Vec<i32> {
    let mut out = Vec::with_capacity(dst_w);
    for i in 0..dst_w {
        let mut acc: i64 = 0;
        for j in 0..f.size {
            let k = (f.pos[i] as i64 + j as i64) as usize;
            if k < line.len() {
                acc += line[k] as i64 * f.coefs[i * f.size + j] as i64;
            }
        }
        out.push((acc >> 7).min((1 << 15) - 1) as i32);
    }
    out
}

/// 垂直趟：15 位中间行 → 8 位输出行（`(Σ + 2^18)>>19` + 截幅）。
fn v_pass_rows(mid: &[Vec<i32>], f: &LineFilter, dst_h: usize, w: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(dst_h * w);
    for r in 0..dst_h {
        for c in 0..w {
            let mut acc: i64 = 0;
            for j in 0..f.size {
                let k = f.pos[r] as i64 + j as i64;
                if k >= 0 && (k as usize) < mid.len() {
                    acc += mid[k as usize][c] as i64 * f.coefs[r * f.size + j] as i64;
                }
            }
            out.push((((acc + (1 << 18)) >> 19).clamp(0, 255)) as u8);
        }
    }
    out
}

/// 单平面缩放（水平趟 → 垂直趟）。
fn scale_plane(flat: &[u8], w: usize, h: usize, dst_w: usize, dst_h: usize) -> Vec<u8> {
    let fh = build_line_filter(w, dst_w, ONE_H);
    let mut mid: Vec<Vec<i32>> = Vec::with_capacity(h);
    for r in 0..h {
        mid.push(h_pass_line(&flat[r * w..(r + 1) * w], &fh, dst_w));
    }
    let fv = build_line_filter(h, dst_h, ONE_V);
    v_pass_rows(&mid, &fv, dst_h, dst_w)
}

/// yuv420p 平面缩放（swscale 双线性语义；三个平面独立同法缩放，色度按
/// 半分辨率几何）。
///
/// # Errors
/// - 任一维为 0，或源/目标宽高非偶（yuv420p 平面几何要求）→
///   [`FilterError::InvalidArguments`]；
/// - 平面切片过短（同 [`Yuv420pView::validate`]）。
pub fn scale_yuv420p(
    src: &Yuv420pView<'_>,
    width: usize,
    height: usize,
    dst_w: usize,
    dst_h: usize,
) -> Result<Yuv420pBuffer, FilterError> {
    if width == 0 || height == 0 || dst_w == 0 || dst_h == 0 {
        return Err(FilterError::InvalidArguments("zero frame dimension"));
    }
    if !width.is_multiple_of(2)
        || !height.is_multiple_of(2)
        || !dst_w.is_multiple_of(2)
        || !dst_h.is_multiple_of(2)
    {
        return Err(FilterError::InvalidArguments(
            "yuv420p requires even width/height (source and target)",
        ));
    }
    src.validate(width, height)?;
    let cw = width / 2;
    let ch = height / 2;
    let y = plane_rows(src.y, src.y_stride, height, width);
    let u = plane_rows(src.u, src.u_stride, ch, cw);
    let v = plane_rows(src.v, src.v_stride, ch, cw);
    let ys = scale_plane(&y, width, height, dst_w, dst_h);
    let us = scale_plane(&u, cw, ch, dst_w / 2, dst_h / 2);
    let vs = scale_plane(&v, cw, ch, dst_w / 2, dst_h / 2);
    let mut data = Vec::with_capacity(ys.len() + us.len() + vs.len());
    data.extend_from_slice(&ys);
    data.extend_from_slice(&us);
    data.extend_from_slice(&vs);
    Ok(Yuv420pBuffer::from_planes(data, dst_w, dst_h))
}

/// 行聚拢（跨距视图 → 紧密行）。
fn plane_rows(plane: &[u8], stride: usize, rows: usize, cols: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(rows * cols);
    for r in 0..rows {
        out.extend_from_slice(&plane[r * stride..r * stride + cols]);
    }
    out
}

/// RGBA（紧密，4 字节/像素）双线性缩放（教科书语义：中心对齐、边缘钳位、
/// 半进位取整；同尺寸恒等逐字节复制）。
///
/// # Errors
/// 任一维为 0，或切片长度不足 `w·h·4` → [`FilterError::InvalidArguments`]。
pub fn scale_rgba(
    rgba: &[u8],
    w: usize,
    h: usize,
    dst_w: usize,
    dst_h: usize,
) -> Result<Vec<u8>, FilterError> {
    if w == 0 || h == 0 || dst_w == 0 || dst_h == 0 {
        return Err(FilterError::InvalidArguments("zero frame dimension"));
    }
    if rgba.len() < w * h * 4 {
        return Err(FilterError::InvalidArguments("rgba slice too short"));
    }
    let mut out = vec![0u8; dst_w * dst_h * 4];
    for dr in 0..dst_h {
        let sy = (dr as f64 + 0.5) * h as f64 / dst_h as f64 - 0.5;
        let y0f = sy.floor();
        let (y0, fy) = if y0f < 0.0 {
            (0usize, 0.0f64)
        } else if y0f as usize >= h - 1 {
            (h - 1, 0.0f64)
        } else {
            (y0f as usize, sy - y0f)
        };
        let y1 = (y0 + 1).min(h - 1);
        for dc in 0..dst_w {
            let sx = (dc as f64 + 0.5) * w as f64 / dst_w as f64 - 0.5;
            let x0f = sx.floor();
            let (x0, fx) = if x0f < 0.0 {
                (0usize, 0.0f64)
            } else if x0f as usize >= w - 1 {
                (w - 1, 0.0f64)
            } else {
                (x0f as usize, sx - x0f)
            };
            let x1 = (x0 + 1).min(w - 1);
            let p00 = (y0 * w + x0) * 4;
            let p01 = (y0 * w + x1) * 4;
            let p10 = (y1 * w + x0) * 4;
            let p11 = (y1 * w + x1) * 4;
            let o = (dr * dst_w + dc) * 4;
            for k in 0..4 {
                let v = rgba[p00 + k] as f64 * (1.0 - fx) * (1.0 - fy)
                    + rgba[p01 + k] as f64 * fx * (1.0 - fy)
                    + rgba[p10 + k] as f64 * (1.0 - fx) * fy
                    + rgba[p11 + k] as f64 * fx * fy;
                out[o + k] = (v + 0.5).floor().clamp(0.0, 255.0) as u8;
            }
        }
    }
    Ok(out)
}

/// 矩形裁剪（RGBA，紧密排布；`(x, y)` 为裁剪窗左上角，行拷贝零运算）。
///
/// # Errors
/// 裁剪窗越界或为零尺寸 → [`FilterError::InvalidArguments`]。
pub fn crop_rgba(
    rgba: &[u8],
    w: usize,
    h: usize,
    x: usize,
    y: usize,
    cw: usize,
    ch: usize,
) -> Result<Vec<u8>, FilterError> {
    if w == 0 || h == 0 || rgba.len() < w * h * 4 {
        return Err(FilterError::InvalidArguments("rgba size mismatch"));
    }
    if cw == 0 || ch == 0 || x + cw > w || y + ch > h {
        return Err(FilterError::InvalidArguments("crop window out of bounds"));
    }
    let mut out = Vec::with_capacity(cw * ch * 4);
    for r in 0..ch {
        let start = ((y + r) * w + x) * 4;
        out.extend_from_slice(&rgba[start..start + cw * 4]);
    }
    Ok(out)
}

/// fps 时基重采样（最近帧）：给定输入帧的呈现时刻（微秒、单调不减）与
/// 目标帧率 `fps_num/fps_den`，返回输出槽依次应发射的输入帧下标序列。
///
/// 输出槽时刻 `t_k = pts[0] + k·1e6·fps_den/fps_num`（k = 0..N，
/// `N = floor((pts_last − pts[0])·fps_num / (1e6·fps_den)) + 1`），
/// 每槽取 `|pts_i − t_k|` 最小的输入帧，**并列取下标更小（更早）的帧**。
/// 与 FFmpeg `fps` 滤镜的最近帧语义为近似对齐（其舍入细节属 M3 校准点，
/// 见模块头注）。
///
/// # Errors
/// - `fps_num`/`fps_den` 为 0 → [`FilterError::InvalidArguments`]；
/// - `pts_us` 非单调不减 → [`FilterError::InvalidArguments`]。
pub fn fps_nearest_frame_indices(
    pts_us: &[i64],
    fps_num: u32,
    fps_den: u32,
) -> Result<Vec<usize>, FilterError> {
    if fps_num == 0 || fps_den == 0 {
        return Err(FilterError::InvalidArguments(
            "fps numerator/denominator must be nonzero",
        ));
    }
    for w in pts_us.windows(2) {
        if w[0] > w[1] {
            return Err(FilterError::InvalidArguments("pts must be non-decreasing"));
        }
    }
    if pts_us.is_empty() {
        return Ok(Vec::new());
    }
    let t0 = pts_us[0];
    let last = pts_us[pts_us.len() - 1];
    // 槽时刻以定点整数比较：k·step_num ≤ (last − t0)·fps_num
    let step_num: i128 = 1_000_000 * fps_den as i128;
    let span: i128 = (last - t0) as i128 * fps_num as i128;
    let slots = (span / step_num + 1).max(1) as usize;
    let mut out = Vec::with_capacity(slots);
    for k in 0..slots {
        let target: i128 = k as i128 * step_num;
        let mut best = 0usize;
        let mut best_dist = i128::MAX;
        for (i, p) in pts_us.iter().enumerate() {
            let d = ((*p as i128 - t0 as i128) * fps_num as i128 - target).abs();
            if d < best_dist {
                best_dist = d;
                best = i;
            }
        }
        out.push(best);
    }
    Ok(out)
}

/// f32 增益乘法（音量槽）：`out = clamp(sample·gain, −1, 1)`。
///
/// **削波语义**：超出 [−1,1] 的乘积饱和到边界（FFmpeg `volume` 滤镜
/// `−1..1` 削波一致）。`gain` 须有限（NaN/±inf 报错）；样本为 NaN 时
/// 结果原样传播 NaN（输入数据契约：解码 PCM 应有限）。
///
/// # Errors
/// `gain` 非有限 → [`FilterError::InvalidArguments`]。
pub fn volume_f32(samples: &[f32], gain: f32) -> Result<Vec<f32>, FilterError> {
    if !gain.is_finite() {
        return Err(FilterError::InvalidArguments("gain must be finite"));
    }
    Ok(samples
        .iter()
        .map(|&s| (s * gain).clamp(-1.0, 1.0))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 平面视图辅助：紧密平面 + 跨距=宽。
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

    /// yuv420p 帧（luma 指定，色度中性 128）。
    fn luma_frame(luma: &[u8], w: usize, h: usize) -> Vec<u8> {
        let mut d = luma.to_vec();
        d.extend_from_slice(&vec![128u8; (w / 2) * (h / 2) * 2]);
        d
    }

    // ---- 冲激响应：与 ffmpeg -vf scale 探测值逐字节锁死（见模块头注）----

    #[test]
    fn impulse_upscale_4_to_8_matches_ffmpeg() {
        // luma [0,255,0,0]（列冲激），4x4 → 8x4，读首行：
        // ffmpeg 实测 [0,64,191,191,64,0,0,0]
        let luma: Vec<u8> = (0..4).flat_map(|_| [0u8, 255, 0, 0]).collect();
        let frame = luma_frame(&luma, 4, 4);
        let out = scale_yuv420p(&tight_view(&frame, 4, 4), 4, 4, 8, 4).unwrap();
        assert_eq!(&out.y()[..8], &[0, 64, 191, 191, 64, 0, 0, 0]);
    }

    #[test]
    fn impulse_downscale_8_to_4_matches_ffmpeg() {
        // luma 冲激在 x=3，8x4 → 4x4：ffmpeg 实测 [0,96,32,0]
        let luma: Vec<u8> = (0..4)
            .flat_map(|_| {
                let mut row = vec![0u8; 8];
                row[3] = 255;
                row
            })
            .collect();
        let frame = luma_frame(&luma, 8, 4);
        let out = scale_yuv420p(&tight_view(&frame, 8, 4), 8, 4, 4, 4).unwrap();
        assert_eq!(&out.y()[..4], &[0, 96, 32, 0]);
    }

    #[test]
    fn impulse_edge_renorm_8_to_4() {
        // 冲激在 x=0（左边缘）：ffmpeg 实测 [128,0,0,0]（出界系数并入）
        let luma: Vec<u8> = (0..4)
            .flat_map(|_| {
                let mut row = vec![0u8; 8];
                row[0] = 255;
                row
            })
            .collect();
        let frame = luma_frame(&luma, 8, 4);
        let out = scale_yuv420p(&tight_view(&frame, 8, 4), 8, 4, 4, 4).unwrap();
        assert_eq!(&out.y()[..4], &[128, 0, 0, 0]);
    }

    #[test]
    fn impulse_noninteger_6_to_4_matches_ffmpeg() {
        // 6→4 全部冲激位置：ffmpeg 实测响应
        let expect = [
            ([159, 0, 0, 0], 0),
            ([96, 85, 0, 0], 1),
            ([0, 142, 28, 0], 2),
            ([0, 28, 142, 0], 3),
            ([0, 0, 85, 85], 4),
            ([0, 0, 0, 170], 5),
        ];
        for (exp, imp) in expect {
            let luma: Vec<u8> = (0..4)
                .flat_map(|_| {
                    let mut row = vec![0u8; 6];
                    row[imp] = 255;
                    row
                })
                .collect();
            let frame = luma_frame(&luma, 6, 4);
            let out = scale_yuv420p(&tight_view(&frame, 6, 4), 6, 4, 4, 4).unwrap();
            assert_eq!(&out.y()[..4], &exp, "impulse at {imp}");
        }
    }

    #[test]
    fn identity_scale_is_exact_copy() {
        let frame = luma_frame(&(0..64).map(|i| (i * 4) as u8).collect::<Vec<_>>(), 8, 8);
        let out = scale_yuv420p(&tight_view(&frame, 8, 8), 8, 8, 8, 8).unwrap();
        assert_eq!(out.as_bytes(), &frame[..]);
    }

    #[test]
    fn even_dimension_requirement() {
        let frame = luma_frame(&[0u8; 36], 6, 6);
        assert!(matches!(
            scale_yuv420p(&tight_view(&frame, 6, 6), 6, 6, 3, 3),
            Err(FilterError::InvalidArguments(_))
        ));
        let frame = luma_frame(&[0u8; 36], 6, 6);
        assert!(scale_yuv420p(&tight_view(&frame, 6, 6), 6, 6, 4, 4).is_ok());
    }

    // ---- scale_rgba ----

    #[test]
    fn scale_rgba_identity_exact() {
        let src: Vec<u8> = (0..12 * 8 * 4).map(|i| (i * 7 % 251) as u8).collect();
        let out = scale_rgba(&src, 12, 8, 12, 8).unwrap();
        assert_eq!(out, src);
    }

    #[test]
    fn scale_rgba_2x_upscale_known_values() {
        // 2x2 灰度 [10,100 / 100,10] → 4x4 教科书双线性（边缘延拓）。
        // fx/fy 相位 = [0, 0.25, 0.75, 1→边缘延拓 0]：
        //   10  33  78 100
        //   33  44  66  78
        //   78  66  44  33
        //  100  78  33  10
        let mut src = vec![0u8; 2 * 2 * 4];
        for (i, v) in [10u8, 100, 100, 10].iter().enumerate() {
            let o = i * 4;
            src[o..o + 4].copy_from_slice(&[*v, *v, *v, 255]);
        }
        let out = scale_rgba(&src, 2, 2, 4, 4).unwrap();
        let expect = [
            [10, 33, 78, 100],
            [33, 44, 66, 78],
            [78, 66, 44, 33],
            [100, 78, 33, 10],
        ];
        for r in 0..4 {
            for c in 0..4 {
                let px = &out[(r * 4 + c) * 4..(r * 4 + c) * 4 + 3];
                assert_eq!(px[0], expect[r][c], "px({r},{c}) R");
                assert_eq!(px[1], expect[r][c], "px({r},{c}) G");
                assert_eq!(px[2], expect[r][c], "px({r},{c}) B");
            }
        }
    }

    // ---- crop ----

    #[test]
    fn crop_middle_window() {
        let mut src = vec![0u8; 4 * 4 * 4];
        for r in 0..4 {
            for c in 0..4 {
                src[(r * 4 + c) * 4] = (r * 4 + c) as u8;
            }
        }
        let out = crop_rgba(&src, 4, 4, 1, 1, 2, 2).unwrap();
        assert_eq!(out.len(), 2 * 2 * 4);
        assert_eq!(out[0], 5); // (1,1)
        assert_eq!(out[4], 6); // (1,2)
        assert_eq!(out[8], 9); // (2,1)
        assert_eq!(out[12], 10); // (2,2)
    }

    #[test]
    fn crop_bounds_errors() {
        let src = vec![0u8; 4 * 4 * 4];
        assert!(crop_rgba(&src, 4, 4, 3, 0, 2, 2).is_err()); // x+cw > w
        assert!(crop_rgba(&src, 4, 4, 0, 3, 4, 2).is_err()); // y+ch > h
        assert!(crop_rgba(&src, 4, 4, 0, 0, 0, 1).is_err()); // 零宽
        assert!(crop_rgba(&src, 4, 4, 0, 0, 4, 4).is_ok()); // 全幅合法
    }

    // ---- fps ----

    #[test]
    fn fps_24_to_30_nearest() {
        // 24fps 源 [0, 41666, 83333, 125000] → 30fps（步距 33333µs）：
        // 槽 0/33333/66666/99999 → 帧 0/1/2/2（手算见函数文档）
        let idx = fps_nearest_frame_indices(&[0, 41666, 83333, 125000], 30, 1).unwrap();
        assert_eq!(idx, vec![0, 1, 2, 2]);
    }

    #[test]
    fn fps_upsample_repeats_frames() {
        // 6fps → 12fps：步距 83333µs，槽 7 个（末槽恰为末帧时刻）
        let pts: Vec<i64> = (0..4).map(|i| i * 166_667).collect();
        let idx = fps_nearest_frame_indices(&pts, 12, 1).unwrap();
        assert_eq!(idx, vec![0, 0, 1, 1, 2, 2, 3]);
    }

    #[test]
    fn fps_tie_takes_earlier_frame() {
        // pts [0, 62500]、32fps（步距 31250µs）：槽 1 恰在 31250 = 两帧中点
        // → 距离并列（缩放后各 1000000），取更早的帧 0
        let idx = fps_nearest_frame_indices(&[0, 62500], 32, 1).unwrap();
        assert_eq!(idx, vec![0, 0, 1]);
    }

    #[test]
    fn fps_rejects_bad_input() {
        assert!(fps_nearest_frame_indices(&[0, 100], 0, 1).is_err());
        assert!(fps_nearest_frame_indices(&[0, 100], 30, 0).is_err());
        assert!(fps_nearest_frame_indices(&[100, 50], 30, 1).is_err());
        assert!(fps_nearest_frame_indices(&[], 30, 1).unwrap().is_empty());
    }

    // ---- volume ----

    #[test]
    fn volume_gain_and_clipping() {
        assert_eq!(
            volume_f32(&[0.5, -0.5, 0.0], 2.0).unwrap(),
            vec![1.0, -1.0, 0.0]
        );
        // 0.9·2 = 1.8 → 削波 1.0
        assert_eq!(volume_f32(&[0.9], 2.0).unwrap(), vec![1.0]);
        assert_eq!(volume_f32(&[-0.9], 2.0).unwrap(), vec![-1.0]);
        // 负增益 = 反相
        assert_eq!(volume_f32(&[0.25], -2.0).unwrap(), vec![-0.5]);
        // 静音
        assert_eq!(volume_f32(&[0.7, -0.7], 0.0).unwrap(), vec![0.0, 0.0]);
        // 界内不削波
        assert_eq!(volume_f32(&[0.5], 0.5).unwrap(), vec![0.25]);
    }

    #[test]
    fn volume_rejects_nonfinite_gain() {
        assert!(volume_f32(&[0.5], f32::NAN).is_err());
        assert!(volume_f32(&[0.5], f32::INFINITY).is_err());
    }

    // ---- ffmpeg oracle 对拍（PATH 无 ffmpeg 时打印跳过信息返回）----

    use crate::video::convert::YuvRange;
    use crate::video::convert::yuv420p_to_rgba;
    use crate::video::testutil::{
        Lcg, ORACLE_CHAIN_MAX_ABS, ORACLE_SCALE_YUV_MAX_ABS, ffmpeg_available, max_abs_diff,
        run_ffmpeg_raw,
    };

    /// 平滑 yuv420p 向量（低频正弦梯度，缩略图/抽帧类内容）。
    fn smooth_frame(seed: u64, w: usize, h: usize) -> Vec<u8> {
        let ph: Vec<f64> = (0..6)
            .map(|i| {
                let mut l = Lcg(seed.wrapping_add(i as u64));
                (l.next_u32() % 6283) as f64 / 1000.0
            })
            .collect();
        let clip = |x: i64| x.clamp(0, 255) as u8;
        let mut d = Vec::with_capacity(w * h * 3 / 2);
        for r in 0..h {
            for c in 0..w {
                let v = 128.0
                    + 100.0 * (ph[0] + (c as f64 / w as f64) * 2.5 + (r as f64 / h as f64) * 1.7);
                d.push(clip(v as i64));
            }
        }
        for r in 0..h / 2 {
            for c in 0..w / 2 {
                let v = 128.0
                    + 60.0 * (ph[1] + (c as f64 / w as f64) * 1.3 + (r as f64 / h as f64) * 2.1);
                d.push(clip(v as i64));
            }
        }
        for r in 0..h / 2 {
            for c in 0..w / 2 {
                let v = 128.0
                    + 60.0 * (ph[2] + (c as f64 / w as f64) * 2.9 + (r as f64 / h as f64) * 0.9);
                d.push(clip(v as i64));
            }
        }
        d
    }

    fn tight_view2(data: &[u8], w: usize, h: usize) -> Yuv420pView<'_> {
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

    /// 平面缩放（yuv420p→yuv420p）vs
    /// `-sws_flags bilinear -vf scale=WxH:flags=bilinear`。
    /// 容差写死 ORACLE_SCALE_YUV_MAX_ABS=2（实测 ≤2，多为 1）。
    #[test]
    fn oracle_scale_yuv420p_matches_swscale() {
        if !ffmpeg_available() {
            eprintln!("skip: PATH 无 ffmpeg，oracle_scale_yuv420p_matches_swscale");
            return;
        }
        let cases = [
            (48usize, 36usize, 32usize, 24usize),
            (32, 24, 64, 48),
            (24, 16, 36, 24),
            (64, 48, 24, 18),
            (48, 36, 48, 36),
        ];
        let mut worst = 0u32;
        for (w, h, dw, dh) in cases {
            for seed in [1u64, 2] {
                let frame = smooth_frame(seed, w, h);
                let vf: String = format!("scale={dw}x{dh}:flags=bilinear");
                let ref_out = run_ffmpeg_raw(&frame, w, h, "yuv420p", &["-vf", &vf], "yuv420p");
                let mine = scale_yuv420p(&tight_view2(&frame, w, h), w, h, dw, dh).expect("缩放");
                let d = max_abs_diff(mine.as_bytes(), &ref_out);
                worst = worst.max(d);
                assert!(
                    d <= ORACLE_SCALE_YUV_MAX_ABS,
                    "scale {w}x{h}->{dw}x{dh} seed{seed} 超容差 {ORACLE_SCALE_YUV_MAX_ABS}: max_abs={d}"
                );
            }
        }
        eprintln!("oracle scale yuv420p: max_abs={worst} (容差 {ORACLE_SCALE_YUV_MAX_ABS})");
    }

    /// 两段式链（本模块 scale_yuv420p + yuv420p_to_rgba 组合）vs
    /// `-sws_flags bilinear -vf scale=WxH:flags=bilinear,format=yuv420p,format=rgba`。
    /// 单段合并链（`scale=WxH,format=rgba`）的结构性偏离见模块头注（登记，不属本对拍）。
    /// 容差写死 ORACLE_CHAIN_MAX_ABS=4（实测 ≤4）。
    #[test]
    fn oracle_scale_convert_chain_two_stage() {
        if !ffmpeg_available() {
            eprintln!("skip: PATH 无 ffmpeg，oracle_scale_convert_chain_two_stage");
            return;
        }
        let cases = [
            (48usize, 36usize, 32usize, 24usize),
            (32, 24, 64, 48),
            (24, 16, 36, 24),
            (64, 48, 24, 18),
            (48, 36, 48, 36),
        ];
        let mut worst = 0u32;
        for (w, h, dw, dh) in cases {
            for seed in [1u64, 2, 3] {
                let frame = smooth_frame(seed, w, h);
                let vf: String =
                    format!("scale={dw}x{dh}:flags=bilinear,format=yuv420p,format=rgba");
                let ref_out = run_ffmpeg_raw(&frame, w, h, "yuv420p", &["-vf", &vf], "rgba");
                let scaled = scale_yuv420p(&tight_view2(&frame, w, h), w, h, dw, dh).expect("缩放");
                let mine =
                    yuv420p_to_rgba(&scaled.view(), dw, dh, YuvRange::Limited).expect("转换");
                let d = max_abs_diff(&mine, &ref_out);
                worst = worst.max(d);
                assert!(
                    d <= ORACLE_CHAIN_MAX_ABS,
                    "chain {w}x{h}->{dw}x{dh} seed{seed} 超容差 {ORACLE_CHAIN_MAX_ABS}: max_abs={d}"
                );
            }
        }
        eprintln!("oracle chain two-stage: max_abs={worst} (容差 {ORACLE_CHAIN_MAX_ABS})");
    }

    /// 施工图字面命令的单段合并链（`scale=WxH,format=rgba`，ffmpeg 滤镜图
    /// 合并为单个 swscale 实例）对拍。结构性偏离（亮度垂直相位按色度网格
    /// 对齐、缩放→转换全程 >8 位精度）见模块头注——本用例把该已知偏离
    /// **量化锁死**：容差写死 8（本机实测 ≤7），收紧它须先补合并路径内核。
    #[test]
    fn oracle_chain_merged_single_stage_literal_command() {
        if !ffmpeg_available() {
            eprintln!("skip: PATH 无 ffmpeg，oracle_chain_merged_single_stage_literal_command");
            return;
        }
        const ORACLE_CHAIN_MERGED_MAX_ABS: u32 = 8;
        let cases = [
            (48usize, 36usize, 32usize, 24usize),
            (32, 24, 64, 48),
            (24, 16, 36, 24),
        ];
        let mut worst = 0u32;
        for (w, h, dw, dh) in cases {
            for seed in [1u64, 2, 3] {
                let frame = smooth_frame(seed, w, h);
                let vf: String = format!("scale={dw}x{dh}:flags=bilinear,format=rgba");
                let ref_out = run_ffmpeg_raw(&frame, w, h, "yuv420p", &["-vf", &vf], "rgba");
                let scaled = scale_yuv420p(&tight_view2(&frame, w, h), w, h, dw, dh).expect("缩放");
                let mine =
                    yuv420p_to_rgba(&scaled.view(), dw, dh, YuvRange::Limited).expect("转换");
                let d = max_abs_diff(&mine, &ref_out);
                worst = worst.max(d);
                assert!(
                    d <= ORACLE_CHAIN_MERGED_MAX_ABS,
                    "merged chain {w}x{h}->{dw}x{dh} seed{seed} 超容差 {ORACLE_CHAIN_MERGED_MAX_ABS}: max_abs={d}"
                );
            }
        }
        eprintln!("oracle chain merged: max_abs={worst} (容差 8)");
    }
}
