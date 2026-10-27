//! 帧内预测模式决策（M3 施工图 T3）——Intra16x16 / Intra4x4 / chroma。
//!
//! 合法性判定与模式改写是解码侧的镜像：解码器解析**原始模式码**后经
//! `rewrite_4x4_modes` / `rewrite_dc_mode` 按邻块可用性改写为实际预测
//! 模式（DC → DC_LEFT/DC_TOP/DC_128 变体、缺侧模式 → 拒收）。编码器
//! 反过来：按可用性筛出合法候选、在**改写后**的模式域里评估代价，
//! 位流里写回原始码（非 DC 模式原始码 = 改写码；luma16x16 DC 写 2、
//! chroma DC 写 0）。
//!
//! - 4x4 块级合法性 [`eff_mode_4x4`]：与 `rewrite_4x4_modes` 的逐块
//!   语义互逆（先顶侧后左侧两遍改写、(0,0) 对角模式的 topleft 门槛），
//!   一致性由穷举对照单测保证（8 可用角 × 16 块位 × 9 模式）；
//! - 16x16/chroma 合法性直接复用 [`crate::macroblock::rewrite_dc_mode`]
//!   （Ok 返回值即改写后模式）。
//!
//! 决策口径（只影响压缩质量，不影响合法性与回环一致性）：
//! - Intra4x4：z 序逐块贪心，块代价 = SATD（残差 Cf 域 l1，见
//!   `transform.rs`）；块间预测依赖直接经**图像平面**传播——决策把
//!   已决块的预测写进本宏块 luma 区域（[`decide_mb_modes`] 因此改动
//!   `pic`），后续块的边沿收集与解码器同走 `gather_edges4`；
//! - Intra16x16 与 chroma：SAD（16x16 / 8x8，预测写临时缓冲）；
//! - I16 vs I4 取舍：`i4_cost + I4_BIAS` 与 `i16_cost` 比较（I16 语法
//!   更省、平坦区天然占优）。
//!
//! 正式编码（`frame.rs`）对每块**重新**预测（边沿含邻块重建值，与
//! 解码器逐位同源）后计算残差，决策期的平面写入只是决策内部状态，
//! 不进入重建。

use crate::encoder::transform::{sad_8x8, sad_16x16, satd_4x4};
use crate::error::H264Error;
use crate::macroblock::{
    MbKind, NeighAvail, P4_DC_128, P4_DC_LEFT, P4_DC_TOP, Picture, Z_ORDER, gather_16x16_edges,
    gather_chroma_edges, gather_edges4, pred_4x4, pred_8x8c, pred_16x16, prev_i4_mode,
    rewrite_4x4_modes, rewrite_dc_mode,
};

/// Intra4x4 相对 Intra16x16 的决策偏置（近似模式比特开销差；经验值）。
/// P 帧的 inter/intra 决策取 `min(i16_cost, i4_cost + I4_BIAS)` 时复用。
pub(crate) const I4_BIAS: i32 = 24;

/// 4x4 块的原始模式 → 改写后模式（不可用 → `None`）。
///
/// 逐块镜像 `rewrite_4x4_modes`：块行 0 按顶侧、块列 0 按左侧两遍改写
/// （第二遍看到第一遍的结果），块 (0,0) 的对角模式（4..=6）额外要求
/// topleft。块内邻居（cx>0 / cy>0）恒可用。
pub(crate) fn eff_mode_4x4(raw: u8, cx: usize, cy: usize, avail: &NeighAvail) -> Option<u8> {
    if raw > 8 {
        return None;
    }
    let mut m = raw;
    if cy == 0 && !avail.top {
        m = match m {
            1 | 8 => m,
            2 => P4_DC_LEFT,
            _ => return None,
        };
    }
    if cx == 0 && !avail.left {
        m = match m {
            0 | 3 | 7 => m,
            2 => P4_DC_TOP,
            P4_DC_LEFT => P4_DC_128,
            _ => return None,
        };
    }
    if cx == 0 && cy == 0 && matches!(raw, 4..=6) && !avail.topleft {
        return None;
    }
    Some(m)
}

