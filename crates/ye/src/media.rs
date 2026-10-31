//! 两消费方共用的实际媒体探测与任务解码预检。
use crate::convert::ConvertError;
use ye_codec::{CodecRegistry, MediaType};
use ye_container::{ContainerRegistry, MediaHeader, ReadSeek};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityIssue {
    pub stream_index: Option<u32>,
    pub reason: String,
}

/// 探测只读取容器，不把成功探测等同于支持该素材解码。
pub fn inspect(input: Box<dyn ReadSeek>) -> Result<MediaHeader, ConvertError> {
    let mut containers = ContainerRegistry::new();
    crate::container::mp4::register_mp4(&mut containers);
    let (_, header) = containers.probe(input)?;
    Ok(header)
}

/// 仅检查任务实际需要解码的流。抽帧不应被不相关的音轨阻塞。
/// 工厂预检不能证明所有压缩包有效，执行期错误仍必须透出。
pub fn validate_decoding(
    header: &MediaHeader,
    codecs: &CodecRegistry,
    video: bool,
    audio: bool,
) -> Vec<CapabilityIssue> {
    let mut issues = Vec::new();
    for (required, kind) in [(video, MediaType::Video), (audio, MediaType::Audio)] {
        if !required {
            continue;
        }
        let streams: Vec<_> = header
            .streams
            .iter()
            .filter(|s| s.media_type == kind)
            .collect();
        if streams.len() != 1 {
            issues.push(CapabilityIssue {
                stream_index: None,
                reason: format!("任务要求恰一条 {kind:?} 流，实际 {} 条", streams.len()),
            });
            continue;
        }
        let stream = streams[0];
        if let Err(error) = codecs.create_decoder(&stream.codec) {
            issues.push(CapabilityIssue {
                stream_index: Some(stream.index),
                reason: error.to_string(),
            });
        }
    }
    issues
}

#[cfg(test)]
mod tests {
    #[test]
    fn invalid_bytes_are_not_a_container_even_with_a_plausible_name() {
        let bytes = std::io::Cursor::new(b"not an mp4".to_vec());
        assert!(super::inspect(Box::new(bytes)).is_err());
    }
}
