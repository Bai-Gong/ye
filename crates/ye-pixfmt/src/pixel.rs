//! 像素格式枚举与帧/平面布局。

use crate::error::PixfmtError;

/// 视频帧默认跨距对齐（字节）。
///
/// 供（后续轮次的）`ye_codec::VideoFrame::new` 作为
/// [`PixelFormat::frame_layout`] 的 `align` 缺省值。
///
/// 取 64 的取舍：64 字节恰为一条缓存行，也覆盖 AVX-512（64 字节）整行访存的
/// 最宽需求，转换内核可按跨距直接做 SIMD 而无需分段；更大的对齐（如 128）
/// 只会徒增每行尾部的填充内存，对常见 8/10bit 逐行格式收益为零；更小
/// （如 16/32）则会让 AVX2/AVX-512 的跨行访存越过未对齐边界、性能打折。
pub const DEFAULT_STRIDE_ALIGN: usize = 64;

/// 像素格式枚举（M1/M2 需要的最小集，扩展属正常演进）。
///
/// 命名对标 FFmpeg `AVPixelFormat`：`10Le` 后缀指每样本 10 bit、小端存储。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq, Hash`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
    /// 8 bit 灰度，单平面。
    Gray8,
    /// 8 bit planar YUV 4:2:0，3 平面（Y/U/V）。
    Yuv420p,
    /// 10 bit（每样本 2 字节小端）planar YUV 4:2:0，3 平面。
    Yuv420p10Le,
    /// 8 bit planar YUV 4:2:2，3 平面。
    Yuv422p,
    /// 10 bit planar YUV 4:2:2，3 平面。
    Yuv422p10Le,
    /// 8 bit planar YUV 4:4:4，3 平面。
    Yuv444p,
    /// 10 bit planar YUV 4:4:4，3 平面。
    Yuv444p10Le,
    /// 8 bit planar YUVA 4:2:0（含全分辨率 alpha 平面），4 平面。
    Yuva420p,
    /// 8 bit Y 平面 + 交织 UV 平面（U 在前），2 平面。
    Nv12,
    /// 8 bit Y 平面 + 交织 VU 平面（V 在前），2 平面。
    Nv21,
    /// 24 bit packed RGB，字节序 R,G,B，单平面。
    Rgb24,
    /// 24 bit packed BGR，字节序 B,G,R，单平面。
    Bgr24,
    /// 32 bit packed RGBA，字节序 R,G,B,A，单平面。
    Rgba8,
    /// 32 bit packed BGRA，字节序 B,G,R,A，单平面。
    Bgra8,
}

impl PixelFormat {
    /// 平面数。
    ///
    /// 例：`Nv12`/`Nv21` = 2（Y + 交织 UV），`Yuva420p` = 4，packed RGB(A)/灰度 = 1。
    pub fn plane_count(self) -> u32 {
        match self {
            PixelFormat::Gray8
            | PixelFormat::Rgb24
            | PixelFormat::Bgr24
            | PixelFormat::Rgba8
            | PixelFormat::Bgra8 => 1,
            PixelFormat::Yuv420p
            | PixelFormat::Yuv420p10Le
            | PixelFormat::Yuv422p
            | PixelFormat::Yuv422p10Le
            | PixelFormat::Yuv444p
            | PixelFormat::Yuv444p10Le => 3,
            PixelFormat::Yuva420p => 4,
            PixelFormat::Nv12 | PixelFormat::Nv21 => 2,
        }
    }

    /// 分量数——与 [`plane_count`](Self::plane_count) 区分：
    /// 分量按颜色成分计，交织平面里的各分量分别计数。
    ///
    /// 例：`Nv12` = 3（Y + 交织 U/V 算两分量）、`Rgba8` = 4、`Rgb24` = 3、
    /// `Gray8` = 1、`Yuva420p` = 4。
    pub fn component_count(self) -> u32 {
        match self {
            PixelFormat::Gray8 => 1,
            PixelFormat::Rgba8 | PixelFormat::Bgra8 | PixelFormat::Yuva420p => 4,
            _ => 3,
        }
    }

