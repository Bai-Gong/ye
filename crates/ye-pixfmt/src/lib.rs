//! 像素格式、色彩空间、采样布局的定义与转换描述。
//!
//! 对标 libavutil 的 `AVPixelFormat` 家族：[`PixelFormat`] 描述格式本身
//! （平面数、每样例字节数、子采样因子、位深），[`FrameLayout`] 把格式与
//! 具体宽高、对齐参数结合成可分配的平面布局（stride 模型与 `AVFrame` 的
//! data/linesize 语义对齐），[`FrameBuffer`] 再按布局持有系统内存。
//! 本 crate 只做描述与布局计算，转换内核归 `ye-filter`；
//! 设备内存（GPU surface）路径到 M5 再抽象，布局几何可直接复用。

#![forbid(unsafe_code)]

use std::fmt;

/// 像素格式（M1 封闭集合，命名对齐 FFmpeg 的 `AVPixelFormat`）。
///
/// 后缀约定：`P` = planar（平面），`10Le` = 每样例 10bit 小端。
/// `#[non_exhaustive]` 为后续扩展（yuvj 系、yuva 系等）留口。
#[non_exhaustive]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum PixelFormat {
    /// 8bit 灰度，单平面。
    Gray8,
    /// packed RGB 8:8:8，内存字节序 R、G、B。
    Rgb24,
    /// packed BGR 8:8:8，内存字节序 B、G、R。
    Bgr24,
    /// packed RGBA 8:8:8:8，内存字节序 R、G、B、A。
    Rgba8,
    /// packed BGRA 8:8:8:8，内存字节序 B、G、R、A。
    Bgra8,
    /// 平面 YUV 4:2:0，Y/U/V 三平面，每样例 8bit。
    Yuv420P,
    /// 平面 YUV 4:2:2，Y/U/V 三平面，每样例 8bit。
    Yuv422P,
    /// 平面 YUV 4:4:4（无子采样），Y/U/V 三平面，每样例 8bit。
    Yuv444P,
    /// 半平面 YUV 4:2:0：Y 平面 + UV 交织平面（U 在前），每样例 8bit。
    Nv12,
    /// 半平面 YUV 4:2:0：Y 平面 + VU 交织平面（V 在前），每样例 8bit。
    Nv21,
    /// 平面 YUV 4:2:0，每样例 10bit 小端。
    Yuv420P10Le,
}

impl PixelFormat {
    /// M1 收录的全部格式，供遍历与 `from_name` 查表。
    pub const ALL: [PixelFormat; 11] = [
        PixelFormat::Gray8,
        PixelFormat::Rgb24,
        PixelFormat::Bgr24,
        PixelFormat::Rgba8,
        PixelFormat::Bgra8,
        PixelFormat::Yuv420P,
        PixelFormat::Yuv422P,
        PixelFormat::Yuv444P,
        PixelFormat::Nv12,
        PixelFormat::Nv21,
        PixelFormat::Yuv420P10Le,
    ];

    /// 平面数：packed（`Rgb24` 等）= 1，`Yuv420P` = 3，`Nv12`/`Nv21` = 2。
    pub fn plane_count(self) -> usize {
        match self {
            PixelFormat::Gray8
            | PixelFormat::Rgb24
            | PixelFormat::Bgr24
            | PixelFormat::Rgba8
            | PixelFormat::Bgra8 => 1,
            PixelFormat::Nv12 | PixelFormat::Nv21 => 2,
            PixelFormat::Yuv420P
            | PixelFormat::Yuv422P
            | PixelFormat::Yuv444P
            | PixelFormat::Yuv420P10Le => 3,
        }
    }

    /// 第 `plane` 个平面每个样例的字节数（packed 格式即每像素字节数）：
    /// `Yuv420P` → [1,1,1]，`Nv12` → [1,2]（UV 交织平面每样例组 2 字节），
    /// `Yuv420P10Le` → [2,2,2]。
    ///
    /// # Panics
    /// `plane >= plane_count()`：程序员错误，debug 断言 + 越界索引 panic。
    pub fn bytes_per_sample(self, plane: usize) -> usize {
        let table: &[usize] = match self {
            PixelFormat::Gray8 => &[1],
            PixelFormat::Rgb24 | PixelFormat::Bgr24 => &[3],
            PixelFormat::Rgba8 | PixelFormat::Bgra8 => &[4],
            PixelFormat::Yuv420P | PixelFormat::Yuv422P | PixelFormat::Yuv444P => &[1, 1, 1],
            PixelFormat::Nv12 | PixelFormat::Nv21 => &[1, 2],
            PixelFormat::Yuv420P10Le => &[2, 2, 2],
        };
        debug_assert!(
            plane < table.len(),
            "plane 序号 {plane} 越界：{} 仅有 {} 个平面",
            self.name(),
            table.len()
        );
        table[plane]
    }