/// 16x16 / chroma 的候选模式：返回 (原始码, 改写后模式)（复用
/// [`rewrite_dc_mode`]，不可用模式被剔除）。
pub(crate) fn candidates_dc_mode(
    top: bool,
    left: bool,
    topleft: bool,
    dc_code: u8,
) -> Vec<(u8, u8)> {
    (0..=3u8)
        .filter_map(|raw| {
            rewrite_dc_mode(raw, top, left, topleft, dc_code)
                .ok()
                .map(|eff| (raw, eff))
        })
        .collect()
}

/// 一个宏块的帧内模式决策结果（`*_mode` 为改写后域、`*_raw` 为位流
/// 原始码）。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq`（纯小值结果包）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MbModes {
    /// true = Intra16x16，false = Intra4x4。
    pub use_i16: bool,
    /// Intra16x16 预测模式（改写后）。
    pub i16_mode: u8,
    /// Intra16x16 原始码（mb_type 用）。
    pub i16_raw: u8,
    /// Intra4x4 预测模式（光栅 16 块，改写后；邻块 prev/rem 读取域）。
    pub i4_modes: [u8; 16],
    /// Intra4x4 原始码（光栅 16 块，prev/rem 写出域）。
    pub i4_raws: [u8; 16],
    /// chroma 预测模式（改写后）。
    pub chroma_mode: u8,
    /// chroma 原始码。
    pub chroma_raw: u8,
    // ---- 代价（T4 P 帧的 inter/intra 模式决策消费；口径见各字段）----
    /// Intra16x16 luma SAD（所选模式）。
    pub i16_cost: i32,
    /// Intra4x4 luma SATD 总和（z 序逐块贪心）。
    pub i4_cost: i32,
    /// chroma 两平面 SAD 之和（所选模式）。
    pub chroma_cost: i32,
}