    /// 第 `plane` 个平面的子采样移位 `(log2_chroma_w, log2_chroma_h)`，
    /// 字段语义对齐 FFmpeg `AVPixFmtDescriptor`。
    ///
    /// 返回值含义：该平面每行样本数为 `ceil(width / 2^w)`、行数为
    /// `ceil(height / 2^h)`。例：`Yuv420p` 平面 1/2 返回 `(1,1)`；
    /// `Yuv422p` 平面 1/2 返回 `(1,0)`；`Nv12` 平面 1（交织 UV）返回 `(1,1)`；
    /// `Yuva420p` 平面 3（alpha，全分辨率）返回 `(0,0)`；luma 平面与
    /// packed RGB(A) 一律 `(0,0)`。
    ///
    /// # Panics（全库五处文档写明编程错误的第③处）
    ///
    /// `plane >= plane_count()` 时 panic，消息含非法索引与该格式的平面数。
    /// 这是纯调用方索引错误，无数据驱动路径（数据驱动失败一律走 [`Result`]）。
    pub fn subsample_log2(self, plane: u32) -> (u32, u32) {
        let count = self.plane_count();
        assert!(
            plane < count,
            "PixelFormat::subsample_log2: plane index out of range: {plane} >= {count} planes of {}",
            self.name()
        );
        if plane == 0 || (self == PixelFormat::Yuva420p && plane == 3) {
            // luma、alpha 与 packed 平面不做子采样
            return (0, 0);
        }
        match self {
            PixelFormat::Yuv420p
            | PixelFormat::Yuv420p10Le
            | PixelFormat::Yuva420p
            | PixelFormat::Nv12
            | PixelFormat::Nv21 => (1, 1),
            PixelFormat::Yuv422p | PixelFormat::Yuv422p10Le => (1, 0),
            // Yuv444p / Yuv444p10Le 无子采样
            _ => (0, 0),
        }
    }

    /// 每样本有效位深：`10Le` 系列为 10，其余为 8。
    pub fn bit_depth(self) -> u32 {
        match self {
            PixelFormat::Yuv420p10Le | PixelFormat::Yuv422p10Le | PixelFormat::Yuv444p10Le => 10,
            _ => 8,
        }
    }

    /// 规范名（FFmpeg 风格小写，如 `"yuv420p"`/`"nv12"`），供探查输出与
    /// [`from_name`](Self::from_name) 往返。精确匹配、大小写敏感。
    pub fn name(self) -> &'static str {
        match self {
            PixelFormat::Gray8 => "gray8",
            PixelFormat::Yuv420p => "yuv420p",
            PixelFormat::Yuv420p10Le => "yuv420p10le",
            PixelFormat::Yuv422p => "yuv422p",
            PixelFormat::Yuv422p10Le => "yuv422p10le",
            PixelFormat::Yuv444p => "yuv444p",
            PixelFormat::Yuv444p10Le => "yuv444p10le",
            PixelFormat::Yuva420p => "yuva420p",
            PixelFormat::Nv12 => "nv12",
            PixelFormat::Nv21 => "nv21",
            PixelFormat::Rgb24 => "rgb24",
            PixelFormat::Bgr24 => "bgr24",
            PixelFormat::Rgba8 => "rgba8",
            PixelFormat::Bgra8 => "bgra8",
        }
    }

    /// 由规范名反查格式；未知名字返回 `None`。
    pub fn from_name(name: &str) -> Option<PixelFormat> {
        Some(match name {
            "gray8" => PixelFormat::Gray8,
            "yuv420p" => PixelFormat::Yuv420p,
            "yuv420p10le" => PixelFormat::Yuv420p10Le,
            "yuv422p" => PixelFormat::Yuv422p,
            "yuv422p10le" => PixelFormat::Yuv422p10Le,
            "yuv444p" => PixelFormat::Yuv444p,
            "yuv444p10le" => PixelFormat::Yuv444p10Le,
            "yuva420p" => PixelFormat::Yuva420p,
            "nv12" => PixelFormat::Nv12,
            "nv21" => PixelFormat::Nv21,
            "rgb24" => PixelFormat::Rgb24,
            "bgr24" => PixelFormat::Bgr24,
            "rgba8" => PixelFormat::Rgba8,
            "bgra8" => PixelFormat::Bgra8,
            _ => return None,
        })
    }

    /// 计算给定尺寸与跨距对齐下的帧布局。
    ///
    /// 规则（舍入一律向上取整 ceil，奇数宽/高合法，无额外错误变体）：
    /// - 每行样本数：packed RGB(A) = `width × component_count`；
    ///   `Nv12`/`Nv21` 的 UV 平面 = `2 × ceil(width/2)`（宽度向上取偶，不论宽度奇偶）；
    ///   其余平面 = `ceil(width / 2^ss_w)`（`ss` 来自
    ///   [`subsample_log2`](Self::subsample_log2)）；
    /// - 每行字节数 = 每行样本数 × 每样本字节数（bit_depth 向上取整到字节：8→1、10→2）；
    /// - `stride = align_up(每行字节数, align)`；`rows = ceil(height / 2^ss_h)`；
    /// - 平面 `offset` 从 0 起前向累加（前一平面 offset+size），`size = stride × rows`。
    ///
    /// 极端尺寸（u32 上限附近）下的乘法/加法按饱和处理（封顶 `usize::MAX`），
    /// 不 panic。
    ///
    /// # Errors
    /// - `width` 或 `height` 为 0 → [`PixfmtError::ZeroDimension`]（先检）；
    /// - `align` 为 0 或非 2 的幂 → [`PixfmtError::BadAlignment`]（携带原值，后检）。
    pub fn frame_layout(
        self,
        width: u32,
        height: u32,
        align: usize,
    ) -> Result<FrameLayout, PixfmtError> {
        if width == 0 || height == 0 {
            return Err(PixfmtError::ZeroDimension);
        }
        // 0 不是 2 的幂，is_power_of_two 一并拒绝
        if !align.is_power_of_two() {
            return Err(PixfmtError::BadAlignment(align));
        }
        let align = align as u64;
        let bytes_per_sample = self.bit_depth().div_ceil(8) as u64;
        let mut planes = [PlaneLayout::ZERO; 4];
        let mut offset: u64 = 0;
        for plane in 0..self.plane_count() {
            let (ss_w, ss_h) = self.subsample_log2(plane);
            // 每行（子采样后的）样本列数：ceil(width / 2^ss_w)
            let cols = ((width as u64) + (1u64 << ss_w) - 1) >> ss_w;
            let row_samples = match self {
                PixelFormat::Nv12 | PixelFormat::Nv21 if plane == 1 => 2 * cols,
                PixelFormat::Rgb24
                | PixelFormat::Bgr24
                | PixelFormat::Rgba8
                | PixelFormat::Bgra8 => cols * self.component_count() as u64,
                _ => cols,
            };
            let row_bytes = row_samples * bytes_per_sample;
            let stride = row_bytes.saturating_add(align - 1) / align * align;
            let rows = ((height as u64) + (1u64 << ss_h) - 1) >> ss_h;
            let size = stride.saturating_mul(rows);
            planes[plane as usize] = PlaneLayout {
                stride: usize::try_from(stride).unwrap_or(usize::MAX),
                rows: usize::try_from(rows).unwrap_or(usize::MAX),
                offset: usize::try_from(offset).unwrap_or(usize::MAX),
                size: usize::try_from(size).unwrap_or(usize::MAX),
            };
            offset = offset.saturating_add(size);
        }
        Ok(FrameLayout {
            width,
            height,
            format: self,
            planes,
            // 尾部累计 offset 恰为末平面 offset+size
            total_size: usize::try_from(offset).unwrap_or(usize::MAX),
        })
    }
}

