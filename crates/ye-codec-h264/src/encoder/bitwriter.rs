//! Exp-Golomb 位写入器（H.264 §9.1/§9.2 写侧：u(1)/u(n)/ue(v)/se(v)）。
//!
//! [`crate::BitReader`] 的写侧对偶：大端位序（MSB first）、只进不退，
//! 产出**未防竞争的 RBSP 字节**。防竞争插入（§7.3.1 emulation
//! prevention）不在位累积层做——RBSP 定义即去防竞争后的形态，与读侧
//! 约定一致；整段 RBSP 定稿后由 [`escape_rbsp`] 一次完成，二者满足
//! `unescape_rbsp(escape_rbsp(x)) == x`（位级回环测试覆盖）。
//!
//! 语义约定（与读侧对称）：调用方参数越界——`u` 的位宽/值域、`ue`/`se`
//! 超出读侧可解范围（前导零 > 31，即 codeNum > 2^32 − 2）——一律
//! `Err(H264Error::InvalidArguments)`，绝不静默截断；位累积本身
//! （`Vec<u8>` 追加）不可失败，故 [`BitWriter::u1`] 不返回 `Result`。

use crate::error::H264Error;

/// ue(v) 可写的值上界：读侧前导零 ≤ 31 ⟹ codeNum ≤ 2^32 − 2
/// （`crate::bitreader::BitReader::ue` 拒收界的镜像）。
const UE_MAX: u64 = 4_294_967_294; // 2^32 − 2

/// se(v) 的 |v| 上界：codeNum = 2|v|−1（v>0）或 2|v|（v≤0）≤ UE_MAX。
const SE_ABS_MAX: i64 = 2_147_483_647; // (2^32 − 2) / 2

/// 大端位序（MSB first）的只进位写入器。
///
/// derive 清单：`Debug, Clone, Default`（`Vec<u8>` 拥有输出故不 `Copy`；
/// `Default` 与 [`BitWriter::new`] 等价——空缓冲、零位）。
#[derive(Debug, Clone, Default)]
pub(crate) struct BitWriter {
    /// 已定稿的整字节。
    buf: Vec<u8>,
    /// 当前未满字节（自 MSB 起累积）。
    cur: u8,
    /// `cur` 已填位数（0..8）。
    cur_len: u32,
}

impl BitWriter {
    /// 空写入器。
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 已写位数（诊断与回环测试的位级核对用；产品路径不消费——
    /// 测试专用面豁免 dead_code）。
    #[allow(dead_code)]
    pub(crate) fn bit_position(&self) -> usize {
        self.buf.len() * 8 + self.cur_len as usize
    }

    /// 写 1 位（H.264 语法 u(1)）。位累积不可失败。
    pub(crate) fn u1(&mut self, bit: bool) {
        self.cur = (self.cur << 1) | u8::from(bit);
        if self.cur_len == 7 {
            self.buf.push(self.cur);
            self.cur = 0;
            self.cur_len = 0;
        } else {
            self.cur_len += 1;
        }
    }

    /// 写 n 位（H.264 语法 u(n)，MSB first）。n==0 或 n>64，或 value
    /// 高位超出 n 位 → [`H264Error::InvalidArguments`]（读侧 `u`
    /// 的同款口径，调用方误用而非数据错误）。
    pub(crate) fn u(&mut self, n: u32, value: u64) -> Result<(), H264Error> {
        if n == 0 || n > 64 {
            return Err(H264Error::InvalidArguments("u() needs 1..=64 bits"));
        }
        if n < 64 && (value >> n) != 0 {
            return Err(H264Error::InvalidArguments("u() value exceeds n bits"));
        }
        let mut i = n;
        while i > 0 {
            i -= 1;
            self.u1((value >> i) & 1 == 1);
        }
        Ok(())
    }