/// 帧内模式决策入口。源面：luma 16x16（`src_luma`，行距 `src_stride`）
/// 与 chroma 各 8x8（`src_cb`/`src_cr`，行距 `src_c_stride`）。
///
/// **副作用**：Intra4x4 决策把逐块预测写进 `pic` 的本宏块 luma 区域
/// （决策内部状态；调用方无论最终选哪条路径都会重写该区域）。
///
/// # Errors
/// 可用性下无合法 4x4 模式（理论不可达——DC 恒合法，防御路径）或
/// 模式改写失败 → [`H264Error::InvalidData`]。
#[allow(clippy::too_many_arguments)] // 决策入参组，聚类反而伤可读性
pub(crate) fn decide_mb_modes(
    pic: &mut Picture,
    mbx: usize,
    mby: usize,
    avail: &NeighAvail,
    src_luma: &[u8],
    src_stride: usize,
    src_cb: &[u8],
    src_cr: &[u8],
    src_c_stride: usize,
) -> Result<MbModes, H264Error> {
    // ---- chroma 模式（两平面 SAD 之和；预测写临时缓冲）----
    let cb_edges = gather_chroma_edges(&pic.cb, pic.chroma_stride(), mbx, mby, avail);
    let cr_edges = gather_chroma_edges(&pic.cr, pic.chroma_stride(), mbx, mby, avail);
    let mut chroma_raw = 0u8;
    let mut chroma_cost = i32::MAX;
    for (raw, eff) in candidates_dc_mode(avail.top, avail.left, avail.topleft, 0) {
        let mut cost = 0i32;
        for (edges, src) in [(cb_edges, src_cb), (cr_edges, src_cr)] {
            let mut pred = [0u8; 64];
            pred_8x8c(eff, &edges.0, &edges.1, edges.2, &mut pred, 8);
            cost += sad_8x8(src, src_c_stride, &pred);
        }
        if cost < chroma_cost {
            chroma_cost = cost;
            chroma_raw = raw;
        }
    }
    let chroma_mode = rewrite_dc_mode(chroma_raw, avail.top, avail.left, avail.topleft, 0)?;

    // ---- Intra16x16（SAD；预测写临时缓冲，不落平面）----
    let edges16 = gather_16x16_edges(pic, mbx, mby, avail);
    let mut i16_raw = 2u8;
    let mut i16_cost = i32::MAX;
    for (raw, eff) in candidates_dc_mode(avail.top, avail.left, avail.topleft, 2) {
        let mut pred = [0u8; 256];
        pred_16x16(eff, &edges16.0, &edges16.1, edges16.2, &mut pred, 16);
        let cost = sad_16x16(src_luma, src_stride, &pred);
        if cost < i16_cost {
            i16_cost = cost;
            i16_raw = raw;
        }
    }
    let i16_mode = rewrite_dc_mode(i16_raw, avail.top, avail.left, avail.topleft, 2)?;

    // ---- Intra4x4：z 序逐块贪心（SATD），预测落平面传播依赖 ----
    let ys = pic.luma_stride();
    // prev 链的跨宏块来源（邻块**改写后**存储模式；非 I4 邻块贡献 2）
    let mb_xy = mby * pic.mb_width + mbx;
    let left_nb = (avail.left && mbx > 0).then(|| {
        let m = &pic.mbs[mb_xy - 1];
        (m.kind, m.i4_modes)
    });
    let top_nb = (avail.top && mby > 0).then(|| {
        let m = &pic.mbs[mb_xy - pic.mb_width];
        (m.kind, m.i4_modes)
    });
    let nb_mode = |nb: Option<(MbKind, [u8; 16])>, slot: usize| -> Option<u8> {
        nb.map(|(kind, modes)| {
            if kind == MbKind::Intra4x4 {
                modes[slot]
            } else {
                2
            }
        })
    };
    let mut i4_raws = [0u8; 16];
    let mut i4_cost = 0i32;
    for &b in Z_ORDER.iter() {
        let (cx, cy) = (b % 4, b / 4);
        // prev（解析域链：块内取原始码、跨宏块取存储模式）
        let prev = {
            let l = if cx > 0 {
                Some(i4_raws[b - 1])
            } else {
                nb_mode(left_nb, 3 + cy * 4)
            };
            let t = if cy > 0 {
                Some(i4_raws[b - 4])
            } else {
                nb_mode(top_nb, cx + 12)
            };
            prev_i4_mode(l, t)
        };
        let edges = gather_edges4(pic, mbx, mby, b, avail);
        let mut best = (0u8, i32::MAX, [0u8; 16]);
        for raw in 0..=8u8 {
            // prev ∈ DC 变体域（9..=11）时 rem = raw ≤ 7 才可解（u(3)），
            // raw == 8（HU）不可达——从候选中剔除
            if prev > 8 && raw == 8 {
                continue;
            }
            let Some(eff) = eff_mode_4x4(raw, cx, cy, avail) else {
                continue;
            };
            let mut out = [0u8; 16];
            pred_4x4(eff, &edges, &mut out);
            let mut diff = [0i16; 16];
            for k in 0..16usize {
                // src_luma 为本宏块左上角起的尾段切片（相对索引）
                let s = src_luma[(cy * 4 + k / 4) * src_stride + cx * 4 + k % 4];
                diff[k] = i16::from(s) - i16::from(out[k]);
            }
            let cost = satd_4x4(&diff);
            if cost < best.1 {
                best = (raw, cost, out);
            }
        }
        let (raw, _, out) = best;
        if best.1 == i32::MAX {
            return Err(H264Error::InvalidData(
                "no legal intra4x4 mode under availability".into(),
            ));
        }
        i4_raws[b] = raw;
        i4_cost += best.1;
        // 已决块预测落平面：后续块的块内邻居取值与解码器同源
        let blk = mby * 16 * ys + mbx * 16 + (cy * 4) * ys + cx * 4;
        for y in 0..4usize {
            for x in 0..4usize {
                pic.luma[blk + y * ys + x] = out[y * 4 + x];
            }
        }
    }
    let mut i4_modes = i4_raws;
    rewrite_4x4_modes(&mut i4_modes, avail.top, avail.left, avail.topleft)?;

    let use_i16 = i16_cost <= i4_cost + I4_BIAS;
    Ok(MbModes {
        use_i16,
        i16_mode,
        i16_raw,
        i4_modes,
        i4_raws,
        chroma_mode,
        chroma_raw,
        i16_cost,
        i4_cost,
        chroma_cost,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macroblock::{P8_DC_128, P8_DC_TOP, rewrite_4x4_modes};

    /// eff_mode_4x4 与解码侧 rewrite_4x4_modes 的全组合互逆对照
    ///（8 可用角 × 16 块位 × 9 原始模式）。整阵改写对本块位置的产出
    /// 与本块判定一致；整阵失败当且仅当失败点是本块（同 raw 同位置）。
    #[test]
    fn eff_mode_4x4_mirrors_rewrite_exhaustive() {
        for top in [false, true] {
            for left in [false, true] {
                for topleft in [false, true] {
                    let avail = NeighAvail {
                        top,
                        left,
                        topleft,
                        er: true,
                    };
                    for b in 0..16usize {
                        for raw in 0..=8u8 {
                            let per_block = eff_mode_4x4(raw, b % 4, b / 4, &avail);
                            let mut modes = [raw; 16];
                            if rewrite_4x4_modes(&mut modes, top, left, topleft).is_ok() {
                                // 整阵改写成功时：本块位置的产出必须与
                                // 逐块判定一致（非改写位置保持 raw）
                                assert_eq!(
                                    per_block,
                                    Some(modes[b]),
                                    "top{top} left{left} tl{topleft} b{b} raw{raw}"
                                );
                            } else {
                                // 整阵失败：失败点必在本块的改写敏感位置
                                //（其余块同 raw 不触发）——若本块不敏感则
                                // 逐块判定也不得失败
                                let sensitive = (b / 4 == 0 && !top && !matches!(raw, 1 | 8 | 2))
                                    || (b % 4 == 0 && !left && {
                                        let m0 = if b / 4 == 0 && !top {
                                            match raw {
                                                1 | 8 => raw,
                                                2 => P4_DC_LEFT,
                                                _ => raw,
                                            }
                                        } else {
                                            raw
                                        };
                                        !matches!(m0, 0 | 3 | 7 | 2 | P4_DC_LEFT)
                                    })
                                    || (b == 0 && !topleft && matches!(raw, 4..=6));
                                if !sensitive {
                                    assert!(
                                        per_block.is_some(),
                                        "top{top} left{left} tl{topleft} b{b} raw{raw}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// candidates_dc_mode：边界可用性下的候选集（角点只有 DC）。
    #[test]
    fn dc_mode_candidates_by_availability() {
        // I16 族（dc_code=2）：无任何邻块 → 只有 DC → P8_DC_128
        let none = candidates_dc_mode(false, false, false, 2);
        assert_eq!(none, vec![(2u8, P8_DC_128)]);
        // 全可用 → 4 模式
        let full = candidates_dc_mode(true, true, true, 2);
        assert_eq!(full.len(), 4);
        // 仅顶：V(0)、DC(2)→DC_TOP
        let top_only = candidates_dc_mode(true, false, false, 2);
        assert_eq!(top_only, vec![(0u8, 0u8), (2u8, P8_DC_TOP)]);
        // 仅左：H(1)、DC(2)→DC_LEFT
        let left_only = candidates_dc_mode(false, true, false, 2);
        assert_eq!(
            left_only,
            vec![(1u8, 1u8), (2u8, crate::macroblock::P8_DC_LEFT)]
        );
        // chroma 族（dc_code=0）：无顶无左 → DC(0)→128
        let cnone = candidates_dc_mode(false, false, false, 0);
        assert_eq!(cnone, vec![(0u8, P8_DC_128)]);
        // Plane 恒需 topleft（16x16/chroma 平面预测读角点）
        assert!(
            !candidates_dc_mode(true, true, false, 2)
                .iter()
                .any(|&(r, _)| r == 3)
        );
        assert!(
            candidates_dc_mode(true, true, true, 2)
                .iter()
                .any(|&(r, _)| r == 3)
        );
    }
}
