//! 门面 PNG 出口（`ye::png`）的回归：feature 开启后入口可达，
//! 默认档编码出合法 PNG 结构（签名/chunk/CRC），档位 API 可选。
//!
//! 默认 feature 集不含 `png`，本文件仅在
//! `cargo test -p ye --features png` 下有内容（与 stubs.rs 同套路，
//! 不属 `cargo test --workspace` 的默认门槛）。
//! 无损性对拍（ffmpeg 逐字节 0 容差）归实现 crate 的
//! `ye-png/tests/oracle_png.rs`，此处只验门面接线。

#![cfg(feature = "png")]
#![forbid(unsafe_code)]

use ye::png::{DeflateProfile, FilterStrategy, PngError, SIGNATURE, crc32};

/// 3x2 渐变经门面默认档编码：签名、chunk 序列（IHDR13/IDAT/IEND）、
/// 每 chunk CRC 复算一致。
#[test]
fn facade_encodes_valid_png() {
    let (w, h) = (3u32, 2u32);
    let rgba: Vec<u8> = (0..w * h * 4).map(|i| i as u8).collect();
    let png = ye::png::encode_rgba(w, h, &rgba).expect("编码失败");

    assert_eq!(&png[0..8], &SIGNATURE);
    let mut pos = 8usize;
    let mut seen = 0usize;
    while pos < png.len() {
        let len = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
        let ctype = &png[pos + 4..pos + 8];
        let data = &png[pos + 8..pos + 8 + len];
        let crc = u32::from_be_bytes(png[pos + 8 + len..pos + 12 + len].try_into().unwrap());
        let mut typed = Vec::with_capacity(4 + len);
        typed.extend_from_slice(ctype);
        typed.extend_from_slice(data);
        assert_eq!(crc32(&typed), crc, "chunk #{seen} CRC");
        match seen {
            0 => assert_eq!((ctype.as_ref(), len), (b"IHDR".as_slice(), 13)),
            1 => {
                assert_eq!(ctype, b"IDAT");
                assert_eq!(&data[0..2], &[0x78, 0x9C], "fixed-Huffman 默认档 zlib 头");
            }
            2 => assert_eq!((ctype.as_ref(), len), (b"IEND".as_slice(), 0)),
            _ => panic!("多余 chunk #{seen}"),
        }
        pos += 12 + len;
        seen += 1;
    }
    assert_eq!((seen, pos), (3, png.len()));
}

/// 档位 API 与错误面经门面路径可达。
#[test]
fn facade_options_and_errors_reachable() {
    let rgba = vec![7u8; 4];
    let stored =
        ye::png::encode_rgba_with(1, 1, &rgba, DeflateProfile::Stored, FilterStrategy::Sub)
            .expect("stored 编码失败");
    assert_eq!(&stored[0..8], &SIGNATURE);
    assert_eq!(
        ye::png::encode_rgba(2, 2, &rgba),
        Err(PngError::BufferSize {
            expected: 16,
            got: 4
        })
    );
    // CRC32/Adler32 公开面经门面可达（已知向量）。
    assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
}
