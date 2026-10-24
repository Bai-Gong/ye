//! ye-codec 的错误类型（全库约定：每 crate 自带错误枚举，禁 thiserror）。

use ye_pixfmt::PixfmtError;
use ye_time::TimeError;

use crate::media::CodecId;

/// 本 crate 全部失败模式的枚举。
///
/// ## 变体语义
///
/// - [`UnsupportedCodec`](Self::UnsupportedCodec)：注册表有工厂但该 codec
///   不被当前实现支持（如工厂按 feature 裁剪后收到禁用项）；
/// - [`UnsupportedStub`](Self::UnsupportedStub)：接口桩调用——契约已定、
///   实现随 M2/M3 里程碑交付（见 `stubs` 模块），文案含里程碑标注；
/// - [`NotRegistered`](Self::NotRegistered)：注册表中根本没有该 codec 的工厂；
/// - [`InvalidData`](Self::InvalidData)：码流数据不合法（来自文件/网络的输入，
///   一律 `Result`，绝不 panic）；
/// - [`InvalidState`](Self::InvalidState) / [`InvalidArguments`](Self::InvalidArguments)：
///   控制流误用（flush 排空后再 feed、参数非法）——这类误用常由外部输入驱动，
///   走 `Result` 而非 panic；
/// - [`BufferMismatch`](Self::BufferMismatch)：外部缓冲长度与布局不符；
/// - [`Time`](Self::Time) / [`Pixfmt`](Self::Pixfmt)：下层错误原样包装
///   （配套 `From` 实现，`?` 直接透传，不许吞掉下层错误类型）。
///
/// ## derive 清单
///
/// `Debug, Clone, PartialEq, Eq`（含 `String` 变体故不 `Copy`）；
/// `Display` 与 `std::error::Error` 为手工实现（全库禁 thiserror），
/// Display 文案为英文短句；doc 注释中文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// 该编解码器不受支持（有注册但被裁剪/拒绝）。
    UnsupportedCodec(CodecId),
    /// 接口桩调用（实现随 M2/M3 里程碑交付），携带含里程碑标注的文案。
    UnsupportedStub(&'static str),
    /// 该编解码器未在 [`crate::CodecRegistry`] 注册。
    NotRegistered(CodecId),
    /// 码流数据不合法，携带描述。
    InvalidData(String),
    /// 控制流误用（如 flush 排空后再 feed），携带描述。
    InvalidState(&'static str),
    /// 参数非法（如 `sample_rate == 0`），携带描述。
    InvalidArguments(&'static str),
    /// 外部缓冲长度与期望布局不符。
    BufferMismatch {
        /// 期望的字节数。
        expected: usize,
        /// 实际的字节数。
        actual: usize,
    },
    /// 下层 ye-time 错误（时戳重标度等）。
    Time(TimeError),
    /// 下层 ye-pixfmt 错误（帧布局计算等），原样透出。
    Pixfmt(PixfmtError),
}

impl core::fmt::Display for CodecError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CodecError::UnsupportedCodec(codec) => {
                write!(f, "unsupported codec: {codec:?}")
            }
            CodecError::UnsupportedStub(msg) => write!(f, "unsupported stub: {msg}"),
            CodecError::NotRegistered(codec) => {
                write!(f, "codec not registered: {codec:?}")
            }
            CodecError::InvalidData(msg) => write!(f, "invalid data: {msg}"),
            CodecError::InvalidState(msg) => write!(f, "invalid state: {msg}"),
            CodecError::InvalidArguments(msg) => write!(f, "invalid arguments: {msg}"),
            CodecError::BufferMismatch { expected, actual } => write!(
                f,
                "buffer length mismatch: expected {expected} bytes, got {actual}"
            ),
            CodecError::Time(err) => write!(f, "time error: {err}"),
            CodecError::Pixfmt(err) => write!(f, "pixel format error: {err}"),
        }
    }
}

impl std::error::Error for CodecError {
    /// 包装变体透出下层错误源（`Time`/`Pixfmt`）。
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CodecError::Time(err) => Some(err),
            CodecError::Pixfmt(err) => Some(err),
            _ => None,
        }
    }
}

impl From<TimeError> for CodecError {
    fn from(err: TimeError) -> CodecError {
        CodecError::Time(err)
    }
}

impl From<PixfmtError> for CodecError {
    fn from(err: PixfmtError) -> CodecError {
        CodecError::Pixfmt(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_time_error() {
        // From 链路：TimeError::Overflow → CodecError::Time(TimeError::Overflow)
        let err: CodecError = TimeError::Overflow.into();
        assert_eq!(err, CodecError::Time(TimeError::Overflow));
        assert_eq!(err.to_string(), "time error: arithmetic overflow");
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn from_pixfmt_error() {
        let err: CodecError = PixfmtError::BadAlignment(3).into();
        assert_eq!(err, CodecError::Pixfmt(PixfmtError::BadAlignment(3)));
        // Display 非空且含下层文案
        let text = err.to_string();
        assert!(!text.is_empty());
        assert!(text.contains("stride alignment"), "got: {text}");
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn display_english_short_messages() {
        assert_eq!(
            CodecError::NotRegistered(CodecId::H264).to_string(),
            "codec not registered: H264"
        );
        assert_eq!(
            CodecError::UnsupportedCodec(CodecId::Av1).to_string(),
            "unsupported codec: Av1"
        );
        assert_eq!(
            CodecError::InvalidData("bad nal".into()).to_string(),
            "invalid data: bad nal"
        );
        assert_eq!(
            CodecError::InvalidState("decoder already finished").to_string(),
            "invalid state: decoder already finished"
        );
        assert_eq!(
            CodecError::InvalidArguments("sample rate is zero").to_string(),
            "invalid arguments: sample rate is zero"
        );
        assert_eq!(
            CodecError::BufferMismatch {
                expected: 10,
                actual: 8
            }
            .to_string(),
            "buffer length mismatch: expected 10 bytes, got 8"
        );
    }

    #[test]
    fn display_unsupported_stub_contains_milestone() {
        // 桩错误文案必须含里程碑标注（M2/M3 交付），供调用方与测试识别
        let decode = CodecError::UnsupportedStub("h264 decode not implemented (桩：M2 交付)");
        assert_eq!(
            decode.to_string(),
            "unsupported stub: h264 decode not implemented (桩：M2 交付)"
        );
        assert!(decode.to_string().contains("桩：M2 交付"));
        let encode = CodecError::UnsupportedStub("h264 encode not implemented (桩：M3 交付)");
        assert!(encode.to_string().contains("桩：M3 交付"));
        // 无下层错误源
        assert!(std::error::Error::source(&decode).is_none());
    }

    #[test]
    fn usable_as_std_error() {
        let boxed: Box<dyn std::error::Error> =
            Box::new(CodecError::Pixfmt(PixfmtError::ZeroDimension));
        assert_eq!(
            boxed.to_string(),
            "pixel format error: zero width or height"
        );
    }
}
