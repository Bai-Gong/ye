//! H.273（ISO/IEC 23091-8 / 原 ITU-T H.274）色彩描述族。
//!
//! 码值与 FFmpeg `AVColorPrimaries` / `AVColorTransferCharacteristic` /
//! `AVColorSpace` / `AVColorRange` / `AVChromaLocation` 对齐；
//! MP4 `colr` box 读写直接使用 `to_code`/`from_code`。
//! 带 [`Unknown`](ColorPrimaries::Unknown) 变体的枚举可无损保存容器读到的
//! 未定义码值（from_code 对未知码返回 `None`，调用方自行构造 `Unknown(code)`
//! 以便原样写回）。

/// 色度原色（H.273 ColourPrimaries）。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq, Hash`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorPrimaries {
    /// 未指定（码值 2）。
    Unspecified,
    /// BT.709（码值 1，sRGB/HD 常用）。
    Bt709,
    /// BT.470 系统 M（码值 4）。
    Bt470M,
    /// BT.470 系统 B/G（码值 5）。
    Bt470Bg,
    /// SMPTE 170M（码值 6，SD NTSC）。
    Smpte170m,
    /// BT.2020（码值 9，UHD）。
    Bt2020,
    /// SMPTE RP 431-2（码值 11，DCI-P3 影院）。
    Smpte431,
    /// SMPTE EG 432-1（码值 12，Display-P3）。
    Smpte432,
    /// 未定义码值，原样保存。
    Unknown(u8),
}

impl ColorPrimaries {
    /// 转 H.273 码值；[`Unknown`](Self::Unknown) 原样返回保存的码值。
    pub fn to_code(self) -> u8 {
        match self {
            ColorPrimaries::Unspecified => 2,
            ColorPrimaries::Bt709 => 1,
            ColorPrimaries::Bt470M => 4,
            ColorPrimaries::Bt470Bg => 5,
            ColorPrimaries::Smpte170m => 6,
            ColorPrimaries::Bt2020 => 9,
            ColorPrimaries::Smpte431 => 11,
            ColorPrimaries::Smpte432 => 12,
            ColorPrimaries::Unknown(code) => code,
        }
    }

    /// 由 H.273 码值构造；未定义的码值返回 `None`
    /// （需无损保存时调用方自行构造 [`Unknown`](Self::Unknown)）。
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => ColorPrimaries::Bt709,
            2 => ColorPrimaries::Unspecified,
            4 => ColorPrimaries::Bt470M,
            5 => ColorPrimaries::Bt470Bg,
            6 => ColorPrimaries::Smpte170m,
            9 => ColorPrimaries::Bt2020,
            11 => ColorPrimaries::Smpte431,
            12 => ColorPrimaries::Smpte432,
            _ => return None,
        })
    }
}

/// 传递函数 / 传递特性（H.273 TransferCharacteristics）。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq, Hash`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorTransfer {
    /// 未指定（码值 2）。
    Unspecified,
    /// BT.709（码值 1）。
    Bt709,
    /// SMPTE 170M（码值 6，SD 伽马 ≈2.2）。
    Smpte170m,
    /// 线性（码值 8）。
    Linear,
    /// IEC 61966-2-1 sRGB（码值 13）。
    Iec61966_2_1,
    /// BT.2020 10 bit（码值 14）。
    Bt2020Ten,
    /// BT.2020 12 bit（码值 15）。
    Bt2020Twelve,
    /// SMPTE ST 2084 PQ（码值 16，HDR）。
    Smpte2084,
    /// ARIB STD-B67 HLG（码值 18，HDR）。
    AribStdB67,
    /// 未定义码值，原样保存。
    Unknown(u8),
}

impl ColorTransfer {
    /// 转 H.273 码值；[`Unknown`](Self::Unknown) 原样返回保存的码值。
    pub fn to_code(self) -> u8 {
        match self {
            ColorTransfer::Unspecified => 2,
            ColorTransfer::Bt709 => 1,
            ColorTransfer::Smpte170m => 6,
            ColorTransfer::Linear => 8,
            ColorTransfer::Iec61966_2_1 => 13,
            ColorTransfer::Bt2020Ten => 14,
            ColorTransfer::Bt2020Twelve => 15,
            ColorTransfer::Smpte2084 => 16,
            ColorTransfer::AribStdB67 => 18,
            ColorTransfer::Unknown(code) => code,
        }
    }

    /// 由 H.273 码值构造；未定义的码值返回 `None`。
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => ColorTransfer::Bt709,
            2 => ColorTransfer::Unspecified,
            6 => ColorTransfer::Smpte170m,
            8 => ColorTransfer::Linear,
            13 => ColorTransfer::Iec61966_2_1,
            14 => ColorTransfer::Bt2020Ten,
            15 => ColorTransfer::Bt2020Twelve,
            16 => ColorTransfer::Smpte2084,
            18 => ColorTransfer::AribStdB67,
            _ => return None,
        })
    }
}

