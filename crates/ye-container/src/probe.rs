//! 探查得分（FFmpeg 0..100 语义）。

/// 解封装工厂对输入开头的打分（对齐 FFmpeg `av_probe_input_format` 的
/// 0..100 得分语义）。
///
/// ## 构造规则与阈值语义
///
/// - [`ProbeScore::new`]：`score ∈ 0..=100` → `Some`，`> 100` → `None`；
/// - [`ProbeScore::MINIMUM`]（25）只是注册表 auto-probe 的**录取阈值**
///   （最高分低于它 → `Err(UnknownFormat)`），**不是构造下限**——
///   工厂必须能表达低于阈值的得分（如扩展名启发 10 分），
///   注册表之外的低分构造一律合法；
/// - [`ProbeScore::EXTENSION`]（50）：仅凭扩展名级别的置信；
/// - [`ProbeScore::MAXIMUM`]（100）：魔数完全命中。
///
/// ## derive 清单
///
/// `Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash`——
/// 全字段 `Copy` 值类型；注册表取最高分需要 `Ord`，作分数桶需要 `Hash`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProbeScore(u8);

impl ProbeScore {
    /// 零分：完全无证据（FFmpeg 0 分语义）。
    pub const NONE: ProbeScore = ProbeScore(0);

    /// 录取阈值（25）：注册表 auto-probe 的最低置信，低于此 → `Err(UnknownFormat)`。
    pub const MINIMUM: ProbeScore = ProbeScore(25);

    /// 扩展名级置信（50）：仅凭文件名/扩展名判断。
    pub const EXTENSION: ProbeScore = ProbeScore(50);

    /// 满分（100）：魔数完全命中。
    pub const MAXIMUM: ProbeScore = ProbeScore(100);

    /// 构造：`score ∈ 0..=100` → `Some`，`> 100` → `None`
    /// （超出 FFmpeg 语义域的分数是编程错误，拒绝而非截断）。
    pub fn new(score: u8) -> Option<ProbeScore> {
        if score <= 100 {
            Some(ProbeScore(score))
        } else {
            None
        }
    }

    /// 原始得分（0..=100）。
    pub fn get(self) -> u8 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constants_round_trip_through_new() {
        assert_eq!(ProbeScore::new(0), Some(ProbeScore::NONE));
        assert_eq!(ProbeScore::new(25), Some(ProbeScore::MINIMUM));
        assert_eq!(ProbeScore::new(100), Some(ProbeScore::MAXIMUM));
        assert_eq!(ProbeScore::new(101), None);
        assert_eq!(ProbeScore::new(u8::MAX), None);
    }

    /// MINIMUM 是注册表录取阈值而非构造下限：低于 25 的分数应能原样构造
    /// （如扩展名启发 10 分）。
    #[test]
    fn score_below_minimum_is_constructible() {
        let low = ProbeScore::new(10).expect("10 must construct");
        assert_eq!(low.get(), 10);
        assert!(low < ProbeScore::MINIMUM);
        assert_eq!(ProbeScore::new(24).map(|s| s.get()), Some(24));
    }

    /// Ord：注册表取最高分依赖全序。
    #[test]
    fn ordering() {
        assert!(ProbeScore::NONE < ProbeScore::MINIMUM);
        assert!(ProbeScore::MINIMUM < ProbeScore::EXTENSION);
        assert!(ProbeScore::EXTENSION < ProbeScore::MAXIMUM);
        let mut scores = [ProbeScore::EXTENSION, ProbeScore::NONE, ProbeScore::MAXIMUM];
        scores.sort();
        assert_eq!(
            scores,
            [ProbeScore::NONE, ProbeScore::EXTENSION, ProbeScore::MAXIMUM]
        );
        assert_eq!(ProbeScore::MAXIMUM.get(), 100);
    }
}
