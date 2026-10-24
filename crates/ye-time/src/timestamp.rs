//! 时间戳 [`Timestamp`]：`i64` ticks + 所属时基，对标 FFmpeg 的 pts/dts
//! 与 `av_rescale_q` / `av_compare_ts`。

use core::cmp::Ordering;

use crate::error::TimeError;
use crate::rational::{Rational, gcd_u128};
use crate::rounding::{Rounding, div_round};

/// 时间戳：`i64` ticks + 所属时基 [`Rational`]（秒/tick），
/// 时刻/时长 = `ticks · time_base`。跨函数边界绝不裸传 `i64`。
///
/// ## 合法值域与不变式
///
/// 任何 `i64` ticks 均合法（**无哨兵值**：没有 `AV_NOPTS_VALUE` 等价物，
/// 缺时戳一律用 `Option<Timestamp>`）；时基为满足 [`Rational`] 不变式的归一化有理数。
///
/// ## derive 清单
///
/// `Debug, Clone, Copy, PartialEq, Eq, Hash`（全字段值类型，`Hash` 供缓存键用；
/// 同 ticks + 同时基才相等——跨时基的数值比较请用 [`Timestamp::compare`]）。
/// 不实现 `Ord`：跨时基排序必须走 [`Timestamp::compare`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Timestamp {
    ticks: i64,
    time_base: Rational,
}

impl Timestamp {
    /// 构造：任何 `i64` ticks 均合法（无哨兵值）。
    pub fn new(ticks: i64, time_base: Rational) -> Timestamp {
        Timestamp { ticks, time_base }
    }

    /// tick 数（属于 [`Timestamp::time_base`] 的时基）。
    pub fn ticks(&self) -> i64 {
        self.ticks
    }

    /// 所属时基（秒/tick）。
    pub fn time_base(&self) -> Rational {
        self.time_base
    }

    /// 重标度到 `to` 时基（`av_rescale_q` 等价），默认就近舍入（[`Rounding::NearInf`]）：
    /// `ticks·(from.num·to.den)/(from.den·to.num)`，全程 i128 中间量，
    /// 结果不落 i64 → `Err(Overflow)`；`to` 为 `0/1` → `Err(DivideByZero)`。
    pub fn rescale(self, to: Rational) -> Result<Timestamp, TimeError> {
        self.rescale_with(to, Rounding::NearInf)
    }

    /// 指定舍入模式的重标度（`av_rescale_q_rnd` 等价；不设 `PASS_MINMAX`——
    /// 本库无哨兵值，无须透传）。舍入语义见 [`Rounding`] 的 ±1.5 舍入表。
    pub fn rescale_with(self, to: Rational, rounding: Rounding) -> Result<Timestamp, TimeError> {
        if to.num() == 0 {
            return Err(TimeError::DivideByZero);
        }
        let from = self.time_base;
        let mut n = self.ticks as i128 * (from.num() as i128 * to.den() as i128);
        let mut d = from.den() as i128 * to.num() as i128;
        // 符号收在 to.num 上时把除数翻正（商不变）
        if d < 0 {
            n = -n;
            d = -d;
        }
        // |n| ≤ 2^63·2^62 = 2^125 < i128::MAX，恒不溢出
        let q = div_round(n, d, rounding);
        if q > i64::MAX as i128 || q < i64::MIN as i128 {
            return Err(TimeError::Overflow);
        }
        Ok(Timestamp {
            ticks: q as i64,
            time_base: to,
        })
    }

    /// 饱和便捷版：重标度溢出时按源 ticks 符号钳到 `i64::MIN`/`i64::MAX`
    /// （对应 FFmpeg 返回 `INT64_MIN` 的场景，改为显式钳位）。
    ///
    /// 目标时基为 `0/1` 的退化情形（重标度无定义）返回 0 ticks——
    /// `0/1` 时基下任何时刻均为 0 秒，取其规范表示 `0@0/1`。
    pub fn rescale_saturating(self, to: Rational, rounding: Rounding) -> Timestamp {
        match self.rescale_with(to, rounding) {
            Ok(t) => t,
            Err(TimeError::Overflow) => {
                let ticks = if self.ticks < 0 { i64::MIN } else { i64::MAX };
                Timestamp {
                    ticks,
                    time_base: to,
                }
            }
            Err(_) => Timestamp {
                ticks: 0,
                time_base: to,
            },
        }
    }