/// 单平面布局（公有字段、无不变式）。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneLayout {
    /// 该平面每行字节数（已含对齐填充）。
    pub stride: usize,
    /// 该平面行数（子采样后）。
    pub rows: usize,
    /// 相对帧缓冲起点的字节偏移。
    pub offset: usize,
    /// 该平面占用字节数 = `stride × rows`。
    pub size: usize,
}

impl PlaneLayout {
    /// 全零布局（仅内部用于数组初始化）。
    const ZERO: PlaneLayout = PlaneLayout {
        stride: 0,
        rows: 0,
        offset: 0,
        size: 0,
    };
}

/// 帧布局（私有字段 + 访问器，带不变式，只能经
/// [`PixelFormat::frame_layout`] 构造以保证与格式描述一致）。
///
/// 不变式：`planes[0..plane_count]` 有效且 `offset` 从 0 前向累加、
/// `size()` = 末平面 `offset + size`；越界槽位保持全零。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLayout {
    width: u32,
    height: u32,
    format: PixelFormat,
    planes: [PlaneLayout; 4],
    total_size: usize,
}

impl FrameLayout {
    /// 帧宽（像素）。
    pub fn width(&self) -> u32 {
        self.width
    }

    /// 帧高（像素）。
    pub fn height(&self) -> u32 {
        self.height
    }

    /// 像素格式。
    pub fn format(&self) -> PixelFormat {
        self.format
    }

    /// 平面数（等于 `format().plane_count()`）。
    pub fn plane_count(&self) -> u32 {
        self.format.plane_count()
    }

    /// 第 `i` 个平面的布局。
    ///
    /// # Panics（全库五处文档写明编程错误的第④处）
    ///
    /// `i >= plane_count()` 时 panic，消息含非法索引与平面数。
    /// 纯调用方索引错误，无数据驱动路径。
    pub fn plane(&self, i: usize) -> &PlaneLayout {
        let count = self.plane_count() as usize;
        assert!(
            i < count,
            "FrameLayout::plane: plane index out of range: {i} >= {count} planes of {}",
            self.format.name()
        );
        &self.planes[i]
    }

