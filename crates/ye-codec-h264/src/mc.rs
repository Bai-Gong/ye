//! 运动补偿核（冻结接口，施工图② split：平面核实现者持有本文件的
//! 优化权；主链只依赖本模块的函数签名与语义契约）。
//!
//! # 冻结接口契约（②b 起，勿改动签名/语义，只可换实现体）
//!
//! - `src` 为参考帧平面切片（无 padding，行主序，`src_w × src_h` 像素）；
//!   越界读一律按「最近边缘复制」取值——与 H.264 边缘扩展语义及
//!   FFmpeg 的 emulated_edge_mc 等值（对拍锁定）。
//! - luma：`(qx, qy)` 为 **QPEL 绝对坐标**（像素坐标 ×4 + MV）。
//!   - 半像素：6 抽头 `(E − 5F + 20G + 20H − 5I + J + 16) >> 5` 裁剪；
//!   - 对角半像素（fx=fy=2）：两遍 6 抽头，**中间量不裁剪**
//!     （FFmpeg `hv_lowpass` 同构，i16 tmp）；
//!   - 四分之一像素：邻近整/半样本 `(a + b + 1) >> 1` 双线性
//!     （FFmpeg `_l2` rnd_avg 语义）。16 档取路表（x264
//!     `common/tables.c` 的 `x264_hpel_ref0/1[16]` 逐字，平面记号
//!     F=整数、b=水平半、h=垂直半、j=对角半；+1 表示该路偏移一个
//!     整数样本，avg 均为 `(a+b+1)>>1`）：
//!
//!     | (fx,fy) | 取路               | (fx,fy) | 取路               |
//!     |---------|--------------------|---------|--------------------|
//!     | (0,0)   | F                  | (0,1)   | avg(F, h)          |
//!     | (1,0)   | avg(F, b)          | (1,1)   | avg(b, h)          |
//!     | (2,0)   | b                  | (2,1)   | **avg(b, j)**      |
//!     | (3,0)   | avg(F→, b)         | (3,1)   | avg(b, h→)         |
//!     | (0,2)   | h                  | (1,2)   | **avg(h, j)**      |
//!     | (1,3)   | avg(b↓, h)         | (2,2)   | j                  |
//!     | (2,3)   | **avg(b↓, j)**     | (3,2)   | **avg(h→, j)**     |
//!     | (0,3)   | avg(F↓, h)         | (3,3)   | avg(b↓, h→)        |
//!
//!     （→ = 该路右移一列，↓ = 该路下移一行。）
//!
//!     注意 (2,1)/(2,3) 走 **b 路**（fy==3 时 b 下移一行）、(1,2)/(3,2)
//!     走 **h 路**（fx==3 时 h 右移一列）——加粗四档是 quarter 与
//!     对角 quarter 最易接反的位置，已由冲激响应黄金值单测锁定。
//! - chroma（4:2:0）：`(qx, qy)` 为 **1/8 样本绝对坐标**（数值上 =
//!   luma QPEL 坐标，单位换算：1 luma qpel = 1/8 chroma 样本）；
//!   双线性 `(A·(8−iFx)·(8−iFy) + … + 32) >> 6`（FFmpeg
//!   `h264chroma_template.c` op_put 逐字）。
//! - `dst` 长度 ≥ `dst_stride·(h−1) + w`；写 `w × h`。
//! - avg 变体为 ②c/B 帧预留（`put` 结果与已有 `dst` 取 rnd 平均），
//!   当前实现为占位。
//!
//! 当前实现体为**正确性优先的参考版**（标量逐像素）；内核实现者可
//! 换成 SIMD/分块版，对拍（tests/conformance）必须保持逐位一致。

/// 最近边缘复制取值。
#[inline]
fn fetch(src: &[u8], stride: usize, w: usize, h: usize, x: i64, y: i64) -> i32 {
    let x = x.clamp(0, (w as i64) - 1) as usize;
    let y = y.clamp(0, (h as i64) - 1) as usize;
    i32::from(src[y * stride + x])
}

/// luma 6 抽头水平滤波（含 +16>>5 与裁剪；FFmpeg `_h_lowpass` 逐字）。
#[inline]
fn tap6(a: i32, b: i32, c: i32, d: i32, e: i32, f: i32) -> i32 {
    ((a - 5 * b + 20 * c + 20 * d - 5 * e + f + 16) >> 5).clamp(0, 255)
}

