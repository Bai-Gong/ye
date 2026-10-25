//! PNG 行过滤器（PNG 规范 §9 "Filtering"）。
//!
//! 本阶段两档 + 自适应（施工图④：Sub/None 自适应或固定，写明取舍）：
//!
//! - [`FilterStrategy::None`]：所有行 filter type 0（原始字节直传）；
//! - [`FilterStrategy::Sub`]：所有行 filter type 1（左邻同通道差值，
//!   RGBA 字节深度 bpp=4）；
//! - [`FilterStrategy::Adaptive`]：逐行在 None/Sub 间择优——代价函数
//!   取过滤后各字节**按有符号解释的绝对值之和**（规范建议启发式），
//!   平坦/渐变区域 Sub 显著更小、噪声区域 None 更稳。等分取 None
//!   （更简单的路径）。Up/Paeth/Average 与逐行混合多类未做——M2
//!   消费方（剪抽帧缩略图）为自然图像，None/Sub 自适应已覆盖主要
//!   收益，按需后补。

/// 过滤策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterStrategy {
    /// 全部行 filter type 0（不过滤）。
    None,
    /// 全部行 filter type 1（Sub：减去左侧像素同通道字节）。
    Sub,
    /// 逐行 None/Sub 择优（最小绝对差和启发式）。
    Adaptive,
}

/// 对紧排 RGBA 数据施加行过滤，产出 PNG IDAT 的原始扫描线流：
/// 每行前置 1 字节过滤类型。
pub(crate) fn apply(rgba: &[u8], width: usize, height: usize, strategy: FilterStrategy) -> Vec<u8> {
    let stride = width * 4; // 紧排输入（调用方已校验长度）
    let mut out = Vec::with_capacity(height * (stride + 1));
    let mut sub_row = vec![0u8; stride];
    for y in 0..height {
        let row = &rgba[y * stride..(y + 1) * stride];
        let (ftype, bytes) = match strategy {
            FilterStrategy::None => (0u8, row),
            FilterStrategy::Sub => {
                filter_sub_row(row, &mut sub_row);
                (1u8, sub_row.as_slice())
            }
            FilterStrategy::Adaptive => {
                filter_sub_row(row, &mut sub_row);
                // 有符号绝对差和：字节 b 解释为 b≥128 ? 256-b : b。
                let cost_sub: u32 = sub_row.iter().map(|&b| sub_abs(b)).sum();
                let cost_none: u32 = row.iter().map(|&b| sub_abs(b)).sum();
                if cost_sub < cost_none {
                    (1u8, sub_row.as_slice())
                } else {
                    (0u8, row)
                }
            }
        };
        out.push(ftype);
        out.extend_from_slice(bytes);
    }
    out
}

/// Sub 过滤单行：`out[i] = row[i] - row[i-4]`（前 bpp=4 字节原样）。
/// 模 256 回绕即 u8 减法语义。
fn filter_sub_row(row: &[u8], out: &mut [u8]) {
    let bpp = 4usize;
    out[..bpp].copy_from_slice(&row[..bpp]);
    for i in bpp..row.len() {
        out[i] = row[i].wrapping_sub(row[i - bpp]);
    }
}

/// 过滤字节的绝对值（有符号解释：0 与 128 等价，都最便宜）。
fn sub_abs(b: u8) -> u32 {
    let v = b as i8 as i32; // 回绕到 [-128,127]
    v.unsigned_abs()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sub 行过滤手工样例：差值回绕正确。
    #[test]
    fn sub_row_arithmetic() {
        let row = [10u8, 200, 0, 255, 10, 201, 1, 254, 11, 199, 2, 0];
        let mut out = vec![0u8; 12];
        filter_sub_row(&row, &mut out);
        assert_eq!(out[..4], row[..4]);
        assert_eq!(out[4], 0);
        assert_eq!(out[5], 1);
        assert_eq!(out[6], 1);
        assert_eq!(out[7], 255); // 254 - 255 = -1 → 255
    }

    /// None/Sub/Adaptive 的输出布局：每行 type 字节 + stride 字节。
    #[test]
    fn layout_per_strategy() {
        // 2x1：平坦 → Sub 全 0 差；噪声 → Adaptive 逐行选 None。
        let flat = vec![7u8; 8];
        let noisy = vec![1u8, 90, 200, 33, 250, 3, 77, 128];
        let s = apply(&flat, 2, 1, FilterStrategy::Sub);
        assert_eq!(s, [1u8, 7, 7, 7, 7, 0, 0, 0, 0]);
        let a = apply(&flat, 2, 1, FilterStrategy::Adaptive);
        assert_eq!(a[0], 1, "平坦行应选 Sub");
        let a = apply(&noisy, 2, 1, FilterStrategy::Adaptive);
        assert_eq!(a[0], 0, "噪声行应选 None");
        assert_eq!(&a[1..], &noisy[..]);
        let n = apply(&noisy, 2, 1, FilterStrategy::None);
        assert_eq!(n, [0u8, 1, 90, 200, 33, 250, 3, 77, 128]);
    }

    /// 多行各自独立（无跨行过滤，Up 未实现）且行序正确。
    #[test]
    fn rows_independent() {
        let head = [0u8; 4];
        let mut rgba = vec![0u8; 4 * 4 * 2];
        rgba[4..8].copy_from_slice(&head); // 两行相同
        let s = apply(&rgba, 4, 2, FilterStrategy::None);
        assert_eq!(s.len(), 2 * (4 * 4 + 1));
        assert_eq!(&s[1..17], &rgba[0..16]);
    }
}
