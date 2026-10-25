//! AAC 滤波器组纯函数核：IMDCT 与窗序列（sine/KBD 双族）。
//!
//! # 接口契约（主链按此消费）
//!
//! **输入**：频谱系数切片（f32，长度 = 每帧系数数）＋窗口信息
//! [`WindowInfo`]（块序列 [`BlockSequence`] × 前半/后半窗形状
//! [`WindowShape`]）＋帧长 [`FrameLength`]（1024 或 960）＋上一帧
//! 留下的重叠尾部 `prev_tail`。
//!
//! **输出**：[`SynthOutput`]——本帧交付的时域样本 `samples`
//!（f32，长度 = 每帧样本数 M）＋留给下一帧的重叠尾部 `tail`（f64，
//! 调用方原样传入下一帧的 `prev_tail`；首帧传空切片按全零处理）。
//!
//! ```text
//! coeffs[M] + WindowInfo + prev_tail ──synth_frame──▶ samples[M] + tail
//!                                          │                        │
//!                                    下游 PCM 输出        下一帧 prev_tail
//! ```
//!
//! **纯函数、无状态**：本模块不持有任何解码状态；帧间重叠所需的
//! 尾部样本由调用方显式传递（`tail` → 下一帧 `prev_tail`）。同一
//! 输入永远得到同一输出（内部 [`OnceLock`] 仅缓存窗表/余弦表等
//! 确定性查表数据，不影响语义）。
//!
//! # 帧型与变换长度
//!
//! | 帧长 [`FrameLength`] | 长窗 IMDCT | 长窗长 2M | 短窗块 | 短块系数 m | 短窗长 2m |
//! |---|---|---|---|---|---|
//! | [`N1024`](FrameLength::N1024) | 1024 系数 | 2048 | 8 块 | 128 | 256 |
//! | [`N960`](FrameLength::N960)   | 960 系数  | 1920 | 8 块 | 120 | 240 |
//!
//! [`EightShort`](BlockSequence::EightShort) 帧的 `coeffs` 为 8 个短块
//! 系数**依时间序拼接**（块 0 在前），每块 m 个。
//!
//! # 窗序列与布局
//!
//! 每帧交付 M 个样本。长块变换窗长 2M，覆盖交付区及其后 M 点；
//! 短帧 8 块步进 m，链首相对交付区起点偏移 `g = (M−m)/2`
//!（1024 帧 g=448、960 帧 g=420），链尾溢出交付区 `g+m` 点。
//! 过渡窗 [`LongStart`](BlockSequence::LongStart) /
//! [`LongStop`](BlockSequence::LongStop) 由四段拼成（M=1024 为例）：
//!
//! ```text
//! LongStart: 长升半[0,1024) │ 恒1[1024,1472) │ 短降[1472,1600) │ 零[1600,2048)
//! LongStop:  零[0,448)      │ 短升[448,576)  │ 恒1[576,1024)   │ 长降半[1024,2048)
//! ```
//!
//! **形状位语义**（与 AAC window_shape 一致）：`shape_curr` 作用于
//! 本帧后半（长降半段 / 短块窗 / LongStart 的短降段 / LongStop 的
//! 长降半段），`shape_prev` 作用于本帧前半（长升半段 / LongStop 的
//! 短升段）。**调用方约束**：本帧 `shape_prev` 必须等于上一帧
//! `shape_curr`（物理交界处两侧须同族窗，TDAC 才能对消）。
//!
//! # 窗表
//!
//! - **sine 族**：`w[n] = sin((n+0.5)·π/(2M))`，对称且严格满足
//!   功率互补（TDAC）`w²[n] + w²[n+M] = 1`；
//! - **KBD 族**（Kaiser-Bessel Derived）：Kaiser 核
//!   `p[j] = I₀(πα·√(1−((2j−(M−1))/M)²))`（j=0..M−1，对称），
//!   半格累加 `w²[n] = (S(n)+S(n−1))/(2·S(M−1))` 后开方、镜像对称
//!   延拓——半格错位使对称性与 TDAC **代数精确**成立。α 按规范
//!   取值：长窗（2048/1920）α=4，短窗（256/240）α=6。I₀ 为第一类
//!   修正贝塞尔函数（幂级数实现）。
//!
//! 表为 f32（贴近主链实际消费精度），计算在 f64 中进行。
//!
//! # 变换定义与标定
//!
//! ```text
//! MDCT:   X[k] = Σ_{n=0}^{2M−1} x[n]·cos(π/M·(n+0.5+M/2)·(k+0.5))
//! IMDCT:  x̂[n] = (2/M)·Σ_{k=0}^{M−1} X[k]·cos(π/M·(n+0.5+M/2)·(k+0.5))
//! ```
//!
//! 该标定下「分析加窗 w → MDCT → IMDCT → 合成加窗 w → 帧间重叠
//! 相加」**精确重构原信号**（TDAC；见端到端单测，1024/960 ×
//! sine/KBD × 长窗/短窗/切换序列误差均在浮点精度量级）。
//! [`mdct`] 供编码侧与一致性测试使用，解码主链只消费
//! [`synth_frame`]。
//!
//! # 性能定位
//!
//! IMDCT 为朴素 O(M²) 直接实现（长块每帧约 2M 次乘加），正确性
//! 与清晰度优先；余弦表按变换长度缓存（约 31 MB 常驻，四个长度
//! 合计）。FFT 化（O(M log M)）留待主链性能验收后按需替换，接口
//! 不变。
//!
//! # 失败模式
//!
//! 尺寸/长度不符一律 [`FilterbankError`]（不 panic）：窗表长度
//! 不在 {2048, 1920, 256, 240}、变换长度不被支持、`coeffs` 长度
//! 与帧型不符、`prev_tail` 超长。

