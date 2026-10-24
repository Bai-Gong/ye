//! Exp-Golomb 位读取器（H.264 §9.1/§9.2：u(n)/ue(v)/se(v)）。
//!
//! 输入是**已去防竞争**的 RBSP 字节（见 [`crate::unescape_rbsp`]）。
//! 语义约定：任何越界（位不够读、Exp-Golomb 前导零超长）一律
//! `Err(H264Error::InvalidData)`，绝不 panic、绝不静默补零——
//! 数据驱动的失败全走 `Result`（全库约定）。

use crate::error::H264Error;

/// 大端位序（MSB first）的只进位读取器。
///
/// derive 清单：`Debug, Clone, Copy`（纯只读视图，借用数据不拥有）。
#[derive(Debug, Clone, Copy)]
pub struct BitReader<'a> {
    data: &'a [u8],
    /// 已消费位数（绝对位置）。
    pos: usize,
}

impl<'a> BitReader<'a> {
    /// 绑定 RBSP 字节。
    pub fn new(data: &'a [u8]) -> Self {
        BitReader { data, pos: 0 }
    }

    /// 剩余可读位数。
    pub fn remaining_bits(&self) -> usize {
        self.data.len() * 8 - self.pos
    }

    /// 当前位偏移（诊断用，如错误信息带字段定位）。
    pub fn bit_position(&self) -> usize {
        self.pos
    }

    fn take_bit(&mut self) -> Result<u8, H264Error> {
        if self.remaining_bits() == 0 {
            return Err(H264Error::InvalidData(format!(
                "bit reader exhausted at bit {}: need 1 more bit",
                self.pos
            )));
        }
        let byte = self.data[self.pos / 8];
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Ok(bit)
    }

    /// 读 1 位（H.264 语法 u(1)）。
    pub fn u1(&mut self) -> Result<bool, H264Error> {
        Ok(self.take_bit()? == 1)
    }

    /// 读 n 位（H.264 语法 u(n)，MSB first）。n==0 或 n>64 →
    /// [`H264Error::InvalidArguments`]（调用方误用，非数据错误）。
    pub fn u(&mut self, n: u32) -> Result<u64, H264Error> {
        if n == 0 || n > 64 {
            return Err(H264Error::InvalidArguments("u() needs 1..=64 bits"));
        }
        let mut v: u64 = 0;
        for _ in 0..n {
            v = (v << 1) | u64::from(self.take_bit()?);
        }
        Ok(v)
    }

    /// 无符号 Exp-Golomb（ue(v)，§9.1）：前导零计数 k，随后 k 位尾数，
    /// 值 = 2^k − 1 + 尾数。前导零 > 31（码长 > 32 位）视为畸形数据。
    pub fn ue(&mut self) -> Result<u64, H264Error> {
        let mut leading_zeros: u32 = 0;
        loop {
            if !self.u1()? {
                leading_zeros += 1;
                if leading_zeros > 31 {
                    return Err(H264Error::InvalidData(format!(
                        "exp-golomb ue() longer than 32 bits at bit {}",
                        self.pos
                    )));
                }
            } else {
                break;
            }
        }
        // 尾数 k 位（可为 0 位）
        let mut v: u64 = 1; // 隐含的前导 1
        for _ in 0..leading_zeros {
            v = (v << 1) | u64::from(self.u1()?);
        }
        Ok(v - 1)
    }

