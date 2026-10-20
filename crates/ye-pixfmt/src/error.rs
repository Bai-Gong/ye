//! ye-pixfmt 的错误类型（全库约定：每 crate 自带错误枚举，禁 thiserror）。

use std::fmt;

/// ye-pixfmt 的错误枚举。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq`。
/// 手工实现 [`fmt::Display`] 与 [`std::error::Error`]，Display 文案为英文短句（全库约定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixfmtError {
    /// 帧尺寸为零：`width` 或 `height` 为 0。
    ZeroDimension,
    /// 跨距对齐非法：必须为非零 2 的幂。携带调用方传入的原值。
    BadAlignment(usize),
    /// 声道布局掩码为 0（没有任何声道）。
    EmptyChannelLayout,
}

impl fmt::Display for PixfmtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PixfmtError::ZeroDimension => write!(f, "zero width or height"),
            PixfmtError::BadAlignment(align) => write!(
                f,
                "stride alignment must be a nonzero power of two, got {align}"
            ),
            PixfmtError::EmptyChannelLayout => write!(f, "channel layout mask is empty"),
        }
    }
}

impl std::error::Error for PixfmtError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages_and_error_trait() {
        assert_eq!(
            PixfmtError::ZeroDimension.to_string(),
            "zero width or height"
        );
        assert_eq!(
            PixfmtError::BadAlignment(3).to_string(),
            "stride alignment must be a nonzero power of two, got 3"
        );
        assert_eq!(
            PixfmtError::EmptyChannelLayout.to_string(),
            "channel layout mask is empty"
        );
        // std::error::Error 必须实现（供上层 From 包装使用）
        fn assert_error<T: std::error::Error>(_: &T) {}
        assert_error(&PixfmtError::ZeroDimension);
    }
}
