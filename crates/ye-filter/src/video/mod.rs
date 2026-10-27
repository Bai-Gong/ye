//! 像素核（M2 起持续扩展）：像素格式转换与缩放/裁剪/fps/音量的
//! **纯函数内核**。
//!
//! 施工图④分工 (b)：本目录是纯函数层——无状态、无 I/O、无 panic 数据路径，
//! 全部失败走 `Result`。convert 泵（固定链序 decode→convert→scale→encode
//! 槽）按本模块的接口签名消费。
//!
//! ## 接口契约（convert 泵消费面）
//!
//! ```text
//! // —— 视频链（decode 槽产出 yuv420p 解码帧）——
//! pub struct Yuv420pView<'a>      // 三平面只读视图（跨距独立，直连解码帧平面）
//! pub struct Yuv420pBuffer       // 紧密 yuv420p 帧（rgb→yuv / scale_yuv 的输出）
//! pub enum YuvRange { Limited, Full }   // 有限/全程范围（映射 ColorRange Mpeg/Jpeg）
//!
//! pub fn yuv420p_to_rgba(src: &Yuv420pView, w: usize, h: usize, range: YuvRange)
//!     -> Result<Vec<u8>, FilterError>            // 紧密 RGBA（A=255），w·h·4 字节
//! pub fn yuv420p_to_rgb24(...) -> Result<Vec<u8>, FilterError>
//! pub fn rgba_to_yuv420p(rgba: &[u8], w, h, range) -> Result<Yuv420pBuffer, FilterError>
//! pub fn rgb24_to_yuv420p(...) -> Result<Yuv420pBuffer, FilterError>
//!
//! pub fn scale_yuv420p(src: &Yuv420pView, w, h, dst_w, dst_h)
//!     -> Result<Yuv420pBuffer, FilterError>      // swscale 双线性（见 scale.rs 头注）；偶数维
//! pub fn scale_rgba(rgba: &[u8], w, h, dst_w, dst_h) -> Result<Vec<u8>, FilterError>
//!                                                // 教科书双线性；泵的 scale 槽（RGBA 域）
//! pub fn crop_rgba(rgba: &[u8], w, h, x, y, cw, ch) -> Result<Vec<u8>, FilterError>
//! pub fn crop_yuv420p(src: &Yuv420pView, w, h, x, y, cw, ch)
//!     -> Result<Yuv420pBuffer, FilterError>      // 平面域裁剪；x/y/cw/ch 全偶校验（M3）
//! pub fn fps_nearest_frame_indices(pts: &[Timestamp], fps_num: u32, fps_den: u32)
//!     -> Result<Vec<usize>, FilterError>         // 时基重采样按最近帧（返回输入下标序列）；
//!                                                // 时戳自描述（M3 起入参为 ye-time Timestamp，
//!                                                // 跨时基比较只走 Timestamp::compare）
//! pub fn volume_f32(samples: &[f32], gain: f32) -> Result<Vec<f32>, FilterError>
//!                                                // 增益乘法 + [−1,1] 削波（音频链）
//! pub fn volume_gain_from_db(db: f32) -> Result<f32, FilterError>
//!                                                // dB → 线性增益：10^(dB/20)（M3）
//! ```
//!
//! 泵的接线形态（M2 剪抽帧）：`decode 帧 → Yuv420pView（frame.plane(i) +
//! frame.layout().plane(i).stride）→ yuv420p_to_rgba（range 取流参数，缺省
//! Limited）→ [crop_rgba] → scale_rgba → PNG/编码槽`；音频链 `AAC 解码 f32
//! → volume_f32 → 峰值/波形`。滤镜图参数承载（crop 的 x/y/w/h、fps 的
//! num/den、volume 的 gain）见 `crate::descriptor::FilterParams`（M3 定形）。
//!
//! ## 对拍口径与容差（写死，**只准收紧**）
//!
//! 对齐测试基线与容差声明见本文件 `testutil` 模块文档与各常量：
//!
//! | 对拍 | ffmpeg 参考 | 容差（写死） | 实测基线 |
//! | --- | --- | --- | --- |
//! | 转换核（同尺寸 yuv420p→rgba/rgb24） | `-sws_flags bilinear` + `-pix_fmt` 直转 | 1 | 有限范围 1（约 0.2% 像素 ±1）、全程范围 0 |
//! | 平面缩放（yuv420p→yuv420p） | `-sws_flags bilinear -vf scale=WxH:flags=bilinear` | 2 | ≤2（多为 1） |
//! | 缩放+转换链 | `scale=WxH:flags=bilinear,format=yuv420p,format=rgba`（两段式） | 4 | ≤4 |
//! | 平面域裁剪（M3） | `-vf crop=CW:CH:X:Y` | 0 | 0（纯字节搬运） |
//! | fps 最近帧选择（M3） | `-vf fps=N:round=near` | 选择序全等（0） | 全等（冻结几何族：24→24 / 24→12 / 25→30 / 单帧 24→30，见用例头注的结构性差异登记） |
//! | volume 增益乘法（M3） | `-af volume=G:precision=float`（f32le） | 1 ULP | 0 ULP |
//!
//! 容差是入库资产：向更严方向修改免评审；任何放宽须回施工图重新定口径
//! （对齐 `tests/conformance/src/tolerance.rs` 的 POLICY_NOTE）。已知结构性
//! 偏离（ffmpeg 单段合并链 vs 本模块两段组合，最大 ~7/通道）登记在
//! [`scale`] 模块头注。

pub mod convert;
pub mod scale;