    /// 无符号 Exp-Golomb（ue(v) 写侧）：k 个前导 0 + 终止 1 + k 位
    /// 尾数，`v + 1 = 2^k + info`。值域 v ≤ 2^32 − 2（读侧 `ue` 可解
    /// 范围的镜像，超界 → [`H264Error::InvalidArguments`]）。
    pub(crate) fn ue(&mut self, v: u64) -> Result<(), H264Error> {
        if v > UE_MAX {
            return Err(H264Error::InvalidArguments(
                "ue() value exceeds the 32-bit exp-golomb range",
            ));
        }
        let k = 63 - (v + 1).leading_zeros(); // floor(log2(v+1))，v>0 ⟹ ≤ 31
        for _ in 0..k {
            self.u1(false);
        }
        self.u1(true);
        if k > 0 {
            let info = (v + 1) - (1u64 << k);
            self.u(k, info)?;
        }
        Ok(())
    }

    /// 有符号 Exp-Golomb（se(v) 写侧，§9.2 表 9-3 映射）：
    /// v>0 → codeNum = 2v − 1，v≤0 → codeNum = −2v（读侧 `se`
    /// 的逆映射）。|v| ≤ 2^31 − 1，超界 → [`H264Error::InvalidArguments`]。
    pub(crate) fn se(&mut self, v: i64) -> Result<(), H264Error> {
        if !(-SE_ABS_MAX..=SE_ABS_MAX).contains(&v) {
            return Err(H264Error::InvalidArguments(
                "se() magnitude exceeds the 32-bit exp-golomb range",
            ));
        }
        let code = if v > 0 {
            v.unsigned_abs() * 2 - 1
        } else {
            v.unsigned_abs() * 2
        };
        self.ue(code)
    }

    /// rbsp_trailing_bits（§7.3.2.4 收尾）：stop bit `1` + 补零到字节
    /// 边界。已对齐时恰好写 1 位、落一个新字节（末字节 ≥ 0x80）。
    pub(crate) fn rbsp_trailing_bits(&mut self) {
        self.u1(true); // stop bit
        self.align_zeros();
    }

    /// 字节对齐（补零）。I_PCM 的 pcm_alignment_zero_bit、对齐类语法
    /// 元素用；读侧对应 `r.u(8 − bit)` 的零读。
    pub(crate) fn align_zeros(&mut self) {
        if self.cur_len > 0 {
            self.buf.push(self.cur << (8 - self.cur_len));
            self.cur = 0;
            self.cur_len = 0;
        }
    }

    /// 取出 RBSP 字节并复位写入器。未满字节以零补齐——语义完整的
    /// RBSP 应先经 [`BitWriter::rbsp_trailing_bits`] 收尾再取。
    pub(crate) fn take_rbsp(&mut self) -> Vec<u8> {
        self.align_zeros();
        core::mem::take(&mut self.buf)
    }
}