use std::fmt;
use std::sync::OnceLock;

/// 帧长（每帧频谱系数数 / 交付样本数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameLength {
    /// 标准长窗：1024 系数，短窗 8×128。
    N1024,
    /// 低延迟变体帧长：960 系数，短窗 8×120。
    N960,
}

impl FrameLength {
    /// 每帧频谱系数数 M（= 交付样本数）。
    pub const fn coeffs(self) -> usize {
        match self {
            FrameLength::N1024 => 1024,
            FrameLength::N960 => 960,
        }
    }
    /// 短窗块系数数 m = M/8。
    pub const fn short_coeffs(self) -> usize {
        match self {
            FrameLength::N1024 => 128,
            FrameLength::N960 => 120,
        }
    }
    /// 过渡段长度 g = (M−m)/2（LongStart/LongStop 的恒 1 段与零段长度）。
    const fn transition(self) -> usize {
        (self.coeffs() - self.short_coeffs()) / 2
    }
}

/// 窗序列（ISO/IEC 14496-3 window_sequence）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockSequence {
    /// 仅长窗：前帧长、后帧长。
    OnlyLong,
    /// 长→短过渡窗。
    LongStart,
    /// 八短窗块。
    EightShort,
    /// 短→长过渡窗。
    LongStop,
}

/// 窗形状族（ISO/IEC 14496-3 window_shape）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowShape {
    /// 正弦窗。
    Sine,
    /// Kaiser-Bessel Derived 窗。
    Kbd,
}

/// 窗口信息：块序列 + 前半/后半窗形状。
///
/// 调用方约束：本帧 `shape_prev` 必须等于上一帧 `shape_curr`。
#[derive(Debug, Clone, Copy)]
pub struct WindowInfo {
    /// 块序列。
    pub sequence: BlockSequence,
    /// 前半窗形状（= 上一帧的 shape_curr）。
    pub shape_prev: WindowShape,
    /// 后半窗形状（短块窗族 / 本帧后半段）。
    pub shape_curr: WindowShape,
}

/// 单帧合成结果：交付样本 + 留给下一帧的重叠尾部。
#[derive(Debug, Clone)]
pub struct SynthOutput {
    /// 本帧交付的时域样本（f32，长度 M）。
    pub samples: Vec<f32>,
    /// 重叠尾部（f64）。长度：长块序列（OnlyLong/LongStart/LongStop）
    /// 为 M；EightShort 为 g+m。原样传入下一帧 `prev_tail`。
    pub tail: Vec<f64>,
}

/// 滤波器组错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterbankError {
    /// 窗表长度不在支持集 {2048, 1920, 256, 240}。
    UnsupportedWindowLen(usize),
    /// 变换长度（系数数）不被支持：仅 1024/960/128/120。
    UnsupportedTransformLen(usize),
    /// 频谱系数长度与帧型不符（期望，实际）。
    BadCoeffsLen { expect: usize, got: usize },
    /// prev_tail 长度超过本帧交付区宽度 M。
    BadPrevTailLen(usize),
    /// 对 EightShort 请求长块帧窗（八短块直接用短窗表）。
    ShortSequenceHasNoLongWindow,
}

impl fmt::Display for FilterbankError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FilterbankError::UnsupportedWindowLen(n) => {
                write!(f, "窗表长度不支持：{n}（仅 2048/1920/256/240）")
            }
            FilterbankError::UnsupportedTransformLen(n) => {
                write!(f, "变换长度不支持：{n} 个系数（仅 1024/960/128/120）")
            }
            FilterbankError::BadCoeffsLen { expect, got } => {
                write!(f, "系数长度不符：期望 {expect}，实际 {got}")
            }
            FilterbankError::BadPrevTailLen(n) => {
                write!(f, "prev_tail 超长：{n} > 交付区宽度")
            }
            FilterbankError::ShortSequenceHasNoLongWindow => {
                write!(f, "EightShort 无长块帧窗，短块直接用短窗表")
            }
        }
    }
}

impl std::error::Error for FilterbankError {}

// ---------------------------------------------------------------------------
// 窗表
// ---------------------------------------------------------------------------

/// KBD 窗 α 参数：长窗 4、短窗 6（ISO/IEC 14496-3 规范值）。
const fn kbd_alpha(len: usize) -> f64 {
    if len >= 512 { 4.0 } else { 6.0 }
}

/// 第一类修正贝塞尔函数 I₀(x)，幂级数（自变量范围 ≤ 6π，收敛快）。
fn bessel_i0(x: f64) -> f64 {
    // I₀(x) = Σ_{k≥0} ((x/2)^k / k!)²，项比 t_{k+1}/t_k = (x/2)²/(k+1)²
    let half_sq = (x * 0.5) * (x * 0.5);
    let mut sum = 1.0;
    let mut term = 1.0;
    for k in 1..128 {
        term *= half_sq / ((k * k) as f64);
        sum += term;
        if term < f64::EPSILON * sum {
            break;
        }
    }
    sum
}

