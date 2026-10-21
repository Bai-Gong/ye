//! 错误类型。

use std::fmt;

/// MP4 解封装错误。
#[derive(Debug)]
pub enum Mp4Error {
    /// 底层 I/O 失败。
    Io(std::io::Error),
    /// 结构损坏。`path` 为出问题的 box 路径，如
    /// `"moov.trak[0].mdia.minf.stbl.stsz"`。
    Malformed { path: String, reason: String },
    /// 明确不支持的特性（混合轨、多 sample description 等；
    /// progressive 与 fMP4 均支持，见 lib.rs 头注）。
    Unsupported(&'static str),
    /// 流索引越界。
    InvalidStreamIndex { index: usize, count: usize },
    /// 样例序号越界。
    SampleIndexOutOfRange {
        stream: usize,
        sample: usize,
        count: usize,
    },
}

impl fmt::Display for Mp4Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Mp4Error::Io(e) => write!(f, "I/O 错误：{e}"),
            Mp4Error::Malformed { path, reason } => {
                write!(f, "MP4 结构错误（{path}）：{reason}")
            }
            Mp4Error::Unsupported(what) => write!(f, "不支持的 MP4 特性：{what}"),
            Mp4Error::InvalidStreamIndex { index, count } => {
                write!(f, "流索引越界：{index}（共 {count} 路）")
            }
            Mp4Error::SampleIndexOutOfRange {
                stream,
                sample,
                count,
            } => write!(
                f,
                "样例序号越界：流 {stream} 第 {sample} 个（共 {count} 个）"
            ),
        }
    }
}

impl std::error::Error for Mp4Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Mp4Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Mp4Error {
    fn from(e: std::io::Error) -> Self {
        Mp4Error::Io(e)
    }
}

/// 构造 [`Mp4Error::Malformed`] 的便捷函数。
pub(crate) fn malformed(path: &str, reason: impl Into<String>) -> Mp4Error {
    Mp4Error::Malformed {
        path: path.to_string(),
        reason: reason.into(),
    }
}
