//! 编码侧位写入器：MSB-first（与解码侧 [`crate::syntax::BitReader`] 对齐）。
//!
//! ## 位序裁决
//!
//! AAC GA 位流（ISO/IEC 14496-3）在**字节内自最高有效位**逐位写入：
//! 本 crate 解码器的 [`crate::syntax::BitReader`] 按
//! `(byte >> (7 − pos%8)) & 1` 读位且经 FFmpeg 一致性向量对拍验证，
//! 编码侧写入器与之严格镜像（同一位序、同一 Huffman 码字截取口径：
//! `CODES[sym]` 的低 `BITS[sym]` 位依序入流）。
//!
//! 容量不做运行时防御：调用方按 ADTS 13 位帧长上限约束预算
//! （见 [`super::element`] 的码率环），写入器只管追加。补零对齐
//! （[`BitWriter::align_zero`]）对应 AAC 句法的 byte_aligned 收尾。

/// MSB-first 位写入器（`u32::MAX` 位宽上界：句法最宽字段 24 位）。
#[derive(Default)]
pub(crate) struct BitWriter {
    bytes: Vec<u8>,
    /// 已写入总位数（`bytes.len()*8 − 尾字节空位`）。
    bit_len: usize,
}

impl BitWriter {
    /// 空写入器。
    pub(crate) fn new() -> BitWriter {
        BitWriter::default()
    }

    /// 写入 `v` 的低 `n` 位（高位在前；`n` ≤ 32，调用方句法保证）。
    pub(crate) fn put(&mut self, v: u32, n: u32) {
        debug_assert!(n <= 32);
        for i in (0..n).rev() {
            let bit = (v >> i) & 1;
            if self.bit_len.is_multiple_of(8) {
                self.bytes.push(0);
            }
            let last = self.bytes.last_mut().expect("刚 push");
            *last |= (bit as u8) << (7 - self.bit_len % 8);
            self.bit_len += 1;
        }
    }

    /// 写入单个位。
    pub(crate) fn put_bit(&mut self, bit: bool) {
        self.put(u32::from(bit), 1);
    }

    /// 补零到字节边界（未满则置 0 填充）。
    pub(crate) fn align_zero(&mut self) {
        let rem = self.bit_len % 8;
        if rem != 0 {
            self.put(0, 8 - rem as u32);
        }
    }

    /// 已写入位数。
    pub(crate) fn bit_len(&self) -> usize {
        self.bit_len
    }

    /// 收尾取载荷（须先 [`align_zero`；未对齐时尾位隐含丢弃属调用方
    /// 编程错误，故以 `debug_assert` 兜底）。
    pub(crate) fn into_bytes(self) -> Vec<u8> {
        debug_assert_eq!(self.bit_len % 8, 0);
        self.bytes
    }
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::BitReader;
    use crate::tables;

    /// 基础回环：put → 读回（ADTS 头位形态做样例）；跨字节字段、
    /// align、位长。
    #[test]
    fn put_read_roundtrip() {
        let mut w = BitWriter::new();
        w.put(0xFFF, 12);
        w.put(0, 1);
        w.put(0, 2);
        w.put(1, 1);
        w.put(1, 2); // profile = LC−1
        w.put(4, 4);
        w.put(0, 1);
        w.put(2, 3);
        w.put(0, 2);
        w.put(0, 2);
        w.put(7, 13);
        w.put(0x7FF, 11);
        w.put(0, 2);
        assert_eq!(w.bit_len(), 56);
        let bytes = w.into_bytes();
        assert_eq!(bytes.len(), 7);
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.bits(12).unwrap(), 0xFFF);
        assert_eq!(r.bits(1).unwrap(), 0);
        assert_eq!(r.bits(2).unwrap(), 0);
        assert_eq!(r.bits(1).unwrap(), 1);
        assert_eq!(r.bits(2).unwrap(), 1);
        assert_eq!(r.bits(4).unwrap(), 4);
        assert_eq!(r.bits(1).unwrap(), 0);
        assert_eq!(r.bits(3).unwrap(), 2);
        assert_eq!(r.bits(2).unwrap(), 0);
        assert_eq!(r.bits(2).unwrap(), 0);
        assert_eq!(r.bits(13).unwrap(), 7);
        assert_eq!(r.bits(11).unwrap(), 0x7FF);
        assert_eq!(r.bits(2).unwrap(), 0);
        assert_eq!(r.remaining(), 0);
    }

    /// 补零对齐：未满字节补 0；已对齐无操作。
    #[test]
    fn align_zero_pads() {
        let mut w = BitWriter::new();
        w.put(0b1, 1);
        w.align_zero();
        assert_eq!(w.bit_len(), 8);
        w.put(0b1, 1);
        w.align_zero();
        assert_eq!(w.bit_len(), 16);
        let b = w.into_bytes();
        assert_eq!(b, vec![0b1000_0000, 0b1000_0000]);
    }

    /// Huffman 写入：11 个谱码本 + scalefactor 码表逐符号写 → 解码器
    /// huff 读回同一符号（编码 = 按符号取 code/len 的口径验证）。
    #[test]
    fn huffman_roundtrip_all_codebooks() {
        for cb in 1u8..=11 {
            let codes = tables::codebook_codes(cb).unwrap();
            let bits = tables::codebook_bits(cb).unwrap();
            let mut w = BitWriter::new();
            for sym in 0..codes.len() {
                w.put(u32::from(codes[sym]), u32::from(bits[sym]));
            }
            w.align_zero();
            let bytes = w.into_bytes();
            let mut r = BitReader::new(&bytes);
            for sym in 0..codes.len() {
                assert_eq!(
                    r.spectral_symbol(cb).unwrap(),
                    sym as u16,
                    "cb={cb} sym={sym}"
                );
            }
        }
        // scalefactor 码表：符号 = delta+60
        let huff = tables::scalefactor_huff();
        let (codes, bits) = tables::scalefactor_codes();
        let mut w = BitWriter::new();
        for (&code, &len) in codes.iter().zip(bits.iter()) {
            w.put(code, u32::from(len));
        }
        w.align_zero();
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        for sym in 0..121i32 {
            assert_eq!(r.huff(huff).unwrap() as i32, sym, "sf sym={sym}");
        }
        // delta 语义抽查：符号 = delta+60（delta 0 = 符号 60，1 位码 0）
        let mut w2 = BitWriter::new();
        w2.put(codes[60], u32::from(bits[60]));
        w2.align_zero();
        let b2 = w2.into_bytes();
        let mut r2 = BitReader::new(&b2);
        assert_eq!(r2.sf_delta().unwrap(), 0);
        r2.byte_align();
        assert_eq!(r2.remaining(), 0);
    }
}