    /// 缓冲总字节数（= 末平面 offset + size）。
    pub fn size(&self) -> usize {
        self.total_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_FORMATS: [PixelFormat; 14] = [
        PixelFormat::Gray8,
        PixelFormat::Yuv420p,
        PixelFormat::Yuv420p10Le,
        PixelFormat::Yuv422p,
        PixelFormat::Yuv422p10Le,
        PixelFormat::Yuv444p,
        PixelFormat::Yuv444p10Le,
        PixelFormat::Yuva420p,
        PixelFormat::Nv12,
        PixelFormat::Nv21,
        PixelFormat::Rgb24,
        PixelFormat::Bgr24,
        PixelFormat::Rgba8,
        PixelFormat::Bgra8,
    ];

    #[test]
    fn yuv420p_descriptor() {
        let f = PixelFormat::Yuv420p;
        assert_eq!(f.plane_count(), 3);
        assert_eq!(f.component_count(), 3);
        assert_eq!(f.subsample_log2(0), (0, 0));
        assert_eq!(f.subsample_log2(1), (1, 1));
        assert_eq!(f.subsample_log2(2), (1, 1));
        assert_eq!(f.bit_depth(), 8);
    }

    #[test]
    fn nv12_descriptor() {
        let f = PixelFormat::Nv12;
        assert_eq!(f.plane_count(), 2);
        assert_eq!(f.component_count(), 3);
        assert_eq!(f.subsample_log2(0), (0, 0));
        assert_eq!(f.subsample_log2(1), (1, 1));
        assert_eq!(f.bit_depth(), 8);
    }

    #[test]
    fn packed_gray_and_10bit_descriptors() {
        assert_eq!(PixelFormat::Rgba8.plane_count(), 1);
        assert_eq!(PixelFormat::Rgba8.component_count(), 4);
        assert_eq!(PixelFormat::Rgba8.subsample_log2(0), (0, 0));
        assert_eq!(PixelFormat::Rgb24.component_count(), 3);
        assert_eq!(PixelFormat::Gray8.component_count(), 1);
        assert_eq!(PixelFormat::Yuv420p10Le.bit_depth(), 10);
        assert_eq!(PixelFormat::Yuv422p.subsample_log2(1), (1, 0));
        assert_eq!(PixelFormat::Yuv444p.subsample_log2(1), (0, 0));
        assert_eq!(PixelFormat::Yuva420p.plane_count(), 4);
        assert_eq!(PixelFormat::Yuva420p.component_count(), 4);
        assert_eq!(PixelFormat::Yuva420p.subsample_log2(1), (1, 1));
        assert_eq!(PixelFormat::Yuva420p.subsample_log2(3), (0, 0));
    }

    #[test]
    #[should_panic(expected = "plane index out of range")]
    fn subsample_log2_out_of_range_panics() {
        let f = PixelFormat::Yuv420p;
        let _ = f.subsample_log2(f.plane_count());
    }

    #[test]
    fn layout_yuv420p_1366x768_align64() {
        let l = PixelFormat::Yuv420p.frame_layout(1366, 768, 64).unwrap();
        assert_eq!(l.width(), 1366);
        assert_eq!(l.height(), 768);
        assert_eq!(l.format(), PixelFormat::Yuv420p);
        assert_eq!(l.plane_count(), 3);
        assert_eq!(
            *l.plane(0),
            PlaneLayout {
                stride: 1408,
                rows: 768,
                offset: 0,
                size: 1081344
            }
        );
        assert_eq!(
            *l.plane(1),
            PlaneLayout {
                stride: 704,
                rows: 384,
                offset: 1081344,
                size: 270336
            }
        );
        assert_eq!(
            *l.plane(2),
            PlaneLayout {
                stride: 704,
                rows: 384,
                offset: 1351680,
                size: 270336
            }
        );
        assert_eq!(l.size(), 1622016);
    }

    #[test]
    fn layout_nv12_640x480_align16() {
        let l = PixelFormat::Nv12.frame_layout(640, 480, 16).unwrap();
        assert_eq!(
            *l.plane(0),
            PlaneLayout {
                stride: 640,
                rows: 480,
                offset: 0,
                size: 307200
            }
        );
        assert_eq!(
            *l.plane(1),
            PlaneLayout {
                stride: 640,
                rows: 240,
                offset: 307200,
                size: 153600
            }
        );
        assert_eq!(l.size(), 460800);
    }

    #[test]
    fn layout_yuv420p10le_1280x720_align1() {
        let l = PixelFormat::Yuv420p10Le.frame_layout(1280, 720, 1).unwrap();
        assert_eq!(
            *l.plane(0),
            PlaneLayout {
                stride: 2560,
                rows: 720,
                offset: 0,
                size: 1843200
            }
        );
        assert_eq!(
            *l.plane(1),
            PlaneLayout {
                stride: 1280,
                rows: 360,
                offset: 1843200,
                size: 460800
            }
        );
        assert_eq!(
            *l.plane(2),
            PlaneLayout {
                stride: 1280,
                rows: 360,
                offset: 2304000,
                size: 460800
            }
        );
        assert_eq!(l.size(), 2764800);
    }

    #[test]
    fn layout_rgba8_3x3_align4() {
        let l = PixelFormat::Rgba8.frame_layout(3, 3, 4).unwrap();
        assert_eq!(l.plane_count(), 1);
        assert_eq!(
            *l.plane(0),
            PlaneLayout {
                stride: 12,
                rows: 3,
                offset: 0,
                size: 36
            }
        );
        assert_eq!(l.size(), 36);
    }

    #[test]
    fn layout_yuv420p_odd_3x3_align1() {
        let l = PixelFormat::Yuv420p.frame_layout(3, 3, 1).unwrap();
        assert_eq!(
            *l.plane(0),
            PlaneLayout {
                stride: 3,
                rows: 3,
                offset: 0,
                size: 9
            }
        );
        assert_eq!(
            *l.plane(1),
            PlaneLayout {
                stride: 2,
                rows: 2,
                offset: 9,
                size: 4
            }
        );
        assert_eq!(
            *l.plane(2),
            PlaneLayout {
                stride: 2,
                rows: 2,
                offset: 13,
                size: 4
            }
        );
        assert_eq!(l.size(), 17);
    }

    #[test]
    fn layout_nv12_odd_width_3x3_align1() {
        let l = PixelFormat::Nv12.frame_layout(3, 3, 1).unwrap();
        // Y 平面 3×3 = 9
        assert_eq!(
            *l.plane(0),
            PlaneLayout {
                stride: 3,
                rows: 3,
                offset: 0,
                size: 9
            }
        );
        // UV 平面：每行 2·ceil(3/2) = 4 字节（宽度向上取偶），rows = ceil(3/2) = 2
        assert_eq!(
            *l.plane(1),
            PlaneLayout {
                stride: 4,
                rows: 2,
                offset: 9,
                size: 8
            }
        );
        assert_eq!(l.size(), 17);
    }

    #[test]
    fn layout_smoke_all_formats() {
        for f in ALL_FORMATS {
            let l = f.frame_layout(1920, 1080, DEFAULT_STRIDE_ALIGN).unwrap();
            let last = l.plane(l.plane_count() as usize - 1);
            assert_eq!(
                l.size(),
                last.offset + last.size,
                "size mismatch: {}",
                f.name()
            );
            assert!(l.size() > 0, "empty layout: {}", f.name());
        }
    }

    #[test]
    fn layout_zero_dimension() {
        assert_eq!(
            PixelFormat::Yuv420p.frame_layout(0, 5, 1),
            Err(PixfmtError::ZeroDimension)
        );
        assert_eq!(
            PixelFormat::Yuv420p.frame_layout(5, 0, 1),
            Err(PixfmtError::ZeroDimension)
        );
    }

    #[test]
    fn layout_bad_alignment() {
        for align in [0usize, 3, 48, 100] {
            assert_eq!(
                PixelFormat::Yuv420p.frame_layout(5, 5, align),
                Err(PixfmtError::BadAlignment(align))
            );
        }
    }

    #[test]
    fn name_roundtrip_all_variants() {
        for f in ALL_FORMATS {
            assert_eq!(
                PixelFormat::from_name(f.name()),
                Some(f),
                "roundtrip failed for {}",
                f.name()
            );
        }
        assert_eq!(PixelFormat::from_name("nope"), None);
        assert_eq!(PixelFormat::Yuv420p.name(), "yuv420p");
        assert_eq!(PixelFormat::Nv12.name(), "nv12");
    }

    #[test]
    #[should_panic(expected = "plane index out of range")]
    fn frame_layout_plane_out_of_range_panics() {
        let l = PixelFormat::Yuv420p.frame_layout(16, 16, 1).unwrap();
        let _ = l.plane(l.plane_count() as usize);
    }
}
