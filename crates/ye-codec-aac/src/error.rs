//! ye-codec-aac 的错误类型（全库约定：每 crate 自带错误枚举，禁 thiserror）。

use crate::filterbank::FilterbankError;

/// 本 crate 全部失败模式的枚举。
///
/// ## 变体语义
///
/// - [`Unsupported`](Self::Unsupported)：形态超出 AAC-LC 解码范围
///   （PCE/多声道配置、CCE、PNS 噪声码本、SBR/HE-AAC、Main/LTP 等
///   predictor、ER/BSAC/ELD 等 object type）——如实报错，绝不静默错帧；
/// - [`InvalidData`](Self::InvalidData)：码流数据不合法（位读取越界、
///   码本索引非法、section 越界等）；
/// - [`InvalidArguments`](Self::InvalidArguments)：带外参数非法
///   （extradata 缺失或不可识别等）；
/// - [`Filterbank`](Self::Filterbank)：滤波器组错误原样透出（配套
///   `From` 实现，`?` 直接传递，不吞下层错误）。
///
/// ## derive 清单
///
/// `Debug, Clone, PartialEq, Eq`；`Display` 与 `std::error::Error`
/// 手工实现（全库禁 thiserror），Display 文案为英文短句；doc 注释中文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AacError {
    /// 形态不支持，携带描述（如实报错，不静默错帧）。
    Unsupported(String),
    /// 码流数据不合法，携带描述。
    InvalidData(String),
    /// 带外参数非法，携带描述。
    InvalidArguments(String),
    /// 滤波器组错误（下层 [`FilterbankError`] 原样包装）。
    Filterbank(FilterbankError),
}

impl core::fmt::Display for AacError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            AacError::Unsupported(msg) => write!(f, "unsupported aac stream: {msg}"),
            AacError::InvalidData(msg) => write!(f, "invalid aac data: {msg}"),
            AacError::InvalidArguments(msg) => write!(f, "invalid aac arguments: {msg}"),
            AacError::Filterbank(err) => write!(f, "aac filterbank: {err}"),
        }
    }
}

impl std::error::Error for AacError {
    /// 包装变体透出下层错误源。
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            AacError::Filterbank(err) => Some(err),
            _ => None,
        }
    }
}

impl From<FilterbankError> for AacError {
    fn from(err: FilterbankError) -> AacError {
        AacError::Filterbank(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_short_messages() {
        assert_eq!(
            AacError::Unsupported("PNS noise codebook".into()).to_string(),
            "unsupported aac stream: PNS noise codebook"
        );
        assert_eq!(
            AacError::InvalidData("bit overrun".into()).to_string(),
            "invalid aac data: bit overrun"
        );
        assert_eq!(
            AacError::InvalidArguments("extradata missing".into()).to_string(),
            "invalid aac arguments: extradata missing"
        );
        let err = AacError::Filterbank(FilterbankError::BadCoeffsLen {
            expect: 1024,
            got: 512,
        });
        assert!(err.to_string().contains("1024"));
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn from_filterbank_error() {
        let err: AacError = FilterbankError::UnsupportedWindowLen(512).into();
        assert_eq!(
            err,
            AacError::Filterbank(FilterbankError::UnsupportedWindowLen(512))
        );
    }
}