/// luma 半像素水平值 h(x+0.5, y)（整像素坐标 x，行 y）。
#[inline]
fn hpel_h(src: &[u8], stride: usize, w: usize, h: usize, x: i64, y: i64) -> i32 {
    tap6(
        fetch(src, stride, w, h, x - 2, y),
        fetch(src, stride, w, h, x - 1, y),
        fetch(src, stride, w, h, x, y),
        fetch(src, stride, w, h, x + 1, y),
        fetch(src, stride, w, h, x + 2, y),
        fetch(src, stride, w, h, x + 3, y),
    )
}

/// luma 半像素垂直值 v(x, y+0.5)。
#[inline]
fn hpel_v(src: &[u8], stride: usize, w: usize, h: usize, x: i64, y: i64) -> i32 {
    tap6(
        fetch(src, stride, w, h, x, y - 2),
        fetch(src, stride, w, h, x, y - 1),
        fetch(src, stride, w, h, x, y),
        fetch(src, stride, w, h, x, y + 1),
        fetch(src, stride, w, h, x, y + 2),
        fetch(src, stride, w, h, x, y + 3),
    )
}

/// luma 对角半像素 j(x+0.5, y+0.5)：垂直 6 抽头作用于**未裁剪的**
/// 水平中间量（FFmpeg `hv_lowpass` 两遍结构逐字）。
fn hpel_j(src: &[u8], stride: usize, w: usize, h: usize, x: i64, y: i64) -> i32 {
    // 水平中间量（不裁剪）：行 y−2 .. y+3
    let mut tmp = [0i32; 6];
    for (i, t) in tmp.iter_mut().enumerate() {
        let yy = y - 2 + i as i64;
        let a = fetch(src, stride, w, h, x - 2, yy);
        let b = fetch(src, stride, w, h, x - 1, yy);
        let c = fetch(src, stride, w, h, x, yy);
        let d = fetch(src, stride, w, h, x + 1, yy);
        let e = fetch(src, stride, w, h, x + 2, yy);
        let f = fetch(src, stride, w, h, x + 3, yy);
        *t = a - 5 * b + 20 * c + 20 * d - 5 * e + f;
    }
    // FFmpeg op2_put：两遍 6 抽头的总舍入合并为 +512 >> 10
    ((tmp[0] - 5 * tmp[1] + 20 * tmp[2] + 20 * tmp[3] - 5 * tmp[4] + tmp[5] + 512) >> 10)
        .clamp(0, 255)
}

/// rnd 平均（FFmpeg rnd_avg：(a|b) − (((a^b)&~1)>>1) ≡ (a+b+1)>>1）。
#[inline]
fn rnd_avg(a: i32, b: i32) -> i32 {
    (a + b + 1) >> 1
}

