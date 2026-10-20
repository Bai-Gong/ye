//! 解码输出 [`Frame`]（视频帧/音频帧）与拉取结果枚举 [`Decoded`]/[`Encoded`]。

use ye_pixfmt::{
    ChannelLayout, ColorDesc, DEFAULT_STRIDE_ALIGN, FrameLayout, PixelFormat, SampleFormat,
};
use ye_time::Timestamp;

use crate::error::CodecError;
use crate::media::MediaType;
use crate::packet::Packet;

/// 视频帧（私有字段 + 访问器，带不变式）。
///
/// ## 不变式（构造函数负责建立）
///
/// `data.len() == layout.size()`——缓冲长度与布局恒一致，只能经
/// [`VideoFrame::new`]（内部计算布局并按 `DEFAULT_STRIDE_ALIGN` 分配）或
/// [`VideoFrame::from_parts`]（校验外部缓冲）构造。
/// 宽高/格式以 `layout` 为唯一事实源（`FrameLayout` 自带三者）。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    layout: FrameLayout,
    data: Box<[u8]>,
    color: ColorDesc,
    pts: Option<Timestamp>,
    duration: Option<Timestamp>,
}

impl VideoFrame {
    /// 分配全新视频帧：按 [`PixelFormat::frame_layout`]（`align =
    /// DEFAULT_STRIDE_ALIGN`）计算布局并零初始化缓冲。
    ///
    /// # Errors
    /// 布局计算失败原样以 [`CodecError::Pixfmt`] 透出——宽或高为 0 →
    /// `Err(Pixfmt(PixfmtError::ZeroDimension))`（`?` 经 `From<PixfmtError>` 转换，
    /// 不吞下层错误）。
    pub fn new(
        format: PixelFormat,
        width: u32,
        height: u32,
        color: ColorDesc,
        pts: Option<Timestamp>,
        duration: Option<Timestamp>,
    ) -> Result<VideoFrame, CodecError> {
        let layout = format.frame_layout(width, height, DEFAULT_STRIDE_ALIGN)?;
        let data = vec![0u8; layout.size()].into_boxed_slice();
        Ok(VideoFrame {
            layout,
            data,
            color,
            pts,
            duration,
        })
    }

    /// 由外部缓冲组装视频帧（零分配路径，包装解码器输出）。
    ///
    /// # Errors
    /// - `data.len() != layout.size()` → [`CodecError::BufferMismatch`]（携带
    ///   期望/实际字节数）；
    /// - `layout` 与传入的 `format`/`width`/`height` 不一致 →
    ///   [`CodecError::InvalidArguments`]（布局与描述不符，继续使用将破坏
    ///   `plane()` 切片不变式）。
    // 8 个参数（超 clippy 默认上限 1 个）：`format`/`width`/`height` 与 `layout`
    // 冗余存在，专门供调用方交叉校验外部缓冲描述；收拢成结构体会把
    // 「校验三件套 + 载荷 + 元数据」的调用形状搅碎，得不偿失。
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        format: PixelFormat,
        width: u32,
        height: u32,
        layout: FrameLayout,
        data: Box<[u8]>,
        color: ColorDesc,
        pts: Option<Timestamp>,
        duration: Option<Timestamp>,
    ) -> Result<VideoFrame, CodecError> {
        if data.len() != layout.size() {
            return Err(CodecError::BufferMismatch {
                expected: layout.size(),
                actual: data.len(),
            });
        }
        if layout.format() != format || layout.width() != width || layout.height() != height {
            return Err(CodecError::InvalidArguments(
                "frame layout does not match format/width/height",
            ));
        }
        Ok(VideoFrame {
            layout,
            data,
            color,
            pts,
            duration,
        })
    }

    /// 帧宽（像素）。
    pub fn width(&self) -> u32 {
        self.layout.width()
    }

    /// 帧高（像素）。
    pub fn height(&self) -> u32 {
        self.layout.height()
    }

    /// 像素格式。
    pub fn format(&self) -> PixelFormat {
        self.layout.format()
    }

    /// 帧布局（跨距/行数/偏移，缓冲分配的依据）。
    pub fn layout(&self) -> &FrameLayout {
        &self.layout
    }

    /// 色彩描述。
    pub fn color(&self) -> ColorDesc {
        self.color
    }

    /// 设置色彩描述。
    pub fn set_color(&mut self, color: ColorDesc) {
        self.color = color;
    }

    /// 显示时戳（自带时基）。
    pub fn pts(&self) -> Option<Timestamp> {
        self.pts
    }

    /// 设置显示时戳。
    pub fn set_pts(&mut self, pts: Option<Timestamp>) {
        self.pts = pts;
    }

    /// 帧时长（自带时基）。
    pub fn duration(&self) -> Option<Timestamp> {
        self.duration
    }

    /// 设置帧时长。
    pub fn set_duration(&mut self, duration: Option<Timestamp>) {
        self.duration = duration;
    }

    /// 第 `plane` 个平面的只读切片（`data[offset..offset + size]`）。
    ///
    /// # Panics（全库五处文档写明编程错误的第⑤处）
    ///
    /// `plane >= layout.plane_count()` 时 panic，消息含非法索引与平面数。
    /// 纯调用方索引错误，无数据驱动路径（数据驱动失败一律走 `Result`）。
    pub fn plane(&self, plane: usize) -> &[u8] {
        let count = self.layout.plane_count() as usize;
        assert!(
            plane < count,
            "VideoFrame::plane: plane index out of range: {plane} >= {count} planes of {}",
            self.layout.format().name()
        );
        let p = self.layout.plane(plane);
        &self.data[p.offset..p.offset + p.size]
    }

    /// 第 `plane` 个平面的可变切片。panic 条件同 [`VideoFrame::plane`]。
    pub fn plane_mut(&mut self, plane: usize) -> &mut [u8] {
        let count = self.layout.plane_count() as usize;
        assert!(
            plane < count,
            "VideoFrame::plane: plane index out of range: {plane} >= {count} planes of {}",
            self.layout.format().name()
        );
        let p = self.layout.plane(plane);
        &mut self.data[p.offset..p.offset + p.size]
    }
}

