//! 运动估计（M3 施工图 T4）：整像素 SAD 穷举搜索（范围可配）+
//! 半像素细化。
//!
//! 决策与最终运动补偿**共用同一插值核**：所有候选 MV 的预测块一律经
//! [`mc::mc_luma_put`]/[`mc::mc_chroma_put`]（`mc.rs` 冻结接口）生成——
//! 越界读的边缘复制、6 抽头半像素、双线性 chroma 与解码侧逐位一致，
//! 「决策看好的块」与「重建写出的块」因此是同一个块（编码器运动
//! 补偿残差复用 `mc.rs:114+` 插值核的硬要求）。整像素候选走同一入口
//! 还保证 SAD 的钳位语义与重建完全相同（绝无决策/重建两套取数路径）。
//!
//! 搜索策略（只影响压缩质量，不影响回环一致性）：
//! - 整像素：以 MV=(0,0) 为中心的 `[-range, +range]²` 穷举 SAD，取
//!   严格更小者（同值先到先得，扫描序 = 行主序，确定性）；
//! - 半像素：整像素最优点周围 3x3 半像素格（含中心）再穷举，胜出即
//!   取半像素 MV（QPEL 偶数分量）。
//!
//! 未做四分之一像素细化与预测子中心搜索（T4 收紧范围，见 m3-plan §5）。

use crate::error::H264Error;
use crate::macroblock::Picture;
use crate::mc;

/// 搜索范围上限（i32 界；过大的穷举范围在标量参考实现下无意义）。
pub(crate) const MAX_SEARCH_RANGE: i32 = 64;

/// 校验搜索范围（0..=[`MAX_SEARCH_RANGE`]）。
pub(crate) fn check_range(range: i32) -> Result<(), H264Error> {
    if !(0..=MAX_SEARCH_RANGE).contains(&range) {
        return Err(H264Error::InvalidArguments(
            "search range must be in 0..=64 pixels",
        ));
    }
    Ok(())
}

/// 源 16x16 块与预测块的 SAD（`src` 为本宏块左上角起的尾段切片，
/// 行距 `src_stride`；`pred` 为 16 行距紧凑块）。
pub(crate) fn sad_16x16_vs_pred(src: &[u8], src_stride: usize, pred: &[u8; 256]) -> i32 {
    let mut sad = 0i32;
    for y in 0..16usize {
        for x in 0..16usize {
            sad += i32::from(src[y * src_stride + x]).abs_diff(i32::from(pred[y * 16 + x])) as i32;
        }
    }
    sad
}

/// 经 mc 核取一个 16x16 luma 预测块（`(px, py)` 为参考图上的**整像素**
/// 绝对坐标；越界由核内边缘复制钳位）。
fn mc_block(refp: &Picture, px: i64, py: i64, out: &mut [u8; 256]) {
    mc::mc_luma_put(
        out,
        16,
        &refp.luma,
        refp.luma_stride(),
        refp.mb_width * 16,
        refp.mb_height * 16,
        px * 4,
        py * 4,
        16,
        16,
    );
}

/// 运动估计结果：QPEL MV（相对宏块原点）与 luma SAD。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq`（纯小值结果对）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MvCost {
    /// QPEL 运动矢量（x, y）。
    pub mv: [i16; 2],
    /// 最优候选的 luma SAD。
    pub sad: i32,
}

