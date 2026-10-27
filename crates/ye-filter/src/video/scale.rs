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
//! - [`crop_rgba`]：RGBA 矩形裁剪（行拷贝，零运算）；
//! - [`crop_yuv420p`]：yuv420p 平面域裁剪（逐平面行拷贝，零运算；
//!   x/y/w/h 全偶校验，对 ffmpeg 0 容差）；
//! - [`fps_nearest_frame_indices`]：时基重采样按最近帧（输入为
//!   [`Timestamp`] 列表、时基可逐帧不同，输出槽时刻 `t0 + k/fps`，
//!   取 |pts−t| 最小的输入帧，并列取更早帧；跨时基比较只走
//!   [`Timestamp::compare`]，全程精确无舍入）；
//! - [`volume_f32`]：f32 增益乘法 + 削波到 [−1,1]；
//!   [`volume_gain_from_db`]：dB → 线性增益参数换算（10^(dB/20)）。

use core::cmp::Ordering;

use super::convert::Yuv420pBuffer;
use super::convert::Yuv420pView;
use crate::error::FilterError;
use ye_time::{Rational, Timestamp};

/// fps 分子/分母超出 i32 表示域的失败文案。
const FPS_MUST_FIT_I32: &str = "fps numerator/denominator must fit i32";

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

/// yuv420p 平面域矩形裁剪：`(x, y)` 为裁剪窗左上角，`cw × ch` 为窗尺寸。
///
/// 纯字节搬运（逐平面行拷贝，零运算、零舍入——对 ffmpeg `crop` 滤镜
/// 0 容差），色度平面按半分辨率几何对齐（原点 `(x/2, y/2)`、尺寸
/// `cw/2 × ch/2`）。yuv420p 平面几何要求 `x`/`y`/`cw`/`ch` **全偶**
/// （奇数窗会割裂 2×2 色度块），违者 `Err`。
///
/// 消费形态对齐 [`FilterParams::Crop`](crate::FilterParams::Crop) 的
/// `x`/`y`/`width`/`height` 四字段。
///
/// # Errors
/// - 任一维为 0、或裁剪窗越界 → [`FilterError::InvalidArguments`]；
/// - `x`/`y`/`cw`/`ch` 任一为奇 → [`FilterError::InvalidArguments`]；
/// - 平面切片过短（同 [`Yuv420pView::validate`]）。
pub fn crop_yuv420p(
    src: &Yuv420pView<'_>,
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    cw: usize,
    ch: usize,
) -> Result<Yuv420pBuffer, FilterError> {
    if width == 0 || height == 0 {
        return Err(FilterError::InvalidArguments("zero frame dimension"));
    }
    if cw == 0 || ch == 0 || x + cw > width || y + ch > height {
        return Err(FilterError::InvalidArguments("crop window out of bounds"));
    }
    if !x.is_multiple_of(2)
        || !y.is_multiple_of(2)
        || !cw.is_multiple_of(2)
        || !ch.is_multiple_of(2)
    {
        return Err(FilterError::InvalidArguments(
            "yuv420p crop requires even x/y/width/height",
        ));
    }
    src.validate(width, height)?;
    let ccw = cw / 2;
    let cch = ch / 2;
    let mut data = Vec::with_capacity(cw * ch + 2 * ccw * cch);
    for r in 0..ch {
        let start = (y + r) * src.y_stride + x;
        data.extend_from_slice(&src.y[start..start + cw]);
    }
    for r in 0..cch {
        let start = (y / 2 + r) * src.u_stride + x / 2;
        data.extend_from_slice(&src.u[start..start + ccw]);
    }
    for r in 0..cch {
        let start = (y / 2 + r) * src.v_stride + x / 2;
        data.extend_from_slice(&src.v[start..start + ccw]);
    }
    Ok(Yuv420pBuffer::from_planes(data, cw, ch))
}

/// dB → 线性增益换算（volume 滤镜参数的 dB 口径）：
/// `gain = 10^(dB/20)`。
///
/// 对齐 FFmpeg `volume` 滤镜的 dB 语义（`volume=6dB` ⇒ ×10^(6/20)）：
/// 0 dB = ×1（恒等）、+20 dB = ×10、−20 dB = ×0.1、+6 dB ≈ ×1.9953。
/// [`FilterParams::Volume`](crate::FilterParams::Volume) 载荷即本口径的
/// 线性倍率；本函数只做参数换算（f32 `powf`），不作权威时戳/尺寸运算。
///
/// # Errors
/// `db` 非有限、或换算结果溢出 f32（如 db > 10^7 量级）→
/// [`FilterError::InvalidArguments`]。
pub fn volume_gain_from_db(db: f32) -> Result<f32, FilterError> {
    if !db.is_finite() {
        return Err(FilterError::InvalidArguments("db must be finite"));
    }
    let gain = 10f32.powf(db / 20.0);
    if !gain.is_finite() {
        return Err(FilterError::InvalidArguments(
            "db out of representable range",
        ));
    }
    Ok(gain)
}

