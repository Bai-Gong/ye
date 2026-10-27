//! ye-filter 的错误类型（全库约定：每 crate 自带错误枚举，禁 thiserror）。

use ye_time::TimeError;

/// 本 crate 全部失败模式的枚举。
///
/// ## 变体语义
///
/// - [`InvalidArguments`](Self::InvalidArguments)：参数非法——构造图时
///   引用了不存在的节点或垫索引、连线两侧媒体类型失配，或执行核消费
///   参数时校验失败（裁剪窗越界、奇数尺寸、帧率零分子/分母、增益
///   非有限等）（外部输入驱动，走 `Result` 不 panic）；
/// - [`Cycle`](Self::Cycle)：处理图存在循环依赖（拓扑排序排空后仍有
///   节点未出队），M3 处理图最小集随 `topological_order` 真实现引入
///   （取代原「桩未交付」的 `Unsupported` 变体——桩已删除，该变体无
///   生产者，一并移除）；
/// - [`Time`](Self::Time)：下层 `ye-time` 错误的包装透出（fps 重采样
///   的时戳精确加减在极端量级下溢出；对齐 `CodecError::Time` /
///   `ContainerError::Time` 先例，配对 `From` 使 `?` 直达）。
///
/// ## derive 清单
///
/// `Debug, Clone, PartialEq, Eq`（`&'static str` 与 `TimeError` 载荷，
/// 均为纯值类型、无 `String`）；`Display` 与 `std::error::Error` 为手工
/// 实现（全库禁 thiserror），Display 文案为英文短句、`source()` 透出
/// 下层；doc 注释中文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterError {
    /// 参数非法（节点/垫索引越界、媒体类型失配、执行核参数校验失败），
    /// 携带描述。
    InvalidArguments(&'static str),
    /// 处理图存在循环依赖（含自环），携带描述。
    Cycle(&'static str),
    /// 下层 ye-time 错误（时戳精确运算溢出等），原样透出。
    Time(TimeError),
}

impl core::fmt::Display for FilterError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FilterError::InvalidArguments(msg) => write!(f, "invalid arguments: {msg}"),
            FilterError::Cycle(msg) => write!(f, "cycle: {msg}"),
            FilterError::Time(err) => write!(f, "time: {err}"),
        }
    }
}

impl std::error::Error for FilterError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FilterError::Time(err) => Some(err),
            _ => None,
        }
    }
}

impl From<TimeError> for FilterError {
    fn from(err: TimeError) -> FilterError {
        FilterError::Time(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_english_short_messages() {
        assert_eq!(
            FilterError::InvalidArguments("pad references unknown node").to_string(),
            "invalid arguments: pad references unknown node"
        );
        assert_eq!(
            FilterError::Cycle("filter graph contains a cycle").to_string(),
            "cycle: filter graph contains a cycle"
        );
        assert_eq!(
            FilterError::Time(TimeError::Overflow).to_string(),
            "time: arithmetic overflow"
        );
    }

    #[test]
    fn usable_as_std_error() {
        let boxed: Box<dyn std::error::Error> = Box::new(FilterError::Cycle("stub"));
        assert_eq!(boxed.to_string(), "cycle: stub");
        assert!(std::error::Error::source(&*boxed).is_none());
        // Time 包装变体 source() 透出下层
        let wrapped: Box<dyn std::error::Error> = Box::new(FilterError::Time(TimeError::Overflow));
        assert!(std::error::Error::source(&*wrapped).is_some());
    }

    #[test]
    fn from_time_error_for_question_mark() {
        // From 成对 ⇒ 下层错误经 `?` 直达 FilterError
        fn lifted() -> Result<(), FilterError> {
            Err(TimeError::Overflow)?;
            Ok(())
        }
        assert_eq!(lifted(), Err(FilterError::Time(TimeError::Overflow)));
    }
}
