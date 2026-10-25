//! ye-png：PNG **编码器**（encode-only，施工图④ M2）。
//!
//! RGBA8 → PNG（颜色类型 6、位深 8、非隔行），压缩与校验全部自实现：
//!
//! - DEFLATE 两档（[`DeflateProfile`]）：stored（BTYPE=00）与
//!   fixed-Huffman（BTYPE=01，自实现 LZ77：哈希链贪心 + 一步 lazy）；
//! - zlib 包装（RFC 1950）：0x78 头 + Adler-32 尾；
//! - CRC-32（IEEE）自实现，chunk 逐段校验；
//! - 行过滤两档 + 自适应（[`FilterStrategy`]：None/Sub 逐行择优，
//!   最小绝对差和启发式，见 `filter` 模块文档）。
//!
//! ## 验证口径（施工图原文）
//!
//! 无损性**不给容差**：oracle 测试（`tests/oracle_png.rs`）用 ffmpeg
//! 解码本编码器输出的 PNG 到 rawvideo RGBA，与送入字节**逐字节相等**；
//! 测试图案为渐变/边缘/LCG 确定性随机。本 crate 不提供解码 API。
//!
//! ## 全库约定
//!
//! 零第三方依赖（仅 std）、不含 unsafe、失败一律 `Result` 不 panic。
//!
//! ## 模块划分（私有实现 + 根 re-export）
//!
//! - [`PngError`]：错误枚举；
//! - [`encode_rgba`] / [`encode_rgba_with`]：整幅编码入口；
//! - `crc32` / `zlib`（[`adler32`]）/ `filter` / `deflate`：实现模块，
//!   其中 [`crc32`]、[`adler32`]、[`DeflateProfile`]、
//!   [`FilterStrategy`] 公开可测。

mod crc32;
mod deflate;
mod filter;
mod zlib;

pub use crc32::crc32;
pub use deflate::DeflateProfile;
pub use filter::FilterStrategy;
pub use zlib::adler32;

/// PNG 文件签名（8 字节）。
pub const SIGNATURE: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];

/// 编码失败模式。
///
/// ## derive 清单
///
/// `Debug, Clone, PartialEq, Eq`；`Display` 与 `std::error::Error`
/// 手工实现（全库禁 thiserror），Display 文案为英文短句；doc 注释中文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PngError {
    /// 宽或高为 0（PNG 规范要求非零）。
    EmptyImage,
    /// 宽或高 ≥ 2^31（PNG 规范上限 2^31−1），携带 (width, height)。
    TooLarge { width: u32, height: u32 },
    /// 输入缓冲长度 ≠ width×height×4（紧排 RGBA），携带期望/实际值。
    BufferSize { expected: usize, got: usize },
}

impl core::fmt::Display for PngError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PngError::EmptyImage => write!(f, "png: width/height must be nonzero"),
            PngError::TooLarge { width, height } => {
                write!(
                    f,
                    "png: dimension {width}x{height} exceeds PNG limit 2^31-1"
                )
            }
            PngError::BufferSize { expected, got } => {
                write!(
                    f,
                    "png: rgba buffer size mismatch, expected {expected}, got {got}"
                )
            }
        }
    }
}

impl std::error::Error for PngError {}