/// 矩阵系数 / 色彩空间（H.273 MatrixCoefficients）。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq, Hash`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorSpace {
    /// 未指定（码值 2）。
    Unspecified,
    /// RGB，无矩阵变换（码值 0，GBR/RGB 平面）。
    Rgb,
    /// BT.709（码值 1）。
    Bt709,
    /// SMPTE 170M（码值 6，SD）。
    Smpte170m,
    /// SMPTE 240M（码值 7）。
    Smpte240m,
    /// BT.2020 非恒定亮度（码值 9）。
    Bt2020Nc,
    /// BT.2020 恒定亮度（码值 10）。
    Bt2020C,
    /// 未定义码值，原样保存。
    Unknown(u8),
}

impl ColorSpace {
    /// 转 H.273 码值；[`Unknown`](Self::Unknown) 原样返回保存的码值。
    pub fn to_code(self) -> u8 {
        match self {
            ColorSpace::Unspecified => 2,
            ColorSpace::Rgb => 0,
            ColorSpace::Bt709 => 1,
            ColorSpace::Smpte170m => 6,
            ColorSpace::Smpte240m => 7,
            ColorSpace::Bt2020Nc => 9,
            ColorSpace::Bt2020C => 10,
            ColorSpace::Unknown(code) => code,
        }
    }

    /// 由 H.273 码值构造；未定义的码值返回 `None`。
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => ColorSpace::Rgb,
            1 => ColorSpace::Bt709,
            2 => ColorSpace::Unspecified,
            6 => ColorSpace::Smpte170m,
            7 => ColorSpace::Smpte240m,
            9 => ColorSpace::Bt2020Nc,
            10 => ColorSpace::Bt2020C,
            _ => return None,
        })
    }
}

/// 量化范围（H.273 VideoFullRangeFlag）。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq, Hash`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorRange {
    /// 未指定（码值 0）。
    Unspecified,
    /// MPEG / limited：Y ∈ [16,235]（8bit 标度，码值 1）。
    Mpeg,
    /// JPEG / full：全量程 [0,255]（码值 2）。
    Jpeg,
}

impl ColorRange {
    /// 转 H.273 码值。
    pub fn to_code(self) -> u8 {
        match self {
            ColorRange::Unspecified => 0,
            ColorRange::Mpeg => 1,
            ColorRange::Jpeg => 2,
        }
    }

    /// 由 H.273 码值构造；未定义的码值返回 `None`（本枚举无 Unknown 变体）。
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => ColorRange::Unspecified,
            1 => ColorRange::Mpeg,
            2 => ColorRange::Jpeg,
            _ => return None,
        })
    }
}

/// 色度样本位置（H.273 ChromaSamplePosition）。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq, Hash`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChromaLocation {
    /// 未指定（码值 0）。
    Unspecified,
    /// 左：与 luma 同列（码值 1，MPEG-2/4:2:0 常见）。
    Left,
    /// 中心（码值 2，JPEG/ITU-R 601）。
    Center,
    /// 左上（码值 3）。
    TopLeft,
    /// 上（码值 4）。
    Top,
    /// 左下（码值 5）。
    BottomLeft,
    /// 下（码值 6）。
    Bottom,
}

impl ChromaLocation {
    /// 转 H.273 码值。
    pub fn to_code(self) -> u8 {
        match self {
            ChromaLocation::Unspecified => 0,
            ChromaLocation::Left => 1,
            ChromaLocation::Center => 2,
            ChromaLocation::TopLeft => 3,
            ChromaLocation::Top => 4,
            ChromaLocation::BottomLeft => 5,
            ChromaLocation::Bottom => 6,
        }
    }

    /// 由 H.273 码值构造；未定义的码值返回 `None`（本枚举无 Unknown 变体）。
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => ChromaLocation::Unspecified,
            1 => ChromaLocation::Left,
            2 => ChromaLocation::Center,
            3 => ChromaLocation::TopLeft,
            4 => ChromaLocation::Top,
            5 => ChromaLocation::BottomLeft,
            6 => ChromaLocation::Bottom,
            _ => return None,
        })
    }
}

/// 色彩描述聚合（公有字段、无不变式）。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq`；另手工实现 [`Default`]
/// （返回与 [`UNSPECIFIED`](Self::UNSPECIFIED) 相等的全 Unspecified 描述）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorDesc {
    /// 色度原色。
    pub primaries: ColorPrimaries,
    /// 传递函数。
    pub transfer: ColorTransfer,
    /// 矩阵系数（色彩空间）。
    pub space: ColorSpace,
    /// 量化范围。
    pub range: ColorRange,
    /// 色度样本位置。
    pub chroma_location: ChromaLocation,
}

