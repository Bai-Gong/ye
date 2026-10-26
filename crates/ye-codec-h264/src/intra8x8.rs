//! Intra 8x8（I_NxN + transform_8x8 的 luma 8x8 路径，High 档）。
//!
//! 预测公式逐字取自 FFmpeg h264pred_template.c 的 pred8x8l 系列
//!（含 spec 8.3.2.2.1 的边沿三抽头滤波——`t*`/`l*` 载入即滤波，
//! DC 变体同用滤波边）；8x8 整数反变换逐字取自 FFmpeg
//! h264idct_template.c `ff_h264_idct8_add`/`ff_h264_idct8_dc_add`；
//! 8x8 zigzag = spec 表 7-6（FFmpeg ff_zigzag_direct）。
//!
//! 边沿可用性按象限判定（progressive 光栅解码序）：
//! - 顶边：qy==0 需顶宏块，否则恒可用（MB 内已重建）；
//! - 左边：qx==0 需左宏块；
//! - 左上：(0,0) 需左上宏块、(1,0) 需顶宏块、(0,1) 需左宏块、
//!   (1,1) 恒可用；
//! - 右上（DDL/VL 的 t8..t15）：(0,0) 需顶宏块、(1,0) 需右上宏块、
//!   qy==1 恒不可得（右宏块未解码）——不可得时按 FFmpeg
//!   `has_topright=0` 复制 t7；
//! - 模式改写沿用 4x4 族的规则（FFmpeg check_intra8x8_pred_mode
//!   同族语义）：缺顶 → 仅 H/HU 存留、DC→LEFT；缺左 → V/DDL/VL
//!   存留、DC→TOP、LEFT→128；缺左上的对角模式 → Unsupported。

use crate::error::H264Error;
use crate::macroblock::{NeighAvail, Picture, neighbor_avail};

/// 8x8 zigzag（spec 表 7-6）。
pub(crate) const ZIGZAG8: [usize; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// 8x8 模式变体码（FFmpeg 编号：9=LEFT_DC、10=TOP_DC、11=DC_128；
/// spec 模式 0..8 无冲突）。
const LEFT_DC: u8 = 9;
const TOP_DC: u8 = 10;
const DC_128: u8 = 11;

/// 象限 (qx, qy) 的边沿可用性。
pub(crate) struct QuadAvail {
    top: bool,
    left: bool,
    topleft: bool,
    topright: bool,
}

pub(crate) fn quad_avail(avail: &NeighAvail, qx: usize, qy: usize) -> QuadAvail {
    QuadAvail {
        top: qy > 0 || avail.top,
        left: qx > 0 || avail.left,
        topleft: match (qx, qy) {
            (1, 1) => true,
            (0, 0) => avail.topleft,
            (1, 0) => avail.top,
            _ => avail.left,
        },
        topright: match (qx, qy) {
            (0, 0) => avail.top,
            (1, 0) => avail.er,
            _ => false,
        },
    }
}

/// 模式改写（FFmpeg check_intra8x8_pred_mode 同族规则；见模块文档）。
pub(crate) fn rewrite_8x8_mode(mode: u8, q: &QuadAvail) -> Result<u8, H264Error> {
    if mode > 8 {
        return Err(H264Error::InvalidData(format!(
            "intra8x8 pred mode {mode} out of range"
        )));
    }
    let mut m = mode;
    if !q.top {
        m = match m {
            1 | 8 => m,   // H、HU 不需要顶
            2 => LEFT_DC, // DC → DC_LEFT
            _ => {
                return Err(H264Error::InvalidData(
                    "top samples unavailable for intra8x8 mode".into(),
                ));
            }
        };
    }
    if !q.left {
        m = match m {
            0 | 3 | 7 => m, // V、DDL、VL 不需要左
            2 => TOP_DC,    // DC → DC_TOP
            LEFT_DC => DC_128,
            _ => {
                return Err(H264Error::InvalidData(
                    "left samples unavailable for intra8x8 mode".into(),
                ));
            }
        };
    }
    // 对角族（DDR=4/VR=5/HD=6）需要左上角样本；不可用时如实收紧
    if matches!(m, 4..=6) && !q.topleft {
        return Err(H264Error::Unsupported(
            "intra8x8 diagonal mode needs unavailable topleft sample",
        ));
    }
    Ok(m)
}

/// 象限 q 的邻块 8x8 模式（side：0=左、1=上；FFmpeg
/// pred_intra_mode 于象限左上 4x4 的缓存位——8x8 邻块象限模式已按
/// 象限复制到 i4_modes，4x4 邻块按对应块位读取，非 I4x4 → DC）。
pub(crate) fn neighbor_i8_mode_quads(
    pic: &Picture,
    mbx: usize,
    mby: usize,
    q: usize,
    side: usize,
) -> u8 {
    use crate::macroblock::MbKind;
    let (w, mb_xy) = (pic.mb_width, mby * pic.mb_width + mbx);
    let nb = if side == 0 {
        if mbx == 0 {
            None
        } else {
            Some(&pic.mbs[mb_xy - 1])
        }
    } else if mby == 0 {
        None
    } else {
        Some(&pic.mbs[mb_xy - w])
    };
    let Some(nb) = nb else { return 2 };
    if nb.kind != MbKind::Intra4x4 {
        return 2;
    }
    // 左：右列块 3 + 8·(q>>1)；上：底行块 12 + 2·(q&1)
    let b = if side == 0 {
        3 + 8 * (q >> 1)
    } else {
        12 + 2 * (q & 1)
    };
    nb.i4_modes[b]
}

/// 邻块贡献的 8x8 模式（非 I8x8 邻块贡献 2=DC——与 4x4 族同语义）。
// M2 WIP（CABAC 对拍收尾）：邻块取值改走象限版 neighbor_i8_mode_quads
// 后保留的标量形态变体，收尾轮定去留。
#[allow(dead_code)]
pub(crate) fn neighbor_i8_mode(pic: &Picture, mb_xy: usize, quad: usize) -> u8 {
    use crate::macroblock::MbKind;
    let mb = &pic.mbs[mb_xy];
    if mb.kind == MbKind::Intra4x4 && mb.transform_8x8 {
        mb.i4_modes[quad * 4]
    } else {
        2
    }
}

// ---------------------------------------------------------------------------
// 边沿载入（载入即三抽头滤波；FFmpeg PREDICT_8x8_LOAD_* 逐字）
// ---------------------------------------------------------------------------

/// 读 8x8 象限的邻域原始样本：`src(x, y)` 相对宏块左上角（−1..=16）。
struct Edge8<'a> {
    luma: &'a [u8],
    stride: usize,
    ox: usize,
    oy: usize,
}

