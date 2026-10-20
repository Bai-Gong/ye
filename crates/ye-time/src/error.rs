//! ye-time 的错误类型。

/// [`crate::Rational`] 与 [`crate::Timestamp`] 全部失败模式的枚举。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq`（全单元变体，纯值类型）。
/// `Display` 与 `std::error::Error` 为手工实现（全库禁 thiserror），
/// Display 文案为英文短句；doc 注释中文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeError {
    /// 分母非法：[`crate::Rational::try_new`] 的 `den == 0`，
    /// 或 [`crate::Rational::from_f64`] 的 `max_den <= 0`。
    InvalidDenominator,
    /// 除以零值：有理数除法/求倒的被除数为 `0/1`，
    /// 或重标度的目标时基为 `0/1`、帧率为 `0/1`。
    DivideByZero,
    /// 中间量（i128）归约后无法落回 i32/i64 表示范围。
    Overflow,
    /// f64 输入非有限（NaN 或 ±∞）。
    NotFinite,
}

impl core::fmt::Display for TimeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            TimeError::InvalidDenominator => "denominator is zero or non-positive",
            TimeError::DivideByZero => "division by zero",
            TimeError::Overflow => "arithmetic overflow",
            TimeError::NotFinite => "floating-point value is not finite",
        };
        f.write_str(s)
    }
}

impl std::error::Error for TimeError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_is_english_short() {
        assert_eq!(
            TimeError::InvalidDenominator.to_string(),
            "denominator is zero or non-positive"
        );
        assert_eq!(TimeError::DivideByZero.to_string(), "division by zero");
        assert_eq!(TimeError::Overflow.to_string(), "arithmetic overflow");
        assert_eq!(
            TimeError::NotFinite.to_string(),
            "floating-point value is not finite"
        );
    }

    #[test]
    fn usable_as_std_error() {
        // 手工 impl 的 std::error::Error 可装箱、可作错误源
        let boxed: Box<dyn std::error::Error> = Box::new(TimeError::Overflow);
        assert_eq!(boxed.to_string(), "arithmetic overflow");
    }
}
