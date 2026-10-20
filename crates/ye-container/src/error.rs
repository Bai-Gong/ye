//! ye-container 的错误类型。

use ye_time::TimeError;

/// 封装/解封装全部失败模式的枚举。
///
/// ## derive 清单
///
/// 仅 `Debug`（显式例外于 derive 兜底政策 (d)：含 [`std::io::Error`] 变体，
/// `io::Error` 既非 `Clone` 也非 `PartialEq`，无法参加对应 derive）——
/// **测试断言一律用 `matches!`，不得 `assert_eq`**。
/// `Display` 与 [`std::error::Error`] 为手工实现（全库禁 thiserror），
/// Display 文案为英文短句；doc 注释中文。
#[derive(Debug)]
pub enum ContainerError {
    /// 底层 IO 失败（输入/输出流读写错误）。
    Io(std::io::Error),
    /// 探查打分不足或点名格式未注册，无法识别容器格式。
    UnknownFormat,
    /// 码流数据非法（携带人类可读细节，如出错字段的上下文）。
    InvalidData(String),
    /// 调用顺序或状态违约（如 `write_packet` 前未 `write_header`），
    /// 携带静态说明。
    InvalidState(&'static str),
    /// 流索引未出现在当前流表中。
    StreamNotFound(u32),
    /// 时基/时戳换算失败（[`TimeError`] 的包装变体，成对 [`From`]）。
    Time(TimeError),
}

impl core::fmt::Display for ContainerError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ContainerError::Io(e) => write!(f, "io error: {e}"),
            ContainerError::UnknownFormat => f.write_str("unknown container format"),
            ContainerError::InvalidData(detail) => write!(f, "invalid data: {detail}"),
            ContainerError::InvalidState(what) => write!(f, "invalid state: {what}"),
            ContainerError::StreamNotFound(index) => write!(f, "stream not found: {index}"),
            ContainerError::Time(e) => write!(f, "time error: {e}"),
        }
    }
}

impl std::error::Error for ContainerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ContainerError::Io(e) => Some(e),
            ContainerError::Time(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ContainerError {
    fn from(e: std::io::Error) -> Self {
        ContainerError::Io(e)
    }
}

impl From<TimeError> for ContainerError {
    fn from(e: TimeError) -> Self {
        ContainerError::Time(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ContainerError 无 PartialEq/Clone：全部断言用 matches!（derive 兜底政策 (d) 例外）。

    #[test]
    fn from_io_error() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such file");
        let err: ContainerError = io_err.into();
        assert!(matches!(err, ContainerError::Io(_)));
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn from_time_error() {
        let err: ContainerError = TimeError::Overflow.into();
        assert!(matches!(err, ContainerError::Time(TimeError::Overflow)));
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn display_is_english_short() {
        assert_eq!(
            ContainerError::UnknownFormat.to_string(),
            "unknown container format"
        );
        assert_eq!(
            ContainerError::InvalidState("write_header first").to_string(),
            "invalid state: write_header first"
        );
        assert_eq!(
            ContainerError::StreamNotFound(3).to_string(),
            "stream not found: 3"
        );
        assert_eq!(
            ContainerError::InvalidData("bad box size".to_string()).to_string(),
            "invalid data: bad box size"
        );
    }

    #[test]
    fn source_chains_to_lower_layer() {
        let err = ContainerError::Time(TimeError::DivideByZero);
        assert!(std::error::Error::source(&err).is_some());
        let io_err = std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof");
        let err = ContainerError::Io(io_err);
        assert!(std::error::Error::source(&err).is_some());
        // 无下层错误源
        assert!(std::error::Error::source(&ContainerError::UnknownFormat).is_none());
    }
}