    /// 转到全局混流时钟 [`crate::CANONICAL_TIME_BASE`]（NearInf 就近舍入）。
    ///
    /// 7,056,000 可被 24/25/30/50/60/120/44100/48000 整除，
    /// 这些速率的流转换**无舍入**；NTSC（·/1001）只在流自身的有理数时基里精确，
    /// 转 canonical 有 ≤1 tick 的舍入（1 tick ≈ 0.14 ns）。
    pub fn to_canonical(self) -> Result<Timestamp, TimeError> {
        self.rescale(crate::CANONICAL_TIME_BASE)
    }

    /// 显示用秒：`ticks · time_base` 的 f64 近似。
    ///
    /// infallible：|ticks·num/den| ≤ ~2^94 << f64 上限，不可能非有限。
    /// **仅用于 UI**，不作权威值、不参与累积运算（全库约定）。
    pub fn to_seconds(self) -> f64 {
        self.ticks as f64 * self.time_base.to_f64()
    }

    /// 秒 → 时间戳（UI/探查输入的便捷路径）：`ticks = seconds/time_base`，
    /// f64 除法后就近取整（半数远离零，与 [`Rounding::NearInf`] 一致）。
    ///
    /// - `seconds` 非有限 → `Err(NotFinite)`；
    /// - 换算出的 ticks 超出 i64 → `Err(Overflow)`；
    /// - `time_base` 为 `0/1` → `Err(DivideByZero)`。
    ///
    /// f64 输入本身只有 ~15~16 位有效数字，结果不保证精确；
    /// 权威换算请走整数 ticks 与 [`Timestamp::rescale`]。
    pub fn from_seconds_f64(seconds: f64, time_base: Rational) -> Result<Timestamp, TimeError> {
        if !seconds.is_finite() {
            return Err(TimeError::NotFinite);
        }
        if time_base.num() == 0 {
            return Err(TimeError::DivideByZero);
        }
        let t = seconds * (time_base.den() as f64) / (time_base.num() as f64);
        let t = t.round();
        // ±2^63 恰可被 f64 精确表示；越过即落不回 i64
        if !(-9223372036854775808.0..9223372036854775808.0).contains(&t) {
            return Err(TimeError::Overflow);
        }
        Ok(Timestamp::new(t as i64, time_base))
    }

    /// 跨时基比较（`av_compare_ts` 真实语义）：i128 精确交叉相乘
    /// `ticks_a·(tb_a.num·tb_b.den)` vs `ticks_b·(tb_b.num·tb_a.den)`。
    ///
    /// 上限 2^63·2^62 = 2^125 < i128::MAX：**无舍入、永不失败**。
    /// 禁止经重标度实现比较（舍入会令不等值判等，MP4 mux 跨流 dts 排序依赖本方法）；
    /// 不提供带舍入参数的变体。
    pub fn compare(self, other: Timestamp) -> Ordering {
        let lhs =
            self.ticks as i128 * (self.time_base.num() as i128 * other.time_base.den() as i128);
        let rhs =
            other.ticks as i128 * (other.time_base.num() as i128 * self.time_base.den() as i128);
        lhs.cmp(&rhs)
    }

    /// 精确加法：结果时基取两时基的公共精确基（速率意义上的 lcm——
    /// 对既约 `n_a/d_a`、`n_b/d_b` 取 `gcd(n_a,n_b)/lcm(d_a,d_b)`，自动既约），
    /// 换算系数为整数，**无舍入**。例：`1@1/2 + 1@1/3 = 5@1/6`。
    ///
    /// 公共基分母（lcm）或和落不回 i32/i64 → `Err(Overflow)`。
    pub fn checked_add(self, other: Timestamp) -> Result<Timestamp, TimeError> {
        let (base, ka, kb) = common_exact_base(self.time_base, other.time_base)?;
        let ticks = self.ticks as i128 * ka + other.ticks as i128 * kb;
        if ticks > i64::MAX as i128 || ticks < i64::MIN as i128 {
            return Err(TimeError::Overflow);
        }
        Ok(Timestamp {
            ticks: ticks as i64,
            time_base: base,
        })
    }

