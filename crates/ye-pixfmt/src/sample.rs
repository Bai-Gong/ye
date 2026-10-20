//! 音频采样格式。

/// 音频采样格式（`P` 后缀 = 平面 planar，每声道一块连续缓冲；其余为 packed 交织）。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq, Hash`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SampleFormat {
    /// 8 bit 无符号，packed。
    U8,
    /// 8 bit 无符号，planar。
    U8P,
    /// 16 bit 有符号整数，packed。
    I16,
    /// 16 bit 有符号整数，planar。
    I16P,
    /// 32 bit 有符号整数，packed。
    I32,
    /// 32 bit 有符号整数，planar。
    I32P,
    /// 64 bit 有符号整数，packed。
    I64,
    /// 64 bit 有符号整数，planar。
    I64P,
    /// 32 bit IEEE 浮点，packed。
    F32,
    /// 32 bit IEEE 浮点，planar。
    F32P,
    /// 64 bit IEEE 浮点，packed。
    F64,
    /// 64 bit IEEE 浮点，planar。
    F64P,
}

impl SampleFormat {
    /// 每采样字节数（`U8`=1、`I16`=2、`I32`/`F32`=4、`I64`/`F64`=8）。
    pub fn bytes_per_sample(self) -> usize {
        match self {
            SampleFormat::U8 | SampleFormat::U8P => 1,
            SampleFormat::I16 | SampleFormat::I16P => 2,
            SampleFormat::I32 | SampleFormat::I32P | SampleFormat::F32 | SampleFormat::F32P => 4,
            SampleFormat::I64 | SampleFormat::I64P | SampleFormat::F64 | SampleFormat::F64P => 8,
        }
    }

    /// 是否平面格式；`P` 后缀变体为 `true`。
    pub fn is_planar(self) -> bool {
        matches!(
            self,
            SampleFormat::U8P
                | SampleFormat::I16P
                | SampleFormat::I32P
                | SampleFormat::I64P
                | SampleFormat::F32P
                | SampleFormat::F64P
        )
    }

    /// 给定声道数与帧数所需的音频缓冲总字节数。
    ///
    /// 布局语义：
    /// - planar：`channels × frames × bps`（每声道一块 `frames × bps` 的平面）；
    /// - packed：`frames × channels × bps`（每帧 `channels` 个交织样本）。
    ///
    /// `frames` 是帧数（一帧 = 同一时刻所有声道各一个样本），不乘采样率。
    /// 溢出按饱和处理（封顶 `usize::MAX`），不 panic；`channels` 为 0 时返回 0。
    pub fn buffer_len(format: SampleFormat, channels: u32, frames: usize) -> usize {
        let bytes_per_sample = format.bytes_per_sample() as u64;
        let bytes = if format.is_planar() {
            (channels as u64)
                .saturating_mul(frames as u64)
                .saturating_mul(bytes_per_sample)
        } else {
            (frames as u64)
                .saturating_mul(channels as u64)
                .saturating_mul(bytes_per_sample)
        };
        usize::try_from(bytes).unwrap_or(usize::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_format_properties() {
        // 契约点名的断言
        assert_eq!(SampleFormat::I16.bytes_per_sample(), 2);
        assert!(!SampleFormat::I16.is_planar());
        assert_eq!(SampleFormat::F32P.bytes_per_sample(), 4);
        assert!(SampleFormat::F32P.is_planar());
        // 全变体的位宽与平面性
        let widths = [
            (SampleFormat::U8, 1),
            (SampleFormat::U8P, 1),
            (SampleFormat::I16, 2),
            (SampleFormat::I16P, 2),
            (SampleFormat::I32, 4),
            (SampleFormat::I32P, 4),
            (SampleFormat::I64, 8),
            (SampleFormat::I64P, 8),
            (SampleFormat::F32, 4),
            (SampleFormat::F32P, 4),
            (SampleFormat::F64, 8),
            (SampleFormat::F64P, 8),
        ];
        for (format, width) in widths {
            assert_eq!(format.bytes_per_sample(), width);
        }
        for format in [
            SampleFormat::U8,
            SampleFormat::I16,
            SampleFormat::I32,
            SampleFormat::I64,
            SampleFormat::F32,
            SampleFormat::F64,
        ] {
            assert!(!format.is_planar());
        }
        for format in [
            SampleFormat::U8P,
            SampleFormat::I16P,
            SampleFormat::I32P,
            SampleFormat::I64P,
            SampleFormat::F32P,
            SampleFormat::F64P,
        ] {
            assert!(format.is_planar());
        }
    }

    #[test]
    fn buffer_len_values() {
        // packed：frames × channels × bps = 480·2·2
        assert_eq!(SampleFormat::buffer_len(SampleFormat::I16, 2, 480), 1920);
        // planar：channels × frames × bps = 2·480·4
        assert_eq!(SampleFormat::buffer_len(SampleFormat::F32P, 2, 480), 3840);
        // frames 位不乘采样率：1 声道 44100 帧 U8 = 44100 字节
        assert_eq!(SampleFormat::buffer_len(SampleFormat::U8, 1, 44100), 44100);
        // 零声道 → 0
        assert_eq!(SampleFormat::buffer_len(SampleFormat::I16, 0, 480), 0);
    }
}
