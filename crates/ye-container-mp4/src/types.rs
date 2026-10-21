//! 基础数据类型：FourCc、媒体类别、[`Stream`] 与 [`Packet`]。

use std::fmt;

use ye_time::{Rational, Timestamp};

/// 四字符码（如 `avc1`、`mp4a`）。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FourCc {
    pub bytes: [u8; 4],
}

impl FourCc {
    pub fn new(bytes: [u8; 4]) -> FourCc {
        FourCc { bytes }
    }

    /// 长度恰为 4 时才转换，否则 `None`。
    pub fn from_bytes(b: &[u8]) -> Option<FourCc> {
        let arr: [u8; 4] = b.try_into().ok()?;
        Some(FourCc::new(arr))
    }

    pub fn as_bytes(&self) -> [u8; 4] {
        self.bytes
    }

    /// 四字节皆 ASCII 可打印则原样输出；否则逐字节转义为 `\xNN`。
    pub fn as_str(&self) -> String {
        if self.bytes.iter().all(|&b| (0x20..=0x7e).contains(&b)) {
            self.bytes.iter().map(|&b| b as char).collect()
        } else {
            self.bytes.iter().map(|&b| format!("\\x{b:02X}")).collect()
        }
    }
}

impl fmt::Display for FourCc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

impl fmt::Debug for FourCc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

/// 媒体类别（hdlr：vide/soun/其他）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MediaKind {
    Video,
    Audio,
    Other,
}

/// 视频参数，取自 stsd visual entry（像素），不取 tkhd 的 16.16 显示尺寸。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
}

/// 音频参数，取自 stsd audio entry 字段。`sample_rate` 为 16.16 定点取高 16 位。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioInfo {
    pub channel_count: u32,
    pub sample_rate: u32,
}

/// 一路 trak 的元数据（对应 architecture.md 的 Stream）。
#[derive(Clone, Debug)]
pub struct Stream {
    /// 0 起，按 moov 内 trak 出现顺序。
    pub index: usize,
    /// tkhd。
    pub track_id: u32,
    /// hdlr：vide/soun/其他。
    pub kind: MediaKind,
    /// stsd 首 entry 类型：avc1/mp4a/…。
    pub codec: FourCc,
    /// `1/mdhd:timescale`，方向「每单位=多少秒」。
    pub timebase: Rational,
    /// mdhd 时长换算到全局 tick（rescale 舍入）。
    pub duration: Timestamp,
    /// kind==Video 时 Some。
    pub video: Option<VideoInfo>,
    /// kind==Audio 时 Some。
    pub audio: Option<AudioInfo>,
    /// stsd 子 box 的载荷（去掉 8 字节 box 头后的全部字节）：
    /// avc1→avcC 载荷即 AVCDecoderConfigurationRecord 本体；
    /// mp4a→esds 载荷（含 4 字节版本/标志）。
    /// 契约：原样透传给解码器，本 crate 不解释。
    pub extra_data: Vec<u8>,
}

/// 压缩域数据 + 流索引 + 时戳（对应 architecture.md 的 Packet，
/// 时戳为全局 tick 整数）。
#[derive(Clone, Debug)]
pub struct Packet {
    pub stream_index: usize,
    /// 本轨内 0 起序号。
    pub sample_index: usize,
    /// 解码时戳，全局 tick，可为负。
    pub dts: Timestamp,
    /// 展示时戳（ctts 已应用）；`None` 表示等于 dts。
    pub pts: Option<Timestamp>,
    /// 本样例时长（stts delta 换算）。
    pub duration: Timestamp,
    /// stss 含此样例（1-based 样号）；视频轨无 stss 表则恒 true；音频轨恒 true。
    pub keyframe: bool,
    /// 压缩数据，容器存储原样——H.264 为 AVCC（长度前缀 NAL），不转 Annex B；
    /// 长度 = stsz 该样例大小。
    pub data: Vec<u8>,
}
