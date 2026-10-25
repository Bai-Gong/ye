//! zlib 流包装（RFC 1950）：2 字节头 + DEFLATE 数据 + 4 字节大端
//! Adler-32。
//!
//! 头部 CMF=0x78（CM=8 deflate、CINFO=7 即 32 KiB 窗口），FLG 只用
//! FLEVEL 位（stored 档 0=fastest、fixed 档 2=default），FDICT=0；
//! `(CMF<<8 | FLG) % 31 == 0` 由构造断言与单测双重覆盖。

use crate::deflate::{DeflateProfile, deflate};

/// Adler-32（RFC 1950）：模 65521 的 s1/s2 和，初值 1。
pub fn adler32(bytes: &[u8]) -> u32 {
    const MOD: u32 = 65_521;
    let mut s1: u32 = 1;
    let mut s2: u32 = 0;
    // 分块防 s2 溢出：s1 ≤ 65520+65535*5552 < 2^32（NMAX=5552 经典界）。
    for chunk in bytes.chunks(5552) {
        for &b in chunk {
            s1 += b as u32;
            s2 += s1;
        }
        s1 %= MOD;
        s2 %= MOD;
    }
    (s2 << 16) | s1
}

/// zlib 头 FLG 档位（仅 FLEVEL 位，无 FDICT）。
fn header_flag(profile: DeflateProfile) -> u8 {
    match profile {
        DeflateProfile::Stored => 0x01,       // FLEVEL=0（fastest）
        DeflateProfile::FixedHuffman => 0x9C, // FLEVEL=2（default）
    }
}

/// 打包完整 zlib 流：头 + DEFLATE(stored|fixed) + Adler-32(原始数据)。
pub(crate) fn zlib_compress(data: &[u8], profile: DeflateProfile) -> Vec<u8> {
    let cmf: u8 = 0x78;
    let flg = header_flag(profile);
    debug_assert_eq!(((cmf as u16) << 8 | flg as u16) % 31, 0);
    let mut out = Vec::with_capacity(data.len() / 2 + 64);
    out.push(cmf);
    out.push(flg);
    out.extend_from_slice(&deflate(data, profile));
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 经典向量：空 → 1；"Wikipedia" → 0x11E60398。
    #[test]
    fn adler_known_vectors() {
        assert_eq!(adler32(&[]), 1);
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
        // 长输入跨 NMAX 分块：分块与整段等价（分块算法即定义）。
        let data: Vec<u8> = (0..=255u8).cycle().take(20_000).collect();
        let (mut s1, mut s2) = (1u64, 0u64);
        for &b in &data {
            s1 = (s1 + b as u64) % 65_521;
            s2 = (s2 + s1) % 65_521;
        }
        assert_eq!(adler32(&data), ((s2 as u32) << 16) | s1 as u32);
    }

    /// 两种档位的头都满足 `(CMF<<8|FLG) % 31 == 0`，尾四字节为原始
    /// 数据 Adler-32 大端。
    #[test]
    fn zlib_framing() {
        for profile in [DeflateProfile::Stored, DeflateProfile::FixedHuffman] {
            let data = b"hello hello hello".as_slice();
            let z = zlib_compress(data, profile);
            assert_eq!(&z[0..2], &[0x78, header_flag(profile)]);
            assert_eq!((0x78u32 * 256 + header_flag(profile) as u32) % 31, 0);
            assert_eq!(&z[z.len() - 4..], &adler32(data).to_be_bytes());
        }
    }
}
