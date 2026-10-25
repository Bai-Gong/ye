//! CRC-32（IEEE 802.3，即 PNG/IHDR 等 chunk 校验用的多项式）。
//!
//! 查表法（256 项，运行时由多项式 0xEDB88320 生成，无静态大表），
//! 初值 0xFFFFFFFF、结果异或 0xFFFFFFFF——与 `zlib.crc32`/PNG 规范
//! 一致。输入按前缀增量接口 [`Crc32::update`] 提供，整段便捷入口
//! [`crc32`]。

/// 整段数据的 CRC-32（IEEE）。
pub fn crc32(bytes: &[u8]) -> u32 {
    let mut c = Crc32::new();
    c.update(bytes);
    c.finish()
}

/// 增量 CRC-32 计算器（chunk 逐段校验用）。
#[derive(Debug, Clone)]
pub struct Crc32 {
    /// 当前寄存器（已含初值翻转，`finish` 前处于「未异或」状态）。
    state: u32,
}

impl Crc32 {
    /// 新建计算器（等价于空数据的 CRC 初态）。
    pub fn new() -> Self {
        Crc32 { state: 0xFFFF_FFFF }
    }

    /// 并入一段数据（可多次调用，按序拼接）。
    pub fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.state = TABLE[((self.state ^ b as u32) & 0xFF) as usize] ^ (self.state >> 8);
        }
    }

    /// 收尾：异或输出得到最终 CRC-32 值（不改变后续继续 update 的能力
    /// 语义——如需续算请勿复用已 finish 的语义，直接继续 update 即可，
    /// 本方法本身不破坏内部状态）。
    pub fn finish(&self) -> u32 {
        self.state ^ 0xFFFF_FFFF
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

/// 256 项查表（由反射多项式 0xEDB88320 生成）。
static TABLE: [u32; 256] = build_table();

const fn build_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            // 反射多项式的位驱动：最低位为 1 则异或多项式（右移方向）。
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 经典向量："123456789" → 0xCBF43926（CRC-32/ISO-HDLC 标准检验值）。
    #[test]
    fn known_vector() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    /// 空数据的 CRC（初值翻转）：0x00000000。
    #[test]
    fn empty() {
        assert_eq!(crc32(&[]), 0);
    }

    /// 增量接口与整段接口等价（任意切分点）。
    #[test]
    fn incremental_equals_oneshot() {
        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        let whole = crc32(&data);
        let mut c = Crc32::new();
        c.update(&data[..7]);
        c.update(&data[7..700]);
        c.update(&data[700..]);
        assert_eq!(c.finish(), whole);
    }

    /// "IEND" 空 chunk 的 CRC（PNG 规范示例值 0xAE426082）。
    #[test]
    fn iend_chunk_crc() {
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
    }
}
