//! 声道布局（FFmpeg 位序声道掩码）。

use crate::error::PixfmtError;

/// 声道布局：u64 位掩码，位序对齐 FFmpeg `AV_CH_*`。
///
/// 不变式：`mask != 0`——字段私有，只能经 [`try_from_mask`](Self::try_from_mask)
/// 或本类型常量（`MONO`/`STEREO`）构造，构造时校验。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq, Hash`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelLayout {
    mask: u64,
}

impl ChannelLayout {
    /// 前左（FFmpeg `AV_CH_FRONT_LEFT`，bit 0）。
    pub const CH_FRONT_LEFT: u64 = 0x1;
    /// 前右（bit 1）。
    pub const CH_FRONT_RIGHT: u64 = 0x2;
    /// 前中（bit 2）。
    pub const CH_FRONT_CENTER: u64 = 0x4;
    /// 低频效果 LFE（bit 3）。
    pub const CH_LOW_FREQUENCY: u64 = 0x8;
    /// 后左（bit 4）。
    pub const CH_BACK_LEFT: u64 = 0x10;
    /// 后右（bit 5）。
    pub const CH_BACK_RIGHT: u64 = 0x20;

    /// 单声道：仅前中（mask = 0x4）。
    pub const MONO: ChannelLayout = ChannelLayout {
        mask: ChannelLayout::CH_FRONT_CENTER,
    };
    /// 立体声：前左 + 前右（mask = 0x3）。
    pub const STEREO: ChannelLayout = ChannelLayout {
        mask: ChannelLayout::CH_FRONT_LEFT | ChannelLayout::CH_FRONT_RIGHT,
    };

    /// 由位掩码构造；`mask == 0` 时返回 [`PixfmtError::EmptyChannelLayout`]。
    ///
    /// 非零但含当前未定义高位 bit 的掩码照常接受（声道集按位演进，
    /// `channels` 按置位数计）。
    pub fn try_from_mask(mask: u64) -> Result<ChannelLayout, PixfmtError> {
        if mask == 0 {
            return Err(PixfmtError::EmptyChannelLayout);
        }
        Ok(ChannelLayout { mask })
    }

    /// 位掩码。
    pub fn mask(&self) -> u64 {
        self.mask
    }

    /// 声道数（掩码置位数）。
    pub fn channels(&self) -> u32 {
        self.mask.count_ones()
    }

    /// 是否包含指定位：`mask & channel_bit != 0`。
    ///
    /// `channel_bit` 通常传单个 `CH_*` 常量；传多 bit 掩码时语义为
    /// "包含其中任一位"，不 panic。
    pub fn has_channel(&self, channel_bit: u64) -> bool {
        self.mask & channel_bit != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_and_mono_constants() {
        assert_eq!(ChannelLayout::STEREO.channels(), 2);
        assert_eq!(ChannelLayout::STEREO.mask(), 0x3);
        assert_eq!(ChannelLayout::MONO.mask(), 0x4);
        assert_eq!(ChannelLayout::MONO.channels(), 1);
    }

    #[test]
    fn try_from_mask_rejects_empty() {
        assert_eq!(
            ChannelLayout::try_from_mask(0),
            Err(PixfmtError::EmptyChannelLayout)
        );
        let full = ChannelLayout::try_from_mask(0x3f).unwrap();
        assert_eq!(full.mask(), 0x3f);
        assert_eq!(full.channels(), 6);
    }

    #[test]
    fn has_channel_bit_by_bit() {
        // 常量位值逐一对齐 FFmpeg 位序
        assert_eq!(ChannelLayout::CH_FRONT_LEFT, 1 << 0);
        assert_eq!(ChannelLayout::CH_FRONT_RIGHT, 1 << 1);
        assert_eq!(ChannelLayout::CH_FRONT_CENTER, 1 << 2);
        assert_eq!(ChannelLayout::CH_LOW_FREQUENCY, 1 << 3);
        assert_eq!(ChannelLayout::CH_BACK_LEFT, 1 << 4);
        assert_eq!(ChannelLayout::CH_BACK_RIGHT, 1 << 5);

        let full = ChannelLayout::try_from_mask(
            ChannelLayout::CH_FRONT_LEFT
                | ChannelLayout::CH_FRONT_RIGHT
                | ChannelLayout::CH_FRONT_CENTER
                | ChannelLayout::CH_LOW_FREQUENCY
                | ChannelLayout::CH_BACK_LEFT
                | ChannelLayout::CH_BACK_RIGHT,
        )
        .unwrap();
        assert!(full.has_channel(ChannelLayout::CH_FRONT_LEFT));
        assert!(full.has_channel(ChannelLayout::CH_FRONT_RIGHT));
        assert!(full.has_channel(ChannelLayout::CH_FRONT_CENTER));
        assert!(full.has_channel(ChannelLayout::CH_LOW_FREQUENCY));
        assert!(full.has_channel(ChannelLayout::CH_BACK_LEFT));
        assert!(full.has_channel(ChannelLayout::CH_BACK_RIGHT));

        let stereo = ChannelLayout::STEREO;
        assert!(stereo.has_channel(ChannelLayout::CH_FRONT_LEFT));
        assert!(stereo.has_channel(ChannelLayout::CH_FRONT_RIGHT));
        assert!(!stereo.has_channel(ChannelLayout::CH_FRONT_CENTER));
        assert!(!stereo.has_channel(ChannelLayout::CH_LOW_FREQUENCY));
        assert!(!stereo.has_channel(ChannelLayout::CH_BACK_LEFT));
        assert!(!stereo.has_channel(ChannelLayout::CH_BACK_RIGHT));
    }
}