    /// 有符号 Exp-Golomb（se(v)，§9.2 表 9-3 的双射映射）：
    /// codeNum k 偶 → −k/2，奇 → (k+1)/2。
    pub fn se(&mut self) -> Result<i64, H264Error> {
        let k = self.ue()?;
        if k % 2 == 0 {
            Ok(-(k as i64 / 2))
        } else {
            Ok((k as i64 + 1) / 2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手工位串 → 字节（补零到字节边界）。
    fn bits(s: &str) -> Vec<u8> {
        assert!(s.len() <= 64);
        let mut bytes = Vec::with_capacity(s.len().div_ceil(8));
        let mut acc: u8 = 0;
        let mut n: u32 = 0;
        for c in s.chars() {
            acc = (acc << 1) | u8::from(c == '1');
            n += 1;
            if n == 8 {
                bytes.push(acc);
                acc = 0;
                n = 0;
            }
        }
        if n > 0 {
            bytes.push(acc << (8 - n));
        }
        bytes
    }

    #[test]
    fn u_reads_msb_first() {
        let data = bits("1011001111000011"); // 0xB3 0xC3
        let mut r = BitReader::new(&data);
        assert!(r.u1().unwrap());
        assert!(!r.u1().unwrap());
        assert_eq!(r.u(2).unwrap(), 0b11);
        assert_eq!(r.u(3).unwrap(), 0b001);
        assert_eq!(r.u(8).unwrap(), 0b11100001);
        assert_eq!(r.remaining_bits(), 1);
        assert_eq!(r.bit_position(), 15);
        assert!(r.u1().unwrap()); // 第 16 位
        assert_eq!(r.remaining_bits(), 0);
        assert_eq!(r.bit_position(), 16);
    }

    #[test]
    fn u_crosses_byte_boundaries() {
        // 12 位值跨两字节：0x0F 0xF0 → 前 12 位 = 0x0FF
        let mut r = BitReader::new(&[0x0F, 0xF0]);
        assert_eq!(r.u(12).unwrap(), 0x0FF);
        assert_eq!(r.u(4).unwrap(), 0x0);
    }

    #[test]
    fn u_rejects_bad_widths() {
        let mut r = BitReader::new(&[0xFF]);
        assert!(matches!(
            r.u(0),
            Err(H264Error::InvalidArguments("u() needs 1..=64 bits"))
        ));
        assert!(matches!(
            r.u(65),
            Err(H264Error::InvalidArguments("u() needs 1..=64 bits"))
        ));
    }

    #[test]
    fn exhaustion_is_err_not_panic() {
        let mut r = BitReader::new(&[0xFF]);
        assert_eq!(r.u(8).unwrap(), 0xFF);
        assert!(matches!(r.u1(), Err(H264Error::InvalidData(_))));
        assert!(matches!(r.u(8), Err(H264Error::InvalidData(_))));
        // 半途耗尽：bits() 按字节边界补零，"101" 即 0b10100000（8 位）
        let three = bits("101");
        let mut r2 = BitReader::new(&three);
        assert_eq!(r2.u(3).unwrap(), 0b101);
        assert_eq!(r2.u(5).unwrap(), 0b00000);
        assert!(matches!(r2.u(1), Err(H264Error::InvalidData(_))));
    }

    /// ue(v) 编码表（§7.3.2.1 示例 / 表 9-2 的手工黄金对照）。
    #[test]
    fn ue_known_codewords() {
        // 值→码字：0→"1"，1→"010"，2→"011"，3→"00100"，4→"00101"，5→"00110"，6→"00111"
        let cases: &[(&str, u64)] = &[
            ("1", 0),
            ("010", 1),
            ("011", 2),
            ("00100", 3),
            ("00101", 4),
            ("00110", 5),
            ("00111", 6),
        ];
        for (codeword, value) in cases {
            let data = bits(codeword);
            let mut r = BitReader::new(&data);
            assert_eq!(r.ue().unwrap(), *value, "codeword {codeword}");
        }
        // 连续读取：3→4→0
        let stream = bits("00100001011");
        let mut r = BitReader::new(&stream);
        assert_eq!(r.ue().unwrap(), 3);
        assert_eq!(r.ue().unwrap(), 4);
        assert_eq!(r.ue().unwrap(), 0);
    }

    /// se(v) 编码表（表 9-3 的手工黄金对照）。
    #[test]
    fn se_known_codewords() {
        let cases: &[(&str, i64)] = &[
            ("1", 0),
            ("010", 1),
            ("011", -1),
            ("00100", 2),
            ("00101", -2),
            ("00110", 3),
            ("00111", -3),
        ];
        for (codeword, value) in cases {
            let data = bits(codeword);
            let mut r = BitReader::new(&data);
            assert_eq!(r.se().unwrap(), *value, "codeword {codeword}");
        }
    }

    #[test]
    fn ue_rejects_overlong_leading_zeros() {
        // 32 个前导零仍无 1 → 畸形（码长 > 32 位）
        let mut r = BitReader::new(&[0x00, 0x00, 0x00, 0x00, 0x00]);
        assert!(matches!(r.ue(), Err(H264Error::InvalidData(msg)) if msg.contains("exp-golomb")));
        // 前导零中途耗尽同样是错
        let mut r2 = BitReader::new(&[0x00]);
        assert!(matches!(r2.ue(), Err(H264Error::InvalidData(_))));
        // 尾数中途耗尽同样是错："00000001"——k=7 的终止 1 落在最后一位，
        // 7 位尾数无位可读
        let tail = bits("00000001");
        let mut r3 = BitReader::new(&tail);
        assert!(matches!(r3.ue(), Err(H264Error::InvalidData(_))));
    }

    #[test]
    fn reader_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<BitReader<'_>>();
    }
}