/// fps 时基重采样（最近帧）：给定输入帧的呈现时刻（[`Timestamp`]，
/// 单调不减、时基可逐帧不同——时戳自描述）与目标帧率
/// `fps_num/fps_den`（每秒帧数），返回输出槽依次应发射的输入帧下标
/// 序列。消费形态对齐 [`FilterParams::Fps`](crate::FilterParams::Fps)
/// 的 `num`/`den` 两字段。
///
/// 输出槽时刻 `t_k = pts[0] + k·fps_den/fps_num` 秒（k = 0..N，
/// `N = floor((pts_last − pts[0])·fps_num/fps_den) + 1`），每槽取
/// `|pts_i − t_k|` 最小的输入帧，**并列取下标更小（更早）的帧**。
/// 与 FFmpeg `fps` 滤镜的 `round=near` 口径对齐（对拍见 oracle 组）。
///
/// ## 精确性口径
///
/// 全程整数/有理数运算，无 f64、无重标度舍入：跨时基的**序比较只走
/// [`Timestamp::compare`]**（i128 精确交叉相乘）；槽时刻 = `t0` 与帧网格
/// 时戳（`k @ fps_den/fps_num`）的 [`Timestamp::checked_add`]——公共
/// 精确基下整系数换算，零舍入；近邻判据 `|t − pts_l| ≤ |pts_r − t|`
/// 等价变形为 `2t ≤ pts_l + pts_r`（两次精确加法 + 一次 `compare`），
/// 不做差值交叉相乘。
///
/// # Errors
/// - `fps_num`/`fps_den` 为 0 → [`FilterError::InvalidArguments`]；
/// - `pts` 非单调不减（经 `Timestamp::compare` 判定）→
///   [`FilterError::InvalidArguments`]；
/// - 时戳量级极端、精确加减/槽计数落不回 i64/usize →
///   [`FilterError::Time`]（下层 [`ye_time::TimeError`] 原样透出）。
pub fn fps_nearest_frame_indices(
    pts: &[Timestamp],
    fps_num: u32,
    fps_den: u32,
) -> Result<Vec<usize>, FilterError> {
    if fps_num == 0 || fps_den == 0 {
        return Err(FilterError::InvalidArguments(
            "fps numerator/denominator must be nonzero",
        ));
    }
    // 单调不减校验：跨时基精确比较只走 Timestamp::compare
    for w in pts.windows(2) {
        if w[0].compare(w[1]) == Ordering::Greater {
            return Err(FilterError::InvalidArguments("pts must be non-decreasing"));
        }
    }
    if pts.is_empty() {
        return Ok(Vec::new());
    }
    let t0 = pts[0];
    let last = pts[pts.len() - 1];
    // 槽数 N = floor(span·fps_num/fps_den) + 1：span 经精确减法
    // （公共精确基，零舍入），比值在 i128 里整除
    let span = last.checked_sub(t0)?;
    let span_num: i128 = span.ticks() as i128 * i128::from(span.time_base().num());
    let span_den: i128 = i128::from(span.time_base().den());
    let slots_i128 = span_num * i128::from(fps_num) / (span_den * i128::from(fps_den)) + 1;
    if slots_i128 < 1 || slots_i128 > usize::MAX as i128 {
        return Err(FilterError::InvalidArguments(
            "fps slot count exceeds addressable range",
        ));
    }
    let slots = slots_i128 as usize;
    // 帧网格时戳 k @ (fps_den/fps_num)：from_frame_index 取帧率倒数作时基，
    // 精确无舍入；rate 经 try_new（数据驱动路径，den 非零已校验，
    // num/den 须落 i32 表示域）
    let rate = Rational::try_new(
        i32::try_from(fps_num).map_err(|_| FilterError::InvalidArguments(FPS_MUST_FIT_I32))?,
        i32::try_from(fps_den).map_err(|_| FilterError::InvalidArguments(FPS_MUST_FIT_I32))?,
    )?;
    let mut out = Vec::with_capacity(slots);
    for k in 0..slots {
        let frame = Timestamp::from_frame_index(k as i64, rate)?;
        let target = t0.checked_add(frame)?;
        out.push(nearest_index(pts, target)?);
    }
    Ok(out)
}

