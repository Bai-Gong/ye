//! DEFLATE 压缩（RFC 1951）——**只压缩，不解压**。
//!
//! 两档profile（施工图④ M2）：
//!
//! - [`DeflateProfile::Stored`]：stored 块（BTYPE=00，不压缩，分块上限
//!   65535 字节）——实现最简、解码端零容差验证的基线档；
//! - [`DeflateProfile::FixedHuffman`]：fixed-Huffman（BTYPE=01）+ 自实现
//!   LZ77（哈希链 + 贪心带一步 lazy），码表为 RFC 1951 §3.2.6 固定表。
//!
//! dynamic-Huffman（BTYPE=10）不在本阶段（按需后补）。位序约定：
//! Huffman 码按码长从高位到低位写入（对 LSB-first 打包器即逐位反转），
//! 其余数据元素（块头、LEN/NLEN、长度/距离 extra bits）按 LSB-first
//! 整数写入（RFC 1951 §3.1.1）。

/// 压缩档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeflateProfile {
    /// stored 块（无压缩，透明分块）。
    Stored,
    /// fixed-Huffman + LZ77（哈希链贪心 + 一步 lazy）。
    FixedHuffman,
}

/// LZ77 最小/最大匹配长度（RFC 1951）。
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
/// 滑动窗口（32 KiB，与 zlib 头 CINFO=7 一致）。
const WINDOW: usize = 32_768;
/// 哈希表位数（32768 桶）。
const HASH_BITS: u32 = 15;
/// 单位置哈希链的最大回溯步数（截断只损压缩率，不影响正确性）。
const MAX_CHAIN: usize = 128;

/// 压缩 `data` 为 DEFLATE 码流（不含 zlib 头尾）。
pub(crate) fn deflate(data: &[u8], profile: DeflateProfile) -> Vec<u8> {
    match profile {
        DeflateProfile::Stored => deflate_stored(data),
        DeflateProfile::FixedHuffman => deflate_fixed(data),
    }
}

// —— stored 块 ——————————————————————————————————————————————

/// stored 块序列：每块 ≤65535 字节，最后一块 BFINAL=1。
fn deflate_stored(data: &[u8]) -> Vec<u8> {
    let mut w = BitWriter::new();
    // 空数据也产出合法的单空块（BFINAL=1, BTYPE=00, LEN=0）。
    let mut chunks: Vec<&[u8]> = data.chunks(65_535).collect();
    if chunks.is_empty() {
        chunks.push(&[]);
    }
    let last = chunks.len() - 1;
    for (i, chunk) in chunks.iter().enumerate() {
        w.write_bits((i == last) as u32, 1);
        w.write_bits(0b00, 2);
        w.align_byte();
        let len = chunk.len() as u16;
        w.write_u16_le(len);
        w.write_u16_le(!len);
        w.write_bytes(chunk);
    }
    w.finish()
}

// —— fixed-Huffman + LZ77 ————————————————————————————————————

/// 长度码 257..=285 的 (基准长度, extra bit 位数)——RFC 1951 §3.2.5。
const LENGTH_CODES: [(u16, u32); 29] = [
    (3, 0),
    (4, 0),
    (5, 0),
    (6, 0),
    (7, 0),
    (8, 0),
    (9, 0),
    (10, 0),
    (11, 1),
    (13, 1),
    (15, 1),
    (17, 1),
    (19, 2),
    (23, 2),
    (27, 2),
    (31, 2),
    (35, 3),
    (43, 3),
    (51, 3),
    (59, 3),
    (67, 4),
    (83, 4),
    (99, 4),
    (115, 4),
    (131, 5),
    (163, 5),
    (195, 5),
    (227, 5),
    (258, 0),
];