    /// 第 `plane` 平面相对亮度的采样缩减 `(宽除数, 高除数)`：
    /// plane 0 → (1,1)，420 色度 → (2,2)，422 色度 → (2,1)，
    /// packed 与灰度 → (1,1)。
    ///
    /// # Panics
    /// `plane >= plane_count()`：程序员错误，debug 断言 + 越界索引 panic。
    pub fn subsampling_factors(self, plane: usize) -> (usize, usize) {
        let table: &[(usize, usize)] = match self {
            PixelFormat::Gray8
            | PixelFormat::Rgb24
            | PixelFormat::Bgr24
            | PixelFormat::Rgba8
            | PixelFormat::Bgra8 => &[(1, 1)],
            PixelFormat::Yuv444P => &[(1, 1), (1, 1), (1, 1)],
            PixelFormat::Yuv422P => &[(1, 1), (2, 1), (2, 1)],
            PixelFormat::Yuv420P | PixelFormat::Yuv420P10Le => &[(1, 1), (2, 2), (2, 2)],
            PixelFormat::Nv12 | PixelFormat::Nv21 => &[(1, 1), (2, 2)],
        };
        debug_assert!(
            plane < self.plane_count(),
            "plane 序号 {plane} 越界：{} 仅有 {} 个平面",
            self.name(),
            self.plane_count()
        );
        table[plane]
    }

    /// 每样例有效位数：8 或 10。
    pub fn bit_depth(self) -> u32 {
        match self {
            PixelFormat::Yuv420P10Le => 10,
            _ => 8,
        }
    }

    /// FFmpeg 对齐名（与 `av_get_pix_fmt_name`/`av_get_pix_fmt` 互通）：
    /// 如 `"yuv420p"`、`"nv12"`、`"yuv420p10le"`。
    /// `Gray8` 取 FFmpeg 同样接受的 `"gray8"`（其规范名 `"gray"` 的别名），
    /// 与变体名保持一致。
    pub fn name(self) -> &'static str {
        match self {
            PixelFormat::Gray8 => "gray8",
            PixelFormat::Rgb24 => "rgb24",
            PixelFormat::Bgr24 => "bgr24",
            PixelFormat::Rgba8 => "rgba",
            PixelFormat::Bgra8 => "bgra",
            PixelFormat::Yuv420P => "yuv420p",
            PixelFormat::Yuv422P => "yuv422p",
            PixelFormat::Yuv444P => "yuv444p",
            PixelFormat::Nv12 => "nv12",
            PixelFormat::Nv21 => "nv21",
            PixelFormat::Yuv420P10Le => "yuv420p10le",
        }
    }

    /// [`PixelFormat::name`] 的反函数：未收录的名字（含大小写不同）→ `None`。
    pub fn from_name(name: &str) -> Option<PixelFormat> {
        PixelFormat::ALL.into_iter().find(|&fmt| fmt.name() == name)
    }
}

/// 单个平面在帧缓冲中的几何布局。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlaneLayout {
    /// 相对缓冲区起点的字节偏移（紧凑排布：前序平面 size 累加）。
    pub offset: usize,
    /// 行间距（字节）：第 r 行行首到第 r+1 行行首的距离，含行尾 padding。
    pub stride: usize,
    /// 本平面有效行数。
    pub rows: usize,
    /// 本平面占用字节（末行也按完整 stride 计）。
    pub size_bytes: usize,
}

/// 一帧的平面布局：格式 + 宽高 + 对齐 → 各平面 offset/stride/rows/size。
///
/// 纯几何描述，不含数据；同一布局既描述系统内存帧，也可描述设备内存
/// 上按相同平面排布的 surface（M5 复用）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameLayout {
    pub format: PixelFormat,
    pub width: usize,
    pub height: usize,
    /// 仅前 `used_planes` 个有效，其余槽位恒为零值。
    pub planes: [PlaneLayout; 4],
    /// 有效平面数 = `format.plane_count()`。
    pub used_planes: usize,
    /// 缓冲区总字节数 = 末平面 offset + size_bytes。
    pub total_bytes: usize,
}

