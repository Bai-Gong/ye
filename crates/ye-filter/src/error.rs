//! ye-filter 的错误类型（全库约定：每 crate 自带错误枚举，禁 thiserror）。

/// 本 crate 全部失败模式的枚举。
///
/// ## 变体语义
///
/// - [`Unsupported`](Self::Unsupported)：能力未交付——接口契约已定、
///   实现随里程碑交付（当前为处理图桩，M4），调用即失败，文案含
///   里程碑标注；
/// - [`InvalidArguments`](Self::InvalidArguments)：构造图时引用了不存在
///   的节点或垫索引（外部输入驱动，走 `Result` 不 panic）。
///
/// ## derive 清单
///
/// `Debug, Clone, PartialEq, Eq`（`&'static str` 载荷，无 `String`）；
/// `Display` 与 `std::error::Error` 为手工实现（全库禁 thiserror），
/// Display 文案为英文短句；doc 注释中文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterError {
    /// 能力未交付（桩），携带描述（含里程碑标注）。
    Unsupported(&'static str),
    /// 参数非法（节点/垫索引越界等），携带描述。
    InvalidArguments(&'static str),
}

impl core::fmt::Display for FilterError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FilterError::Unsupported(msg) => write!(f, "unsupported: {msg}"),
            FilterError::InvalidArguments(msg) => write!(f, "invalid arguments: {msg}"),
        }
    }
}

impl std::error::Error for FilterError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_english_short_messages() {
        assert_eq!(
            FilterError::Unsupported("filter graph topological sort not implemented (桩：M4 交付)")
                .to_string(),
            "unsupported: filter graph topological sort not implemented (桩：M4 交付)"
        );
        assert_eq!(
            FilterError::InvalidArguments("pad references unknown node").to_string(),
            "invalid arguments: pad references unknown node"
        );
    }

    #[test]
    fn usable_as_std_error() {
        let boxed: Box<dyn std::error::Error> = Box::new(FilterError::Unsupported("stub"));
        assert_eq!(boxed.to_string(), "unsupported: stub");
        assert!(std::error::Error::source(&*boxed).is_none());
    }
}