impl Edge8<'_> {
    fn at(&self, x: i32, y: i32) -> i32 {
        let px = self.ox as i64 + i64::from(x);
        let py = self.oy as i64 + i64::from(y);
        if px < 0 || py < 0 || px >= self.luma.len() as i64 / self.stride as i64 {
            return 0; // 不可用侧由调用方保证不读（模式改写）
        }
        if py * self.stride as i64 + px >= self.luma.len() as i64 {
            return 0;
        }
        i32::from(self.luma[(py * self.stride as i64 + px) as usize])
    }
}

/// 滤波顶边 t0..t7（FFmpeg PREDICT_8x8_LOAD_TOP）。
fn load_top(e: &Edge8, q: &QuadAvail) -> [i32; 8] {
    let s = |x: i32| e.at(x, -1);
    let mut t = [0i32; 8];
    t[0] = ((if q.topleft { s(-1) } else { s(0) }) + 2 * s(0) + s(1) + 2) >> 2;
    for (i, slot) in (1..7).zip(t.iter_mut().skip(1)) {
        *slot = (s(i - 1) + 2 * s(i) + s(i + 1) + 2) >> 2;
    }
    t[7] = ((if q.topright { s(8) } else { s(7) }) + 2 * s(7) + s(6) + 2) >> 2;
    t
}

/// 滤波顶边延伸 t8..t15（FFmpeg PREDICT_8x8_LOAD_TOPRIGHT：不可得复制 t7）。
fn load_topright(e: &Edge8, q: &QuadAvail, t7: i32) -> [i32; 8] {
    let s = |x: i32| e.at(x, -1);
    let mut t = [0i32; 8];
    if q.topright {
        for (i, slot) in (0..7).zip(t.iter_mut()) {
            *slot = (s(8 + i - 1) + 2 * s(8 + i) + s(8 + i + 1) + 2) >> 2;
        }
        t[7] = (s(14) + 3 * s(15) + 2) >> 2;
    } else {
        t = [t7; 8];
    }
    t
}

/// 滤波左边 l0..l7（FFmpeg PREDICT_8x8_LOAD_LEFT）。
fn load_left(e: &Edge8, _q: &QuadAvail) -> [i32; 8] {
    let s = |y: i32| e.at(-1, y);
    let mut l = [0i32; 8];
    l[0] = ((if _q.topleft { s(-1) } else { s(0) }) + 2 * s(0) + s(1) + 2) >> 2;
    for (i, slot) in (1..7).zip(l.iter_mut().skip(1)) {
        *slot = (s(i - 1) + 2 * s(i) + s(i + 1) + 2) >> 2;
    }
    l[7] = (s(6) + 3 * s(7) + 2) >> 2;
    l
}

/// 滤波左上角（FFmpeg PREDICT_8x8_LOAD_TOPLEFT）。
fn load_topleft(e: &Edge8) -> i32 {
    (e.at(-1, 0) + 2 * e.at(-1, -1) + e.at(0, -1) + 2) >> 2
}

// ---------------------------------------------------------------------------
// 预测（FFmpeg pred8x8l_* 逐字；写入 8x8 光栅 out）
// ---------------------------------------------------------------------------