impl FrameLayout {
    /// 按紧凑平面排布计算布局。
    ///
    /// 每平面 stride = ceil(width / 宽除数) × bytes_per_sample，
    /// 再向上对齐到 `stride_align`；行数 = ceil(height / 高除数)；
    /// plane i 的 offset = 前序平面 size_bytes 累加。
    ///
    /// # Errors
    /// - `width`/`height` 为 0 → [`PixfmtError::ZeroDimension`]
    /// - `stride_align` 为 0 或非 2 的幂 → [`PixfmtError::BadStrideAlign`]
    ///   （`stride_align == 1` 表示无对齐）。
    pub fn new(
        format: PixelFormat,
        width: usize,
        height: usize,
        stride_align: usize,
    ) -> Result<FrameLayout, PixfmtError> {
        if width == 0 || height == 0 {
            return Err(PixfmtError::ZeroDimension);
        }
        if stride_align == 0 || !stride_align.is_power_of_two() {
            return Err(PixfmtError::BadStrideAlign);
        }
        let mut planes = [PlaneLayout {
            offset: 0,
            stride: 0,
            rows: 0,
            size_bytes: 0,
        }; 4];
        let mut offset = 0;
        for (plane, p) in planes.iter_mut().enumerate().take(format.plane_count()) {
            let (dx, dy) = format.subsampling_factors(plane);
            let stride = align_up(
                ceil_div(width, dx) * format.bytes_per_sample(plane),
                stride_align,
            );
            let rows = ceil_div(height, dy);
            let size_bytes = stride * rows;
            *p = PlaneLayout {
                offset,
                stride,
                rows,
                size_bytes,
            };
            offset += size_bytes;
        }
        Ok(FrameLayout {
            format,
            width,
            height,
            planes,
            used_planes: format.plane_count(),
            total_bytes: offset,
        })
    }

    /// 取第 `index` 个平面的布局；`index >= used_planes` → `Err`。
    pub fn plane(&self, index: usize) -> Result<&PlaneLayout, PixfmtError> {
        if index < self.used_planes {
            Ok(&self.planes[index])
        } else {
            Err(PixfmtError::PlaneIndexOutOfRange)
        }
    }
}

/// 系统内存帧缓冲：`FrameLayout` + 一块按 `total_bytes` 分配的字节缓冲。
///
/// `Frame` 的系统内存形态；平面紧凑排布，行尾可含对齐 padding。
#[derive(Clone)]
pub struct FrameBuffer {
    layout: FrameLayout,
    data: Box<[u8]>,
}

impl FrameBuffer {
    /// 按 `layout.total_bytes` 零初始化分配。
    pub fn alloc(layout: FrameLayout) -> FrameBuffer {
        FrameBuffer {
            data: vec![0u8; layout.total_bytes].into(),
            layout,
        }
    }

    pub fn layout(&self) -> &FrameLayout {
        &self.layout
    }

    /// 第 `index` 个平面的紧密切片：自平面 offset 起，
    /// stride × (rows-1) + 末行有效宽（末行不含 padding）。
    ///
    /// # Errors
    /// `index >= used_planes` → [`PixfmtError::PlaneIndexOutOfRange`]。
    pub fn plane(&self, index: usize) -> Result<&[u8], PixfmtError> {
        let p = self.layout.plane(index)?;
        let len = self.tight_len(index, p);
        Ok(&self.data[p.offset..p.offset + len])
    }

    /// [`FrameBuffer::plane`] 的可变版本。
    ///
    /// # Errors
    /// `index >= used_planes` → [`PixfmtError::PlaneIndexOutOfRange`]。
    pub fn plane_mut(&mut self, index: usize) -> Result<&mut [u8], PixfmtError> {
        let p = *self.layout.plane(index)?;
        let len = self.tight_len(index, &p);
        Ok(&mut self.data[p.offset..p.offset + len])
    }

    /// 整块缓冲（含行尾与末行 padding），长度恒为 `total_bytes`。
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// 紧密切片长度：末行不 padded，按有效宽 ceil(width/dx) × bytes_per_sample 计。
    /// `rows >= 1` 由 `FrameLayout::new` 的非零高保证。
    fn tight_len(&self, index: usize, p: &PlaneLayout) -> usize {
        let (dx, _) = self.layout.format.subsampling_factors(index);
        let last_row = ceil_div(self.layout.width, dx) * self.layout.format.bytes_per_sample(index);
        p.stride * (p.rows - 1) + last_row
    }
}