/// 整+半像素 luma 运动补偿（put；冻结接口，见模块文档）。
#[allow(clippy::too_many_arguments)] // 冻结接口签名（②b 契约），不可聚类
pub(crate) fn mc_luma_put(
    dst: &mut [u8],
    dst_stride: usize,
    src: &[u8],
    src_stride: usize,
    src_w: usize,
    src_h: usize,
    qx: i64,
    qy: i64,
    w: usize,
    h: usize,
) {
    let fx = (qx & 3) as i32;
    let fy = (qy & 3) as i32;
    let x0 = qx >> 2; // 整像素锚点（可为负，fetch 内钳制）
    let y0 = qy >> 2;
    for dy in 0..h as i64 {
        let y = y0 + dy;
        for dx in 0..w as i64 {
            let x = x0 + dx;
            let v: i32 = match (fx, fy) {
                (0, 0) => fetch(src, src_stride, src_w, src_h, x, y),
                (2, 0) => hpel_h(src, src_stride, src_w, src_h, x, y),
                (0, 2) => hpel_v(src, src_stride, src_w, src_h, x, y),
                (2, 2) => hpel_j(src, src_stride, src_w, src_h, x, y),
                // x 四分之一（fx ∈ {1,3}），y 整/半
                (1, 0) | (3, 0) => rnd_avg(
                    fetch(src, src_stride, src_w, src_h, x + i64::from(fx == 3), y),
                    hpel_h(src, src_stride, src_w, src_h, x, y),
                ),
                // y 四分之一（fy ∈ {1,3}），x 整/半
                (0, 1) | (0, 3) => rnd_avg(
                    fetch(src, src_stride, src_w, src_h, x, y + i64::from(fy == 3)),
                    hpel_v(src, src_stride, src_w, src_h, x, y),
                ),
                // (x, ¼/¾)：b(x, y + fy==3) 与 j 平均——x264
                // hpel_ref0/1 表 idx6/idx14（H 路在 fy==3 时下移一行）。
                (2, 1) | (2, 3) => rnd_avg(
                    hpel_h(src, src_stride, src_w, src_h, x, y + i64::from(fy == 3)),
                    hpel_j(src, src_stride, src_w, src_h, x, y),
                ),
                // (¼/¾, y)：h(x + fx==3, y) 与 j 平均——idx9/idx11
                // （V 路在 fx==3 时右移一列）。原占位把 b/h 两路接反且
                // 缺 (3,2) 的 x+1 偏移，逐字表核对后修正（见单测
                // quarter_pel_mixed_axis_golden 的判别用例）。
                (1, 2) | (3, 2) => rnd_avg(
                    hpel_v(src, src_stride, src_w, src_h, x + i64::from(fx == 3), y),
                    hpel_j(src, src_stride, src_w, src_h, x, y),
                ),
                // 双四分之一：两邻近半样本平均（FFmpeg mc11/13/31/33 逐字）
                (1, 1) | (1, 3) | (3, 1) | (3, 3) => rnd_avg(
                    hpel_h(src, src_stride, src_w, src_h, x, y + i64::from(fy == 3)),
                    hpel_v(src, src_stride, src_w, src_h, x + i64::from(fx == 3), y),
                ),
                _ => unreachable!("qpel fractions in 0..=3"),
            };
            dst[(dy as usize) * dst_stride + dx as usize] = v.clamp(0, 255) as u8;
        }
    }
}

/// chroma（4:2:0）双线性运动补偿（put；冻结接口）。
/// `(qx, qy)` 为 1/8 样本绝对坐标（数值 = luma QPEL 坐标）。
#[allow(clippy::too_many_arguments)] // 冻结接口签名（②b 契约），不可聚类
pub(crate) fn mc_chroma_put(
    dst: &mut [u8],
    dst_stride: usize,
    src: &[u8],
    src_stride: usize,
    src_w: usize,
    src_h: usize,
    qx: i64,
    qy: i64,
    w: usize,
    h: usize,
) {
    let fx = (qx & 7) as i32;
    let fy = (qy & 7) as i32;
    let x0 = qx >> 3;
    let y0 = qy >> 3;
    let a = (8 - fx) * (8 - fy);
    let b = fx * (8 - fy);
    let c = (8 - fx) * fy;
    let d = fx * fy;
    for dy in 0..h as i64 {
        let y = y0 + dy;
        for dx in 0..w as i64 {
            let x = x0 + dx;
            let p00 = fetch(src, src_stride, src_w, src_h, x, y);
            let v = if d != 0 {
                let p10 = fetch(src, src_stride, src_w, src_h, x + 1, y);
                let p01 = fetch(src, src_stride, src_w, src_h, x, y + 1);
                let p11 = fetch(src, src_stride, src_w, src_h, x + 1, y + 1);
                a * p00 + b * p10 + c * p01 + d * p11
            } else if b + c != 0 {
                let e = b + c;
                let p1 = if c != 0 {
                    fetch(src, src_stride, src_w, src_h, x, y + 1)
                } else {
                    fetch(src, src_stride, src_w, src_h, x + 1, y)
                };
                a * p00 + e * p1
            } else {
                a * p00
            };
            dst[(dy as usize) * dst_stride + dx as usize] = ((v + 32) >> 6).clamp(0, 255) as u8;
        }
    }
}