/// 写侧防竞争（§7.3.1）：RBSP 中每个 `00 00 x`（x ≤ 0x03）的两个
/// 00 之后插入 `0x03`。[`crate::unescape_rbsp`] 的对偶——
/// `unescape_rbsp(&escape_rbsp(x)) == x` 对任意字节串成立（含尾部
/// `00 00`：无后随字节即无插入；读侧对同形态同样不加干预）。
pub(crate) fn escape_rbsp(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 4);
    let mut i = 0usize;
    while i < rbsp.len() {
        if i + 2 < rbsp.len() && rbsp[i] == 0 && rbsp[i + 1] == 0 && rbsp[i + 2] <= 3 {
            out.extend_from_slice(&[0, 0, 3]);
            i += 2;
        } else {
            out.push(rbsp[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitreader::BitReader;
    use crate::nal::unescape_rbsp;

    /// 手工位串 → 字节（补零到字节边界），读侧测试同款。
    fn bits(s: &str) -> Vec<u8> {
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
    fn u1_and_u_are_msb_first() {
        let mut w = BitWriter::new();
        w.u1(true);
        w.u1(false);
        w.u(2, 0b11).unwrap();
        w.u(3, 0b001).unwrap();
        w.u(8, 0b11100001).unwrap();
        w.u1(true);
        assert_eq!(w.take_rbsp(), [0xB3, 0xC3]); // 读侧 u_reads_msb_first 同款位型
    }

    #[test]
    fn u_rejects_bad_widths_and_overflowing_values() {
        let mut w = BitWriter::new();
        assert!(matches!(
            w.u(0, 0),
            Err(H264Error::InvalidArguments("u() needs 1..=64 bits"))
        ));
        assert!(matches!(
            w.u(65, 0),
            Err(H264Error::InvalidArguments("u() needs 1..=64 bits"))
        ));
        assert!(matches!(
            w.u(3, 8),
            Err(H264Error::InvalidArguments("u() value exceeds n bits"))
        ));
        assert!(w.u(3, 7).is_ok());
        assert!(w.u(64, u64::MAX).is_ok()); // 64 位整宽不检查值域
        assert_eq!(w.bit_position(), 67);
    }

    /// ue(v) 黄金码字（表 9-2，与读侧 `ue_known_codewords` 同一对照）。
    #[test]
    fn ue_golden_codewords() {
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
            let mut w = BitWriter::new();
            w.ue(*value).unwrap();
            assert_eq!(w.take_rbsp(), bits(codeword), "value {value}");
        }
    }

    /// ue 值域边界：k 位前导零的段边界值 2^k−2 / 2^k−1 全段往返，
    /// 上界 2^32−2 可写、2^32−1 拒收（读侧前导零 > 31 的镜像）。
    #[test]
    fn ue_boundary_values_roundtrip() {
        let mut candidates: Vec<u64> = vec![0, 1, 2, 3, 4, 6, 7, 14, 30, 62, 1000];
        for k in 0..=31u32 {
            candidates.push((1u64 << k).saturating_sub(2)); // 段内最大
            candidates.push((1u64 << k) - 1); // 段首
        }
        candidates.push(u64::from(u32::MAX) - 1); // 全域上界 2^32−2
        for v in candidates {
            let mut w = BitWriter::new();
            w.ue(v).unwrap();
            let written = w.bit_position();
            let data = w.take_rbsp();
            let mut r = BitReader::new(&data);
            assert_eq!(r.ue().unwrap(), v, "value {v}");
            assert_eq!(r.bit_position(), written, "value {v} 恰好消费写出的位");
        }
        let mut w = BitWriter::new();
        assert!(matches!(
            w.ue(u64::from(u32::MAX)),
            Err(H264Error::InvalidArguments(_))
        ));
        assert!(matches!(
            w.ue(u64::MAX),
            Err(H264Error::InvalidArguments(_))
        ));
    }

    /// se(v) 黄金码字（表 9-3，与读侧 `se_known_codewords` 同一对照）。
    #[test]
    fn se_golden_codewords() {
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
            let mut w = BitWriter::new();
            w.se(*value).unwrap();
            assert_eq!(w.take_rbsp(), bits(codeword), "value {value}");
        }
    }

    /// se 边界值：±(2^31−1) 可写可读，±2^31 与 i64 极值拒收不 panic。
    #[test]
    fn se_boundary_values_roundtrip() {
        let abs_max = (i64::from(u32::MAX) - 1) / 2;
        for v in [0i64, 1, -1, 2, -2, 1000, -1000, abs_max, -abs_max] {
            let mut w = BitWriter::new();
            w.se(v).unwrap();
            let written = w.bit_position();
            let data = w.take_rbsp();
            let mut r = BitReader::new(&data);
            assert_eq!(r.se().unwrap(), v, "value {v}");
            assert_eq!(r.bit_position(), written, "value {v}");
        }
        for bad in [abs_max + 1, -abs_max - 1, i64::MAX, i64::MIN] {
            let mut w = BitWriter::new();
            assert!(
                matches!(w.se(bad), Err(H264Error::InvalidArguments(_))),
                "{bad}"
            );
        }
    }

    /// rbsp_trailing_bits：stop bit 1 + 补零；已对齐时新落一字节 0x80。
    #[test]
    fn rbsp_trailing_bits_pad_correctly() {
        let mut w = BitWriter::new();
        w.u(3, 0b101).unwrap();
        w.rbsp_trailing_bits();
        // "101" + "1" + "0000" = 0b1011_0000
        assert_eq!(w.take_rbsp(), [0b1011_0000]);

        let mut w = BitWriter::new();
        w.u(8, 0xFF).unwrap();
        w.rbsp_trailing_bits();
        assert_eq!(w.take_rbsp(), [0xFF, 0x80]);
    }

    #[test]
    fn align_zeros_and_take_rbsp_flush_partial_byte() {
        let mut w = BitWriter::new();
        w.u(3, 0b101).unwrap();
        assert_eq!(w.bit_position(), 3);
        w.align_zeros();
        assert_eq!(w.bit_position(), 8);
        assert_eq!(w.take_rbsp(), [0b1010_0000]);
        // 取出后复位：可继续写
        w.u1(true);
        assert_eq!(w.bit_position(), 1);
    }

    /// ue/u1 连续混合写 + 读回（跨字节边界的整段往返）。
    #[test]
    fn mixed_stream_roundtrip() {
        let mut w = BitWriter::new();
        w.ue(3).unwrap();
        w.ue(4).unwrap();
        w.ue(0).unwrap();
        w.u1(true);
        w.u(12, 0xABC).unwrap();
        w.se(-2).unwrap();
        let written = w.bit_position();
        let data = w.take_rbsp();
        let mut r = BitReader::new(&data);
        assert_eq!(r.ue().unwrap(), 3);
        assert_eq!(r.ue().unwrap(), 4);
        assert_eq!(r.ue().unwrap(), 0);
        assert!(r.u1().unwrap());
        assert_eq!(r.u(12).unwrap(), 0xABC);
        assert_eq!(r.se().unwrap(), -2);
        assert_eq!(r.bit_position(), written);
    }

    /// escape 黄金对照：需要插入 / 不插入 / 尾部 00 00 / 连发 00 的各形态。
    #[test]
    fn escape_rbsp_golden_vectors() {
        assert_eq!(
            escape_rbsp(&[0xAB, 0x00, 0x00, 0x01]),
            [0xAB, 0x00, 0x00, 0x03, 0x01]
        );
        assert_eq!(escape_rbsp(&[0x00, 0x00, 0x00]), [0x00, 0x00, 0x03, 0x00]);
        // 后随 > 0x03 不插；读侧 unescape 同样不碰
        assert_eq!(escape_rbsp(&[0x00, 0x00, 0x04]), [0x00, 0x00, 0x04]);
        // 尾部 00 00 无后随字节：不插（读侧同样不加干预）
        assert_eq!(escape_rbsp(&[0x00, 0x00]), [0x00, 0x00]);
        // RBSP 里本就含 00 00 03：插出的 00 00 03 03 经宽松 unescape 原样还原
        assert_eq!(
            escape_rbsp(&[0x00, 0x00, 0x03, 0x03]),
            [0x00, 0x00, 0x03, 0x03, 0x03]
        );
        // 干净数据原样
        let clean = [0x12, 0x34, 0x56, 0xFF];
        assert_eq!(escape_rbsp(&clean), clean);
    }

    /// escape ⇄ unescape 全形态回环：黄金形态 + 确定性伪随机字节串。
    #[test]
    fn escape_unescape_roundtrip_identity() {
        let mut cases: Vec<Vec<u8>> = vec![
            vec![],
            vec![0],
            vec![0, 0],
            vec![0; 10],
            vec![0x00, 0x00, 0x00, 0x00, 0x01],
            vec![0xFF, 0x00, 0x00, 0x02, 0x00, 0x00, 0x03],
        ];
        // 简易 LCG（对齐 ye-filter 测试先例的确定性向量风格）
        let mut state = 0x2545F491u32;
        let mut pseudo = Vec::with_capacity(1024);
        for _ in 0..1024 {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            pseudo.push((state >> 16) as u8);
        }
        pseudo.extend_from_slice(&[0u8; 8]);
        cases.push(pseudo);
        for case in cases {
            let escaped = escape_rbsp(&case);
            assert_eq!(
                unescape_rbsp(&escaped).as_ref(),
                &case[..],
                "roundtrip failed for {case:02X?}"
            );
        }
    }

    #[test]
    fn writer_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<BitWriter>();
    }
}