    /// 精确减法：时基与舍入语义同 [`Timestamp::checked_add`]。
    pub fn checked_sub(self, other: Timestamp) -> Result<Timestamp, TimeError> {
        let (base, ka, kb) = common_exact_base(self.time_base, other.time_base)?;
        let ticks = self.ticks as i128 * ka - other.ticks as i128 * kb;
        if ticks > i64::MAX as i128 || ticks < i64::MIN as i128 {
            return Err(TimeError::Overflow);
        }
        Ok(Timestamp {
            ticks: ticks as i64,
            time_base: base,
        })
    }

    /// 帧号 → 时间戳：时基 = 帧率倒数（`1/rate`），ticks = 帧号，**精确无舍入**。
    /// `frame_rate.num == 0` → `Err(DivideByZero)`。
    pub fn from_frame_index(frame: i64, frame_rate: Rational) -> Result<Timestamp, TimeError> {
        if frame_rate.num() == 0 {
            return Err(TimeError::DivideByZero);
        }
        let time_base = frame_rate.invert()?;
        Ok(Timestamp {
            ticks: frame,
            time_base,
        })
    }

    /// 时间戳 → 帧号：`ticks·tb·rate` 就近舍入（NearInf）。
    /// 任意时基下与 [`Timestamp::from_frame_index`] 精确互逆。
    /// `frame_rate.num == 0` → `Err(DivideByZero)`；结果不落 i64 → `Err(Overflow)`。
    pub fn as_frame_index(self, frame_rate: Rational) -> Result<i64, TimeError> {
        if frame_rate.num() == 0 {
            return Err(TimeError::DivideByZero);
        }
        let n = self.ticks as i128 * (self.time_base.num() as i128 * frame_rate.num() as i128);
        let d = self.time_base.den() as i128 * frame_rate.den() as i128;
        let q = div_round(n, d, Rounding::NearInf);
        if q > i64::MAX as i128 || q < i64::MIN as i128 {
            return Err(TimeError::Overflow);
        }
        Ok(q as i64)
    }
}