impl fmt::Debug for FrameBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 不倾倒整块缓冲，只报长度。
        f.debug_struct("FrameBuffer")
            .field("layout", &self.layout)
            .field("data_len", &self.data.len())
            .finish()
    }
}

/// 布局与缓冲访问的错误。
#[derive(Debug, PartialEq, Eq)]
pub enum PixfmtError {
    /// 帧宽或高为 0。
    ZeroDimension,
    /// stride 对齐为 0 或非 2 的幂。
    BadStrideAlign,
    /// 平面序号超出该像素格式的平面数。
    PlaneIndexOutOfRange,
}

impl fmt::Display for PixfmtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PixfmtError::ZeroDimension => write!(f, "帧宽高必须为正（当前为 0）"),
            PixfmtError::BadStrideAlign => {
                write!(f, "stride 对齐必须为非零 2 的幂（1 表示无对齐）")
            }
            PixfmtError::PlaneIndexOutOfRange => write!(f, "平面序号超出该像素格式的平面数"),
        }
    }
}

impl std::error::Error for PixfmtError {}

/// 向上取整除法：ceil(a / b)，b >= 1。
fn ceil_div(a: usize, b: usize) -> usize {
    a.div_ceil(b)
}

/// 把 v 向上对齐到 a 的倍数（a 为 2 的幂，含 1）。
fn align_up(v: usize, a: usize) -> usize {
    debug_assert!(a > 0 && a.is_power_of_two());
    v.div_ceil(a) * a
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 元数据表的一行：(格式, plane_count, 每平面 bytes_per_sample,
    /// 每平面子采样因子, 位深, 名字)。
    type MetaRow = (
        PixelFormat,
        usize,
        Vec<usize>,
        Vec<(usize, usize)>,
        u32,
        &'static str,
    );

    /// 全格式元数据表。逐项手写期望值，兼作 M1 集合的参考文档。
    #[rustfmt::skip]
    fn table() -> Vec<MetaRow> {
        vec![
            (PixelFormat::Gray8,      1, vec![1],        vec![(1, 1)],                        8,  "gray8"),
            (PixelFormat::Rgb24,      1, vec![3],        vec![(1, 1)],                        8,  "rgb24"),
            (PixelFormat::Bgr24,      1, vec![3],        vec![(1, 1)],                        8,  "bgr24"),
            (PixelFormat::Rgba8,      1, vec![4],        vec![(1, 1)],                        8,  "rgba"),
            (PixelFormat::Bgra8,      1, vec![4],        vec![(1, 1)],                        8,  "bgra"),
            (PixelFormat::Yuv420P,    3, vec![1, 1, 1],  vec![(1, 1), (2, 2), (2, 2)],        8,  "yuv420p"),
            (PixelFormat::Yuv422P,    3, vec![1, 1, 1],  vec![(1, 1), (2, 1), (2, 1)],        8,  "yuv422p"),
            (PixelFormat::Yuv444P,    3, vec![1, 1, 1],  vec![(1, 1), (1, 1), (1, 1)],        8,  "yuv444p"),
            (PixelFormat::Nv12,       2, vec![1, 2],     vec![(1, 1), (2, 2)],                8,  "nv12"),
            (PixelFormat::Nv21,       2, vec![1, 2],     vec![(1, 1), (2, 2)],                8,  "nv21"),
            (PixelFormat::Yuv420P10Le,3, vec![2, 2, 2],  vec![(1, 1), (2, 2), (2, 2)],        10, "yuv420p10le"),
        ]
    }

    #[test]
    fn format_metadata_matches_table() {
        let cases = table();
        // 用例覆盖 ALL 全集，不漏格式。
        assert_eq!(cases.len(), PixelFormat::ALL.len());
        for fmt in PixelFormat::ALL {
            assert!(cases.iter().any(|(f, ..)| *f == fmt), "{fmt:?} 缺测试用例");
        }
        for (fmt, planes, bps, subs, bits, name) in &cases {
            assert_eq!(fmt.plane_count(), *planes, "{fmt:?}.plane_count");
            assert_eq!(fmt.bit_depth(), *bits, "{fmt:?}.bit_depth");
            for plane in 0..*planes {
                assert_eq!(
                    fmt.bytes_per_sample(plane),
                    bps[plane],
                    "{fmt:?}.bytes_per_sample({plane})"
                );
                assert_eq!(
                    fmt.subsampling_factors(plane),
                    subs[plane],
                    "{fmt:?}.subsampling_factors({plane})"
                );
            }
            // name ↔ from_name 往返。
            assert_eq!(fmt.name(), *name, "{fmt:?}.name");
            assert_eq!(
                PixelFormat::from_name(name),
                Some(*fmt),
                "from_name({name})"
            );
        }
    }

    #[test]
    fn from_name_rejects_unknown() {
        // FFmpeg 有、本集合未收录 → None。
        assert_eq!(PixelFormat::from_name("yuva420p10le"), None);
        assert_eq!(PixelFormat::from_name("yuvj420p"), None);
        // 大小写敏感、空串、乱写 → None。
        assert_eq!(PixelFormat::from_name("YUV420P"), None);
        assert_eq!(PixelFormat::from_name(""), None);
        assert_eq!(PixelFormat::from_name("not-a-format"), None);
    }

    #[test]
    #[should_panic(expected = "越界：nv12 仅有 2 个平面")]
    fn bytes_per_sample_out_of_range_panics() {
        PixelFormat::Nv12.bytes_per_sample(2);
    }

    #[test]
    #[should_panic(expected = "越界：yuv420p 仅有 3 个平面")]
    fn subsampling_factors_out_of_range_panics() {
        PixelFormat::Yuv420P.subsampling_factors(3);
    }

    #[test]
    fn layout_yuv420p_1080p_align1() {
        let layout = FrameLayout::new(PixelFormat::Yuv420P, 1920, 1080, 1).expect("合法布局");
        assert_eq!(layout.used_planes, 3);
        // 亮度 1920×1080；色度各 960×540。
        let expect = [
            PlaneLayout {
                offset: 0,
                stride: 1920,
                rows: 1080,
                size_bytes: 2_073_600,
            },
            PlaneLayout {
                offset: 2_073_600,
                stride: 960,
                rows: 540,
                size_bytes: 518_400,
            },
            PlaneLayout {
                offset: 2_592_000,
                stride: 960,
                rows: 540,
                size_bytes: 518_400,
            },
        ];
        for (i, e) in expect.iter().enumerate() {
            assert_eq!(layout.plane(i).unwrap(), e, "plane {i}");
        }
        assert_eq!(layout.total_bytes, 3_110_400);
        // 未用槽位为零值；plane(3) 越界。
        assert_eq!(
            layout.planes[3],
            PlaneLayout {
                offset: 0,
                stride: 0,
                rows: 0,
                size_bytes: 0
            }
        );
        assert_eq!(layout.plane(3), Err(PixfmtError::PlaneIndexOutOfRange));
    }

    #[test]
    fn layout_yuv420p_3x3_ceils_chroma() {
        // 奇数尺寸：ceil 生效，色度 2×2 而非 1×1。
        let layout = FrameLayout::new(PixelFormat::Yuv420P, 3, 3, 1).expect("合法布局");
        assert_eq!(layout.plane(0).unwrap().rows, 3);
        assert_eq!(layout.plane(0).unwrap().stride, 3);
        for i in 1..3 {
            let p = layout.plane(i).unwrap();
            assert_eq!((p.stride, p.rows), (2, 2), "色度平面 {i} 应为 2×2");
        }
        assert_eq!(layout.total_bytes, 9 + 4 + 4);
    }

    #[test]
    fn layout_align32_rounds_strides_and_offsets() {
        let layout = FrameLayout::new(PixelFormat::Yuv420P, 100, 50, 32).expect("合法布局");
        for i in 0..layout.used_planes {
            let p = layout.plane(i).unwrap();
            assert_ne!(p.stride, 0);
            assert_eq!(p.stride % 32, 0, "plane {i} stride 须为 32 的倍数");
        }
        // 亮度 100→128；色度 50→64；offset 随对齐后的 stride 重排。
        assert_eq!(layout.plane(0).unwrap().stride, 128);
        assert_eq!(layout.plane(0).unwrap().size_bytes, 128 * 50);
        assert_eq!(layout.plane(1).unwrap().stride, 64);
        assert_eq!(layout.plane(1).unwrap().offset, 128 * 50);
        assert_eq!(layout.plane(2).unwrap().offset, 128 * 50 + 64 * 25);
        assert_eq!(layout.total_bytes, 128 * 50 + 64 * 25 * 2);
    }

    #[test]
    fn layout_nv12_chroma_is_single_interleaved_plane() {
        let layout = FrameLayout::new(PixelFormat::Nv12, 1920, 1080, 1).expect("合法布局");
        assert_eq!(layout.used_planes, 2);
        assert_eq!(layout.plane(0).unwrap().stride, 1920);
        // 色度单平面：ceil(1920/2) 个样例组 × 每组 2 字节（UV 交织）= 1920。
        assert_eq!(layout.plane(1).unwrap().stride, 1920);
        assert_eq!(layout.plane(1).unwrap().rows, 540);
        assert_eq!(layout.total_bytes, 3_110_400);
        assert_eq!(layout.plane(2), Err(PixfmtError::PlaneIndexOutOfRange));
        // Nv21 仅 U/V 语义顺序不同，几何同 Nv12。
        let nv21 = FrameLayout::new(PixelFormat::Nv21, 1920, 1080, 1).expect("合法布局");
        assert_eq!(nv21.planes[..2], layout.planes[..2]);
    }

    #[test]
    fn layout_yuv420p10le_doubles_stride() {
        let layout = FrameLayout::new(PixelFormat::Yuv420P10Le, 1920, 1080, 1).expect("合法布局");
        // 每样例 2 字节：亮度 stride = 宽×2，色度 = 宽。
        assert_eq!(layout.plane(0).unwrap().stride, 3840);
        assert_eq!(layout.plane(1).unwrap().stride, 1920);
        assert_eq!(layout.plane(2).unwrap().stride, 1920);
        assert_eq!(layout.plane(0).unwrap().rows, 1080);
        assert_eq!(layout.total_bytes, 3840 * 1080 + 1920 * 540 * 2);
        assert_eq!(layout.total_bytes, 6_220_800);
    }

    #[test]
    fn layout_packed_and_gray() {
        let rgb = FrameLayout::new(PixelFormat::Rgb24, 640, 480, 1).expect("合法布局");
        assert_eq!(rgb.used_planes, 1);
        assert_eq!(rgb.plane(0).unwrap().stride, 640 * 3);
        assert_eq!(rgb.total_bytes, 640 * 3 * 480);
        let rgba = FrameLayout::new(PixelFormat::Rgba8, 640, 480, 1).expect("合法布局");
        assert_eq!(rgba.plane(0).unwrap().stride, 640 * 4);
        let gray = FrameLayout::new(PixelFormat::Gray8, 1919, 7, 1).expect("合法布局");
        assert_eq!(gray.plane(0).unwrap().stride, 1919);
        assert_eq!(gray.total_bytes, 1919 * 7);
        // 4:2:2 色度只缩宽不缩高；4:4:4 不缩。
        let y422 = FrameLayout::new(PixelFormat::Yuv422P, 640, 480, 1).expect("合法布局");
        assert_eq!(
            (y422.plane(1).unwrap().stride, y422.plane(1).unwrap().rows),
            (320, 480)
        );
        let y444 = FrameLayout::new(PixelFormat::Yuv444P, 640, 480, 1).expect("合法布局");
        assert_eq!(
            (y444.plane(1).unwrap().stride, y444.plane(1).unwrap().rows),
            (640, 480)
        );
    }

    #[test]
    fn layout_rejects_bad_inputs() {
        use PixfmtError::*;
        assert_eq!(
            FrameLayout::new(PixelFormat::Yuv420P, 0, 1080, 1).unwrap_err(),
            ZeroDimension
        );
        assert_eq!(
            FrameLayout::new(PixelFormat::Yuv420P, 1920, 0, 1).unwrap_err(),
            ZeroDimension
        );
        assert_eq!(
            FrameLayout::new(PixelFormat::Yuv420P, 1920, 1080, 0).unwrap_err(),
            BadStrideAlign
        );
        // 48 非 2 的幂。
        assert_eq!(
            FrameLayout::new(PixelFormat::Yuv420P, 1920, 1080, 48).unwrap_err(),
            BadStrideAlign
        );
        assert_eq!(
            FrameLayout::new(PixelFormat::Yuv420P, 1920, 1080, 3).unwrap_err(),
            BadStrideAlign
        );
        // 1（无对齐）与常见 2 的幂均合法。
        assert!(FrameLayout::new(PixelFormat::Yuv420P, 1920, 1080, 1).is_ok());
        assert!(FrameLayout::new(PixelFormat::Yuv420P, 1920, 1080, 64).is_ok());
    }

    #[test]
    fn error_display_and_impl() {
        assert_eq!(
            PixfmtError::ZeroDimension.to_string(),
            "帧宽高必须为正（当前为 0）"
        );
        assert_eq!(
            PixfmtError::BadStrideAlign.to_string(),
            "stride 对齐必须为非零 2 的幂（1 表示无对齐）"
        );
        assert_eq!(
            PixfmtError::PlaneIndexOutOfRange.to_string(),
            "平面序号超出该像素格式的平面数"
        );
        // 可当 std::error::Error 用（含 Send/Debug）。
        let boxed: Box<dyn std::error::Error> = Box::new(PixfmtError::ZeroDimension);
        assert!(!boxed.to_string().is_empty());
    }

    #[test]
    fn frame_buffer_roundtrip_and_padding_isolation() {
        // 16×16、对齐 32：亮度 stride 32（有效宽 16），色度 stride 32（有效宽 8）。
        let layout = FrameLayout::new(PixelFormat::Yuv420P, 16, 16, 32).expect("合法布局");
        assert_eq!(layout.total_bytes, 32 * 16 + 32 * 8 * 2);
        let mut fb = FrameBuffer::alloc(layout.clone());
        assert_eq!(fb.as_bytes().len(), layout.total_bytes);
        assert!(fb.as_bytes().iter().all(|&b| b == 0), "分配后应零初始化");
        assert_eq!(fb.layout(), &layout);

        // 亮度：每行行首写行号，末行末字节写特征值。
        {
            let y = fb.plane_mut(0).expect("plane 0");
            assert_eq!(y.len(), 32 * 15 + 16, "紧密切片：末行不含 padding");
            for r in 0..16 {
                y[r * 32] = r as u8;
            }
            y[495] = 0xEE;
        }
        assert_eq!(fb.plane(0).unwrap()[0], 0);
        assert_eq!(fb.plane(0).unwrap()[15 * 32], 15);
        assert_eq!(fb.plane(0).unwrap()[495], 0xEE);

        // 色度两平面整体填充不同特征值；紧密切片含行间 padding，不串平面。
        fb.plane_mut(1).unwrap().fill(0x3C);
        fb.plane_mut(2).unwrap().fill(0x5A);
        assert_eq!(fb.plane(1).unwrap().len(), 32 * 7 + 8);
        assert!(fb.plane(1).unwrap().iter().all(|&b| b == 0x3C));
        assert!(fb.plane(2).unwrap().iter().all(|&b| b == 0x5A));
        // 亮度数据未被色度写入破坏。
        assert_eq!(fb.plane(0).unwrap()[7 * 32], 7);

        // 各平面在整块缓冲中的实际位置与隔离带（padding）验证。
        let bytes = fb.as_bytes();
        assert_eq!(&bytes[0..496], fb.plane(0).unwrap());
        assert_eq!(bytes[512], 0x3C);
        assert_eq!(bytes[768], 0x5A);
        // 亮度末行 padding（496..512）与色度 1 末行 padding（744..768）未被触碰。
        assert!(bytes[496..512].iter().all(|&b| b == 0));
        assert!(bytes[744..768].iter().all(|&b| b == 0));

        // 越界访问。
        assert_eq!(fb.plane(3), Err(PixfmtError::PlaneIndexOutOfRange));
        assert_eq!(fb.plane_mut(3), Err(PixfmtError::PlaneIndexOutOfRange));
    }

    #[test]
    fn frame_buffer_packed_single_plane() {
        let layout = FrameLayout::new(PixelFormat::Rgb24, 4, 2, 1).expect("合法布局");
        let mut fb = FrameBuffer::alloc(layout);
        assert_eq!(fb.as_bytes().len(), 24);
        fb.plane_mut(0).unwrap().copy_from_slice(&[
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        ]);
        assert_eq!(fb.plane(0).unwrap()[0..6], [1, 2, 3, 4, 5, 6]);
        assert_eq!(fb.plane(1), Err(PixfmtError::PlaneIndexOutOfRange));
        // Debug 实现不倾倒缓冲内容。
        let dbg = format!("{fb:?}");
        assert!(dbg.contains("data_len: 24"));
    }
}
