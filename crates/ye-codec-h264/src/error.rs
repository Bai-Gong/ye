//! ye-codec-h264 的错误类型（全库约定：每 crate 自带错误枚举，禁 thiserror）。

/// 本 crate 全部失败模式的枚举。
///
/// ## 变体语义
///
/// - [`InvalidData`](Self::InvalidData)：码流数据不合法（起始码缺失、长度
///   前缀越界、SPS/PPS 字段超界、avcC 记录截断……）——来自文件/网络的
///   输入一律 `Result`，绝不 panic（含位读取器越界）；
/// - [`InvalidArguments`](Self::InvalidArguments)：调用方参数误用
///   （如 AVCC 长度前缀字节数不在 1..=4）；
/// - [`Unsupported`](Self::Unsupported)：码流合法但超出当前实现边界
///   （如 PPS 的 FMO 多片组——FFmpeg 同样拒绝）。
///
/// 注意：profile/level 的 Baseline 边界判定**不是**错误——解析层只报告
/// 字段（[`crate::Sps::profile`]/[`crate::Sps::is_baseline`]），是否拒绝
/// 由消费方（M2 软解 ④、硬解 ⑨ 的 profile 门槛）决定，码流层不越权。
///
/// ## derive 清单
///
/// `Debug, Clone, PartialEq, Eq`（含 `String` 变体故不 `Copy`）；
/// `Display` 与 `std::error::Error` 为手工实现（全库禁 thiserror），
/// Display 文案为英文短句；doc 注释中文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum H264Error {
    /// 码流数据不合法，携带描述。
    InvalidData(String),
    /// 调用方参数非法（如 `length_size` 不在 1..=4）。
    InvalidArguments(&'static str),
    /// 码流合法但超出当前实现边界（如 FMO 多片组 PPS）。
    Unsupported(&'static str),
}

impl core::fmt::Display for H264Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            H264Error::InvalidData(msg) => write!(f, "invalid data: {msg}"),
            H264Error::InvalidArguments(msg) => write!(f, "invalid arguments: {msg}"),
            H264Error::Unsupported(msg) => write!(f, "unsupported: {msg}"),
        }
    }
}

impl std::error::Error for H264Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_english_short_messages() {
        assert_eq!(
            H264Error::InvalidData("no annexb start code".into()).to_string(),
            "invalid data: no annexb start code"
        );
        assert_eq!(
            H264Error::InvalidArguments("length_size must be 1..=4").to_string(),
            "invalid arguments: length_size must be 1..=4"
        );
        assert_eq!(
            H264Error::Unsupported("FMO slice groups").to_string(),
            "unsupported: FMO slice groups"
        );
    }

    #[test]
    fn usable_as_std_error() {
        let boxed: Box<dyn std::error::Error> =
            Box::new(H264Error::InvalidData("truncated nal".into()));
        assert_eq!(boxed.to_string(), "invalid data: truncated nal");
        assert!(std::error::Error::source(&*boxed).is_none());
    }
}