impl ColorDesc {
    /// 全部字段为 Unspecified 的色彩描述。
    pub const UNSPECIFIED: ColorDesc = ColorDesc {
        primaries: ColorPrimaries::Unspecified,
        transfer: ColorTransfer::Unspecified,
        space: ColorSpace::Unspecified,
        range: ColorRange::Unspecified,
        chroma_location: ChromaLocation::Unspecified,
    };
}

impl Default for ColorDesc {
    /// 缺省为全 Unspecified（等于 [`UNSPECIFIED`](Self::UNSPECIFIED)）。
    fn default() -> Self {
        Self::UNSPECIFIED
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primaries_code_roundtrip() {
        let pairs = [
            (ColorPrimaries::Bt709, 1u8),
            (ColorPrimaries::Unspecified, 2),
            (ColorPrimaries::Bt470M, 4),
            (ColorPrimaries::Bt470Bg, 5),
            (ColorPrimaries::Smpte170m, 6),
            (ColorPrimaries::Bt2020, 9),
            (ColorPrimaries::Smpte431, 11),
            (ColorPrimaries::Smpte432, 12),
        ];
        for (value, code) in pairs {
            assert_eq!(value.to_code(), code);
            assert_eq!(ColorPrimaries::from_code(code), Some(value));
        }
    }

    #[test]
    fn transfer_code_roundtrip() {
        let pairs = [
            (ColorTransfer::Bt709, 1u8),
            (ColorTransfer::Unspecified, 2),
            (ColorTransfer::Smpte170m, 6),
            (ColorTransfer::Linear, 8),
            (ColorTransfer::Iec61966_2_1, 13),
            (ColorTransfer::Bt2020Ten, 14),
            (ColorTransfer::Bt2020Twelve, 15),
            (ColorTransfer::Smpte2084, 16),
            (ColorTransfer::AribStdB67, 18),
        ];
        for (value, code) in pairs {
            assert_eq!(value.to_code(), code);
            assert_eq!(ColorTransfer::from_code(code), Some(value));
        }
    }

    #[test]
    fn space_code_roundtrip() {
        let pairs = [
            (ColorSpace::Rgb, 0u8),
            (ColorSpace::Bt709, 1),
            (ColorSpace::Unspecified, 2),
            (ColorSpace::Smpte170m, 6),
            (ColorSpace::Smpte240m, 7),
            (ColorSpace::Bt2020Nc, 9),
            (ColorSpace::Bt2020C, 10),
        ];
        for (value, code) in pairs {
            assert_eq!(value.to_code(), code);
            assert_eq!(ColorSpace::from_code(code), Some(value));
        }
    }

    #[test]
    fn range_code_roundtrip() {
        let pairs = [
            (ColorRange::Unspecified, 0u8),
            (ColorRange::Mpeg, 1),
            (ColorRange::Jpeg, 2),
        ];
        for (value, code) in pairs {
            assert_eq!(value.to_code(), code);
            assert_eq!(ColorRange::from_code(code), Some(value));
        }
    }

    #[test]
    fn chroma_code_roundtrip() {
        let pairs = [
            (ChromaLocation::Unspecified, 0u8),
            (ChromaLocation::Left, 1),
            (ChromaLocation::Center, 2),
            (ChromaLocation::TopLeft, 3),
            (ChromaLocation::Top, 4),
            (ChromaLocation::BottomLeft, 5),
            (ChromaLocation::Bottom, 6),
        ];
        for (value, code) in pairs {
            assert_eq!(value.to_code(), code);
            assert_eq!(ChromaLocation::from_code(code), Some(value));
        }
    }

    #[test]
    fn unknown_code_preserved_and_unmapped_rejected() {
        // Unknown 原样保存码值（to_code 方向保号）
        assert_eq!(ColorPrimaries::Unknown(200).to_code(), 200);
        assert_eq!(ColorTransfer::Unknown(200).to_code(), 200);
        assert_eq!(ColorSpace::Unknown(200).to_code(), 200);
        // from_code 对未定义码值返回 None（含无 Unknown 变体的枚举）
        assert_eq!(ColorPrimaries::from_code(200), None);
        assert_eq!(ColorTransfer::from_code(200), None);
        assert_eq!(ColorSpace::from_code(200), None);
        assert_eq!(ColorRange::from_code(200), None);
        assert_eq!(ChromaLocation::from_code(200), None);
    }

    #[test]
    fn color_desc_default_unspecified() {
        let d = ColorDesc::default();
        assert_eq!(d.primaries, ColorPrimaries::Unspecified);
        assert_eq!(d.transfer, ColorTransfer::Unspecified);
        assert_eq!(d.space, ColorSpace::Unspecified);
        assert_eq!(d.range, ColorRange::Unspecified);
        assert_eq!(d.chroma_location, ChromaLocation::Unspecified);
        assert_eq!(d, ColorDesc::UNSPECIFIED);
    }
}