/// 音频帧（私有字段 + 访问器）。
///
/// 帧内样本按 [`SampleFormat`] 平面性组织：planar = 逐声道连续平面
/// （每声道 `frames × bytes_per_sample` 字节），packed = 声道交织。
/// 声道级切片访问推迟到 M4 轮次，本类型只承载整块缓冲。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFrame {
    format: SampleFormat,
    channel_layout: ChannelLayout,
    sample_rate: u32,
    frames: usize,
    data: Box<[u8]>,
    pts: Option<Timestamp>,
}

impl AudioFrame {
    /// 分配全新音频帧：缓冲长度 =
    /// [`SampleFormat::buffer_len`]`(format, channel_layout.channels(), frames)`
    /// 字节，零初始化。
    ///
    /// # Errors
    /// `sample_rate == 0` → [`CodecError::InvalidArguments`]。
    /// （`channel_layout` 非空由 `ChannelLayout` 类型不变式保证。）
    pub fn new(
        format: SampleFormat,
        channel_layout: ChannelLayout,
        sample_rate: u32,
        frames: usize,
        pts: Option<Timestamp>,
    ) -> Result<AudioFrame, CodecError> {
        if sample_rate == 0 {
            return Err(CodecError::InvalidArguments("sample rate is zero"));
        }
        let len = SampleFormat::buffer_len(format, channel_layout.channels(), frames);
        Ok(AudioFrame {
            format,
            channel_layout,
            sample_rate,
            frames,
            data: vec![0u8; len].into_boxed_slice(),
            pts,
        })
    }

    /// 采样格式。
    pub fn format(&self) -> SampleFormat {
        self.format
    }

    /// 声道布局。
    pub fn channel_layout(&self) -> &ChannelLayout {
        &self.channel_layout
    }

    /// 采样率（Hz）。
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// 帧数（一帧 = 同一时刻所有声道各一个样本）。
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// 整块音频缓冲（长度 = `buffer_len`，布局语义见类型文档）。
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// 整块音频缓冲的可变视图。
    pub fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    /// 显示时戳（自带时基）。
    pub fn pts(&self) -> Option<Timestamp> {
        self.pts
    }

    /// 设置显示时戳。
    pub fn set_pts(&mut self, pts: Option<Timestamp>) {
        self.pts = pts;
    }
}