/// 8x8 帧内方向预测（模式分派；仅本模块 [`Edge8`] 载入消费）。
fn pred_8x8l(mode: u8, e: &Edge8, q: &QuadAvail, out: &mut [u8], stride: usize) {
    let put = |out: &mut [u8], stride: usize, x: i32, y: i32, v: i32| {
        out[(y as usize) * stride + x as usize] = v.clamp(0, 255) as u8;
    };
    match mode {
        0 => {
            // V
            let t = load_top(e, q);
            for y in 0..8 {
                for x in 0..8 {
                    put(out, stride, x, y, t[x as usize]);
                }
            }
        }
        1 => {
            // H
            let l = load_left(e, q);
            for y in 0..8 {
                for x in 0..8 {
                    put(out, stride, x, y, l[y as usize]);
                }
            }
        }
        2 | LEFT_DC | TOP_DC | DC_128 => {
            let dc = match mode {
                2 => {
                    let l = load_left(e, q);
                    let t = load_top(e, q);
                    (l.iter().sum::<i32>() + t.iter().sum::<i32>() + 8) >> 4
                }
                LEFT_DC => (load_left(e, q).iter().sum::<i32>() + 4) >> 3,
                TOP_DC => (load_top(e, q).iter().sum::<i32>() + 4) >> 3,
                _ => 128,
            };
            for y in 0..8 {
                for x in 0..8 {
                    put(out, stride, x, y, dc);
                }
            }
        }
        3 => {
            // DDL（FFmpeg pred8x8l_down_left 逐字展开）
            let t = load_top(e, q);
            let tr = load_topright(e, q, t[7]);
            let tt = |i: usize| -> i32 { if i < 8 { t[i] } else { tr[i - 8] } };
            let f = |a: i32, b: i32, c: i32| (a + 2 * b + c + 2) >> 2;
            put(out, stride, 0, 0, f(t[0], t[1], t[2]));
            let v = f(t[1], t[2], t[3]);
            put(out, stride, 1, 0, v);
            put(out, stride, 0, 1, v);
            let v = f(t[2], t[3], t[4]);
            put(out, stride, 2, 0, v);
            put(out, stride, 1, 1, v);
            put(out, stride, 0, 2, v);
            let v = f(t[3], t[4], t[5]);
            put(out, stride, 3, 0, v);
            put(out, stride, 2, 1, v);
            put(out, stride, 1, 2, v);
            put(out, stride, 0, 3, v);
            let v = f(t[4], t[5], t[6]);
            put(out, stride, 4, 0, v);
            put(out, stride, 3, 1, v);
            put(out, stride, 2, 2, v);
            put(out, stride, 1, 3, v);
            put(out, stride, 0, 4, v);
            let v = f(t[5], t[6], t[7]);
            put(out, stride, 5, 0, v);
            put(out, stride, 4, 1, v);
            put(out, stride, 3, 2, v);
            put(out, stride, 2, 3, v);
            put(out, stride, 1, 4, v);
            put(out, stride, 0, 5, v);
            let v = f(t[6], t[7], tt(8));
            put(out, stride, 6, 0, v);
            put(out, stride, 5, 1, v);
            put(out, stride, 4, 2, v);
            put(out, stride, 3, 3, v);
            put(out, stride, 2, 4, v);
            put(out, stride, 1, 5, v);
            put(out, stride, 0, 6, v);
            let v = f(t[7], tt(8), tt(9));
            put(out, stride, 7, 0, v);
            put(out, stride, 6, 1, v);
            put(out, stride, 5, 2, v);
            put(out, stride, 4, 3, v);
            put(out, stride, 3, 4, v);
            put(out, stride, 2, 5, v);
            put(out, stride, 1, 6, v);
            put(out, stride, 0, 7, v);
            let v = f(tt(8), tt(9), tt(10));
            put(out, stride, 7, 1, v);
            put(out, stride, 6, 2, v);
            put(out, stride, 5, 3, v);
            put(out, stride, 4, 4, v);
            put(out, stride, 3, 5, v);
            put(out, stride, 2, 6, v);
            put(out, stride, 1, 7, v);
            let v = f(tt(9), tt(10), tt(11));
            put(out, stride, 7, 2, v);
            put(out, stride, 6, 3, v);
            put(out, stride, 5, 4, v);
            put(out, stride, 4, 5, v);
            put(out, stride, 3, 6, v);
            put(out, stride, 2, 7, v);
            let v = f(tt(10), tt(11), tt(12));
            put(out, stride, 7, 3, v);
            put(out, stride, 6, 4, v);
            put(out, stride, 5, 5, v);
            put(out, stride, 4, 6, v);
            put(out, stride, 3, 7, v);
            let v = f(tt(11), tt(12), tt(13));
            put(out, stride, 7, 4, v);
            put(out, stride, 6, 5, v);
            put(out, stride, 5, 6, v);
            put(out, stride, 4, 7, v);
            let v = f(tt(12), tt(13), tt(14));
            put(out, stride, 7, 5, v);
            put(out, stride, 6, 6, v);
            put(out, stride, 5, 7, v);
            let v = f(tt(13), tt(14), tt(15));
            put(out, stride, 7, 6, v);
            put(out, stride, 6, 7, v);
            put(out, stride, 7, 7, (tt(14) + 3 * tt(15) + 2) >> 2);
        }
        4 => {
            // DDR（FFmpeg pred8x8l_down_right 逐字展开）
            let t = load_top(e, q);
            let l = load_left(e, q);
            let lt = load_topleft(e);
            let f = |a: i32, b: i32, c: i32| (a + 2 * b + c + 2) >> 2;
            put(out, stride, 0, 7, f(l[7], l[6], l[5]));
            let v = f(l[6], l[5], l[4]);
            put(out, stride, 0, 6, v);
            put(out, stride, 1, 7, v);
            let v = f(l[5], l[4], l[3]);
            put(out, stride, 0, 5, v);
            put(out, stride, 1, 6, v);
            put(out, stride, 2, 7, v);
            let v = f(l[4], l[3], l[2]);
            put(out, stride, 0, 4, v);
            put(out, stride, 1, 5, v);
            put(out, stride, 2, 6, v);
            put(out, stride, 3, 7, v);
            let v = f(l[3], l[2], l[1]);
            put(out, stride, 0, 3, v);
            put(out, stride, 1, 4, v);
            put(out, stride, 2, 5, v);
            put(out, stride, 3, 6, v);
            put(out, stride, 4, 7, v);
            let v = f(l[2], l[1], l[0]);
            put(out, stride, 0, 2, v);
            put(out, stride, 1, 3, v);
            put(out, stride, 2, 4, v);
            put(out, stride, 3, 5, v);
            put(out, stride, 4, 6, v);
            put(out, stride, 5, 7, v);
            let v = f(l[1], l[0], lt);
            put(out, stride, 0, 1, v);
            put(out, stride, 1, 2, v);
            put(out, stride, 2, 3, v);
            put(out, stride, 3, 4, v);
            put(out, stride, 4, 5, v);
            put(out, stride, 5, 6, v);
            put(out, stride, 6, 7, v);
            let v = f(l[0], lt, t[0]);
            for d in 0..8 {
                put(out, stride, d, d, v);
            }
            let v = f(lt, t[0], t[1]);
            put(out, stride, 1, 0, v);
            put(out, stride, 2, 1, v);
            put(out, stride, 3, 2, v);
            put(out, stride, 4, 3, v);
            put(out, stride, 5, 4, v);
            put(out, stride, 6, 5, v);
            put(out, stride, 7, 6, v);
            let v = f(t[0], t[1], t[2]);
            put(out, stride, 2, 0, v);
            put(out, stride, 3, 1, v);
            put(out, stride, 4, 2, v);
            put(out, stride, 5, 3, v);
            put(out, stride, 6, 4, v);
            put(out, stride, 7, 5, v);
            let v = f(t[1], t[2], t[3]);
            put(out, stride, 3, 0, v);
            put(out, stride, 4, 1, v);
            put(out, stride, 5, 2, v);
            put(out, stride, 6, 3, v);
            put(out, stride, 7, 4, v);
            let v = f(t[2], t[3], t[4]);
            put(out, stride, 4, 0, v);
            put(out, stride, 5, 1, v);
            put(out, stride, 6, 2, v);
            put(out, stride, 7, 3, v);
            let v = f(t[3], t[4], t[5]);
            put(out, stride, 5, 0, v);
            put(out, stride, 6, 1, v);
            put(out, stride, 7, 2, v);
            let v = f(t[4], t[5], t[6]);
            put(out, stride, 6, 0, v);
            put(out, stride, 7, 1, v);
            put(out, stride, 7, 0, f(t[5], t[6], t[7]));
        }
        5 => {
            // VR（FFmpeg pred8x8l_vertical_right 逐字展开）
            let t = load_top(e, q);
            let l = load_left(e, q);
            let lt = load_topleft(e);
            let f = |a: i32, b: i32, c: i32| (a + 2 * b + c + 2) >> 2;
            let f1 = |a: i32, b: i32| (a + b + 1) >> 1;
            put(out, stride, 0, 6, f(l[5], l[4], l[3]));
            put(out, stride, 0, 7, f(l[6], l[5], l[4]));
            let v = f(l[3], l[2], l[1]);
            put(out, stride, 0, 4, v);
            put(out, stride, 1, 6, v);
            let v = f(l[4], l[3], l[2]);
            put(out, stride, 0, 5, v);
            put(out, stride, 1, 7, v);
            let v = f(l[1], l[0], lt);
            put(out, stride, 0, 2, v);
            put(out, stride, 1, 4, v);
            put(out, stride, 2, 6, v);
            let v = f(l[2], l[1], l[0]);
            put(out, stride, 0, 3, v);
            put(out, stride, 1, 5, v);
            put(out, stride, 2, 7, v);
            let v = f(l[0], lt, t[0]);
            put(out, stride, 0, 1, v);
            put(out, stride, 1, 3, v);
            put(out, stride, 2, 5, v);
            put(out, stride, 3, 7, v);
            let v = f1(lt, t[0]);
            put(out, stride, 0, 0, v);
            put(out, stride, 1, 2, v);
            put(out, stride, 2, 4, v);
            put(out, stride, 3, 6, v);
            let v = f(lt, t[0], t[1]);
            put(out, stride, 1, 1, v);
            put(out, stride, 2, 3, v);
            put(out, stride, 3, 5, v);
            put(out, stride, 4, 7, v);
            let v = f1(t[0], t[1]);
            put(out, stride, 1, 0, v);
            put(out, stride, 2, 2, v);
            put(out, stride, 3, 4, v);
            put(out, stride, 4, 6, v);
            let v = f(t[0], t[1], t[2]);
            put(out, stride, 2, 1, v);
            put(out, stride, 3, 3, v);
            put(out, stride, 4, 5, v);
            put(out, stride, 5, 7, v);
            let v = f1(t[1], t[2]);
            put(out, stride, 2, 0, v);
            put(out, stride, 3, 2, v);
            put(out, stride, 4, 4, v);
            put(out, stride, 5, 6, v);
            let v = f(t[1], t[2], t[3]);
            put(out, stride, 3, 1, v);
            put(out, stride, 4, 3, v);
            put(out, stride, 5, 5, v);
            put(out, stride, 6, 7, v);
            let v = f1(t[2], t[3]);
            put(out, stride, 3, 0, v);
            put(out, stride, 4, 2, v);
            put(out, stride, 5, 4, v);
            put(out, stride, 6, 6, v);
            let v = f(t[2], t[3], t[4]);
            put(out, stride, 4, 1, v);
            put(out, stride, 5, 3, v);
            put(out, stride, 6, 5, v);
            put(out, stride, 7, 7, v);
            let v = f1(t[3], t[4]);
            put(out, stride, 4, 0, v);
            put(out, stride, 5, 2, v);
            put(out, stride, 6, 4, v);
            put(out, stride, 7, 6, v);
            let v = f(t[3], t[4], t[5]);
            put(out, stride, 5, 1, v);
            put(out, stride, 6, 3, v);
            put(out, stride, 7, 5, v);
            let v = f1(t[4], t[5]);
            put(out, stride, 5, 0, v);
            put(out, stride, 6, 2, v);
            put(out, stride, 7, 4, v);
            put(out, stride, 6, 1, f(t[4], t[5], t[6]));
            let v = f1(t[5], t[6]);
            put(out, stride, 6, 0, v);
            put(out, stride, 7, 2, v);
            put(out, stride, 7, 1, f(t[5], t[6], t[7]));
            put(out, stride, 7, 0, f1(t[6], t[7]));
        }
        6 => {
            // HD（FFmpeg pred8x8l_horizontal_down 逐字展开）
            let t = load_top(e, q);
            let l = load_left(e, q);
            let lt = load_topleft(e);
            let f = |a: i32, b: i32, c: i32| (a + 2 * b + c + 2) >> 2;
            let f1 = |a: i32, b: i32| (a + b + 1) >> 1;
            put(out, stride, 0, 7, f1(l[6], l[7]));
            put(out, stride, 1, 7, f(l[5], l[6], l[7]));
            let v = f1(l[5], l[6]);
            put(out, stride, 0, 6, v);
            put(out, stride, 2, 7, v);
            let v = f(l[4], l[5], l[6]);
            put(out, stride, 1, 6, v);
            put(out, stride, 3, 7, v);
            let v = f1(l[4], l[5]);
            put(out, stride, 0, 5, v);
            put(out, stride, 2, 6, v);
            put(out, stride, 4, 7, v);
            let v = f(l[3], l[4], l[5]);
            put(out, stride, 1, 5, v);
            put(out, stride, 3, 6, v);
            put(out, stride, 5, 7, v);
            let v = f1(l[3], l[4]);
            put(out, stride, 0, 4, v);
            put(out, stride, 2, 5, v);
            put(out, stride, 4, 6, v);
            put(out, stride, 6, 7, v);
            let v = f(l[2], l[3], l[4]);
            put(out, stride, 1, 4, v);
            put(out, stride, 3, 5, v);
            put(out, stride, 5, 6, v);
            put(out, stride, 7, 7, v);
            let v = f1(l[2], l[3]);
            put(out, stride, 0, 3, v);
            put(out, stride, 2, 4, v);
            put(out, stride, 4, 5, v);
            put(out, stride, 6, 6, v);
            let v = f(l[1], l[2], l[3]);
            put(out, stride, 1, 3, v);
            put(out, stride, 3, 4, v);
            put(out, stride, 5, 5, v);
            put(out, stride, 7, 6, v);
            let v = f1(l[1], l[2]);
            put(out, stride, 0, 2, v);
            put(out, stride, 2, 3, v);
            put(out, stride, 4, 4, v);
            put(out, stride, 6, 5, v);
            let v = f(l[0], l[1], l[2]);
            put(out, stride, 1, 2, v);
            put(out, stride, 3, 3, v);
            put(out, stride, 5, 4, v);
            put(out, stride, 7, 5, v);
            let v = f1(l[0], l[1]);
            put(out, stride, 0, 1, v);
            put(out, stride, 2, 2, v);
            put(out, stride, 4, 3, v);
            put(out, stride, 6, 4, v);
            let v = f(lt, l[0], l[1]);
            put(out, stride, 1, 1, v);
            put(out, stride, 3, 2, v);
            put(out, stride, 5, 3, v);
            put(out, stride, 7, 4, v);
            let v = f1(lt, l[0]);
            put(out, stride, 0, 0, v);
            put(out, stride, 2, 1, v);
            put(out, stride, 4, 2, v);
            put(out, stride, 6, 3, v);
            let v = f(l[0], lt, t[0]);
            put(out, stride, 1, 0, v);
            put(out, stride, 3, 1, v);
            put(out, stride, 5, 2, v);
            put(out, stride, 7, 3, v);
            let v = f(t[1], t[0], lt);
            put(out, stride, 2, 0, v);
            put(out, stride, 4, 1, v);
            put(out, stride, 6, 2, v);
            let v = f(t[2], t[1], t[0]);
            put(out, stride, 3, 0, v);
            put(out, stride, 5, 1, v);
            put(out, stride, 7, 2, v);
            let v = f(t[3], t[2], t[1]);
            put(out, stride, 4, 0, v);
            put(out, stride, 6, 1, v);
            let v = f(t[4], t[3], t[2]);
            put(out, stride, 5, 0, v);
            put(out, stride, 7, 1, v);
            put(out, stride, 6, 0, f(t[5], t[4], t[3]));
            put(out, stride, 7, 0, f(t[6], t[5], t[4]));
        }
        7 => {
            // VL（FFmpeg pred8x8l_vertical_left 逐字展开）
            let t = load_top(e, q);
            let tr = load_topright(e, q, t[7]);
            let tt = |i: usize| -> i32 { if i < 8 { t[i] } else { tr[i - 8] } };
            let f = |a: i32, b: i32, c: i32| (a + 2 * b + c + 2) >> 2;
            let f1 = |a: i32, b: i32| (a + b + 1) >> 1;
            put(out, stride, 0, 0, f1(t[0], t[1]));
            put(out, stride, 0, 1, f(t[0], t[1], t[2]));
            let v = f1(t[1], t[2]);
            put(out, stride, 0, 2, v);
            put(out, stride, 1, 0, v);
            let v = f(t[1], t[2], t[3]);
            put(out, stride, 0, 3, v);
            put(out, stride, 1, 1, v);
            let v = f1(t[2], t[3]);
            put(out, stride, 0, 4, v);
            put(out, stride, 1, 2, v);
            put(out, stride, 2, 0, v);
            let v = f(t[2], t[3], t[4]);
            put(out, stride, 0, 5, v);
            put(out, stride, 1, 3, v);
            put(out, stride, 2, 1, v);
            let v = f1(t[3], t[4]);
            put(out, stride, 0, 6, v);
            put(out, stride, 1, 4, v);
            put(out, stride, 2, 2, v);
            put(out, stride, 3, 0, v);
            let v = f(t[3], t[4], t[5]);
            put(out, stride, 0, 7, v);
            put(out, stride, 1, 5, v);
            put(out, stride, 2, 3, v);
            put(out, stride, 3, 1, v);
            let v = f1(t[4], t[5]);
            put(out, stride, 1, 6, v);
            put(out, stride, 2, 4, v);
            put(out, stride, 3, 2, v);
            put(out, stride, 4, 0, v);
            let v = f(t[4], t[5], t[6]);
            put(out, stride, 1, 7, v);
            put(out, stride, 2, 5, v);
            put(out, stride, 3, 3, v);
            put(out, stride, 4, 1, v);
            let v = f1(t[5], t[6]);
            put(out, stride, 2, 6, v);
            put(out, stride, 3, 4, v);
            put(out, stride, 4, 2, v);
            put(out, stride, 5, 0, v);
            let v = f(t[5], t[6], t[7]);
            put(out, stride, 2, 7, v);
            put(out, stride, 3, 5, v);
            put(out, stride, 4, 3, v);
            put(out, stride, 5, 1, v);
            let v = f1(t[6], t[7]);
            put(out, stride, 3, 6, v);
            put(out, stride, 4, 4, v);
            put(out, stride, 5, 2, v);
            put(out, stride, 6, 0, v);
            let v = f(t[6], t[7], tt(8));
            put(out, stride, 3, 7, v);
            put(out, stride, 4, 5, v);
            put(out, stride, 5, 3, v);
            put(out, stride, 6, 1, v);
            let v = f1(t[7], tt(8));
            put(out, stride, 4, 6, v);
            put(out, stride, 5, 4, v);
            put(out, stride, 6, 2, v);
            put(out, stride, 7, 0, v);
            let v = f(t[7], tt(8), tt(9));
            put(out, stride, 4, 7, v);
            put(out, stride, 5, 5, v);
            put(out, stride, 6, 3, v);
            put(out, stride, 7, 1, v);
            let v = f1(tt(8), tt(9));
            put(out, stride, 5, 6, v);
            put(out, stride, 6, 4, v);
            put(out, stride, 7, 2, v);
            let v = f(tt(8), tt(9), tt(10));
            put(out, stride, 5, 7, v);
            put(out, stride, 6, 5, v);
            put(out, stride, 7, 3, v);
            let v = f1(tt(9), tt(10));
            put(out, stride, 6, 6, v);
            put(out, stride, 7, 4, v);
            let v = f(tt(9), tt(10), tt(11));
            put(out, stride, 6, 7, v);
            put(out, stride, 7, 5, v);
            put(out, stride, 7, 6, f1(tt(10), tt(11)));
            put(out, stride, 7, 7, f(tt(10), tt(11), tt(12)));
        }
        8 => {
            // HU（FFmpeg pred8x8l_horizontal_up 逐字展开；只用左）
            let l = load_left(e, q);
            let f = |a: i32, b: i32, c: i32| (a + 2 * b + c + 2) >> 2;
            let f1 = |a: i32, b: i32| (a + b + 1) >> 1;
            put(out, stride, 0, 0, f1(l[0], l[1]));
            put(out, stride, 1, 0, f(l[0], l[1], l[2]));
            let v = f1(l[1], l[2]);
            put(out, stride, 2, 0, v);
            put(out, stride, 0, 1, v);
            let v = f(l[1], l[2], l[3]);
            put(out, stride, 3, 0, v);
            put(out, stride, 1, 1, v);
            let v = f1(l[2], l[3]);
            put(out, stride, 2, 1, v);
            put(out, stride, 0, 2, v);
            let v = f(l[2], l[3], l[4]);
            put(out, stride, 3, 1, v);
            put(out, stride, 1, 2, v);
            let v = f1(l[3], l[4]);
            put(out, stride, 2, 2, v);
            put(out, stride, 0, 3, v);
            let v = f(l[3], l[4], l[5]);
            put(out, stride, 3, 2, v);
            put(out, stride, 1, 3, v);
            let v = f1(l[4], l[5]);
            put(out, stride, 2, 3, v);
            put(out, stride, 0, 4, v);
            let v = f(l[4], l[5], l[6]);
            put(out, stride, 3, 3, v);
            put(out, stride, 1, 4, v);
            let v = f1(l[5], l[6]);
            put(out, stride, 2, 4, v);
            put(out, stride, 0, 5, v);
            let v = f(l[5], l[6], l[7]);
            put(out, stride, 3, 4, v);
            put(out, stride, 1, 5, v);
            let v = f1(l[6], l[7]);
            put(out, stride, 2, 5, v);
            put(out, stride, 0, 6, v);
            let v = f(l[6], l[7], l[7]);
            put(out, stride, 3, 5, v);
            put(out, stride, 1, 6, v);
            // 右半与底行：l7 复制（FFmpeg pred8x8l_horizontal_up 的
            // SRC(x,7)=SRC(7,y)=l7 族展开）
            put(out, stride, 3, 6, l[7]);
            put(out, stride, 2, 6, l[7]);
            put(out, stride, 1, 7, l[7]);
            put(out, stride, 0, 7, l[7]);
            put(out, stride, 3, 7, l[7]);
            put(out, stride, 2, 7, l[7]);
            put(out, stride, 4, 0, l[7]);
            put(out, stride, 5, 0, l[7]);
            put(out, stride, 6, 0, l[7]);
            put(out, stride, 7, 0, l[7]);
            for y in 1..8 {
                for x in 4..8 {
                    put(out, stride, x, y, l[7]);
                }
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// 8x8 反变换（FFmpeg ff_h264_idct8_add / _dc_add 逐字）
// ---------------------------------------------------------------------------

/// 8x8 整数反变换 + 叠加。`block` 为 64 项（行主序）；`dst` 指向 8x8
/// 区域左上角。
pub(crate) fn idct8x8_add(dst: &mut [u8], stride: usize, block: &mut [i16; 64]) {
    block[0] += 32;
    // 垂直 pass（列）
    for i in 0..8usize {
        let g = |k: usize| i32::from(block[i + k * 8]);
        let a0 = g(0) + g(4);
        let a2 = g(0) - g(4);
        let a4 = (g(2) >> 1) - g(6);
        let a6 = (g(6) >> 1) + g(2);
        let b0 = a0 + a6;
        let b2 = a2 + a4;
        let b4 = a2 - a4;
        let b6 = a0 - a6;
        let a1 = -g(3) + g(5) - g(7) - (g(7) >> 1);
        let a3 = g(1) + g(7) - g(3) - (g(3) >> 1);
        let a5 = -g(1) + g(7) + g(5) + (g(5) >> 1);
        let a7 = g(3) + g(5) + g(1) + (g(1) >> 1);
        let b1 = (a7 >> 2) + a1;
        let b3 = a3 + (a5 >> 2);
        let b5 = (a3 >> 2) - a5;
        let b7 = a7 - (a1 >> 2);
        let outs = [
            b0 + b7,
            b2 + b5,
            b4 + b3,
            b6 + b1,
            b6 - b1,
            b4 - b3,
            b2 - b5,
            b0 - b7,
        ];
        for (k, &v) in outs.iter().enumerate() {
            block[i + k * 8] = v as i16;
        }
    }
    // 水平 pass（行）+ 叠加
    for i in 0..8usize {
        let g = |k: usize| i32::from(block[k + i * 8]);
        let a0 = g(0) + g(4);
        let a2 = g(0) - g(4);
        let a4 = (g(2) >> 1) - g(6);
        let a6 = (g(6) >> 1) + g(2);
        let b0 = a0 + a6;
        let b2 = a2 + a4;
        let b4 = a2 - a4;
        let b6 = a0 - a6;
        let a1 = -g(3) + g(5) - g(7) - (g(7) >> 1);
        let a3 = g(1) + g(7) - g(3) - (g(3) >> 1);
        let a5 = -g(1) + g(7) + g(5) + (g(5) >> 1);
        let a7 = g(3) + g(5) + g(1) + (g(1) >> 1);
        let b1 = (a7 >> 2) + a1;
        let b3 = a3 + (a5 >> 2);
        let b5 = (a3 >> 2) - a5;
        let b7 = a7 - (a1 >> 2);
        let outs = [
            b0 + b7,
            b2 + b5,
            b4 + b3,
            b6 + b1,
            b6 - b1,
            b4 - b3,
            b2 - b5,
            b0 - b7,
        ];
        for (k, &v) in outs.iter().enumerate() {
            let d = i32::from(dst[i * stride + k]) + (v >> 6);
            dst[i * stride + k] = d.clamp(0, 255) as u8;
        }
    }
}

/// 8x8 DC-only 叠加（FFmpeg ff_h264_idct8_dc_add）。
pub(crate) fn idct8x8_dc_add(dst: &mut [u8], stride: usize, block: &[i16; 64]) {
    let dc = (i32::from(block[0]) + 32) >> 6;
    for y in 0..8 {
        for x in 0..8 {
            let d = i32::from(dst[y * stride + x]) + dc;
            dst[y * stride + x] = d.clamp(0, 255) as u8;
        }
    }
}

/// 重建一个 8x8 象限：预测 + 残差叠加（nnz 判定与 4x4 路径同构）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn reconstruct_8x8(
    pic: &mut Picture,
    mbx: usize,
    mby: usize,
    quad: usize,
    mode: u8,
    block: &mut [i16; 64],
) {
    let qx = quad & 1;
    let qy = quad >> 1;
    let avail = neighbor_avail(
        pic,
        mbx,
        mby,
        pic.mbs[mby * pic.mb_width + mbx].slice_id,
        false,
    );
    let q = quad_avail(&avail, qx, qy);
    let s = pic.luma_stride();
    let (ox, oy) = (mbx * 16 + qx * 8, mby * 16 + qy * 8);
    let base = oy * s + ox;
    {
        let luma = &pic.luma[..];
        let e = Edge8 {
            luma,
            stride: s,
            ox,
            oy,
        };
        // 直接写入平面（pred 无越界：写 8x8 区域内）
        let mut tmp = [0u8; 64];
        pred_8x8l(mode, &e, &q, &mut tmp, 8);
        for y in 0..8 {
            for x in 0..8 {
                pic.luma[base + y * s + x] = tmp[y * 8 + x];
            }
        }
    }
    if block.iter().all(|&v| v == 0) {
        return;
    }
    if block[1..].iter().all(|&v| v == 0) {
        idct8x8_dc_add(&mut pic.luma[base..], s, block);
    } else {
        idct8x8_add(&mut pic.luma[base..], s, block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 8x8 DC（全 16 平面）预测：滤波边全 16 → DC = (8*16+8*16+8)>>4 = 16。
    #[test]
    fn pred8x8l_dc_flat() {
        let plane = vec![16u8; 24 * 24];
        let e = Edge8 {
            luma: &plane,
            stride: 24,
            ox: 8,
            oy: 8,
        };
        let q = QuadAvail {
            top: true,
            left: true,
            topleft: true,
            topright: true,
        };
        let mut out = [0u8; 64];
        pred_8x8l(2, &e, &q, &mut out, 8);
        assert!(out.iter().all(|&v| v == 16));
    }

    /// idct8：仅 DC=64 → 每样本 +1（(64+32)>>6 = 1）。
    #[test]
    fn idct8_dc_only() {
        let mut block = [0i16; 64];
        block[0] = 64;
        let mut dst = vec![100u8; 64];
        idct8x8_add(&mut dst, 8, &mut block);
        assert!(dst.iter().all(|&v| v == 101), "实际：{:?}", &dst[..8]);
    }
}