fn build_sine(len: usize) -> Vec<f32> {
    let n = len as f64;
    (0..len)
        .map(|i| (((i as f64) + 0.5) * std::f64::consts::PI / n).sin() as f32)
        .collect()
}

/// KBD 窗生成（对称半格累加构造，见模块文档）。
fn build_kbd(len: usize) -> Vec<f32> {
    let m = len / 2;
    let alpha = kbd_alpha(len);
    // 对称 Kaiser 核 p[j] = I0(πα√(1−((2j−(M−1))/M)²))，j = 0..M−1
    let mut p = vec![0.0f64; m];
    let mf = m as f64;
    let center = (m - 1) as f64;
    for (j, pj) in p.iter_mut().enumerate() {
        let u = (2.0 * j as f64 - center) / mf;
        let inner = 1.0 - u * u;
        let arg = if inner > 0.0 {
            std::f64::consts::PI * alpha * inner.sqrt()
        } else {
            0.0
        };
        *pj = bessel_i0(arg);
    }
    // 前缀和 S[n] = Σ_{j<n} p[j]；半格均值 u[n] = (S(n)+S(n−1))/(2·S(M))
    let mut s = vec![0.0f64; m + 1];
    for j in 0..m {
        s[j + 1] = s[j] + p[j];
    }
    let total = s[m];
    let mut w = vec![0.0f32; len];
    for n in 0..m {
        let v = ((s[n + 1] + s[n]) / (2.0 * total)).sqrt() as f32;
        w[n] = v;
        w[len - 1 - n] = v; // 镜像对称延拓
    }
    w
}

macro_rules! window_table {
    ($sine:ident, $kbd:ident, $len:expr) => {
        static $sine: OnceLock<Vec<f32>> = OnceLock::new();
        static $kbd: OnceLock<Vec<f32>> = OnceLock::new();
    };
}
window_table!(SINE_2048, KBD_2048, 2048);
window_table!(SINE_1920, KBD_1920, 1920);
window_table!(SINE_256, KBD_256, 256);
window_table!(SINE_240, KBD_240, 240);

/// sine 族窗表（长度 ∈ {2048, 1920, 256, 240}）。
pub fn sine_table(len: usize) -> Result<&'static [f32], FilterbankError> {
    match len {
        2048 => Ok(SINE_2048.get_or_init(|| build_sine(2048))),
        1920 => Ok(SINE_1920.get_or_init(|| build_sine(1920))),
        256 => Ok(SINE_256.get_or_init(|| build_sine(256))),
        240 => Ok(SINE_240.get_or_init(|| build_sine(240))),
        _ => Err(FilterbankError::UnsupportedWindowLen(len)),
    }
}

/// KBD 族窗表（α：长窗 4、短窗 6；长度 ∈ {2048, 1920, 256, 240}）。
pub fn kbd_table(len: usize) -> Result<&'static [f32], FilterbankError> {
    match len {
        2048 => Ok(KBD_2048.get_or_init(|| build_kbd(2048))),
        1920 => Ok(KBD_1920.get_or_init(|| build_kbd(1920))),
        256 => Ok(KBD_256.get_or_init(|| build_kbd(256))),
        240 => Ok(KBD_240.get_or_init(|| build_kbd(240))),
        _ => Err(FilterbankError::UnsupportedWindowLen(len)),
    }
}

/// 按形状族取窗表。
fn shape_table(shape: WindowShape, len: usize) -> Result<&'static [f32], FilterbankError> {
    match shape {
        WindowShape::Sine => sine_table(len),
        WindowShape::Kbd => kbd_table(len),
    }
}

/// 长块帧窗（长 2M）：按块序列拼接（见模块文档布局图）。
///
/// 仅对 OnlyLong/LongStart/LongStop 定义；EightShort 返回
/// [`FilterbankError::ShortSequenceHasNoLongWindow`]（短块直接用
/// [`sine_table`]/[`kbd_table`] 的短窗表）。
pub fn frame_window(win: &WindowInfo, len: FrameLength) -> Result<Vec<f32>, FilterbankError> {
    let m = len.coeffs();
    let wl = 2 * m;
    let ms = len.short_coeffs();
    let g = len.transition();
    match win.sequence {
        BlockSequence::OnlyLong => {
            // 前半（升）用 prev 族、后半（降）用 curr 族，可不同族。
            let prev = shape_table(win.shape_prev, wl)?;
            let curr = shape_table(win.shape_curr, wl)?;
            let mut w = Vec::with_capacity(wl);
            w.extend_from_slice(&prev[..m]);
            w.extend_from_slice(&curr[m..]);
            Ok(w)
        }
        BlockSequence::LongStart => {
            // 长升半(prev) │ 恒1 g │ 短降(curr, m 点) │ 零 g
            let prev = shape_table(win.shape_prev, wl)?;
            let curr = shape_table(win.shape_curr, 2 * ms)?;
            let mut w = Vec::with_capacity(wl);
            w.extend_from_slice(&prev[..m]);
            w.extend(std::iter::repeat_n(1.0f32, g));
            w.extend_from_slice(&curr[ms..]);
            w.extend(std::iter::repeat_n(0.0f32, g));
            debug_assert_eq!(w.len(), wl);
            Ok(w)
        }
        BlockSequence::LongStop => {
            // 零 g │ 短升(prev, m 点) │ 恒1 g │ 长降半(curr)
            let prev = shape_table(win.shape_prev, 2 * ms)?;
            let curr = shape_table(win.shape_curr, wl)?;
            let mut w = Vec::with_capacity(wl);
            w.extend(std::iter::repeat_n(0.0f32, g));
            w.extend_from_slice(&prev[..ms]);
            w.extend(std::iter::repeat_n(1.0f32, g));
            w.extend_from_slice(&curr[m..]);
            debug_assert_eq!(w.len(), wl);
            Ok(w)
        }
        BlockSequence::EightShort => Err(FilterbankError::ShortSequenceHasNoLongWindow),
    }
}