pub use convert::{
    Yuv420pBuffer, Yuv420pView, YuvRange, rgb24_to_yuv420p, rgba_to_yuv420p, yuv420p_to_rgb24,
    yuv420p_to_rgba,
};
pub use scale::{
    crop_rgba, crop_yuv420p, fps_nearest_frame_indices, scale_rgba, scale_yuv420p, volume_f32,
    volume_gain_from_db,
};

/// ffmpeg oracle 对拍的测试公用件（仅测试编译）。
///
/// ## 容差声明（写死，只准收紧）
///
/// - [`ORACLE_CONVERT_MAX_ABS`]：转换核，**1**（实测有限范围 max=1、全程 0）；
/// - [`ORACLE_SCALE_YUV_MAX_ABS`]：平面缩放，**2**（实测 ≤2，多为 1）；
/// - [`ORACLE_CHAIN_MAX_ABS`]：两段式缩放+转换链，**4**（实测 ≤4）。
///
/// 放宽任何一条都须回施工图重新定口径。PATH 无 ffmpeg 时 oracle 组打印
/// 跳过信息返回（不 fail）——与 ye 全仓 oracle 用例的约定一致。
#[cfg(test)]
pub(crate) mod testutil {
    use std::path::PathBuf;
    use std::process::Command;

    /// 转换核容差（写死，只准收紧）。
    pub const ORACLE_CONVERT_MAX_ABS: u32 = 1;
    /// 平面缩放容差（写死，只准收紧）。
    pub const ORACLE_SCALE_YUV_MAX_ABS: u32 = 2;
    /// 两段式链容差（写死，只准收紧）。
    pub const ORACLE_CHAIN_MAX_ABS: u32 = 4;

    /// ffmpeg 是否可用（PATH 探测）。
    pub fn ffmpeg_available() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// 唯一临时文件路径（进程号 + 纳秒时钟，落系统临时目录，用后即删）。
    pub fn tmp_path(tag: &str, ext: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "ye-filter-oracle-{}-{}-{}.{}",
            tag,
            std::process::id(),
            nanos,
            ext
        ))
    }

    /// 确定性 LCG（oracle 向量发生器，避免引入随机库依赖）。
    #[derive(Clone)]
    pub struct Lcg(pub u64);

    impl Lcg {
        /// 下一个 u32。
        pub fn next_u32(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            (self.0 >> 33) as u32
        }

        /// [0, 256) 字节。
        pub fn next_byte(&mut self) -> u8 {
            (self.next_u32() % 256) as u8
        }
    }

    /// 逐字节最大绝对差。
    pub fn max_abs_diff(a: &[u8], b: &[u8]) -> u32 {
        assert_eq!(a.len(), b.len());
        let mut mx = 0u32;
        for (x, y) in a.iter().zip(b.iter()) {
            mx = mx.max((i16::from(*x) - i16::from(*y)).unsigned_abs() as u32);
        }
        mx
    }

    /// 运行 ffmpeg rawvideo 变换，读回输出字节；失败 panic（测试环境契约）。
    pub fn run_ffmpeg_raw(
        input: &[u8],
        in_w: usize,
        in_h: usize,
        in_pix: &str,
        args: &[&str],
        out_pix: &str,
    ) -> Vec<u8> {
        let inp = tmp_path("in", "bin");
        let outp = tmp_path("out", "bin");
        std::fs::write(&inp, input).expect("写 oracle 输入");
        let status = Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-sws_flags", "bilinear"])
            .args(["-f", "rawvideo", "-pix_fmt", in_pix])
            .arg("-s")
            .arg(format!("{in_w}x{in_h}"))
            .arg("-i")
            .arg(&inp)
            .args(args)
            .args(["-pix_fmt", out_pix, "-f", "rawvideo"])
            .arg(&outp)
            .status()
            .expect("启动 ffmpeg（PATH 需有 ffmpeg）");
        assert!(status.success(), "ffmpeg oracle 运行失败: {args:?}");
        let buf = std::fs::read(&outp).expect("读 oracle 输出");
        let _ = std::fs::remove_file(&inp);
        let _ = std::fs::remove_file(&outp);
        buf
    }

    /// 运行 ffmpeg f32le 单声道音频滤镜（`-af`），读回输出 f32 样本；
    /// 失败 panic（测试环境契约）。视频版见 [`run_ffmpeg_raw`]。
    pub fn run_ffmpeg_f32(input: &[f32], af_args: &[&str]) -> Vec<f32> {
        let inp = tmp_path("in", "f32");
        let outp = tmp_path("out", "f32");
        let bytes: Vec<u8> = input.iter().flat_map(|s| s.to_le_bytes()).collect();
        std::fs::write(&inp, bytes).expect("写 oracle 输入");
        let status = Command::new("ffmpeg")
            .args(["-y", "-v", "error"])
            .args(["-f", "f32le", "-ar", "48000", "-ac", "1"])
            .arg("-i")
            .arg(&inp)
            .args(["-af"])
            .args(af_args)
            .args(["-f", "f32le"])
            .arg(&outp)
            .status()
            .expect("启动 ffmpeg（PATH 需有 ffmpeg）");
        assert!(status.success(), "ffmpeg oracle 运行失败: {af_args:?}");
        let buf = std::fs::read(&outp).expect("读 oracle 输出");
        let _ = std::fs::remove_file(&inp);
        let _ = std::fs::remove_file(&outp);
        assert_eq!(buf.len() % 4, 0, "f32 输出字节长须为 4 的倍数");
        let (chunks, rest) = buf.as_chunks::<4>();
        assert!(rest.is_empty(), "f32 输出字节长须为 4 的倍数");
        chunks.iter().map(|c| f32::from_le_bytes(*c)).collect()
    }
}