/// 求两个时基的公共精确基与各自的整数换算系数（供 [`Timestamp::checked_add`]/
/// [`Timestamp::checked_sub`]）。
///
/// 对既约 `tb_a = n_a/d_a`、`tb_b = n_b/d_b`：
/// `base = gcd(n_a,n_b)/lcm(d_a,d_b)`（自动既约——素因子不可能同时进入两侧，
/// 否则与输入既约矛盾），`k = tb/base` 为整数，故任意 ticks 换算**精确无舍入**。
/// 例：1/2 与 1/3 → base = 1/6、k = (3, 2)。
/// 一方为 `0/1` 时公共基取另一方、`0/1` 侧系数为 0（0·任意 ticks 贡献 0）。
fn common_exact_base(a: Rational, b: Rational) -> Result<(Rational, i128, i128), TimeError> {
    if a.num() == 0 || b.num() == 0 {
        let (base, ka, kb) = if a.num() == 0 {
            // a 侧为 0/1：b 侧也为 0/1 时 kb 取 0，和恒为 0@0/1
            let kb = if b.num() == 0 { 0 } else { 1 };
            (b, 0, kb)
        } else {
            (a, 1, 0)
        };
        return Ok((base, ka, kb));
    }
    let g = gcd_u128(
        a.num().unsigned_abs() as u128,
        b.num().unsigned_abs() as u128,
    );
    let gcd_d = gcd_u128(a.den() as u128, b.den() as u128);
    // lcm(d_a, d_b)：先除后乘防溢出（u128 下 ≤ 2^62 本就安全）
    let l = (a.den() as u128 / gcd_d) * (b.den() as u128);
    if l > i32::MAX as u128 {
        return Err(TimeError::Overflow);
    }
    // g/l 自动既约，直接构造
    let base = Rational::new(g as i32, l as i32);
    let ka = (a.num() as i128 / g as i128) * (l as i128 / a.den() as i128);
    let kb = (b.num() as i128 / g as i128) * (l as i128 / b.den() as i128);
    Ok((base, ka, kb))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CANONICAL_TIME_BASE;

    fn r(num: i32, den: i32) -> Rational {
        Rational::new(num, den)
    }

    fn ts(ticks: i64, num: i32, den: i32) -> Timestamp {
        Timestamp::new(ticks, r(num, den))
    }

    // ---- 重标度精确性 ----

    #[test]
    fn rescale_exactness() {
        let t = ts(3600, 1, 90000).rescale(r(1, 25)).unwrap();
        assert_eq!((t.ticks(), t.time_base()), (1, r(1, 25)));
        assert_eq!(
            ts(48_000, 1, 48_000)
                .rescale(Rational::ONE)
                .unwrap()
                .ticks(),
            1
        );
        assert_eq!(
            ts(7_056_000, 1, 7_056_000)
                .rescale(Rational::ONE)
                .unwrap()
                .ticks(),
            1
        );
    }

    /// ±3 ticks @1/2 → 1/1（即 ±1.5）五种模式逐格断言，对齐 AV_ROUND_* 文档语义。
    #[test]
    fn rescale_rounding_table() {
        let cases = [
            (Rounding::Zero, 1i64, -1i64),
            (Rounding::Inf, 2, -2),
            (Rounding::Down, 1, -2),
            (Rounding::Up, 2, -1),
            (Rounding::NearInf, 2, -2),
        ];
        for (mode, pos, neg) in cases {
            assert_eq!(
                ts(3, 1, 2)
                    .rescale_with(Rational::ONE, mode)
                    .map(|t| t.ticks()),
                Ok(pos),
                "rounding = {mode:?}"
            );
            assert_eq!(
                ts(-3, 1, 2)
                    .rescale_with(Rational::ONE, mode)
                    .map(|t| t.ticks()),
                Ok(neg),
                "rounding = {mode:?}"
            );
        }
        // 默认 rescale == NearInf
        assert_eq!(ts(3, 1, 2).rescale(Rational::ONE).map(|t| t.ticks()), Ok(2));
    }

    #[test]
    fn rescale_overflow_direction() {
        // 放大溢出：≈9.22e27 > i64::MAX
        assert_eq!(
            ts(i64::MAX, 1, 1).rescale(r(1, 1_000_000_000)),
            Err(TimeError::Overflow)
        );
        // 饱和钳位（正 → MAX，负 → MIN）
        assert_eq!(
            ts(i64::MAX, 1, 1)
                .rescale_saturating(r(1, 1_000_000_000), Rounding::NearInf)
                .ticks(),
            i64::MAX
        );
        assert_eq!(
            ts(i64::MIN, 1, 1)
                .rescale_saturating(r(1, 1_000_000_000), Rounding::NearInf)
                .ticks(),
            i64::MIN
        );
        // 对照（防方向写反回归）：缩小方向应成功，9.22e9 落 i64 内，NearInf .854 入
        assert_eq!(
            ts(i64::MAX, 1, 1_000_000_000)
                .rescale(Rational::ONE)
                .map(|t| t.ticks()),
            Ok(9_223_372_037)
        );
        // 除零时基
        assert_eq!(
            ts(1, 1, 25).rescale(Rational::ZERO),
            Err(TimeError::DivideByZero)
        );
    }

    // ---- canonical ----

    #[test]
    fn canonical_conversions() {
        assert_eq!(CANONICAL_TIME_BASE, r(1, 7_056_000));
        // 1 秒 @1/48000 → 48000 ticks → 7056000
        assert_eq!(
            ts(48_000, 1, 48_000).to_canonical().map(|t| t.ticks()),
            Ok(7_056_000)
        );
        assert_eq!(
            ts(44_100, 1, 44_100).to_canonical().map(|t| t.ticks()),
            Ok(7_056_000)
        );
        // NTSC 1 帧 @1001/30000 → round(1001·235.2) = 235435（固化舍入语义）
        assert_eq!(
            ts(1, 1001, 30_000).to_canonical().map(|t| t.ticks()),
            Ok(235_435)
        );
        // 往返：canonical 转回仍近似原值（NTSC 侧 ≤1 tick 舍入，1 tick ≈ 1.42e-7 s）
        let back = ts(1, 1001, 30_000).to_canonical().unwrap();
        assert!(
            (back.to_seconds() - 1.0 * 1001.0 / 30000.0).abs() < 1.5e-7,
            "back = {:?}",
            back
        );
    }

    // ---- 帧号往返 ----

    #[test]
    fn frame_index_roundtrip() {
        let rates = [
            Rational::RATE_24,
            Rational::RATE_25,
            Rational::RATE_30,
            Rational::RATE_29_97,
            Rational::RATE_50,
            Rational::RATE_59_94,
            Rational::RATE_60,
            Rational::RATE_120,
        ];
        for &rate in &rates {
            for &n in &[0i64, 1, 999, 1001, 10_000] {
                let t = Timestamp::from_frame_index(n, rate).unwrap();
                assert_eq!(t.as_frame_index(rate), Ok(n), "rate = {rate:?}, n = {n}");
            }
        }
        // NTSC：帧号直接就是 @1001/30000 的 ticks
        let t = Timestamp::from_frame_index(1001, Rational::RATE_29_97).unwrap();
        assert_eq!((t.ticks(), t.time_base()), (1001, r(1001, 30_000)));
        // 零帧率
        assert_eq!(
            Timestamp::from_frame_index(0, Rational::ZERO),
            Err(TimeError::DivideByZero)
        );
        assert_eq!(
            ts(10, 1, 25).as_frame_index(Rational::ZERO),
            Err(TimeError::DivideByZero)
        );
    }

    // ---- 跨时基比较与精确加减 ----

    #[test]
    fn compare_cross_time_base() {
        assert_eq!(ts(1, 1, 90000).compare(ts(1, 1, 1000)), Ordering::Less);
        // 防回归：不得经重标度实现——1@1/3 经 NearInf 会舍为 333333 而误判 Equal；
        // 精确交叉相乘 1e6 > 999999
        assert_eq!(
            ts(1, 1, 3).compare(ts(333_333, 1, 1_000_000)),
            Ordering::Greater
        );
        assert_eq!(
            ts(1, 1, 3).compare(ts(1_000_000, 1, 3_000_000)),
            Ordering::Equal
        );
    }

    #[test]
    fn timestamp_add_sub_exact() {
        // 精确 lcm 公共基：1@1/2 + 1@1/3 = 5@1/6
        let sum = ts(1, 1, 2).checked_add(ts(1, 1, 3)).unwrap();
        assert_eq!((sum.ticks(), sum.time_base()), (5, r(1, 6)));
        let diff = ts(1, 1, 2).checked_sub(ts(1, 1, 3)).unwrap();
        assert_eq!((diff.ticks(), diff.time_base()), (1, r(1, 6)));
        // 同基直加
        let same = ts(7, 1, 25).checked_add(ts(3, 1, 25)).unwrap();
        assert_eq!((same.ticks(), same.time_base()), (10, r(1, 25)));
        // 公共基分母溢出：lcm(2^31-1, 2^31-1 以外的互素大数) 超 i32
        assert_eq!(
            ts(1, 1, 2_000_000_039).checked_add(ts(1, 1, 2_000_000_041)),
            Err(TimeError::Overflow)
        );
        // 和溢出 i64
        assert_eq!(
            ts(i64::MAX, 1, 1).checked_add(ts(1, 1, 1)),
            Err(TimeError::Overflow)
        );
    }

    // ---- 秒换算 ----

    #[test]
    fn seconds_conversions() {
        let t = ts(48_000, 1, 48_000);
        assert!((t.to_seconds() - 1.0).abs() < 1e-12);
        assert!((ts(25, 1, 25).to_seconds() - 1.0).abs() < 1e-12);

        let back = Timestamp::from_seconds_f64(1.0, r(1, 48_000)).unwrap();
        assert_eq!(back.ticks(), 48_000);
        assert_eq!(back.time_base(), r(1, 48_000));
        assert_eq!(
            Timestamp::from_seconds_f64(0.0, r(1, 25)).unwrap().ticks(),
            0
        );
        // 非有限
        assert_eq!(
            Timestamp::from_seconds_f64(f64::NAN, r(1, 25)),
            Err(TimeError::NotFinite)
        );
        assert_eq!(
            Timestamp::from_seconds_f64(f64::INFINITY, r(1, 25)),
            Err(TimeError::NotFinite)
        );
        // 溢出：1e30 秒 @1ns → 1e39 ticks 超 i64
        assert_eq!(
            Timestamp::from_seconds_f64(1e30, r(1, 1_000_000_000)),
            Err(TimeError::Overflow)
        );
        // 零时基
        assert_eq!(
            Timestamp::from_seconds_f64(1.0, Rational::ZERO),
            Err(TimeError::DivideByZero)
        );
    }

    // ---- 线程安全（全库约定：公共数据类型隐式 Send + Sync）----

    #[test]
    fn send_sync_static_assertions() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Rational>();
        assert_send_sync::<Timestamp>();
        assert_send_sync::<Rounding>();
        assert_send_sync::<TimeError>();
    }
}