// ---------------------------------------------------------------------------
// MDCT / IMDCT（朴素 O(M²)，余弦表按变换长度缓存）
// ---------------------------------------------------------------------------

macro_rules! cos_table {
    ($name:ident) => {
        static $name: OnceLock<Vec<f64>> = OnceLock::new();
    };
}
cos_table!(COS_1024);
cos_table!(COS_960);
cos_table!(COS_128);
cos_table!(COS_120);

/// 余弦基表 cos(π/M·(n+0.5+M/2)·(k+0.5))，行 n（2M 行）× 列 k（M 列）。
fn cos_table(m: usize) -> Result<&'static [f64], FilterbankError> {
    let slot = match m {
        1024 => &COS_1024,
        960 => &COS_960,
        128 => &COS_128,
        120 => &COS_120,
        _ => return Err(FilterbankError::UnsupportedTransformLen(m)),
    };
    Ok(slot.get_or_init(|| {
        let mf = m as f64;
        let mut t = Vec::with_capacity(2 * m * m);
        for n in 0..2 * m {
            let base = std::f64::consts::PI / mf * (n as f64 + 0.5 + mf * 0.5);
            for k in 0..m {
                t.push((base * (k as f64 + 0.5)).cos());
            }
        }
        t
    }))
}

/// 正向 MDCT（编码侧/一致性测试用）。输入 2M 个 f64 样本，输出 M 个系数。
pub fn mdct(samples: &[f64]) -> Result<Vec<f32>, FilterbankError> {
    let two_m = samples.len();
    if !two_m.is_multiple_of(2) {
        return Err(FilterbankError::UnsupportedTransformLen(two_m / 2));
    }
    let m = two_m / 2;
    let table = cos_table(m)?;
    let mut out = vec![0.0f32; m];
    for k in 0..m {
        let mut acc = 0.0f64;
        for n in 0..two_m {
            acc += samples[n] * table[n * m + k];
        }
        out[k] = acc as f32;
    }
    Ok(out)
}