/// 整像素穷举 + 半像素细化。
///
/// `src`/`src_stride`：当前帧本宏块的 luma 源块；`(mbx, mby)`：宏块
/// 光栅坐标（推参考图绝对坐标）。
pub(crate) fn search_motion(
    refp: &Picture,
    mbx: usize,
    mby: usize,
    src: &[u8],
    src_stride: usize,
    range: i32,
) -> MvCost {
    let ox = (mbx * 16) as i64;
    let oy = (mby * 16) as i64;
    // ---- 整像素穷举（行主序扫描，严格更小者替换——同值先到先得）----
    let mut best = (0i64, 0i64, i32::MAX);
    let mut buf = [0u8; 256];
    for dy in -i64::from(range)..=i64::from(range) {
        for dx in -i64::from(range)..=i64::from(range) {
            mc_block(refp, ox + dx, oy + dy, &mut buf);
            let sad = sad_16x16_vs_pred(src, src_stride, &buf);
            if sad < best.2 {
                best = (dx, dy, sad);
            }
        }
    }
    // ---- 半像素细化：3x3 半像素格（含整像素中心）----
    // 初值 = 整像素最优（细化仅在严格更小时改写）
    let mut best_qpel = [(best.0 * 4) as i16, (best.1 * 4) as i16];
    let mut best_sad = best.2;
    for hy in -1i32..=1 {
        for hx in -1i32..=1 {
            let qx = (ox + best.0) * 4 + i64::from(hx) * 2;
            let qy = (oy + best.1) * 4 + i64::from(hy) * 2;
            mc::mc_luma_put(
                &mut buf,
                16,
                &refp.luma,
                refp.luma_stride(),
                refp.mb_width * 16,
                refp.mb_height * 16,
                qx,
                qy,
                16,
                16,
            );
            let sad = sad_16x16_vs_pred(src, src_stride, &buf);
            if sad < best_sad {
                best_sad = sad;
                best_qpel = [(qx - ox * 4) as i16, (qy - oy * 4) as i16];
            }
        }
    }
    MvCost {
        mv: best_qpel,
        sad: best_sad,
    }
}