/// 解码输出（枚举分载，替代 FFmpeg 的 EAGAIN/EOF 哨兵值）。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`
/// （支撑 `pull() == NeedInput` 一类相等断言）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// 视频帧。
    Video(VideoFrame),
    /// 音频帧。
    Audio(AudioFrame),
}

impl Frame {
    /// 媒体类型（委托内层帧种类）。
    pub fn media_type(&self) -> MediaType {
        match self {
            Frame::Video(_) => MediaType::Video,
            Frame::Audio(_) => MediaType::Audio,
        }
    }

    /// 显示时戳（委托内层帧）。
    pub fn pts(&self) -> Option<Timestamp> {
        match self {
            Frame::Video(v) => v.pts(),
            Frame::Audio(a) => a.pts(),
        }
    }

    /// 设置显示时戳（委托内层帧）。
    pub fn set_pts(&mut self, pts: Option<Timestamp>) {
        match self {
            Frame::Video(v) => v.set_pts(pts),
            Frame::Audio(a) => a.set_pts(pts),
        }
    }
}

/// [`crate::Decoder::pull`] 的结果。
///
/// 语义：`NeedInput` = 需继续喂包（或尚未 flush 排空前的等待）；
/// `Finished` = flush 后已排空，且**此后** `pull` 幂等返回 `Finished`、
/// `feed` 返回 `Err(InvalidState(..))`——解码器实现必须遵守该状态机。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`。
// 不 Box `Frame`：帧数据本体已在 `Box<[u8]>` 上，多出的 ~224 字节只是
// 栈上头部；`pull()` 按值返回、逐帧一次，Box 化徒增每次解码的堆分配。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoded {
    /// 取得一帧。
    Frame(Frame),
    /// 暂无输出，需要更多输入（或等待 flush）。
    NeedInput,
    /// flush 后已排空，终态。
    Finished,
}

