//! 重标度舍入模式 [`Rounding`] 与内部取整辅助 [`div_round`]。

/// 整数重标度（除法）的舍入模式，语义对齐 FFmpeg 的 `AV_ROUND_*`。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq`（单元枚举，纯值类型）。
///
/// 以商 n/d（d > 0）为例，±1.5 的完整舍入表：
///
/// | 值   | Zero | Inf | Down | Up | NearInf |
/// |------|------|-----|------|----|---------|
/// | +1.5 |   +1 |  +2 |   +1 | +2 |      +2 |
/// | −1.5 |   −1 |  −2 |   −2 | −1 |      −2 |
///
/// - [`Rounding::Zero`]：向零取整（截断），对齐 `AV_ROUND_ZERO`；
/// - [`Rounding::Inf`]：远离零，对齐 `AV_ROUND_INF`；
/// - [`Rounding::Down`]：向 −∞（下取整），对齐 `AV_ROUND_DOWN`；
/// - [`Rounding::Up`]：向 +∞（上取整），对齐 `AV_ROUND_UP`；
/// - [`Rounding::NearInf`]：就近取整、恰为半数（.5）时远离零，
///   对齐 `AV_ROUND_NEAR_INF`，是全库重标度的默认模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rounding {
    /// 向零取整（截断）。
    Zero,
    /// 远离零取整。
    Inf,
    /// 向 −∞ 取整（下取整）。
    Down,
    /// 向 +∞ 取整（上取整）。
    Up,
    /// 就近取整，恰为半数时远离零。
    NearInf,
}

/// 整数除法 + 舍入（crate 内部辅助）：计算 `n / d`（`d > 0`）按 `rounding` 取整。
///
/// 调用方保证 `d > 0`；`|n| ≤ 2^125`、`d ≤ 2^62`（本库全部路径的上限），
/// i128 中间量（`2·|rem|`、商）恒不溢出。无舍入误差以外的失败路径。
pub(crate) fn div_round(n: i128, d: i128, rounding: Rounding) -> i128 {
    debug_assert!(d > 0, "div_round: divisor must be positive");
    let q = n / d; // i128 除法向零截断
    let rem = n % d; // 与 n 同号或为 0
    if rem == 0 {
        return q;
    }
    match rounding {
        Rounding::Zero => q,
        Rounding::Inf => {
            if rem > 0 {
                q + 1
            } else {
                q - 1
            }
        }
        Rounding::Down => {
            // 向 −∞：正数截断即下取整，负数需再减 1
            if rem > 0 { q } else { q - 1 }
        }
        Rounding::Up => {
            // 向 +∞：负数截断即上取整，正数需再加 1
            if rem > 0 { q + 1 } else { q }
        }
        Rounding::NearInf => {
            // 就近、半数远离零：2·|rem| ≥ d 时远离零
            let twice = if rem > 0 { 2 * rem } else { -2 * rem };
            if twice >= d {
                if rem > 0 { q + 1 } else { q - 1 }
            } else {
                q
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ±1.5（n = ±3, d = 2）逐格断言，对齐 AV_ROUND_* 文档语义。
    #[test]
    fn div_round_matches_av_round_table() {
        // +1.5
        assert_eq!(div_round(3, 2, Rounding::Zero), 1);
        assert_eq!(div_round(3, 2, Rounding::Inf), 2);
        assert_eq!(div_round(3, 2, Rounding::Down), 1);
        assert_eq!(div_round(3, 2, Rounding::Up), 2);
        assert_eq!(div_round(3, 2, Rounding::NearInf), 2);
        // −1.5
        assert_eq!(div_round(-3, 2, Rounding::Zero), -1);
        assert_eq!(div_round(-3, 2, Rounding::Inf), -2);
        assert_eq!(div_round(-3, 2, Rounding::Down), -2);
        assert_eq!(div_round(-3, 2, Rounding::Up), -1);
        assert_eq!(div_round(-3, 2, Rounding::NearInf), -2);
        // 非半数就近：+1.4 → 1、−1.6 → −2（全部模式补充抽查）
        assert_eq!(div_round(7, 5, Rounding::NearInf), 1);
        assert_eq!(div_round(-8, 5, Rounding::NearInf), -2);
        // 整除不受模式影响
        for mode in [
            Rounding::Zero,
            Rounding::Inf,
            Rounding::Down,
            Rounding::Up,
            Rounding::NearInf,
        ] {
            assert_eq!(div_round(4, 2, mode), 2);
            assert_eq!(div_round(-4, 2, mode), -2);
        }
    }
}
