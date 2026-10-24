//! 探查得分（FFmpeg 0..100 语义）。

/// 解封装工厂对输入开头的打分（对齐 FFmpeg `av_probe_input_format` 的
/// 0..100 得分语义）。
///
/// ## 构造规则与阈值语义
///
/// - [`ProbeScore::new`]：`score ∈ 0..=100` → `Some`，`> 100` → `None`；
/// - [`ProbeScore::MINIMUM`]（25）只是注册表 auto-probe 的**录取阈值**
///   （最高分低于它 → `Err(UnknownFormat)`），**不是构造下限**——
///   工厂必须能表达低于阈值的得分（如弱启发 10 分），
///   注册表之外的低分构造一律合法；
/// - [`ProbeScore::EXTENSION`]（50）：**部分魔数命中/非权威启发**。
///   探查管线只喂字节窗口、无文件名输入，本档的实际用途是「box 头
///   可信但 brand 未知」一类的半命中证据（mp4 工厂如此使用，2026-10-24
///   评审澄清）——历史名 EXTENSION 保留（对齐 FFmpeg 档位值），真
///   扩展名证据须等管线接入文件名后再用此档。多工厂竞争时本档不应
///   被视作权威证据；当前同分取先注册者的规则下，50 分可压过其他
///   工厂 <50 的真魔数证据——多格式注册时的注册顺序是使用方的显式
///   责任（接入文件名证据时应一并收紧）；
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

    /// 部分魔数命中/非权威启发（50）：探查管线无文件名输入，实际承载
    /// 「box 头可信但内容未知」的半命中证据（见类型文档）。
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
