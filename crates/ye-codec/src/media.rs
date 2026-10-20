//! 媒体类型与编解码器标识。

/// 媒体类型（流的分类标签）。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq, Hash`
/// （全字段单元变体，纯值类型；`Hash` 供流分组/缓存键用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaType {
    /// 视频流。
    Video,
    /// 音频流。
    Audio,
    /// 字幕流。
    Subtitle,
}

/// 编解码器标识（M1/M2 够用集，对标 FFmpeg `AVCodecID` 的最小子集）。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash`——
/// `Ord` 必需：作 [`crate::CodecRegistry`] 内部 `BTreeMap` 的键
/// （`register_*` 的 BTreeMap insert 语义、`decoder_ids`/`encoder_ids`
/// 的有序迭代均依赖它）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CodecId {
    /// H.264 / AVC（ITU-T H.264，视频）。
    H264,
    /// H.265 / HEVC（ITU-T H.265，视频）。
    Hevc,
    /// AOMedia Video 1（视频）。
    Av1,
    /// Google VP9（视频）。
    Vp9,
    /// MPEG-2/4 AAC 家族（音频）。
    Aac,
    /// FLAC 无损音频。
    Flac,
    /// Opus（音频）。
    Opus,
    /// MPEG-1/2/2.5 Layer III（音频）。
    Mp3,
    /// 未知/未识别的编解码器（容器读到的未定义标识）。
    Unknown,
}

impl CodecId {
    /// 所属媒体类型：`H264`/`Hevc`/`Av1`/`Vp9` → [`MediaType::Video`]；
    /// `Aac`/`Flac`/`Opus`/`Mp3` → [`MediaType::Audio`]；
    /// [`Unknown`](Self::Unknown) → `None`（无法判定归属）。
    pub fn media_type(self) -> Option<MediaType> {
        match self {
            CodecId::H264 | CodecId::Hevc | CodecId::Av1 | CodecId::Vp9 => Some(MediaType::Video),
            CodecId::Aac | CodecId::Flac | CodecId::Opus | CodecId::Mp3 => Some(MediaType::Audio),
            CodecId::Unknown => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_type_mapping() {
        assert_eq!(CodecId::H264.media_type(), Some(MediaType::Video));
        assert_eq!(CodecId::Hevc.media_type(), Some(MediaType::Video));
        assert_eq!(CodecId::Av1.media_type(), Some(MediaType::Video));
        assert_eq!(CodecId::Vp9.media_type(), Some(MediaType::Video));
        assert_eq!(CodecId::Aac.media_type(), Some(MediaType::Audio));
        assert_eq!(CodecId::Flac.media_type(), Some(MediaType::Audio));
        assert_eq!(CodecId::Opus.media_type(), Some(MediaType::Audio));
        assert_eq!(CodecId::Mp3.media_type(), Some(MediaType::Audio));
        assert_eq!(CodecId::Unknown.media_type(), None);
    }

    #[test]
    fn codec_id_is_ordered_and_hashable() {
        // BTreeMap 键依赖（编译即验证 derive 清单齐全；此处再验语义自洽）
        use std::collections::BTreeMap;
        let mut m: BTreeMap<CodecId, u32> = BTreeMap::new();
        m.insert(CodecId::Mp3, 1);
        m.insert(CodecId::H264, 2);
        assert_eq!(
            m.keys().copied().collect::<Vec<_>>(),
            vec![CodecId::H264, CodecId::Mp3]
        );
        assert_eq!(m.get(&CodecId::H264), Some(&2));
    }
}