/// 距离码 0..=29 的 (基准距离, extra bit 位数)——RFC 1951 §3.2.5。
const DIST_CODES: [(u16, u32); 30] = [
    (1, 0),
    (2, 0),
    (3, 0),
    (4, 0),
    (5, 1),
    (7, 1),
    (9, 2),
    (13, 2),
    (17, 3),
    (25, 3),
    (33, 4),
    (49, 4),
    (65, 5),
    (97, 5),
    (129, 6),
    (193, 6),
    (257, 7),
    (385, 7),
    (513, 8),
    (769, 8),
    (1025, 9),
    (1537, 9),
    (2049, 10),
    (3073, 10),
    (4097, 11),
    (6145, 11),
    (8193, 12),
    (12289, 12),
    (16385, 13),
    (24577, 13),
];

/// fixed-Huffman 码表：符号 → (反转后的码字, 位数)，可直接 LSB-first 写入。
struct FixedCodes {
    lit: [(u32, u32); 288],
    dist: [(u32, u32); 30],
}

impl FixedCodes {
    /// 由 RFC 1951 §3.2.6 的固定码长做规范 Huffman 赋码（§3.2.2 算法），
    /// 再按位反转适配 LSB-first 打包。距离表全部 5 位，符号即码值。
    fn build() -> FixedCodes {
        // 字面/长度码长：0-143→8、144-255→9、256-279→7、280-287→8。
        let mut lit_lens = [0u32; 288];
        for (sym, len) in lit_lens.iter_mut().enumerate() {
            *len = match sym {
                0..=143 => 8,
                144..=255 => 9,
                256..=279 => 7,
                _ => 8,
            };
        }
        let mut lit = [(0u32, 0u32); 288];
        assign_canonical(&lit_lens, &mut lit);
        for entry in &mut lit {
            entry.0 = reverse_bits(entry.0, entry.1);
        }

        let mut dist = [(0u32, 5u32); 30];
        for (sym, entry) in dist.iter_mut().enumerate() {
            // 等长规范赋码即「符号值 = 码值」；再反转。
            entry.0 = reverse_bits(sym as u32, 5);
        }
        FixedCodes { lit, dist }
    }

    /// 长度（3..=258）→ (符号, extra 值, extra 位数)。查表从长到短
    /// 线性扫描（29 项，编码热路径上限可控）。
    fn length_symbol(length: usize) -> (usize, u32, u32) {
        debug_assert!((MIN_MATCH..=MAX_MATCH).contains(&length));
        // LENGTH_CODES 升序排列且末项 (258,0)：从高位找第一个基准 ≤ length。
        for idx in (0..LENGTH_CODES.len()).rev() {
            let (base, extra_bits) = LENGTH_CODES[idx];
            if length >= base as usize {
                return (257 + idx, (length - base as usize) as u32, extra_bits);
            }
        }
        unreachable!("length {length} out of range")
    }

    /// 距离（1..=32768）→ (符号, extra 值, extra 位数)。
    fn dist_symbol(dist: usize) -> (usize, u32, u32) {
        debug_assert!((1..=WINDOW).contains(&dist));
        for idx in (0..DIST_CODES.len()).rev() {
            let (base, extra_bits) = DIST_CODES[idx];
            if dist >= base as usize {
                return (idx, (dist - base as usize) as u32, extra_bits);
            }
        }
        unreachable!("dist {dist} out of range")
    }
}

/// 规范 Huffman 赋码（RFC 1951 §3.2.2）：按码长升序、同长按符号升序
/// 递增赋码。输入码长表，输出 (码字, 位数)（未反转，MSB-first 语义）。
fn assign_canonical(lengths: &[u32], out: &mut [(u32, u32)]) {
    let max_len = lengths.iter().copied().max().unwrap_or(0) as usize;
    let mut bl_count = vec![0u32; max_len + 1];
    for &len in lengths {
        if len > 0 {
            bl_count[len as usize] += 1;
        }
    }
    let mut next_code = vec![0u32; max_len + 1];
    let mut code = 0u32;
    for bits in 1..=max_len {
        code = (code + bl_count[bits - 1]) << 1;
        next_code[bits] = code;
    }
    for (sym, &len) in lengths.iter().enumerate() {
        if len > 0 {
            out[sym] = (next_code[len as usize], len);
            next_code[len as usize] += 1;
        }
    }
}