/// avg 变体（②c/B 预留）：put 结果与 dst 现值 rnd 平均。
#[allow(dead_code)] // B 片接线（②c）前无消费方；冻结接口预留
#[allow(clippy::too_many_arguments)] // 冻结接口签名（②b 契约），不可聚类
pub(crate) fn mc_luma_avg(
    dst: &mut [u8],
    dst_stride: usize,
    src: &[u8],
    src_stride: usize,
    src_w: usize,
    src_h: usize,
    qx: i64,
    qy: i64,
    w: usize,
    h: usize,
) {
    let mut tmp = vec![0u8; w * h];
    mc_luma_put(&mut tmp, w, src, src_stride, src_w, src_h, qx, qy, w, h);
    for dy in 0..h {
        for dx in 0..w {
            let i = dy * dst_stride + dx;
            let a = i32::from(dst[i]);
            let b = i32::from(tmp[dy * w + dx]);
            dst[i] = rnd_avg(a, b).clamp(0, 255) as u8;
        }
    }
}

/// chroma avg（②c/B 预留）。
#[allow(dead_code)] // B 片接线（②c）前无消费方；冻结接口预留
#[allow(clippy::too_many_arguments)] // 冻结接口签名（②b 契约），不可聚类
pub(crate) fn mc_chroma_avg(
    dst: &mut [u8],
    dst_stride: usize,
    src: &[u8],
    src_stride: usize,
    src_w: usize,
    src_h: usize,
    qx: i64,
    qy: i64,
    w: usize,
    h: usize,
) {
    let mut tmp = vec![0u8; w * h];
    mc_chroma_put(&mut tmp, w, src, src_stride, src_w, src_h, qx, qy, w, h);
    for dy in 0..h {
        for dx in 0..w {
            let i = dy * dst_stride + dx;
            let a = i32::from(dst[i]);
            let b = i32::from(tmp[dy * w + dx]);
            dst[i] = rnd_avg(a, b).clamp(0, 255) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // 手算黄金值底座：底 100、单点 +52 的冲激平面。
    // 期望值独立推导自 tap=[1,−5,20,20,−5,1]、常数和 32、+16>>5 /
    // +512>>10、avg=(a+b+1)>>1，并经独立 Python 仿真（同钳位语义）
    // 机器复核；δ=52 时最大中间量 20·52=1040，全程无裁剪，值唯一。
    // 冲激放在平面内部，避免边缘钳位污染滤波窗口。
    // ------------------------------------------------------------------

    /// 12×12、(5,5)=152、其余 100（对称位置：b/h/j 同式互验）。
    fn impulse_at_5_5() -> Vec<u8> {
        let mut p = vec![100u8; 144];
        p[5 * 12 + 5] = 152;
        p
    }

    /// 10×10、(3,1)=152、其余 100（非对称位置：b/h 路值不同，判别用）。
    fn impulse_at_3_1() -> Vec<u8> {
        let mut p = vec![100u8; 100];
        p[10 + 3] = 152; // (3,1)：行 1 列 3
        p
    }

    fn luma(src: &[u8], sw: usize, sh: usize, qx: i64, qy: i64, w: usize, h: usize) -> Vec<u8> {
        let mut dst = vec![0u8; w * h];
        mc_luma_put(&mut dst, w, src, sw, sw, sh, qx, qy, w, h);
        dst
    }

    #[test]
    fn tap6_coefficient_golden() {
        // 手算：10 −100 +600 +800 −250 +60 = 1120 → (1120+16)>>5 = 35
        assert_eq!(tap6(10, 20, 30, 40, 50, 60), 35);
        // 冲激：第 3 位（权重 20）→ (3200+1040+16)>>5 = 133
        assert_eq!(tap6(100, 100, 152, 100, 100, 100), 133);
        // 常数：32c + 16 >> 5 = c（6 抽头对常数精确）
        assert_eq!(tap6(77, 77, 77, 77, 77, 77), 77);
        // 第 5 位（权重 −5）放低值 48：−5·48 比 −5·100 多 +260
        // → (3200+260+16)>>5 = 108
        assert_eq!(tap6(100, 100, 100, 100, 48, 100), 108);
    }

    #[test]
    fn halfpel_horizontal_impulse_golden() {
        // (5,5)=152 的 12×12 平面，行 5 上扫 x=2..7：冲激列落入权重
        // 1,−5,20,20,−5,1 → (3252+16)>>5=102、(2940+16)>>5=92、
        // (4240+16)>>5=133、133、92、102
        let src = impulse_at_5_5();
        let out = luma(&src, 12, 12, 2 * 4 + 2, 5 * 4, 6, 1); // x0=2, fx=2
        assert_eq!(out, vec![102, 92, 133, 133, 92, 102]);
        // 垂直对称：列 5 上扫 y=2..7 同样式
        let out = luma(&src, 12, 12, 5 * 4, 2 * 4 + 2, 1, 6); // y0=2, fy=2
        assert_eq!(out, vec![102, 92, 133, 133, 92, 102]);
    }

    #[test]
    fn halfpel_center_impulse_golden() {
        // j(5,5)：行和仅行 5 含冲激（列 5 权重 20 → 4240），垂直行 5
        // 再落权重 20：20·4240 + 12·3200 = 123200 → +512>>10 = 120
        let src = impulse_at_5_5();
        assert_eq!(luma(&src, 12, 12, 5 * 4 + 2, 5 * 4 + 2, 1, 1), vec![120]);
        // 冲激 5×5 影响十字区外：j(8,8) 回到常数 100
        assert_eq!(luma(&src, 12, 12, 8 * 4 + 2, 8 * 4 + 2, 1, 1), vec![100]);
    }

    #[test]
    fn quarter_pel_axis_and_diagonal_golden() {
        let src = impulse_at_5_5();
        let at = |fx: i64, fy: i64| luma(&src, 12, 12, 5 * 4 + fx, 5 * 4 + fy, 1, 1);
        // F(5,5)=152、F(6,5)=100、b(5,5)=h(5,5)=133、b(5,6)=h(6,5)=100、
        // j(5,5)=120（见上三测）
        assert_eq!(at(1, 0), vec![143]); // avg(F=152, b=133)
        assert_eq!(at(3, 0), vec![117]); // avg(F(6,5)=100, b=133)
        assert_eq!(at(0, 1), vec![143]); // avg(F=152, h=133)
        assert_eq!(at(0, 3), vec![117]); // avg(F(5,6)=100, h=133)
        assert_eq!(at(1, 1), vec![133]); // avg(b=133, h=133)
        assert_eq!(at(3, 1), vec![117]); // avg(b=133, h(6,5)=100)
        assert_eq!(at(1, 3), vec![117]); // avg(b(5,6)=100, h=133)
        assert_eq!(at(3, 3), vec![100]); // avg(100, 100)
        assert_eq!(at(2, 1), vec![127]); // avg(b=133, j=120)
        assert_eq!(at(1, 2), vec![127]); // avg(h=133, j=120)
        assert_eq!(at(3, 2), vec![110]); // avg(h(6,5)=100, j=120)
        assert_eq!(at(2, 3), vec![110]); // avg(b(5,6)=100, j=120)
    }

    /// 判别用例：非对称冲激 (3,1) 使 b/h/j 三路值互异，锁定
    /// (2,1)/(2,3) 走 b 路、(1,2)/(3,2) 走 h 路（含 (3,2) 的 x+1 偏移）。
    /// 修正前的占位实现在此四档全部给出错误值（b/h 接反 + 缺偏移）。
    #[test]
    fn quarter_pel_mixed_axis_golden() {
        let src = impulse_at_3_1();
        // 参考值（quarter_pel_mixed_axis_reference 逐项推导 + Python 复核）：
        // b(0,1)=102、h(0,1)=100、h(1,1)=100、j(0,1)=j(0,0)=101
        // (2,1)：qy = 1*4+1 → y0=1、fy=1
        assert_eq!(luma(&src, 10, 10, 2, 5, 1, 1), vec![102]);
        // qy = 1*4+2 → y0=1、fy=2
        assert_eq!(luma(&src, 10, 10, 1, 6, 1, 1), vec![101]); // (1,2)
        assert_eq!(luma(&src, 10, 10, 3, 6, 1, 1), vec![101]); // (3,2)
        assert_eq!(luma(&src, 10, 10, 2, 3, 1, 1), vec![102]); // (2,3)@(0,0)
    }

    /// mixed-axis 判别用例的参考值推导（逐项手算 + Python 仿真复核）。
    #[test]
    fn quarter_pel_mixed_axis_reference() {
        let src = impulse_at_3_1();
        // b(0,1)：窗口列 −2..3（负列钳位到 0，无冲激），冲激列 3 为末位
        // （权重 1）：(3200 + 52 + 16)>>5 = 102
        assert_eq!(hpel_h(&src, 10, 10, 10, 0, 1), 102);
        // h(0,1)/h(1,1)：列 0/1 无冲激（冲激在列 3）→ 100
        assert_eq!(hpel_v(&src, 10, 10, 10, 0, 1), 100);
        assert_eq!(hpel_v(&src, 10, 10, 10, 1, 1), 100);
        // h(3,1)：冲激行落入权重 20 → 133（对照）
        assert_eq!(hpel_v(&src, 10, 10, 10, 3, 1), 133);
        // j(0,1)：行 1 水平和 = 3252（冲激列末位权重 1），垂直行 1 落
        // 权重 20：20·3252 + 12·3200 = 103440 → (103440+512)>>10 = 101
        assert_eq!(hpel_j(&src, 10, 10, 10, 0, 1), 101);
        // j(0,0)：窗口行 −1..4（负行钳位），行 1 同落权重 20 → 101
        assert_eq!(hpel_j(&src, 10, 10, 10, 0, 0), 101);
    }

    #[test]
    fn ramp_halfpel_golden() {
        // p(x,y) = x：行和 = 32x + 16，(+16>>5) → b(x) = x + 1
        let mut src = vec![0u8; 256];
        for y in 0..16usize {
            for x in 0..16usize {
                src[y * 16 + x] = x as u8;
            }
        }
        let mut dst = vec![0u8; 12];
        mc_luma_put(&mut dst, 6, &src, 16, 16, 16, 2 * 4 + 2, 4 * 4, 6, 2);
        assert_eq!(dst, vec![3, 4, 5, 6, 7, 8, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn constant_plane_all_16_positions() {
        let src = vec![137u8; 64];
        for fx in 0..=3i64 {
            for fy in 0..=3i64 {
                let mut dst = vec![0u8; 16];
                mc_luma_put(&mut dst, 4, &src, 8, 8, 8, 8 + fx, 8 + fy, 4, 4);
                assert!(dst.iter().all(|&v| v == 137), "fx={fx} fy={fy}");
            }
        }
    }

    #[test]
    fn edge_clamp_replication_golden() {
        // 4×4 平面 p = y*4+x；x0=y0=−2、(0,0)：行列 −2,−1,0,1 钳位 0,0,0,1
        let src: Vec<u8> = (0..16u8).collect();
        let mut dst = vec![0u8; 16];
        mc_luma_put(&mut dst, 4, &src, 4, 4, 4, -2 * 4, -2 * 4, 4, 4);
        assert_eq!(dst, vec![0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 1, 4, 4, 4, 5]);
        // 半像素经钳位为常数 0 → (0+16)>>5 = 0
        assert_eq!(luma(&src, 4, 4, -2 * 4 + 2, -2 * 4 + 2, 1, 1), vec![0]);
    }

    #[test]
    fn negative_qpel_floor_semantics() {
        // qx=−1 → x0=−1、fx=3（Rust 算术 >> 的 floor 分解）；常数平面不变式
        let src = vec![90u8; 36];
        for (fx, fy) in [(3i64, 0i64), (0, 3), (3, 3), (2, 2), (1, 1)] {
            let qx = -4 + fx;
            let qy = -4 + fy;
            let out = luma(&src, 6, 6, qx, qy, 3, 3);
            assert!(out.iter().all(|&v| v == 90), "fx={fx} fy={fy}");
            assert_eq!(qx >> 2, -1);
            assert_eq!(qx & 3, fx);
        }
    }

    // ------------------------------------------------------------------
    // chroma 黄金值
    // ------------------------------------------------------------------

    #[test]
    fn chroma_bilinear_golden() {
        // 4×4 平面 p = y*4 + x；右缘/底缘越界读复制末列/末行
        let src: Vec<u8> = (0..16u8).collect();
        let mut dst = vec![0u8; 8];
        // fx=fy=4：四点等权 (Σ + 2) >> 2；qx=qy=4 → 原点 (0,0)，行 0/1
        mc_chroma_put(&mut dst, 4, &src, 4, 4, 4, 4, 4, 4, 2);
        // 行 0：x=0..3，x=3 的 TR/BR 钳位复制列 3
        assert_eq!(dst[..4], [3, 4, 5, 5]);
        // 行 1：p(x,1)=4+x、p(x,2)=8+x
        assert_eq!(dst[4..], [7, 8, 9, 9]);
        // fx=fy=0：整点复制（qx=0, qy=8 → 原点 (0,1)；第 4 行越界复制末行）
        let mut c = vec![0u8; 16];
        mc_chroma_put(&mut c, 4, &src, 4, 4, 4, 0, 8, 4, 4);
        assert_eq!(
            c,
            vec![4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 12, 13, 14, 15]
        );
        // fx=7, fy=0：权重 (TL,TR) = (8,56)，@x=0: (8·0+56·1+32)>>6 = 1；
        // TR 钳位使 x=3 也取列 3 → 3
        let mut d = vec![0u8; 4];
        mc_chroma_put(&mut d, 4, &src, 4, 4, 4, 7, 0, 4, 1);
        assert_eq!(d, vec![1, 2, 3, 3]);
        // fy=7 底行（qy=31 → y0=3）：y+1 越界复制末行，fx=0 时
        // 权重全落 TL/BL（各 8/56，同列同值）→ 恒等于 p(x,3)
        let mut e = vec![0u8; 4];
        mc_chroma_put(&mut e, 4, &src, 4, 4, 4, 0, 3 * 8 + 7, 4, 1);
        assert_eq!(e, vec![12, 13, 14, 15]);
    }

    // ------------------------------------------------------------------
    // ②b 原有回归（保留）
    // ------------------------------------------------------------------

    /// 平坦源：任何分数位置都等于同一值。
    #[test]
    fn flat_source_identity() {
        let src = vec![100u8; 16 * 16];
        let mut dst = [0u8; 16];
        for (fx, fy) in [(0i64, 0i64), (1, 0), (2, 0), (3, 3), (2, 2), (1, 2)] {
            mc_luma_put(&mut dst, 4, &src, 16, 16, 16, 8 + fx, 8 + fy, 4, 4);
            assert!(dst.iter().all(|&v| v == 100), "fx={fx} fy={fy}");
        }
        let csrc = vec![60u8; 8 * 8];
        let mut cdst = [0u8; 4];
        for (fx, fy) in [(0i64, 0i64), (3, 5), (7, 7)] {
            mc_chroma_put(&mut cdst, 2, &csrc, 8, 8, 8, 8 + fx, 8 + fy, 2, 2);
            assert!(cdst.iter().all(|&v| v == 60), "fx={fx} fy={fy}");
        }
    }

    /// 全零 MV = 整像素拷贝。
    #[test]
    fn integer_copy() {
        let src: Vec<u8> = (0..64).map(|i| (i * 7) as u8).collect();
        let mut dst = [0u8; 16];
        mc_luma_put(&mut dst, 4, &src, 8, 8, 8, 8, 8, 4, 4);
        for y in 0..4 {
            for x in 0..4 {
                assert_eq!(dst[y * 4 + x], src[(y + 2) * 8 + (x + 2)]);
            }
        }
    }

    /// 半像素：6 抽头对线性信号精确（h(x+.5,y) = round(x+0.5+y)）。
    #[test]
    fn hpel_linear_ramp() {
        let w = 12usize;
        let src: Vec<u8> = (0..w * w).map(|i| ((i % w + i / w) % 256) as u8).collect();
        assert_eq!(hpel_h(&src, w, w, w, 5, 5), 11);
        assert_eq!(hpel_j(&src, w, w, w, 5, 5), 11);
    }

    /// 越界读按边缘复制（MV 指向画面左上角外）。
    #[test]
    fn edge_clamp() {
        let src: Vec<u8> = (0..64).map(|i| 200u8 - (i % 32) as u8).collect();
        let mut dst = [0u8; 4];
        mc_luma_put(&mut dst, 1, &src, 8, 8, 8, -4, -4, 2, 2);
        assert_eq!(dst[0], src[0]);
    }

    /// avg 变体：put 结果与 dst 现值的 rnd 平均。
    #[test]
    fn avg_variants() {
        let src = vec![200u8; 64];
        let mut dst = vec![100u8; 16];
        mc_luma_avg(&mut dst, 4, &src, 8, 8, 8, 16, 16, 4, 4);
        assert!(dst.iter().all(|&v| v == 150)); // (100+200+1)>>1
        let mut cdst = vec![10u8; 16];
        mc_chroma_avg(&mut cdst, 4, &src, 8, 8, 8, 8, 8, 4, 4);
        assert!(cdst.iter().all(|&v| v == 105)); // (10+200+1)>>1
    }
}