/// 最优 MV 的 chroma 双平面 SAD（模式决策的 inter 代价分量；预测块
/// 同样经 mc 核生成，保证与重建一致）。
pub(crate) fn chroma_sad(
    refp: &Picture,
    mbx: usize,
    mby: usize,
    mv: [i16; 2],
    src_cb: &[u8],
    src_cr: &[u8],
    src_stride: usize,
) -> i32 {
    // qpel 绝对坐标 = luma QPEL 坐标（数值同源：mc_chroma 以 1/8 单位
    // 解释，mc_partition 对两平面传同一 mv——此处同款换算）
    let qx = (mbx * 16) as i64 * 4 + i64::from(mv[0]);
    let qy = (mby * 16) as i64 * 4 + i64::from(mv[1]);
    let mut buf = [0u8; 64];
    let mut sad = 0i32;
    for (srcp, plane) in [(src_cb, &refp.cb), (src_cr, &refp.cr)] {
        mc::mc_chroma_put(
            &mut buf,
            8,
            plane,
            refp.chroma_stride(),
            refp.mb_width * 8,
            refp.mb_height * 8,
            qx,
            qy,
            8,
            8,
        );
        for y in 0..8usize {
            for x in 0..8usize {
                sad +=
                    i32::from(srcp[y * src_stride + x]).abs_diff(i32::from(buf[y * 8 + x])) as i32;
            }
        }
    }
    sad
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macroblock::MbKind;

    /// 造一个 4x3 宏块（64x48）的参考图：luma 由调用方给定；chroma
    /// 平坦。宏块全标 intra 已解码（MV 预测的邻块语义：intra → UNUSED）。
    fn ref_pic(luma: Vec<u8>) -> Picture {
        let mut p = Picture::new(4, 3, 64, 48);
        p.luma.copy_from_slice(&luma);
        for v in p.cb.iter_mut() {
            *v = 96;
        }
        for v in p.cr.iter_mut() {
            *v = 160;
        }
        for mb in p.mbs.iter_mut() {
            mb.decoded = true;
            mb.kind = MbKind::Intra16x16;
        }
        p
    }

    /// 取 src 宏块（左上角起、行距 64 的尾段切片）。
    fn mb_src(luma: &[u8], mbx: usize, mby: usize) -> &[u8] {
        &luma[mby * 16 * 64 + mbx * 16..]
    }

    /// 整数平移：冲激信号（平坦底 + 单点）左移 2、上移 2（源冲激在
    /// 参考冲激的左上方向）——除正确位移外任何候选都让冲激错位
    /// （SAD > 0），最优点唯一。取宏块 (2,1)：搜索窗内冲激始终可见，
    /// 无钳位污染。MV 语义：预测读 ref(x+mv) ⇒ 内容左移 ⇔ MV=(+2,+2)。
    #[test]
    fn integer_translation_found_with_zero_sad() {
        let mut base = vec![100u8; 64 * 48];
        base[26 * 64 + 42] = 152; // 参考冲激 (42, 26)
        let refp = ref_pic(base);
        let mut src = vec![100u8; 64 * 48];
        src[24 * 64 + 40] = 152; // 源冲激 = 参考冲激移 (−2,−2)
        let r = search_motion(&refp, 2, 1, mb_src(&src, 2, 1), 64, 8);
        assert_eq!(r.mv, [8, 8]); // (+2,+2) 像素 = 8 qpel
        assert_eq!(r.sad, 0, "整数平移的残差应精确为零");
    }

    /// 半像素细化：源块 = 参考经 mc 核在 (−0.5, +0.5) 像素处的插值
    /// （冲激 + 常数底的非自相似信号——任何整数平移都无法重演 6 抽头
    /// 的展布），细化必须在半像素格点找到零 SAD。
    #[test]
    fn halfpel_refinement_reaches_zero() {
        let mut base = vec![100u8; 64 * 48];
        for y in 0..12usize {
            for x in 0..12usize {
                base[(20 + y) * 64 + 20 + x] = 100 + (((x * 7 + y * 11) % 13) * 6) as u8;
            }
        }
        let refp = ref_pic(base);
        // 源 = 参考在 (24.5, 24.5)（宏块 (1,1) 原点 (16,16) 的
        // (+0.5,+0.5) 半像素位）的 mc 输出
        let mut src_mb = [0u8; 256];
        mc::mc_luma_put(
            &mut src_mb,
            16,
            &refp.luma,
            64,
            64,
            48,
            (16 * 4) + 2,
            (16 * 4) + 2,
            16,
            16,
        );
        let r = search_motion(&refp, 1, 1, &src_mb, 16, 4);
        assert_eq!(r.mv, [2, 2]);
        assert_eq!(r.sad, 0, "半像素候选应精确命中核生成的源");
    }

    /// range=0：只允许 (0,0) 整像素及其半像素邻域。
    #[test]
    fn zero_range_confines_search_to_origin() {
        let base: Vec<u8> = (0..48 * 64)
            .map(|i| 50 + (i % 64) as u8 + 2 * (i / 64) as u8)
            .collect();
        let refp = ref_pic(base.clone());
        // 大幅平移的源在原点邻域找不到好匹配——MV 被限制在 ±2 qpel
        let mut src = vec![0u8; 64 * 48];
        for y in 0..48usize {
            for x in 0..64usize {
                let sx = (x + 6).min(63);
                let sy = (y + 5).min(47);
                src[y * 64 + x] = base[sy * 64 + sx];
            }
        }
        let r = search_motion(&refp, 1, 1, mb_src(&src, 1, 1), 64, 0);
        assert!(r.mv[0].abs() <= 2 && r.mv[1].abs() <= 2, "mv {:?}", r.mv);
        assert!(r.sad > 0);
        // 范围校验
        assert!(check_range(0).is_ok());
        assert!(check_range(65).is_err());
    }

    /// chroma SAD：参考标记左移 1 chroma 像素（MV=(8,0) qpel ⇔ 预测
    /// 读 ref(x+1)）→ 对齐时 SAD 0；mv=0 错位 SAD > 0。标记放窗口
    /// 内部，避免边缘钳位干扰。
    #[test]
    fn chroma_sad_tracks_chroma_motion() {
        let base = vec![100u8; 64 * 48];
        let mut refp = ref_pic(base);
        for y in 2..6usize {
            for x in 2..6usize {
                refp.cb[y * 32 + x] = 200;
            }
        }
        // 源标记 = 参考标记左移 1（cols 1..4）
        let mut cb = vec![96u8; 32 * 24];
        for y in 2..6usize {
            for x in 1..5usize {
                cb[y * 32 + x] = 200;
            }
        }
        let cr = vec![160u8; 32 * 24];
        let zero = chroma_sad(&refp, 0, 0, [8, 0], &cb, &cr, 32);
        assert_eq!(zero, 0, "chroma 整数对齐 → SAD 0");
        let mis = chroma_sad(&refp, 0, 0, [0, 0], &cb, &cr, 32);
        assert!(mis > 0, "chroma 错位 → SAD > 0");
    }

    /// 确定性：同输入两次搜索结果逐位相同。
    #[test]
    fn search_is_deterministic() {
        let base: Vec<u8> = (0..48 * 64).map(|i| 70 + (i % 64) as u8).collect();
        let refp = ref_pic(base.clone());
        let src: Vec<u8> = base.iter().map(|&v| v.saturating_add(3)).collect();
        let a = search_motion(&refp, 2, 2, mb_src(&src, 2, 2), 64, 8);
        let b = search_motion(&refp, 2, 2, mb_src(&src, 2, 2), 64, 8);
        assert_eq!(a, b);
    }
}