/// 槽时刻 `target` 的最近输入帧下标（并列取更早——等值/等距游程取
/// 首个）。前提：`pts` 非空、非降，且 `pts[0] ≤ target ≤ 末帧`。
fn nearest_index(pts: &[Timestamp], target: Timestamp) -> Result<usize, FilterError> {
    // 首个 pts_i ≥ target 的下标（谓词随非降序单调；序比较跨时基精确）
    let r = pts.partition_point(|p| p.compare(target) == Ordering::Less);
    if r < pts.len() && pts[r].compare(target) == Ordering::Equal {
        // 恰有帧等于槽时刻：并列取更早 ⇒ 取等值游程首个
        return Ok(first_of_equal_run(pts, r));
    }
    // 构造上 1 ≤ r ≤ len−1（槽时刻 ∈ [pts[0], 末帧]），左右候选 r−1 / r；
    // 保守钳位防御极端量级下的构造失真
    let l = r.saturating_sub(1).min(pts.len() - 1);
    let right = r.min(pts.len() - 1);
    if right == l {
        return Ok(first_of_equal_run(pts, l));
    }
    // pts_l < t < pts_r 时：|t − pts_l| ≤ |pts_r − t| ⟺ 2t ≤ pts_l + pts_r
    // （t 在中点左侧或恰在中点 ⇒ 取左，并列取更早；精确加法 + 比较序）
    let mid = pts[l].checked_add(pts[right])?;
    let target2 = target.checked_add(target)?;
    if target2.compare(mid) != Ordering::Greater {
        Ok(first_of_equal_run(pts, l))
    } else {
        Ok(first_of_equal_run(pts, right))
    }
}

