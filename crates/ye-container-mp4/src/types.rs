//! 基础数据类型：四字符码 [`FourCc`] 与 stsd codec ↔ [`CodecId`] 映射。
//!
//! 移植裁决（见 docs/contracts.md）：`Stream`/`Packet` 已换成 develop 的
//! `ye_container::Stream`/`ye_codec::Packet`；`FourCc` 保留在本 crate——
//! box 类型（moov/stts/…）与 stsd entry 类型不是 codec，`CodecId` 表达不了，
//! 仅在流元数据处映射为 `CodecId`。

use std::fmt;

use ye_codec::CodecId;
use ye_pixfmt::ChannelLayout;

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

/// stsd entry 类型 → [`CodecId`]（读取方向）。
///
/// 覆盖 M1/M2 够用集内的常见 fourcc；未映射的 entry 一律
/// [`CodecId::Unknown`]（原始 fourcc 经 [`crate::Mp4Reader::track_info`]
/// 仍可查到）。
pub(crate) fn codec_id_of(codec: &FourCc) -> CodecId {
    match &codec.bytes {
        b"avc1" | b"avc3" => CodecId::H264,
        b"hvc1" | b"hev1" => CodecId::Hevc,
        b"mp4a" => CodecId::Aac,
        _ => CodecId::Unknown,
    }
}

/// [`CodecId`] → stsd entry 类型（写入方向）；本封装器仅支持
/// H264→`avc1`、Aac→`mp4a`。
pub(crate) fn fourcc_of_codec(codec: CodecId) -> Option<FourCc> {
    match codec {
        CodecId::H264 => Some(FourCc::new(*b"avc1")),
        CodecId::Aac => Some(FourCc::new(*b"mp4a")),
        _ => None,
    }
}

/// 声道数 → FFmpeg 默认声道布局（`av_get_default_channel_layout` 语义）。
///
/// stsd 只携带 channelcount 一个 u16，不含布局；develop 的
/// `AudioParams` 无 channel_count 字段，声道数经默认布局的
/// `channels()` 保全。1..=8 按 FFmpeg 表，其余 `None`（不猜）。
///
/// 位序对齐 `ChannelLayout::CH_*`（前左 0x1 / 前右 0x2 / 前中 0x4 /
/// LFE 0x8 / 后左 0x10 / 后右 0x20）；7/8 声道额外用到 FFmpeg 位序的
/// 后中 0x100 / 侧左 0x200 / 侧右 0x400（本表以原始掩码写出）。
pub(crate) fn default_channel_layout(channels: u32) -> Option<ChannelLayout> {
    let mask = match channels {
        1 => 0x004, // mono：前中
        2 => 0x003, // stereo：前左|前右
        3 => 0x00B, // 2.1：前左|前右|LFE
        4 => 0x033, // quad：前左|前右|后左|后右
        5 => 0x037, // 5.0：quad + 前中
        6 => 0x03F, // 5.1：5.0 + LFE
        7 => 0x13F, // 6.1：5.1 + 后中
        8 => 0x63F, // 7.1：5.1 + 后左后右换成侧左侧右+后左后右
        _ => return None,
    };
    ChannelLayout::try_from_mask(mask).ok()
}