/// 按位反转（Huffman 码 MSB-first → LSB-first 打包）。
fn reverse_bits(code: u32, len: u32) -> u32 {
    let mut out = 0u32;
    for i in 0..len {
        out |= ((code >> i) & 1) << (len - 1 - i);
    }
    out
}

/// LZ77 匹配器：哈希链（head=最新位置，prev=同哈希更旧位置）。
struct Matcher<'a> {
    data: &'a [u8],
    head: Vec<i32>,
    prev: Vec<i32>,
}

impl<'a> Matcher<'a> {
    fn new(data: &'a [u8]) -> Matcher<'a> {
        Matcher {
            data,
            head: vec![-1; 1 << HASH_BITS],
            prev: vec![-1; data.len()],
        }
    }

    /// 位置 `i` 起三字节的哈希（不足三字节返回 None）。
    fn hash_at(&self, i: usize) -> Option<usize> {
        let d = self.data;
        if i + MIN_MATCH > d.len() {
            return None;
        }
        let v = u32::from_le_bytes([d[i], d[i + 1], d[i + 2], 0]);
        Some(((v.wrapping_mul(0x9E37_79B1)) >> (32 - HASH_BITS)) as usize)
    }

    /// 把位置 `i` 插入哈希链（链上位置恒 < 后续搜索位置，距离 ≥1）。
    fn insert(&mut self, i: usize) {
        if let Some(h) = self.hash_at(i) {
            self.prev[i] = self.head[h];
            self.head[h] = i as i32;
        }
    }

    /// 在 `i` 处找最优匹配，返回 (长度, 距离)；无匹配 → (0, 0)。
    /// 链按位置递减回溯，距离超窗即可断；匹配允许重叠（解码端逐字节
    /// 复制语义），比较即普通字节比对。
    fn find_match(&self, i: usize) -> (usize, usize) {
        let Some(h) = self.hash_at(i) else {
            return (0, 0);
        };
        let max_len = (self.data.len() - i).min(MAX_MATCH);
        if max_len < MIN_MATCH {
            return (0, 0);
        }
        let oldest = i.saturating_sub(WINDOW) as i32;
        let mut cand = self.head[h];
        let mut best = (0usize, 0usize);
        let mut tries = MAX_CHAIN;
        while cand >= 0 && cand >= oldest && tries > 0 {
            let dist = i - cand as usize;
            debug_assert!(dist >= 1);
            let mut len = 0;
            while len < max_len && self.data[cand as usize + len] == self.data[i + len] {
                len += 1;
            }
            if len > best.0 {
                best = (len, dist);
                if len >= MAX_MATCH {
                    break; // nice-length：已到上限
                }
            }
            cand = self.prev[cand as usize];
            tries -= 1;
        }
        best
    }
}

/// fixed-Huffman 压缩主流程：贪心 + 一步 lazy（zlib 同型策略）。
fn deflate_fixed(data: &[u8]) -> Vec<u8> {
    let codes = FixedCodes::build();
    let mut w = BitWriter::new();
    w.write_bits(1, 1); // BFINAL=1（单块）
    w.write_bits(0b01, 2); // BTYPE=01：fixed-Huffman

    let mut m = Matcher::new(data);
    let n = data.len();
    let mut i = 0usize;
    while i < n {
        // 先搜索后入链：链上候选恒 < i（距离 ≥1，无自匹配）。
        let (len, dist) = m.find_match(i);
        m.insert(i);
        if len < MIN_MATCH {
            let (code, bits) = codes.lit[data[i] as usize];
            w.write_bits(code, bits);
            i += 1;
            continue;
        }
        // lazy：若 i+1 处有更长匹配，则当前位置出字面量、延后一步。
        if i + 1 < n {
            let (next_len, _) = {
                // i+1 尚未入链（insert 只对已决策位置调用），搜索安全。
                m.find_match(i + 1)
            };
            if next_len > len {
                let (code, bits) = codes.lit[data[i] as usize];
                w.write_bits(code, bits);
                i += 1;
                continue;
            }
        }
        // 采纳匹配：写 (长度码 + extra)(距离码 + extra)，被覆盖位置
        // 全部入链（i 已入，i+1..i+len 现在入）。
        let (sym, extra, extra_bits) = FixedCodes::length_symbol(len);
        let (dcode, dextra, dextra_bits) = FixedCodes::dist_symbol(dist);
        let (lcode, lbits) = codes.lit[sym];
        w.write_bits(lcode, lbits);
        if extra_bits > 0 {
            w.write_bits(extra, extra_bits);
        }
        let (dcode_val, dbits) = codes.dist[dcode];
        w.write_bits(dcode_val, dbits);
        if dextra_bits > 0 {
            w.write_bits(dextra, dextra_bits);
        }
        for p in i + 1..i + len {
            m.insert(p);
        }
        i += len;
    }

    // 块结束符：字面/长度字母表的符号 256。
    let (eob, eob_bits) = codes.lit[256];
    w.write_bits(eob, eob_bits);
    w.finish()
}

// —— 位写入器（LSB-first，RFC 1951 §3.1.1）———————————————————

struct BitWriter {
    out: Vec<u8>,
    /// 位积累缓冲（低 `bit_count` 位有效）。
    acc: u32,
    bit_count: u32,
}

impl BitWriter {
    fn new() -> BitWriter {
        BitWriter {
            out: Vec::new(),
            acc: 0,
            bit_count: 0,
        }
    }