/// RGBA8 → PNG（默认档：fixed-Huffman + Adaptive 过滤）。
///
/// `rgba` 必须是 `width × height` 的紧排 RGBA8（行间无 padding）。
/// 返回完整 PNG 文件字节（签名 + IHDR + 单 IDAT + IEND）。
pub fn encode_rgba(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, PngError> {
    encode_rgba_with(
        width,
        height,
        rgba,
        DeflateProfile::FixedHuffman,
        FilterStrategy::Adaptive,
    )
}

/// 同 [`encode_rgba`]，显式指定压缩档与过滤策略。
pub fn encode_rgba_with(
    width: u32,
    height: u32,
    rgba: &[u8],
    profile: DeflateProfile,
    strategy: FilterStrategy,
) -> Result<Vec<u8>, PngError> {
    if width == 0 || height == 0 {
        return Err(PngError::EmptyImage);
    }
    if width >= 1 << 31 || height >= 1 << 31 {
        return Err(PngError::TooLarge { width, height });
    }
    let expected = width as u64 * height as u64 * 4;
    if expected != rgba.len() as u64 {
        // 64 位平台天然相等比较；32 位平台上超大 expected 也走此分支
        //（usize 截断后不可能等于超大值），如实报期望字节数。
        return Err(PngError::BufferSize {
            expected: expected as usize,
            got: rgba.len(),
        });
    }
    let (w, h) = (width as usize, height as usize);

    // 行过滤 → zlib(IDAT 载荷) → chunk 组装。
    let raw = filter::apply(rgba, w, h, strategy);
    let idat = zlib::zlib_compress(&raw, profile);

    let mut out = Vec::with_capacity(idat.len() + 128);
    out.extend_from_slice(&SIGNATURE);
    push_chunk(&mut out, b"IHDR", &ihdr_payload(width, height));
    push_chunk(&mut out, b"IDAT", &idat);
    push_chunk(&mut out, b"IEND", &[]);
    Ok(out)
}

/// IHDR 载荷：宽/高大端 u32 + 位深 8 + 颜色类型 6（RGBA）+
/// 压缩 0（deflate）+ 过滤 0 + 隔行 0（非隔行）。
fn ihdr_payload(width: u32, height: u32) -> [u8; 13] {
    let mut p = [0u8; 13];
    p[0..4].copy_from_slice(&width.to_be_bytes());
    p[4..8].copy_from_slice(&height.to_be_bytes());
    p[8] = 8; // bit depth
    p[9] = 6; // color type: truecolor with alpha
    p[10] = 0; // compression: deflate
    p[11] = 0; // filter method
    p[12] = 0; // interlace: none
    p
}

/// 追加一个 chunk：长度(大端 u32) + 类型 + 数据 + CRC32(类型+数据)。
fn push_chunk(out: &mut Vec<u8>, ctype: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(ctype);
    out.extend_from_slice(data);
    let mut c = crc32::Crc32::new();
    c.update(ctype);
    c.update(data);
    out.extend_from_slice(&c.finish().to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2x2 已知图的 IHDR 字节逐项核对。
    #[test]
    fn ihdr_bytes() {
        assert_eq!(ihdr_payload(2, 2), [0, 0, 0, 2, 0, 0, 0, 2, 8, 6, 0, 0, 0]);
    }

    /// 输出为合法 chunk 序列（签名 + IHDR13 + IDAT + IEND0），
    /// 每 chunk CRC 复算一致。IDAT zlib 头两档分别为 78 01 / 78 9C。
    #[test]
    fn chunk_sequence_valid() {
        let rgba: Vec<u8> = (0..3 * 2 * 4).map(|i| i as u8 * 5).collect();
        for profile in [DeflateProfile::Stored, DeflateProfile::FixedHuffman] {
            let png = encode_rgba_with(3, 2, &rgba, profile, FilterStrategy::None).unwrap();
            assert_eq!(&png[0..8], &SIGNATURE);
            let mut pos = 8usize;
            let mut chunk_no = 0;
            while pos < png.len() {
                let len = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
                let ctype = &png[pos + 4..pos + 8];
                let data = &png[pos + 8..pos + 8 + len];
                let crc =
                    u32::from_be_bytes(png[pos + 8 + len..pos + 12 + len].try_into().unwrap());
                let mut c = crc32::Crc32::new();
                c.update(ctype);
                c.update(data);
                assert_eq!(c.finish(), crc, "chunk #{chunk_no} CRC");
                match chunk_no {
                    0 => assert_eq!((ctype, len), (b"IHDR".as_slice(), 13)),
                    1 => {
                        assert_eq!(ctype, b"IDAT".as_slice());
                        assert_eq!(
                            &data[0..2],
                            &[
                                0x78,
                                if profile == DeflateProfile::Stored {
                                    0x01
                                } else {
                                    0x9C
                                }
                            ]
                        );
                        // 尾部 Adler-32 与过滤后数据一致（None 策略布局已知）。
                        let mut raw = Vec::new();
                        for y in 0..2 {
                            raw.push(0u8);
                            raw.extend_from_slice(&rgba[y * 12..(y + 1) * 12]);
                        }
                        assert_eq!(&data[data.len() - 4..], &adler32(&raw).to_be_bytes());
                    }
                    2 => assert_eq!((ctype, len), (b"IEND".as_slice(), 0)),
                    _ => panic!("多余 chunk #{chunk_no}"),
                }
                pos += 12 + len;
                chunk_no += 1;
            }
            assert_eq!(chunk_no, 3);
            assert_eq!(pos, png.len());
        }
    }

    /// stored+None 档可离线全文复核：测试侧按 stored 框架拆块（见
    /// deflate 模块测试）重组原始流 == 过滤布局（行 type 0 + 行字节）。
    /// （fixed 档的重组需要 inflate，归 ffmpeg oracle，不在本 crate。）
    #[test]
    fn stored_payload_equals_expected_scanlines() {
        let w = 5u32;
        let h = 3u32;
        let rgba: Vec<u8> = (0..w * h * 4).map(|i| (i * 31 % 251) as u8).collect();
        let png =
            encode_rgba_with(w, h, &rgba, DeflateProfile::Stored, FilterStrategy::None).unwrap();
        // 定位 IDAT → 剥 zlib 头尾 → stored 框架重组。
        let data = idat_of(&png);
        let mut pos = 2usize;
        let mut raw = Vec::new();
        loop {
            let bfinal = data[pos] & 1;
            assert_eq!((data[pos] >> 1) & 0b11, 0);
            let len = u16::from_le_bytes([data[pos + 1], data[pos + 2]]) as usize;
            let nlen = u16::from_le_bytes([data[pos + 3], data[pos + 4]]);
            assert_eq!(!(len as u16), nlen);
            raw.extend_from_slice(&data[pos + 5..pos + 5 + len]);
            pos += 5 + len;
            if bfinal == 1 {
                break;
            }
        }
        let stride = w as usize * 4;
        for y in 0..h as usize {
            let row = &raw[y * (stride + 1)..(y + 1) * (stride + 1)];
            assert_eq!(row[0], 0, "None 策略行 type 0");
            assert_eq!(&row[1..], &rgba[y * stride..(y + 1) * stride]);
        }
    }

    /// 错误面：零尺寸、超 2^31、缓冲长度不符。
    #[test]
    fn error_cases() {
        assert_eq!(encode_rgba(0, 5, &[]), Err(PngError::EmptyImage));
        assert_eq!(encode_rgba(5, 0, &[]), Err(PngError::EmptyImage));
        assert_eq!(
            encode_rgba(1 << 31, 1, &[0; 8]),
            Err(PngError::TooLarge {
                width: 1 << 31,
                height: 1
            })
        );
        assert_eq!(
            encode_rgba(2, 2, &[0; 15]),
            Err(PngError::BufferSize {
                expected: 16,
                got: 15
            })
        );
        // Display 冒烟（英文短句）。
        assert!(PngError::EmptyImage.to_string().contains("nonzero"));
    }

    /// 最小图 1x1 可编码，且两档输出都以 IEND 的 CRC（0xAE426082）结尾。
    #[test]
    fn one_pixel() {
        let px = [12u8, 34, 56, 78];
        for profile in [DeflateProfile::Stored, DeflateProfile::FixedHuffman] {
            let png = encode_rgba_with(1, 1, &px, profile, FilterStrategy::Adaptive).unwrap();
            assert_eq!(&png[0..8], &SIGNATURE);
            assert_eq!(&png[png.len() - 4..], &0xAE42_6082u32.to_be_bytes());
        }
    }

    /// 从 PNG 字节中取 IDAT 载荷（chunk 遍历辅助，测试侧用）。
    fn idat_of(png: &[u8]) -> &[u8] {
        let mut pos = 8usize;
        while pos < png.len() {
            let len = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
            let ctype = &png[pos + 4..pos + 8];
            if ctype == b"IDAT" {
                return &png[pos + 8..pos + 8 + len];
            }
            pos += 12 + len;
        }
        panic!("no IDAT");
    }
}