/// 从 `i` 向前走到与 `pts[i]` 等值的游程首个下标（并列取更早）。
fn first_of_equal_run(pts: &[Timestamp], i: usize) -> usize {
    let mut first = i;
    while first > 0 && pts[first - 1].compare(pts[first]) == Ordering::Equal {
        first -= 1;
    }
    first
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

    // ---- crop_yuv420p（M3：yuv420p 平面域裁剪）----

    /// 8×8 帧：luma = 行列线性值，色度按半分辨率网格独立编号。
    fn grid_frame(w: usize, h: usize) -> Vec<u8> {
        let mut d = Vec::with_capacity(w * h * 3 / 2);
        for r in 0..h {
            for c in 0..w {
                d.push((r * w + c) as u8);
            }
        }
        for r in 0..h / 2 {
            for c in 0..w / 2 {
                d.push((100 + r * (w / 2) + c) as u8);
            }
        }
        for r in 0..h / 2 {
            for c in 0..w / 2 {
                d.push((200 + r * (w / 2) + c) as u8);
            }
        }
        d
    }

    #[test]
    fn crop_yuv420p_known_values() {
        // 8×8 → (2,4) 起 4×2：luma 取原行 4..6、列 2..6；
        // 色度原点 (1,2)、尺寸 2×1
        let frame = grid_frame(8, 8);
        let out = crop_yuv420p(&tight_view(&frame, 8, 8), 8, 8, 2, 4, 4, 2).unwrap();
        assert_eq!((out.width(), out.height()), (4, 2));
        assert_eq!(out.y(), &[34, 35, 36, 37, 42, 43, 44, 45]);
        assert_eq!(out.u(), &[109, 110]); // 100 + 2·4 + 1（色度行 y/2=2）,+1
        assert_eq!(out.v(), &[209, 210]);
    }

    #[test]
    fn crop_yuv420p_identity_full_frame() {
        let frame = grid_frame(8, 8);
        let out = crop_yuv420p(&tight_view(&frame, 8, 8), 8, 8, 0, 0, 8, 8).unwrap();
        assert_eq!(out.as_bytes(), &frame[..]);
    }

    #[test]
    fn crop_yuv420p_respects_stride_padding() {
        // 跨距 = 宽 + 4 的填充视图：裁剪须按跨距寻行，不得把填充字节
        // 混进结果
        let (w, h) = (8usize, 6usize);
        let (cw, ch) = (4usize, 2usize);
        let ys = w + 4;
        let cs = w / 2 + 4;
        let mut y = vec![0xAAu8; (h - 1) * ys + w];
        let mut u = vec![0xBBu8; (h / 2 - 1) * cs + w / 2];
        let mut v = vec![0xCCu8; (h / 2 - 1) * cs + w / 2];
        for r in 0..h {
            for c in 0..w {
                y[r * ys + c] = (r * w + c) as u8;
            }
        }
        for r in 0..h / 2 {
            for c in 0..w / 2 {
                u[r * cs + c] = (100 + r * (w / 2) + c) as u8;
                v[r * cs + c] = (200 + r * (w / 2) + c) as u8;
            }
        }
        let view = Yuv420pView {
            y: &y,
            y_stride: ys,
            u: &u,
            u_stride: cs,
            v: &v,
            v_stride: cs,
        };
        let out = crop_yuv420p(&view, w, h, 2, 2, cw, ch).unwrap();
        // luma：原行 2..4、列 2..6
        assert_eq!(out.y(), &[18, 19, 20, 21, 26, 27, 28, 29]);
        // 色度原点 (1,1)、尺寸 2×1：100 + 4 + 1 = 105 起
        assert_eq!(out.u(), &[105, 106]);
        assert_eq!(out.v(), &[205, 206]);
        // 填充字节不入结果
        assert!(!out.as_bytes().contains(&0xAA));
        assert!(!out.as_bytes().contains(&0xBB));
        assert!(!out.as_bytes().contains(&0xCC));
    }

    #[test]
    fn crop_yuv420p_even_alignment_required() {
        let frame = grid_frame(8, 8);
        let view = tight_view(&frame, 8, 8);
        // 奇 x / 奇 y / 奇宽 / 奇高各拒绝（割裂 2×2 色度块）
        assert!(crop_yuv420p(&view, 8, 8, 1, 0, 4, 4).is_err());
        assert!(crop_yuv420p(&view, 8, 8, 0, 1, 4, 4).is_err());
        assert!(crop_yuv420p(&view, 8, 8, 0, 0, 3, 4).is_err());
        assert!(crop_yuv420p(&view, 8, 8, 0, 0, 4, 3).is_err());
        // 全偶合法
        assert!(crop_yuv420p(&view, 8, 8, 2, 2, 4, 4).is_ok());
    }

    #[test]
    fn crop_yuv420p_bounds_errors() {
        let frame = grid_frame(8, 8);
        let view = tight_view(&frame, 8, 8);
        assert!(crop_yuv420p(&view, 8, 8, 6, 0, 4, 4).is_err()); // x+cw > w
        assert!(crop_yuv420p(&view, 8, 8, 0, 6, 4, 4).is_err()); // y+ch > h
        assert!(crop_yuv420p(&view, 8, 8, 0, 0, 0, 2).is_err()); // 零宽
        assert!(crop_yuv420p(&view, 8, 8, 0, 0, 4, 0).is_err()); // 零高
    }

    // ---- fps（Timestamp 入参形态，2026-10-26 M3 改造）----

    /// 微秒时基构造便捷（Rational::new 为字面量/已校验路径）。
    fn us(t: i64) -> Timestamp {
        Timestamp::new(t, Rational::new(1, 1_000_000))
    }

    #[test]
    fn fps_24_to_30_nearest() {
        // 24fps 源 [0, 41666, 83333, 125000]µs → 30fps（步距 33333µs）：
        // 槽 0/33333/66666/99999 → 帧 0/1/2/2（手算见函数文档）
        let pts = [us(0), us(41666), us(83333), us(125000)];
        let idx = fps_nearest_frame_indices(&pts, 30, 1).unwrap();
        assert_eq!(idx, vec![0, 1, 2, 2]);
    }

    #[test]
    fn fps_upsample_repeats_frames() {
        // 6fps → 12fps：步距 83333µs，槽 7 个（末槽恰为末帧时刻）
        let pts: Vec<Timestamp> = (0..4).map(|i| us(i * 166_667)).collect();
        let idx = fps_nearest_frame_indices(&pts, 12, 1).unwrap();
        assert_eq!(idx, vec![0, 0, 1, 1, 2, 2, 3]);
    }

    #[test]
    fn fps_tie_takes_earlier_frame() {
        // pts [0, 62500]µs、32fps（步距 31250µs）：槽 1 恰在 31250 = 两帧
        // 中点 → 距离并列，取更早的帧 0（精确加法判据 mid == 2t）
        let pts = [us(0), us(62500)];
        let idx = fps_nearest_frame_indices(&pts, 32, 1).unwrap();
        assert_eq!(idx, vec![0, 0, 1]);
    }

    #[test]
    fn fps_rejects_bad_input() {
        let ok = [us(0), us(100)];
        assert!(fps_nearest_frame_indices(&ok, 0, 1).is_err());
        assert!(fps_nearest_frame_indices(&ok, 30, 0).is_err());
        // 非单调（比较经 Timestamp::compare）
        let dec = [us(100), us(50)];
        assert!(fps_nearest_frame_indices(&dec, 30, 1).is_err());
        assert!(fps_nearest_frame_indices(&[], 30, 1).unwrap().is_empty());
    }

    #[test]
    fn fps_single_frame_single_slot() {
        let pts = [us(424_242)];
        let idx = fps_nearest_frame_indices(&pts, 30, 1).unwrap();
        assert_eq!(idx, vec![0]);
    }

    #[test]
    fn fps_duplicate_pts_takes_first_of_run() {
        // 重复时戳：胜者值有重复时取游程首个（更早）下标。
        // pts [0, 8000, 8000, 21000]µs、100fps（步距 10000µs，3 槽）：
        // 槽 1（10000µs）最近 8000 游程 ⇒ 取帧 1（非帧 2）；
        // 槽 2（20000µs）最近 21000 ⇒ 帧 3
        let pts = [us(0), us(8000), us(8000), us(21000)];
        let idx = fps_nearest_frame_indices(&pts, 100, 1).unwrap();
        assert_eq!(idx, vec![0, 1, 3]);
    }

    #[test]
    fn fps_cross_time_base_stream_vs_grid() {
        // 输入流时基 1/90000（MPEG 常用），目标 25fps：网格时基 1/25，
        // 槽时刻落在与输入不同的时基上（跨时基比较全走 compare）
        let tb = Rational::new(1, 90_000);
        let pts = [
            Timestamp::new(0, tb),
            Timestamp::new(2_700, tb),  // 30 ms
            Timestamp::new(9_000, tb),  // 100 ms
            Timestamp::new(11_700, tb), // 130 ms
        ];
        // 25fps 步距 40ms：槽 0/40/80/120ms → 最近帧 0/30/100/130ms
        let idx = fps_nearest_frame_indices(&pts, 25, 1).unwrap();
        assert_eq!(idx, vec![0, 1, 2, 3]);
        // 槽数：span 130ms·25 = 3.25 ⇒ 4 槽
    }

    #[test]
    fn fps_mixed_time_bases_per_frame() {
        // 逐帧不同时基（时戳自描述）：0 / 41666µs / 1@1/12(=83333µs)
        let pts = [us(0), us(41666), Timestamp::new(1, Rational::new(1, 12))];
        // 30fps 步距 33333µs：span = 83333µs ⇒ floor(2.49999)+1 = 3 槽；
        // 槽 0/33333/66667 → 帧 0/1/2
        let idx = fps_nearest_frame_indices(&pts, 30, 1).unwrap();
        assert_eq!(idx, vec![0, 1, 2]);
    }

    #[test]
    fn fps_mixed_time_base_non_decreasing_rejected() {
        // 跨时基降序：1s @1/1e6 之后跟 0.04s @1/25 ⇒ compare 判 Greater ⇒ 拒
        let pts = [us(1_000_000), Timestamp::new(1, Rational::new(1, 25))];
        assert_eq!(
            fps_nearest_frame_indices(&pts, 30, 1),
            Err(FilterError::InvalidArguments("pts must be non-decreasing"))
        );
    }

    #[test]
    fn fps_params_feed_kernel_shape() {
        // FilterParams::Fps 的 num/den 两字段直接对位 fps 核入参
        let params = crate::FilterParams::Fps { num: 30, den: 1 };
        let crate::FilterParams::Fps { num, den } = params else {
            panic!("期望 Fps 参数");
        };
        let pts = [us(0), us(41666), us(83333), us(125000)];
        let idx = fps_nearest_frame_indices(&pts, num, den).unwrap();
        assert_eq!(idx, vec![0, 1, 2, 2]);
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

    // ---- volume_gain_from_db（dB → 线性增益口径）----

    #[test]
    fn volume_gain_from_db_conversion() {
        // 0 dB = ×1（恒等，精确）
        assert_eq!(volume_gain_from_db(0.0).unwrap(), 1.0);
        // ±20 dB = ×10 / ×0.1
        assert!((volume_gain_from_db(20.0).unwrap() - 10.0).abs() < 1e-5);
        assert!((volume_gain_from_db(-20.0).unwrap() - 0.1).abs() < 1e-7);
        // +6 dB ≈ ×1.9952623（10^(6/20)）
        assert!(
            (volume_gain_from_db(6.0).unwrap() - 1.995_262_3).abs() < 1e-5,
            "g6 = {}",
            volume_gain_from_db(6.0).unwrap()
        );
        // 正负 dB 互逆
        let gp = volume_gain_from_db(6.0).unwrap();
        let gm = volume_gain_from_db(-6.0).unwrap();
        assert!((gp * gm - 1.0).abs() < 1e-5);
    }

    #[test]
    fn volume_gain_from_db_rejects_bad_input() {
        assert!(volume_gain_from_db(f32::NAN).is_err());
        assert!(volume_gain_from_db(f32::INFINITY).is_err());
        assert!(volume_gain_from_db(f32::NEG_INFINITY).is_err());
        // 换算溢出 f32（10^(1e9/20) 不可表示）
        assert!(volume_gain_from_db(1e9).is_err());
    }

    // ---- 滤镜参数 → 执行核 消费形态（M3：crop / volume）----

    #[test]
    fn crop_params_feed_kernel_shape() {
        // 图节点的 FilterParams::Crop 四字段直接对位 crop_yuv420p 入参
        use crate::descriptor::{FilterDescriptor, FilterParams, PadDescriptor};
        let desc = FilterDescriptor {
            name: "crop".to_string(),
            inputs: vec![PadDescriptor {
                name: "default".to_string(),
                media: ye_codec::MediaType::Video,
            }],
            outputs: vec![PadDescriptor {
                name: "default".to_string(),
                media: ye_codec::MediaType::Video,
            }],
            params: FilterParams::Crop {
                x: 2,
                y: 2,
                width: 4,
                height: 4,
            },
        };
        let FilterParams::Crop {
            x,
            y,
            width,
            height,
        } = desc.params
        else {
            panic!("期望 Crop 参数");
        };
        let frame = grid_frame(8, 8);
        let out = crop_yuv420p(
            &tight_view(&frame, 8, 8),
            8,
            8,
            x as usize,
            y as usize,
            width as usize,
            height as usize,
        )
        .unwrap();
        assert_eq!((out.width(), out.height()), (4, 4));
        assert_eq!(
            out.y(),
            &[
                18, 19, 20, 21, 26, 27, 28, 29, 34, 35, 36, 37, 42, 43, 44, 45
            ]
        );
    }

    #[test]
    fn volume_params_feed_kernel_shape() {
        // FilterParams::Volume { gain }（线性倍率）直接对位 volume_f32 的
        // gain 入参；dB 口径经 volume_gain_from_db 换算（−6.0206 dB ≈ ×0.5）
        let gain = volume_gain_from_db(-6.0206).unwrap();
        let params = crate::FilterParams::Volume { gain };
        let crate::FilterParams::Volume { gain } = params else {
            panic!("期望 Volume 参数");
        };
        let out = volume_f32(&[0.5, -0.25], gain).unwrap();
        assert!((out[0] - 0.25).abs() < 1e-3, "out0 = {}", out[0]);
        assert!((out[1] + 0.125).abs() < 1e-3, "out1 = {}", out[1]);
    }

    // ---- ffmpeg oracle 对拍（PATH 无 ffmpeg 时打印跳过信息返回）----

    use crate::video::convert::YuvRange;
    use crate::video::convert::yuv420p_to_rgba;
    use crate::video::testutil::{
        Lcg, ORACLE_CHAIN_MAX_ABS, ORACLE_SCALE_YUV_MAX_ABS, ffmpeg_available, max_abs_diff,
        run_ffmpeg_f32, run_ffmpeg_raw, tmp_path,
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

    /// crop（yuv420p 平面域纯字节搬运）vs `-vf crop=CW:CH:X:Y`
    /// （yuv420p → yuv420p）。容差写死 **0**（零运算零舍入；
    /// `ORACLE_CROP_MAX_ABS`，只准持平、不存在放宽余地）。
    #[test]
    fn oracle_crop_yuv420p_matches_ffmpeg() {
        if !ffmpeg_available() {
            eprintln!("skip: PATH 无 ffmpeg，oracle_crop_yuv420p_matches_ffmpeg");
            return;
        }
        const ORACLE_CROP_MAX_ABS: u32 = 0;
        let cases = [
            (48usize, 36usize, 0usize, 0usize, 32usize, 24usize),
            (48, 36, 8, 4, 32, 24),
            (48, 36, 16, 12, 16, 12),
            (32, 24, 2, 2, 28, 20),
        ];
        let mut lcg = Lcg(7);
        for (w, h, x, y, cw, ch) in cases {
            let frame: Vec<u8> = (0..w * h * 3 / 2).map(|_| lcg.next_byte()).collect();
            let vf = format!("crop={cw}:{ch}:{x}:{y}");
            let ref_out = run_ffmpeg_raw(&frame, w, h, "yuv420p", &["-vf", &vf], "yuv420p");
            let mine = crop_yuv420p(&tight_view2(&frame, w, h), w, h, x, y, cw, ch).expect("裁剪");
            let d = max_abs_diff(mine.as_bytes(), &ref_out);
            // 容差写死 0（ORACLE_CROP_MAX_ABS）：断言即逐字节相等
            assert!(
                d == ORACLE_CROP_MAX_ABS,
                "crop {w}x{h}+({x},{y})->{cw}x{ch} 超容差 0: max_abs={d}"
            );
        }
        eprintln!("oracle crop yuv420p: max_abs=0 (容差 0)");
    }

    /// volume（f32 增益乘法）vs `-af volume=G:precision=float`（f32le）。
    /// 容差写死 **1 ULP**（f32 的 LSB；`ORACLE_VOLUME_MAX_ULP`，只准收紧）。
    /// 向量域 `|x| ≤ 0.5` 且增益 ≤ 2 ⇒ 乘积不出 ±1，避开「本核 [−1,1]
    /// 削波 / ffmpeg float 路径不削波」的口径差（削波语义由单测覆盖）。
    #[test]
    fn oracle_volume_f32_matches_ffmpeg() {
        if !ffmpeg_available() {
            eprintln!("skip: PATH 无 ffmpeg，oracle_volume_f32_matches_ffmpeg");
            return;
        }
        const ORACLE_VOLUME_MAX_ULP: u32 = 1;
        let n = 4096usize;
        let mut lcg = Lcg(11);
        let samples: Vec<f32> = (0..n)
            .map(|_| lcg.next_byte() as f32 / 255.0 - 0.5)
            .collect();
        for gain in [0.5f32, 1.3, 2.0] {
            let af = format!("volume={gain}:precision=float");
            let ref_out = run_ffmpeg_f32(&samples, &[&af]);
            let mine = volume_f32(&samples, gain).unwrap();
            assert_eq!(ref_out.len(), mine.len(), "样本数不一致 gain={gain}");
            let mut worst = 0u32;
            for (a, b) in mine.iter().zip(ref_out.iter()) {
                worst = worst.max(f32_ulp_diff(*a, *b));
            }
            assert!(
                worst <= ORACLE_VOLUME_MAX_ULP,
                "volume gain={gain} 超容差 {ORACLE_VOLUME_MAX_ULP} ULP: worst={worst}"
            );
            eprintln!("oracle volume gain={gain}: worst={worst} ULP (容差 1)");
        }
    }

    /// f32 的 ULP 距离（符号幅值位序映射后取差；±0.0 视为相等）。
    fn f32_ulp_diff(a: f32, b: f32) -> u32 {
        if a == b {
            return 0;
        }
        let ordered = |x: f32| -> u32 {
            let bits = x.to_bits();
            if bits & 0x8000_0000 != 0 {
                !bits
            } else {
                bits | 0x8000_0000
            }
        };
        ordered(a).abs_diff(ordered(b))
    }

    /// fps oracle 专用 runner：rawvideo 逐帧纯色输入 + `-vf fps=…`，
    /// 帧率由 `-framerate` 显式给定（`run_ffmpeg_raw` 无此项）。
    fn run_ffmpeg_fps_oracle(frames: &[u8], w: usize, h: usize, in_rate: u32, vf: &str) -> Vec<u8> {
        use std::process::Command;
        let inp = tmp_path("in", "bin");
        let outp = tmp_path("out", "bin");
        std::fs::write(&inp, frames).expect("写 oracle 输入");
        let status = Command::new("ffmpeg")
            .args(["-y", "-v", "error"])
            .args(["-f", "rawvideo", "-pix_fmt", "yuv420p"])
            .arg("-s")
            .arg(format!("{w}x{h}"))
            .arg("-framerate")
            .arg(in_rate.to_string())
            .arg("-i")
            .arg(&inp)
            .args(["-vf", vf])
            .args(["-pix_fmt", "yuv420p", "-f", "rawvideo"])
            .arg(&outp)
            .status()
            .expect("启动 ffmpeg（PATH 需有 ffmpeg）");
        assert!(status.success(), "ffmpeg oracle 运行失败: -vf {vf}");
        let buf = std::fs::read(&outp).expect("读 oracle 输出");
        let _ = std::fs::remove_file(&inp);
        let _ = std::fs::remove_file(&outp);
        buf
    }

    /// fps（最近帧选择）vs `-vf fps=N:round=near`：逐帧纯色输入，
    /// 从输出帧内容反解所选输入下标，与 [`fps_nearest_frame_indices`]
    /// 的选择序**全等**比较（帧数与逐槽选择都全等）。容差写死 **0**
    /// （选择序列全等；`ORACLE_FPS_MAX_MISMATCH = 0`，只准持平）。
    ///
    /// ## 对拍几何的选取依据（结构性差异登记，2026-10-26 实测锁定）
    ///
    /// ffmpeg `fps` 滤镜（round=near）实现为**逐帧 pts 取整到输出槽 +
    /// 槽间保持前帧 + EOF 补发一帧**；本核是**逐输出槽取最近输入帧
    /// （并列取更早）**。多数槽上两方案一致，但保持边界并列处（如
    /// 24→30 的槽 2）与尾部槽数（EOF 补发）存在结构性分歧——与
    /// `scale.rs` 头注登记的「单段合并链」偏离同类，**登记非 bug**。
    /// 本对拍只在两方案**完全一致**的几何族上冻结（每格均已对
    /// ffmpeg N-127696-g8b3525e718-20261019 实测验证帧数与选择序）：
    /// 24→24（恒等）、24→12（降频、无保持边界歧义）、25→30（含一次
    /// 距离并列，两方案并列裁决一致取更早）、单帧 24→30。新增几何须
    /// 先实测两侧全等再入组。
    #[test]
    fn oracle_fps_round_near_matches_ffmpeg() {
        if !ffmpeg_available() {
            eprintln!("skip: PATH 无 ffmpeg，oracle_fps_round_near_matches_ffmpeg");
            return;
        }
        const ORACLE_FPS_MAX_MISMATCH: usize = 0;
        let (w, h) = (8usize, 8usize);
        let frame_bytes = w * h * 3 / 2;
        // 帧 k = 纯色 luma `16 + k·32`（互相可区分），色度 128
        fn solid_frames(n: usize, w: usize, h: usize) -> Vec<u8> {
            let frame_bytes = w * h * 3 / 2;
            let mut input = vec![128u8; n * frame_bytes];
            for k in 0..n {
                let luma = (16 + k * 32) as u8;
                input[k * frame_bytes..k * frame_bytes + w * h].fill(luma);
            }
            input
        }
        // (输入帧数, 输入帧率, 目标 num/den)；pts = k ticks @ 1/输入帧率
        // （rawvideo demuxer 同此打戳）
        let cases: [(usize, u32, u32, u32); 4] = [
            (6, 24, 24, 1),
            (6, 24, 12, 1),
            (6, 25, 30, 1),
            (1, 24, 30, 1),
        ];
        for (n, in_rate, num, den) in cases {
            let input = solid_frames(n, w, h);
            let rate_in = Rational::new(in_rate as i32, 1);
            let pts: Vec<Timestamp> = (0..n as i64)
                .map(|k| Timestamp::from_frame_index(k, rate_in).unwrap())
                .collect();
            let mine = fps_nearest_frame_indices(&pts, num, den).unwrap();
            let vf = format!("fps={num}:round=near");
            let ref_out = run_ffmpeg_fps_oracle(&input, w, h, in_rate, &vf);
            assert_eq!(
                ref_out.len() % frame_bytes,
                0,
                "fps oracle 输出字节非整帧倍数（{vf}）"
            );
            let out_frames = ref_out.len() / frame_bytes;
            let luma_to_index = |luma: u8| usize::from(luma.saturating_sub(16) / 32);
            let got: Vec<usize> = (0..out_frames)
                .map(|i| luma_to_index(ref_out[i * frame_bytes]))
                .collect();
            // 容差写死 0（ORACLE_FPS_MAX_MISMATCH）：帧数与逐槽选择全等
            assert_eq!(
                got.len(),
                mine.len(),
                "{in_rate}fps x{n} -> {num}/{den}：ffmpeg 输出 {out_frames} 帧 vs 我方 {} 槽，选择序我方 = {mine:?}",
                mine.len()
            );
            assert_eq!(
                got, mine,
                "{in_rate}fps x{n} -> {num}/{den} 选择序失配（容差 {ORACLE_FPS_MAX_MISMATCH}）"
            );
            eprintln!("oracle fps {in_rate}->{num}/{den} x{n}: 选择序 {mine:?} 全等 (容差 0)");
        }
    }
}