    /// 写入 `value` 的低 `count` 位（LSB 先行）。`count ≤ 16`：调用点
    /// 最大单次写入 16 位（u16），accumulate 上限 16+16=32 不溢出。
    fn write_bits(&mut self, value: u32, count: u32) {
        debug_assert!(count <= 16);
        self.acc |= (value & ((1 << count) - 1)) << self.bit_count;
        self.bit_count += count;
        while self.bit_count >= 8 {
            self.out.push((self.acc & 0xFF) as u8);
            self.acc >>= 8;
            self.bit_count -= 8;
        }
    }

    /// 字节对齐（stored 块 LEN 前必须；补零位）。
    fn align_byte(&mut self) {
        if self.bit_count > 0 {
            self.out.push((self.acc & 0xFF) as u8);
            self.acc = 0;
            self.bit_count = 0;
        }
    }

    /// 对齐后写小端 u16。
    fn write_u16_le(&mut self, v: u16) {
        self.align_byte();
        self.out.extend_from_slice(&v.to_le_bytes());
    }

    /// 对齐后原样写字节串。
    fn write_bytes(&mut self, bytes: &[u8]) {
        self.align_byte();
        self.out.extend_from_slice(bytes);
    }

    fn finish(mut self) -> Vec<u8> {
        self.align_byte();
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fixed-Huffman 字面量码表与 RFC 1951 §3.2.6 逐点对拍
    /// （构建后反转回 MSB-first 与规范值比较）。
    #[test]
    fn fixed_lit_codes_match_rfc() {
        let codes = FixedCodes::build();
        let rfc: &[(usize, u32, u32)] = &[
            (0, 0b0011_0000, 8),
            (143, 0b1011_1111, 8),
            (144, 0b1_1001_0000, 9),
            (255, 0b1_1111_1111, 9),
            (256, 0b000_0000, 7),
            (279, 0b001_0111, 7),
            (280, 0b1100_0000, 8),
            (287, 0b1100_0111, 8),
        ];
        for &(sym, want, bits) in rfc {
            let (rev, got_bits) = codes.lit[sym];
            assert_eq!(got_bits, bits, "symbol {sym} bits");
            assert_eq!(reverse_bits(rev, got_bits), want, "symbol {sym} code");
        }
        // 距离码：符号 s ↔ 5 位码值 s。
        for sym in 0..30 {
            let (rev, bits) = codes.dist[sym];
            assert_eq!(bits, 5);
            assert_eq!(reverse_bits(rev, 5), sym as u32);
        }
    }

    /// 长度/距离符号划分对 RFC 表逐项自洽（基准 + extra 覆盖区间连续）。
    #[test]
    fn length_dist_symbol_tables() {
        // 长度 3..=258 全覆盖且符号单调。
        for len in MIN_MATCH..=MAX_MATCH {
            let (sym, extra, extra_bits) = FixedCodes::length_symbol(len);
            assert!((257..=285).contains(&sym));
            assert!(extra < (1 << extra_bits), "len {len} extra overflow");
        }
        assert_eq!(FixedCodes::length_symbol(3).0, 257);
        assert_eq!(FixedCodes::length_symbol(10).0, 264);
        assert_eq!(FixedCodes::length_symbol(11).0, 265);
        assert_eq!(FixedCodes::length_symbol(258).0, 285);
        assert_eq!(FixedCodes::length_symbol(257).0, 284);
        // 距离 1..=32768 全覆盖。
        for dist in 1..=WINDOW {
            let (sym, extra, extra_bits) = FixedCodes::dist_symbol(dist);
            assert!(sym < 30);
            assert!(extra < (1 << extra_bits), "dist {dist} extra overflow");
        }
        assert_eq!(FixedCodes::dist_symbol(1).0, 0);
        assert_eq!(FixedCodes::dist_symbol(4).0, 3);
        assert_eq!(FixedCodes::dist_symbol(32768).0, 29);
    }

    /// stored 块框架自检：块数 = ceil(n/65535)（空数据 1 块），
    /// LEN/NLEN 互补、载荷拼接等于原文（测试侧仅走框架，不做解压）。
    #[test]
    fn stored_blocks_frame_roundtrip() {
        for size in [0usize, 1, 100, 65_535, 65_536, 200_000] {
            let data: Vec<u8> = (0..size).map(|i| (i * 7 % 251) as u8).collect();
            let stream = deflate(&data, DeflateProfile::Stored);
            // 逐块解析。
            let mut pos = 0usize;
            let mut rebuilt = Vec::with_capacity(size);
            let mut blocks = 0usize;
            loop {
                assert!(pos < stream.len(), "size {size}: 缺块头");
                let bfinal = stream[pos] & 1;
                let btype = (stream[pos] >> 1) & 0b11;
                assert_eq!(btype, 0b00);
                let len = u16::from_le_bytes([stream[pos + 1], stream[pos + 2]]) as usize;
                let nlen = u16::from_le_bytes([stream[pos + 3], stream[pos + 4]]);
                assert_eq!(!len as u16, nlen);
                rebuilt.extend_from_slice(&stream[pos + 5..pos + 5 + len]);
                pos += 5 + len;
                blocks += 1;
                if bfinal == 1 {
                    break;
                }
            }
            assert_eq!(pos, stream.len());
            assert_eq!(blocks, size.div_ceil(65_535).max(1));
            assert_eq!(rebuilt, data);
        }
    }

    /// 位写入器 LSB-first 语义：单字节手算样例。
    #[test]
    fn bit_writer_packing() {
        let mut w = BitWriter::new();
        w.write_bits(0b1, 1);
        w.write_bits(0b00, 2);
        w.write_bits(0b101, 3);
        w.write_bits(0b10, 2);
        // 字节 = 1 | 00<<1 | 101<<3 | 10<<6 = 0b10_1010_01 = 0xA9
        assert_eq!(w.finish(), vec![0b1010_1001]);
        // 反转语义。
        assert_eq!(reverse_bits(0b0011_0000, 8), 0b0000_1100);
        assert_eq!(reverse_bits(0b1, 1), 0b1);
    }

    /// fixed-Huffman：结构上可压缩（重复数据明显小于原文；stored 路径
    /// 恒大于原文 + 块开销）。真实解码对拍在 tests/oracle_png.rs。
    #[test]
    fn fixed_huffman_shrinks_repetitive_data() {
        let data = vec![0xABu8; 100_000];
        let stored = deflate(&data, DeflateProfile::Stored);
        let fixed = deflate(&data, DeflateProfile::FixedHuffman);
        assert!(stored.len() > data.len());
        assert!(fixed.len() < data.len() / 10, "fixed len = {}", fixed.len());
        // 无重复数据也不劣化成错（长度有限即可，允许略涨——码长 8/9 位）。
        let noisy: Vec<u8> = (0..10_000u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        let _ = deflate(&noisy, DeflateProfile::FixedHuffman);
    }
}