/// 逆向 IMDCT（合成核）。输入 M 个系数，输出 2M 个 f64 时域样本
///（含 TDAC 标定 2/M；加窗与重叠相加在 [`synth_frame`] 中进行）。
pub fn imdct(coeffs: &[f32]) -> Result<Vec<f64>, FilterbankError> {
    let m = coeffs.len();
    let table = cos_table(m)?;
    let scale = 2.0 / m as f64;
    let mut out = vec![0.0f64; 2 * m];
    for (n, o) in out.iter_mut().enumerate() {
        let row = &table[n * m..n * m + m];
        let mut acc = 0.0f64;
        for (c, t) in coeffs.iter().zip(row) {
            acc += *c as f64 * t;
        }
        *o = acc * scale;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// 帧合成（主链入口）
// ---------------------------------------------------------------------------

/// 单帧合成：频谱系数 → 时域样本（纯函数，见模块文档契约）。
///
/// `coeffs` 长度必须等于帧系数数（EightShort 为 8m 拼接）；
/// `prev_tail` 为上一帧 [`SynthOutput::tail`]（首帧传 `&[]` 按全零），
/// 长度不得超过 M。
pub fn synth_frame(
    coeffs: &[f32],
    win: &WindowInfo,
    len: FrameLength,
    prev_tail: &[f64],
) -> Result<SynthOutput, FilterbankError> {
    let m = len.coeffs();
    let ms = len.short_coeffs();
    let g = len.transition();
    if coeffs.len() != m {
        return Err(FilterbankError::BadCoeffsLen {
            expect: m,
            got: coeffs.len(),
        });
    }
    if prev_tail.len() > m {
        return Err(FilterbankError::BadPrevTailLen(prev_tail.len()));
    }

    // 本帧对绝对时间轴的全部贡献（从交付区起点起，超出部分进 tail）
    let mut contrib: Vec<f64>;
    let tail_len: usize;

    match win.sequence {
        BlockSequence::OnlyLong | BlockSequence::LongStart | BlockSequence::LongStop => {
            let w = frame_window(win, len)?;
            let raw = imdct(coeffs)?;
            contrib = Vec::with_capacity(2 * m);
            for (c, wi) in raw.into_iter().zip(w.iter()) {
                contrib.push(c * (*wi as f64));
            }
            tail_len = m;
        }
        BlockSequence::EightShort => {
            // 8 块链：块 i 覆盖 [g+i·m, g+i·m+2m)，链总长 M+g+m
            let ws = shape_table(win.shape_curr, 2 * ms)?;
            let chain_len = m + g + ms;
            contrib = vec![0.0f64; chain_len];
            for i in 0..8 {
                let blk = &coeffs[i * ms..(i + 1) * ms];
                let raw = imdct(blk)?;
                let off = g + i * ms;
                for (j, rj) in raw.into_iter().enumerate() {
                    contrib[off + j] += rj * (ws[j] as f64);
                }
            }
            tail_len = g + ms;
        }
    }

    // 交付 = 本帧贡献前 M 点 + 上帧尾部
    let mut samples = Vec::with_capacity(m);
    for (j, c) in contrib.iter().enumerate().take(m) {
        let prev = prev_tail.get(j).copied().unwrap_or(0.0);
        samples.push((c + prev) as f32);
    }
    let tail = contrib.split_off(m.min(contrib.len()));
    debug_assert_eq!(tail.len(), tail_len);

    Ok(SynthOutput { samples, tail })
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const TOL_WIN: f64 = 5e-7; // f32 窗表 vs f64 黄金值
    const TOL_IMDCT: f64 = 5e-8; // f64 通路解析样例
    const TOL_PR: f64 = 1e-5; // 端到端完美重构（f32 输出）

    // ---- 黄金值：窗表首/中/尾（python+numpy 独立计算，2026-10-25） ----

    #[test]
    fn sine_table_golden() {
        // sine L=2048: w[0]=sin(π/4096), w[1024]=cos(π/4096), w[2047]=sin(π/4096)
        let w = sine_table(2048).unwrap();
        assert!((w[0] as f64 - 7.669903187427045e-4).abs() < TOL_WIN);
        assert!((w[1024] as f64 - 0.9999997058628822).abs() < TOL_WIN);
        assert!((w[2047] as f64 - 7.669903187427045e-4).abs() < TOL_WIN);
        // sine L=1920
        let w = sine_table(1920).unwrap();
        assert!((w[0] as f64 - 8.181229956072532e-4).abs() < TOL_WIN);
        assert!((w[960] as f64 - 0.999999665337326).abs() < TOL_WIN);
        assert!((w[1919] as f64 - 8.181229956072532e-4).abs() < TOL_WIN);
        // sine L=256（短窗）
        let w = sine_table(256).unwrap();
        assert!((w[0] as f64 - 6.135884649154475e-3).abs() < TOL_WIN);
        assert!((w[128] as f64 - 0.9999811752826011).abs() < TOL_WIN);
        assert!((w[255] as f64 - 6.135884649154475e-3).abs() < TOL_WIN);
        // sine L=240（960 帧短窗）
        let w = sine_table(240).unwrap();
        assert!((w[0] as f64 - 6.544937967351858e-3).abs() < TOL_WIN);
        assert!((w[120] as f64 - 0.9999785816641292).abs() < TOL_WIN);
        assert!((w[239] as f64 - 6.544937967351858e-3).abs() < TOL_WIN);
        // 不支持长度
        assert!(matches!(
            sine_table(1024),
            Err(FilterbankError::UnsupportedWindowLen(1024))
        ));
    }

    #[test]
    fn kbd_table_golden() {
        // KBD 长窗 α=4（首尾相等：对称构造）
        let w = kbd_table(2048).unwrap();
        assert!((w[0] as f64 - 2.148452073462322e-4).abs() < TOL_WIN);
        assert!((w[1024] as f64 - 0.9999999769207681).abs() < TOL_WIN);
        assert!((w[2047] as f64 - 2.148452073462322e-4).abs() < TOL_WIN);
        let w = kbd_table(1920).unwrap();
        assert!((w[0] as f64 - 2.2244004163424167e-4).abs() < TOL_WIN);
        assert!((w[960] as f64 - 0.9999999752602136).abs() < TOL_WIN);
        // KBD 短窗 α=6
        let w = kbd_table(256).unwrap();
        assert!((w[0] as f64 - 5.310323524847102e-5).abs() < TOL_WIN);
        assert!((w[128] as f64 - 0.9999999985900232).abs() < TOL_WIN);
        assert!((w[255] as f64 - 5.310323524847102e-5).abs() < TOL_WIN);
        let w = kbd_table(240).unwrap();
        assert!((w[0] as f64 - 5.645269558308988e-5).abs() < TOL_WIN);
        assert!((w[120] as f64 - 0.9999999984065465).abs() < TOL_WIN);
        assert!((w[239] as f64 - 5.645269558308988e-5).abs() < TOL_WIN);
        assert!(matches!(
            kbd_table(512),
            Err(FilterbankError::UnsupportedWindowLen(512))
        ));
    }

    /// 窗族数学性质：对称性 + TDAC 功率互补（代数精确，浮点容差内）。
    #[test]
    fn window_properties_symmetry_and_tdac() {
        const WINDOW_LENS: [usize; 4] = [2048, 1920, 256, 240];
        for len in WINDOW_LENS {
            for table in [sine_table(len).unwrap(), kbd_table(len).unwrap()] {
                let half = len / 2;
                let mut max_sym = 0.0f64;
                let mut max_tdac = 0.0f64;
                for n in 0..len {
                    let a = table[n] as f64;
                    let b = table[len - 1 - n] as f64;
                    max_sym = max_sym.max((a - b).abs());
                    if n < half {
                        let c = table[n + half] as f64;
                        max_tdac = max_tdac.max((a * a + c * c - 1.0).abs());
                    }
                }
                assert!(max_sym < 1e-7, "L={len} 对称性 {max_sym}");
                assert!(max_tdac < 1e-6, "L={len} TDAC {max_tdac}");
            }
        }
    }

    /// 过渡窗四段结构：恒 1 段与零段的边界值（M=1024: [1024,1472)=1、
    /// [1600,2048)=0；[1472,1600) 短降；LongStop 镜像布局）。
    #[test]
    fn transition_window_segments() {
        let len = FrameLength::N1024;
        let (m, ms, g) = (1024usize, 128usize, 448usize);
        let st = frame_window(
            &WindowInfo {
                sequence: BlockSequence::LongStart,
                shape_prev: WindowShape::Sine,
                shape_curr: WindowShape::Sine,
            },
            len,
        )
        .unwrap();
        assert_eq!(st.len(), 2 * m);
        assert!(st[m] == 1.0 && st[m + g - 1] == 1.0, "恒1段");
        assert!(st[m + g + ms] == 0.0 && st[2 * m - 1] == 0.0, "零段");
        // 短降段首值 = 短窗降半首值（sine: cos 对称末端）
        let ws = sine_table(2 * ms).unwrap();
        assert!((st[m + g] - ws[ms]).abs() < 1e-7);
        // 升半段与长窗前半一致
        let wl = sine_table(2 * m).unwrap();
        for i in 0..m {
            assert!((st[i] - wl[i]).abs() < 1e-9);
        }
        let sp = frame_window(
            &WindowInfo {
                sequence: BlockSequence::LongStop,
                shape_prev: WindowShape::Kbd,
                shape_curr: WindowShape::Kbd,
            },
            len,
        )
        .unwrap();
        assert!(sp[0] == 0.0 && sp[g - 1] == 0.0, "零段");
        assert!(sp[g + ms] == 1.0 && sp[m - 1] == 1.0, "恒1段");
        let wks = kbd_table(2 * ms).unwrap();
        assert!((sp[g] - wks[0]).abs() < 1e-7, "短升段首 = 短窗升半首");
        // EightShort 无长块窗
        assert!(matches!(
            frame_window(
                &WindowInfo {
                    sequence: BlockSequence::EightShort,
                    shape_prev: WindowShape::Sine,
                    shape_curr: WindowShape::Sine
                },
                len
            ),
            Err(FilterbankError::ShortSequenceHasNoLongWindow)
        ));
    }

    // ---- 黄金值：IMDCT 解析样例（单系数余弦，python 独立计算） ----

    #[test]
    fn imdct_single_coefficient_golden() {
        // X[0]=1, M=1024: x̂[n] = (2/1024)·cos(π/1024·(n+512.5)·0.5)
        let mut x = vec![0.0f32; 1024];
        x[0] = 1.0;
        let y = imdct(&x).unwrap();
        assert_eq!(y.len(), 2048);
        assert!((y[0] - 1.380008260048261e-3).abs() < TOL_IMDCT);
        assert!((y[1] - 1.3778864812887739e-3).abs() < TOL_IMDCT);
        assert!((y[512] - (-1.4980279662943826e-6)).abs() < TOL_IMDCT);
        assert!((y[1023] - (-1.3800082600482605e-3)).abs() < TOL_IMDCT);
        assert!((y[2047] - (-1.3821267915150087e-3)).abs() < TOL_IMDCT);
        // X[60]=−3, M=120（960 帧短块）
        let mut x = vec![0.0f32; 120];
        x[60] = -3.0;
        let y = imdct(&x).unwrap();
        assert_eq!(y.len(), 240);
        assert!((y[0] - 3.272468983670084e-4).abs() < TOL_IMDCT);
        assert!((y[59] - (-3.558598030775858e-2)).abs() < TOL_IMDCT);
        assert!((y[119] - (-3.272468983682821e-4)).abs() < TOL_IMDCT);
        assert!((y[239] - 4.9998929083206446e-2).abs() < TOL_IMDCT);
        // X[100]=2, M=960
        let mut x = vec![0.0f32; 960];
        x[100] = 2.0;
        let y = imdct(&x).unwrap();
        assert_eq!(y.len(), 1920);
        assert!((y[0] - 2.4242187223737226e-3).abs() < TOL_IMDCT);
        assert!((y[480] - (-6.820942269915109e-4)).abs() < TOL_IMDCT);
        assert!((y[1919] - (-3.3888456290017788e-3)).abs() < TOL_IMDCT);
        // 不支持长度
        assert!(matches!(
            imdct(&[0.0; 100]),
            Err(FilterbankError::UnsupportedTransformLen(100))
        ));
    }

    // ---- 端到端：MDCT → IMDCT → 重叠相加 完美重构 ----

    /// 简单 LCG 伪随机（测试信号确定性）。
    struct Lcg(u64);
    impl Lcg {
        fn next_f32(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) as f32 / (u32::MAX >> 1) as f32) - 1.0
        }
    }

    /// 端到端流：给定帧型与窗口序列（含形状位），MDCT 分析整条流后
    /// 逐帧 synth_frame 合成，比对交付区中段误差。
    fn roundtrip(len: FrameLength, frames: &[(BlockSequence, WindowShape, WindowShape)]) {
        let m = len.coeffs();
        let ms = len.short_coeffs();
        let g = len.transition();
        let nf = frames.len();
        let total = nf * m + 2 * m; // 尾部留补零余量
        let mut rng = Lcg(0xC0FFEE);
        let x: Vec<f64> = (0..total)
            .map(|t| {
                (2.0 * std::f64::consts::PI * 440.0 * t as f64 / 48000.0).sin() * 0.6
                    + rng.next_f32() as f64 * 0.05
            })
            .collect();
        let seg = |a: usize, n: usize| -> Vec<f64> {
            let mut v: Vec<f64> = x[a..(a + n).min(total)].to_vec();
            v.resize(n, 0.0);
            v
        };

        // 分析（编码侧）：与合成同窗（frame_window / 短窗表）
        let mut all_coeffs: Vec<Vec<f32>> = Vec::with_capacity(nf);
        for (p, (seq, sp, sc)) in frames.iter().enumerate() {
            let wi = WindowInfo {
                sequence: *seq,
                shape_prev: *sp,
                shape_curr: *sc,
            };
            match seq {
                BlockSequence::EightShort => {
                    let ws = shape_table(*sc, 2 * ms).unwrap();
                    let mut coeffs = Vec::with_capacity(m);
                    for i in 0..8 {
                        let s0 = p * m + g + i * ms;
                        let src: Vec<f64> = seg(s0, 2 * ms)
                            .iter()
                            .zip(ws)
                            .map(|(v, w)| v * *w as f64)
                            .collect();
                        coeffs.extend(mdct(&src).unwrap());
                    }
                    all_coeffs.push(coeffs);
                }
                _ => {
                    let w = frame_window(&wi, len).unwrap();
                    let src: Vec<f64> = seg(p * m, 2 * m)
                        .iter()
                        .zip(&w)
                        .map(|(v, w)| v * *w as f64)
                        .collect();
                    all_coeffs.push(mdct(&src).unwrap());
                }
            }
        }

        // 合成（解码侧）：tail 链式传递
        let mut tail: Vec<f64> = Vec::new();
        let mut out: Vec<f32> = Vec::with_capacity(nf * m);
        for (p, (seq, sp, sc)) in frames.iter().enumerate() {
            let wi = WindowInfo {
                sequence: *seq,
                shape_prev: *sp,
                shape_curr: *sc,
            };
            let o = synth_frame(&all_coeffs[p], &wi, len, &tail).unwrap();
            let expect_tail = match seq {
                BlockSequence::EightShort => g + ms,
                _ => m,
            };
            assert_eq!(o.tail.len(), expect_tail, "帧 {p} tail 长度");
            tail = o.tail;
            out.extend(o.samples);
        }

        // 检查区：跳过流头 3 帧与流尾 1 帧（边缘帧不完美属 TDAC 预期）
        let (a0, a1) = (3 * m, (nf - 1) * m);
        let mut max_err = 0.0f64;
        for t in a0..a1 {
            let e = (out[t] as f64 - x[t]).abs();
            max_err = max_err.max(e);
        }
        assert!(
            max_err < TOL_PR,
            "{len:?} 帧序列 {:?}：max_err={max_err:.3e}",
            frames.iter().map(|f| f.0).collect::<Vec<_>>()
        );
    }

    const S: WindowShape = WindowShape::Sine;
    const K: WindowShape = WindowShape::Kbd;

    /// 长窗全序列（仅 OnlyLong）。
    #[test]
    fn perfect_reconstruction_long_only() {
        let f = |sp, sc| (BlockSequence::OnlyLong, sp, sc);
        roundtrip(FrameLength::N1024, &[f(S, S); 10]);
        roundtrip(FrameLength::N1024, &[f(K, K); 10]);
        roundtrip(FrameLength::N960, &[f(S, S); 10]);
        roundtrip(FrameLength::N960, &[f(K, K); 10]);
    }

    /// 短窗块全序列（仅 EightShort）。
    #[test]
    fn perfect_reconstruction_short_only() {
        let f = |sp, sc| (BlockSequence::EightShort, sp, sc);
        roundtrip(FrameLength::N1024, &[f(S, S); 10]);
        roundtrip(FrameLength::N1024, &[f(K, K); 10]);
        roundtrip(FrameLength::N960, &[f(S, S); 10]);
        roundtrip(FrameLength::N960, &[f(K, K); 10]);
    }

    /// 窗切换完整序列：长×3 → LongStart → 短×3 → LongStop → 长×4。
    #[test]
    fn perfect_reconstruction_window_switching() {
        let l = |sp, sc| (BlockSequence::OnlyLong, sp, sc);
        let st = |sp, sc| (BlockSequence::LongStart, sp, sc);
        let sh = |sp, sc| (BlockSequence::EightShort, sp, sc);
        let sp = |sp, sc| (BlockSequence::LongStop, sp, sc);
        // sine 全程
        roundtrip(
            FrameLength::N1024,
            &[
                l(S, S),
                l(S, S),
                l(S, S),
                st(S, S),
                sh(S, S),
                sh(S, S),
                sh(S, S),
                sp(S, S),
            ]
            .into_iter()
            .chain([l(S, S); 4])
            .collect::<Vec<_>>(),
        );
        // KBD 全程
        roundtrip(
            FrameLength::N1024,
            &[
                l(K, K),
                l(K, K),
                l(K, K),
                st(K, K),
                sh(K, K),
                sh(K, K),
                sh(K, K),
                sp(K, K),
            ]
            .into_iter()
            .chain([l(K, K); 4])
            .collect::<Vec<_>>(),
        );
        // 960 帧 sine / KBD
        roundtrip(
            FrameLength::N960,
            &[
                l(S, S),
                l(S, S),
                l(S, S),
                st(S, S),
                sh(S, S),
                sh(S, S),
                sh(S, S),
                sp(S, S),
            ]
            .into_iter()
            .chain([l(S, S); 4])
            .collect::<Vec<_>>(),
        );
        roundtrip(
            FrameLength::N960,
            &[
                l(K, K),
                l(K, K),
                l(K, K),
                st(K, K),
                sh(K, K),
                sh(K, K),
                sh(K, K),
                sp(K, K),
            ]
            .into_iter()
            .chain([l(K, K); 4])
            .collect::<Vec<_>>(),
        );
        // 频繁切换：短↔长往返
        roundtrip(
            FrameLength::N1024,
            &[
                l(S, S),
                st(S, S),
                sh(S, S),
                sp(S, S),
                l(S, S),
                st(S, S),
                sh(S, S),
                sp(S, S),
                l(S, S),
            ],
        );
    }

    /// 帧中途换窗族（shape_prev ≠ shape_curr，交界两侧各自同族）：
    /// L(sine,sine)×3 → L(sine,kbd) → L(kbd,kbd)×3。
    #[test]
    fn perfect_reconstruction_shape_change() {
        let l = |sp, sc| (BlockSequence::OnlyLong, sp, sc);
        roundtrip(
            FrameLength::N1024,
            &[
                l(S, S),
                l(S, S),
                l(S, S),
                l(S, K),
                l(K, K),
                l(K, K),
                l(K, K),
            ],
        );
        // 短窗族切换：START(sine→sine) SHORT(sine) STOP(sine→kbd) L(kbd)
        let st = |sp, sc| (BlockSequence::LongStart, sp, sc);
        let sh = |sp, sc| (BlockSequence::EightShort, sp, sc);
        let spo = |sp, sc| (BlockSequence::LongStop, sp, sc);
        roundtrip(
            FrameLength::N1024,
            &[
                l(S, S),
                l(S, S),
                st(S, S),
                sh(S, S),
                spo(S, K),
                l(K, K),
                l(K, K),
                l(K, K),
            ],
        );
    }

    /// 契约校验：错误路径。
    #[test]
    fn contract_errors() {
        let wi = WindowInfo {
            sequence: BlockSequence::OnlyLong,
            shape_prev: WindowShape::Sine,
            shape_curr: WindowShape::Sine,
        };
        // 系数长度不符
        assert!(matches!(
            synth_frame(&[0.0; 512], &wi, FrameLength::N1024, &[]),
            Err(FilterbankError::BadCoeffsLen {
                expect: 1024,
                got: 512
            })
        ));
        // EightShort 系数长度按 8m 计
        let ws = WindowInfo {
            sequence: BlockSequence::EightShort,
            shape_prev: WindowShape::Sine,
            shape_curr: WindowShape::Sine,
        };
        assert!(matches!(
            synth_frame(&[0.0; 1024], &ws, FrameLength::N960, &[]),
            Err(FilterbankError::BadCoeffsLen {
                expect: 960,
                got: 1024
            })
        ));
        // prev_tail 超长
        assert!(matches!(
            synth_frame(&[0.0; 1024], &wi, FrameLength::N1024, &[0.0; 1025]),
            Err(FilterbankError::BadPrevTailLen(1025))
        ));
        // 首帧空 tail：全零系数 → 静音输出、tail 长度 M
        let o = synth_frame(&[0.0; 1024], &wi, FrameLength::N1024, &[]).unwrap();
        assert_eq!(o.samples.len(), 1024);
        assert!(o.samples.iter().all(|&v| v == 0.0));
        assert_eq!(o.tail.len(), 1024);
        // mdct 奇数长度
        assert!(matches!(
            mdct(&[0.0; 7]),
            Err(FilterbankError::UnsupportedTransformLen(3))
        ));
    }

    /// 契约演示（文档示例）：静音帧 → 尾部传第二帧。
    #[test]
    fn tail_threading_between_frames() {
        let wi = WindowInfo {
            sequence: BlockSequence::OnlyLong,
            shape_prev: WindowShape::Sine,
            shape_curr: WindowShape::Sine,
        };
        let a = synth_frame(&[0.5; 1024], &wi, FrameLength::N1024, &[]).unwrap();
        // 第二帧用第一帧 tail：samples = 本帧贡献 + 上帧尾部
        let b1 = synth_frame(&[0.0; 1024], &wi, FrameLength::N1024, &a.tail).unwrap();
        let b0 = synth_frame(&[0.0; 1024], &wi, FrameLength::N1024, &[]).unwrap();
        // 全零第二帧在有/无上帧尾部时输出不同（重叠相加生效）
        assert_ne!(b1.samples, b0.samples);
        assert!(b0.samples.iter().all(|&v| v == 0.0));
    }
}