/// [`crate::Encoder::pull`] 的结果，语义与 [`Decoded`] 对称。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Encoded {
    /// 取得一包。
    Packet(Packet),
    /// 暂无输出，需要更多输入（或等待 flush）。
    NeedInput,
    /// flush 后已排空，终态。
    Finished,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ye_pixfmt::PixfmtError;
    use ye_time::Rational;

    fn ts(ticks: i64) -> Timestamp {
        Timestamp::new(ticks, Rational::new(1, 25))
    }

    // ---- testPlan §2：Yuv420p 1920×1080 布局与缓冲 ----

    #[test]
    fn video_frame_new_yuv420p_1080p() {
        let f = VideoFrame::new(
            PixelFormat::Yuv420p,
            1920,
            1080,
            ColorDesc::UNSPECIFIED,
            Some(ts(10)),
            Some(ts(1)),
        )
        .unwrap();
        assert_eq!(f.width(), 1920);
        assert_eq!(f.height(), 1080);
        assert_eq!(f.format(), PixelFormat::Yuv420p);
        // DEFAULT_STRIDE_ALIGN=64 下 1920/960 本就 64 对齐，无行尾填充：
        // 总长恰为 1920×1080 + 960×540×2 = 3110400
        assert_eq!(f.layout().size(), 3_110_400);
        // data 长度恰为布局总长（不变式；经三平面切片全覆盖核对）
        assert_eq!(
            f.plane(0).len() + f.plane(1).len() + f.plane(2).len(),
            3_110_400
        );
        // Y 平面 1920×1080；U 平面起点紧接 Y 平面之后
        assert_eq!(f.plane(0).len(), 2_073_600);
        assert_eq!(f.layout().plane(1).offset, 2_073_600);
        // pts/duration 存取
        assert_eq!(f.pts(), Some(ts(10)));
        assert_eq!(f.duration(), Some(ts(1)));
        // 零初始化
        assert!(f.plane(0).iter().all(|&b| b == 0));
    }

    // ---- testPlan §3：ZeroDimension 透出与 From 链路 ----

    #[test]
    fn video_frame_new_zero_dimension() {
        // 宽高 0 经 CodecError::Pixfmt 包装透出，assert_eq 可用（CodecError 有 PartialEq）
        assert_eq!(
            VideoFrame::new(
                PixelFormat::Yuv420p,
                0,
                0,
                ColorDesc::UNSPECIFIED,
                None,
                None
            ),
            Err(CodecError::Pixfmt(PixfmtError::ZeroDimension))
        );
        assert_eq!(
            VideoFrame::new(
                PixelFormat::Yuv420p,
                0,
                5,
                ColorDesc::UNSPECIFIED,
                None,
                None
            ),
            Err(CodecError::Pixfmt(PixfmtError::ZeroDimension))
        );
        assert_eq!(
            VideoFrame::new(
                PixelFormat::Yuv420p,
                5,
                0,
                ColorDesc::UNSPECIFIED,
                None,
                None
            ),
            Err(CodecError::Pixfmt(PixfmtError::ZeroDimension))
        );
    }

    /// `?` 转换链路断言：内部布局计算失败经 `From<PixfmtError>` 透出。
    fn zero_sized_frame() -> Result<VideoFrame, CodecError> {
        let frame = VideoFrame::new(
            PixelFormat::Yuv420p,
            0,
            0,
            ColorDesc::UNSPECIFIED,
            None,
            None,
        )?;
        Ok(frame)
    }

    #[test]
    fn from_pixfmt_question_mark_chain() {
        assert_eq!(
            zero_sized_frame(),
            Err(CodecError::Pixfmt(PixfmtError::ZeroDimension))
        );
    }

    // ---- testPlan §4：from_parts 校验与 plane 越界 panic ----

    #[test]
    fn from_parts_buffer_mismatch() {
        let layout = PixelFormat::Yuv420p
            .frame_layout(64, 64, DEFAULT_STRIDE_ALIGN)
            .unwrap();
        let too_short = vec![0u8; layout.size() - 1].into_boxed_slice();
        assert_eq!(
            VideoFrame::from_parts(
                PixelFormat::Yuv420p,
                64,
                64,
                layout,
                too_short,
                ColorDesc::UNSPECIFIED,
                None,
                None
            ),
            Err(CodecError::BufferMismatch {
                expected: layout.size(),
                actual: layout.size() - 1
            })
        );
        let too_long = vec![0u8; layout.size() + 8].into_boxed_slice();
        assert_eq!(
            VideoFrame::from_parts(
                PixelFormat::Yuv420p,
                64,
                64,
                layout,
                too_long,
                ColorDesc::UNSPECIFIED,
                None,
                None
            ),
            Err(CodecError::BufferMismatch {
                expected: layout.size(),
                actual: layout.size() + 8
            })
        );
    }

    #[test]
    fn from_parts_accepts_matching_buffer() {
        let layout = PixelFormat::Nv12
            .frame_layout(320, 240, DEFAULT_STRIDE_ALIGN)
            .unwrap();
        let data = vec![0u8; layout.size()].into_boxed_slice();
        let f = VideoFrame::from_parts(
            PixelFormat::Nv12,
            320,
            240,
            layout,
            data,
            ColorDesc::UNSPECIFIED,
            None,
            None,
        )
        .unwrap();
        assert_eq!(f.width(), 320);
        assert_eq!(f.height(), 240);
        assert_eq!(f.format(), PixelFormat::Nv12);
        assert_eq!(f.layout().size(), 320 * 240 * 3 / 2); // 320/240 恰 64 对齐
    }

    #[test]
    fn from_parts_rejects_mismatched_description() {
        // 缓冲长度正确，但 layout 与 format/width/height 描述不符
        let layout = PixelFormat::Yuv420p
            .frame_layout(64, 64, DEFAULT_STRIDE_ALIGN)
            .unwrap();
        let data = vec![0u8; layout.size()].into_boxed_slice();
        assert_eq!(
            VideoFrame::from_parts(
                PixelFormat::Yuv422p, // 宽高相同但格式不同 → 布局不同
                64,
                64,
                layout,
                data,
                ColorDesc::UNSPECIFIED,
                None,
                None
            ),
            Err(CodecError::InvalidArguments(
                "frame layout does not match format/width/height"
            ))
        );
    }

    #[test]
    #[should_panic(expected = "plane index out of range")]
    fn video_frame_plane_out_of_range_panics() {
        let f = VideoFrame::new(
            PixelFormat::Yuv420p,
            16,
            16,
            ColorDesc::UNSPECIFIED,
            None,
            None,
        )
        .unwrap();
        let _ = f.plane(f.layout().plane_count() as usize);
    }

    #[test]
    fn video_frame_plane_mut_and_color() {
        let mut f =
            VideoFrame::new(PixelFormat::Gray8, 4, 4, ColorDesc::UNSPECIFIED, None, None).unwrap();
        for b in f.plane_mut(0).iter_mut() {
            *b = 0xAB;
        }
        assert!(f.plane(0).iter().all(|&b| b == 0xAB));
        let mut color = ColorDesc::UNSPECIFIED;
        color.range = ye_pixfmt::ColorRange::Jpeg;
        f.set_color(color);
        assert_eq!(f.color(), color);
        // pts/duration 可变
        f.set_pts(Some(ts(3)));
        f.set_duration(None);
        assert_eq!(f.pts(), Some(ts(3)));
        assert_eq!(f.duration(), None);
    }

    // ---- testPlan §5：AudioFrame ----

    #[test]
    fn audio_frame_new_i16_stereo() {
        let pts = Timestamp::new(0, Rational::new(1, 48_000));
        let mut f = AudioFrame::new(
            SampleFormat::I16,
            ChannelLayout::STEREO,
            48_000,
            480,
            Some(pts),
        )
        .unwrap();
        // buffer_len(I16, 2, 480) = 480 帧 × 2 声道 × 2 字节 = 1920
        assert_eq!(SampleFormat::buffer_len(SampleFormat::I16, 2, 480), 1920);
        assert_eq!(f.data().len(), 1920);
        assert_eq!(f.frames(), 480);
        assert_eq!(f.sample_rate(), 48_000);
        assert_eq!(f.format(), SampleFormat::I16);
        assert_eq!(*f.channel_layout(), ChannelLayout::STEREO);
        assert_eq!(f.pts(), Some(pts));
        // data_mut 可写
        for b in f.data_mut() {
            *b = 0x7F;
        }
        assert!(f.data().iter().all(|&b| b == 0x7F));
        f.set_pts(None);
        assert_eq!(f.pts(), None);
    }

    #[test]
    fn audio_frame_planar_layout() {
        // planar：channels × frames × bps = 2 × 240 × 4 = 1920
        let f =
            AudioFrame::new(SampleFormat::F32P, ChannelLayout::STEREO, 44_100, 240, None).unwrap();
        assert_eq!(f.data().len(), 2 * 240 * 4);
        assert_eq!(f.frames(), 240);
        assert_eq!(f.sample_rate(), 44_100);
    }

    #[test]
    fn audio_frame_zero_sample_rate() {
        assert_eq!(
            AudioFrame::new(SampleFormat::I16, ChannelLayout::STEREO, 0, 480, None),
            Err(CodecError::InvalidArguments("sample rate is zero"))
        );
    }

    // ---- testPlan §9：Frame 统一入口 ----

    #[test]
    fn frame_media_type_and_pts_delegation() {
        let v = VideoFrame::new(
            PixelFormat::Gray8,
            8,
            8,
            ColorDesc::UNSPECIFIED,
            Some(ts(5)),
            None,
        )
        .unwrap();
        let a = AudioFrame::new(
            SampleFormat::I16,
            ChannelLayout::MONO,
            48_000,
            16,
            Some(ts(6)),
        )
        .unwrap();
        let mut fv = Frame::Video(v);
        let mut fa = Frame::Audio(a);
        assert_eq!(fv.media_type(), MediaType::Video);
        assert_eq!(fa.media_type(), MediaType::Audio);
        assert_eq!(fv.pts(), Some(ts(5)));
        assert_eq!(fa.pts(), Some(ts(6)));
        // set_pts 委托内层生效
        fv.set_pts(Some(ts(50)));
        fa.set_pts(None);
        assert_eq!(fv.pts(), Some(ts(50)));
        assert_eq!(fa.pts(), None);
        match &mut fv {
            Frame::Video(inner) => assert_eq!(inner.pts(), Some(ts(50))),
            Frame::Audio(_) => panic!("variant changed"),
        }
    }
}
