//! 有理数时基、时间戳与帧率换算。
//!
//! 对标 libavutil 的 `AVRational` 与 `av_rescale_q`：
//! 全库时间统一以整数 tick 表示，避免浮点累积漂移；
//! tick 基数定死为 [`TICKS_PER_SECOND`]（7,056,000），对常见帧率
//! （24/25/30/50/60/120）与音频采样率（44100/48000）整除。
//! 浮点仅限调试显示（[`Rational::to_f64`]），禁止参与时间运算。

#![forbid(unsafe_code)]

use core::cmp::Ordering;
use core::fmt;
use core::ops::{Add, Sub};

/// 全局 tick 基数：每秒 tick 数。
/// = LCM(24,25,30,50,60,120) 与 LCM(44100,48000) 的最小公倍数 = 7,056,000，
/// 对任务列出的全部帧率/采样率整除（24→294000、25→282240、30→235200、50→141120、
/// 60→117600、120→58800、44100→160、48000→147）。
pub const TICKS_PER_SECOND: i64 = 7_056_000;

/// 有理数，对标 FFmpeg 的 `AVRational`。
///
/// 不变量：`den > 0` 且 `gcd(|num|, den) == 1`
/// （[`new`](Rational::new) 构造即约分；直接改 pub 字段后调用
/// [`reduce`](Rational::reduce) 恢复）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rational {
    pub num: i64,
    pub den: i64,
}

impl Rational {
    /// 0/1。
    pub const ZERO: Rational = Rational { num: 0, den: 1 };

    /// 1/1。
    pub const ONE: Rational = Rational { num: 1, den: 1 };

    /// 构造即约分，den < 0 时符号翻转到分母（i64::MIN 用 i128 中间量安全处理）。
    ///
    /// `None` 当且仅当 `den == 0`；唯一例外是规范化结果超出 i64 的
    /// `i64::MIN` 边界组合（如 `new(i64::MIN, -1)` 的真值 +2^63、
    /// `new(1, i64::MIN)` 的分母 2^63），同样返回 `None`。
    #[must_use]
    pub fn new(num: i64, den: i64) -> Option<Rational> {
        if den == 0 {
            return None;
        }
        Self::from_parts(num as i128, den as i128)
    }

    /// 就地约分，den 规范为正（用于直接改 pub 字段后恢复不变量）。
    ///
    /// # Panics
    ///
    /// `den == 0`，或规范化结果超出 i64（如 `num = i64::MIN, den = -1`）。
    #[must_use]
    pub fn reduce(self) -> Rational {
        Self::from_parts(self.num as i128, self.den as i128)
            .expect("Rational::reduce: den == 0 或结果超出 i64")
    }

    /// 倒数。`None` 当且仅当 `num == 0`（唯一溢出例外：`num == i64::MIN`
    /// 时结果分母 2^63 超出 i64，同样返回 `None`）。
    #[must_use]
    pub fn inverse(self) -> Option<Rational> {
        Self::from_parts(self.den as i128, self.num as i128)
    }

    /// 乘法。溢出返回 `None`（i128 中间量，先约分再回 i64）。
    // 契约定死固有方法名 mul（返回 Option，无法对标 std::ops::Mul 的非 Option 语义）。
    #[allow(clippy::should_implement_trait)]
    #[must_use]
    pub fn mul(self, rhs: Rational) -> Option<Rational> {
        // 两个 i64 之积不超过 2^126，i128 必然装得下。
        Self::from_parts(
            self.num as i128 * rhs.num as i128,
            self.den as i128 * rhs.den as i128,
        )
    }

    /// 除法。除零（`rhs.num == 0`）或溢出返回 `None`。
    // 契约定死固有方法名 div（返回 Option，无法对标 std::ops::Div 的非 Option 语义）。
    #[allow(clippy::should_implement_trait)]
    #[must_use]
    pub fn div(self, rhs: Rational) -> Option<Rational> {
        // (a/b) / (c/d) = (a·d) / (b·c)；rhs.num == 0 时分母为零。
        Self::from_parts(
            self.num as i128 * rhs.den as i128,
            self.den as i128 * rhs.num as i128,
        )
    }

    /// 转浮点。仅用于显示与调试，禁止参与时间运算。
    #[must_use]
    pub fn to_f64(self) -> f64 {
        self.num as f64 / self.den as f64
    }

    /// 由 i128 分子/分母构造规范化 Rational：翻符号使 den > 0、约分、回 i64。
    /// `d == 0` 或装不回 i64 时返回 `None`。
    fn from_parts(n: i128, d: i128) -> Option<Rational> {
        if d == 0 {
            return None;
        }
        let (mut n, mut d) = (n, d);
        if d < 0 {
            n = -n;
            d = -d;
        }
        let g = gcd_u128(n.unsigned_abs(), d.unsigned_abs()) as i128;
        Some(Rational {
            num: i64::try_from(n / g).ok()?,
            den: i64::try_from(d / g).ok()?,
        })
    }
}

/// 按数值比较（i128 交叉相乘），与 `Eq` 一致（表示已规范化；
/// 不变量被破坏时行为不作保证）。
impl Ord for Rational {
    fn cmp(&self, other: &Rational) -> Ordering {
        // den > 0（不变量）下交叉相乘保序；两个 i64 之积 i128 必装得下。
        (self.num as i128 * other.den as i128).cmp(&(other.num as i128 * self.den as i128))
    }
}

impl PartialOrd for Rational {
    fn partial_cmp(&self, other: &Rational) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// 显示为 "{num}/{den}"（如 "3/2"、"0/1"），仅用于调试。
impl fmt::Display for Rational {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.num, self.den)
    }
}

/// 全局 tick 基数（[`TICKS_PER_SECOND`]）下的整数时刻/时长。
/// 可为负（如音频 encoder delay）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(i64);

impl Timestamp {
    /// 零时刻。
    pub const ZERO: Timestamp = Timestamp(0);

    /// 由 tick 数构造（平凡包装）。
    #[must_use]
    pub fn from_ticks(ticks: i64) -> Timestamp {
        Timestamp(ticks)
    }

    /// 取 tick 数。
    #[must_use]
    pub fn as_ticks(self) -> i64 {
        self.0
    }

    /// 整秒 → tick；溢出 `None`。
    #[must_use]
    pub fn from_seconds(seconds: i64) -> Option<Timestamp> {
        seconds.checked_mul(TICKS_PER_SECOND).map(Timestamp)
    }

    /// 单位换算（核心换算，i128 中间量，四舍五入 half-away-from-zero）：
    /// `timebase` 方向为『每单位 = num/den 秒』（FFmpeg `AVStream.time_base` 同向）。
    /// 例：`from_units(delta, Rational::new(1, 1000))` 把 mdhd timescale=1000
    /// 的增量换为 tick。溢出返回 `None`（`timebase.den == 0` 即不变量被破坏
    /// 时同样返回 `None`）。
    #[must_use]
    pub fn from_units(units: i64, timebase: Rational) -> Option<Timestamp> {
        // ticks = units · num/den · TICKS_PER_SECOND；三个 i64 连乘可能超 i128，逐步 checked。
        let mut n = (units as i128)
            .checked_mul(timebase.num as i128)?
            .checked_mul(TICKS_PER_SECOND as i128)?;
        let mut d = timebase.den as i128;
        if d == 0 {
            return None;
        }
        if d < 0 {
            n = -n;
            d = -d;
        }
        i64::try_from(div_round_half_away(n, d)).ok().map(Timestamp)
    }

    /// 反向：tick → timebase 单位数，同样四舍五入 half-away-from-zero。
    /// 零时基（`num == 0`）、`den == 0` 或溢出返回 `None`。
    #[must_use]
    pub fn to_units(self, timebase: Rational) -> Option<i64> {
        // units = ticks · den / (num · TICKS_PER_SECOND)
        if timebase.num == 0 || timebase.den == 0 {
            return None;
        }
        let mut n = (self.0 as i128).checked_mul(timebase.den as i128)?;
        let mut d = (timebase.num as i128).checked_mul(TICKS_PER_SECOND as i128)?;
        if d < 0 {
            n = -n;
            d = -d;
        }
        i64::try_from(div_round_half_away(n, d)).ok()
    }

    /// 溢出返回 `None` 的加法。
    #[must_use]
    pub fn checked_add(self, rhs: Timestamp) -> Option<Timestamp> {
        self.0.checked_add(rhs.0).map(Timestamp)
    }

    /// 溢出返回 `None` 的减法。
    #[must_use]
    pub fn checked_sub(self, rhs: Timestamp) -> Option<Timestamp> {
        self.0.checked_sub(rhs.0).map(Timestamp)
    }
}

/// 直接相加（内部走 [`checked_add`](Timestamp::checked_add)）。
///
/// # Panics
///
/// 溢出时 panic。7056000 tick/s 下 i64 上限约 4.1 万年，实际不可达。
impl Add<Timestamp> for Timestamp {
    type Output = Timestamp;

    fn add(self, rhs: Timestamp) -> Timestamp {
        self.checked_add(rhs).expect("Timestamp 加法溢出")
    }
}

/// 直接相减（内部走 [`checked_sub`](Timestamp::checked_sub)）。
///
/// # Panics
///
/// 溢出时 panic。7056000 tick/s 下 i64 上限约 4.1 万年，实际不可达。
impl Sub<Timestamp> for Timestamp {
    type Output = Timestamp;

    fn sub(self, rhs: Timestamp) -> Timestamp {
        self.checked_sub(rhs).expect("Timestamp 减法溢出")
    }
}

/// 格式固定："{ticks} ticks"。
impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ticks", self.0)
    }
}

/// 一帧时长：`frame_duration_ticks(Rational::new(24, 1))` == 294000 tick。
/// 非正帧率（`num <= 0`）与溢出返回 `None`。
#[must_use]
pub fn frame_duration_ticks(frame_rate: Rational) -> Option<Timestamp> {
    if frame_rate.num <= 0 {
        return None;
    }
    // 每帧秒数 = den/num，ticks = TICKS_PER_SECOND · den / num。
    let n = TICKS_PER_SECOND as i128 * frame_rate.den as i128;
    i64::try_from(div_round_half_away(n, frame_rate.num as i128))
        .ok()
        .map(Timestamp)
}

/// 一样本时长：`sample_duration_ticks(48000)` == 147、`(44100)` == 160。
/// 溢出/非正 rate 返回 `None`。
#[must_use]
pub fn sample_duration_ticks(sample_rate: i64) -> Option<Timestamp> {
    if sample_rate <= 0 {
        return None;
    }
    i64::try_from(div_round_half_away(
        TICKS_PER_SECOND as i128,
        sample_rate as i128,
    ))
    .ok()
    .map(Timestamp)
}

/// n/d 四舍五入（half away from zero），要求 d > 0。
/// 用商/余判断而非 (n + d/2) / d，避免 n 接近 i128 极值时加法溢出。
fn div_round_half_away(n: i128, d: i128) -> i128 {
    debug_assert!(d > 0);
    let (q, r) = (n / d, n % d); // 截断除法：r 与 n 同号，|r| < d
    if 2 * r.unsigned_abs() >= d as u128 {
        q + if r > 0 { 1 } else { -1 }
    } else {
        q
    }
}

/// 欧几里得 gcd，约定 gcd(0, x) == x。
fn gcd_u128(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // —— 1) 常量整除性 ——

    #[test]
    fn ticks_per_second_divides_all_listed_rates() {
        let cases = [
            (24i64, 294_000i64),
            (25, 282_240),
            (30, 235_200),
            (50, 141_120),
            (60, 117_600),
            (120, 58_800),
            (44_100, 160),
            (48_000, 147),
        ];
        for (rate, quotient) in cases {
            assert_eq!(
                TICKS_PER_SECOND % rate,
                0,
                "TICKS_PER_SECOND 需被 {rate} 整除"
            );
            assert_eq!(TICKS_PER_SECOND / rate, quotient, "{rate} 的商");
        }
    }

    // —— 2) from_units / to_units ——

    #[test]
    fn from_units_direction_semantics() {
        // 每单位 1/24 秒：一单位 = 一帧 @24fps = 294000 tick。
        let frame_tb = Rational::new(1, 24).unwrap();
        assert_eq!(
            Timestamp::from_units(1, frame_tb),
            Some(Timestamp::from_ticks(294_000))
        );
        assert_eq!(Timestamp::from_ticks(294_000).to_units(frame_tb), Some(1));

        // mdhd timescale=1000（毫秒）方向。
        let ms = Rational::new(1, 1000).unwrap();
        assert_eq!(
            Timestamp::from_units(2500, ms),
            Some(Timestamp::from_ticks(17_640_000)) // 2.5 s
        );
        assert_eq!(Timestamp::from_ticks(17_640_000).to_units(ms), Some(2500));
    }

    #[test]
    fn units_round_trip_common_timescales() {
        // timescale ≤ TICKS_PER_SECOND 时两次四舍五入的误差互相抵消，往返严格还原：
        // ticks = u·a + e（|e| ≤ 0.5），回程 round(u + e/a) 中 |e/a| < 0.5，必回 u。
        for timescale in [1000i64, 15_360, 48_000, 90_000] {
            let tb = Rational::new(1, timescale).unwrap();
            for units in [0i64, 1, 2, 3, 7, timescale, timescale * 3 + 1, 12_345] {
                let t = Timestamp::from_units(units, tb).unwrap();
                assert_eq!(
                    t.to_units(tb),
                    Some(units),
                    "timescale={timescale} units={units}"
                );
                let tn = Timestamp::from_units(-units, tb).unwrap();
                assert_eq!(
                    tn.to_units(tb),
                    Some(-units),
                    "timescale={timescale} units=-{units}"
                );
            }
        }
    }

    #[test]
    fn units_rounding_half_away_from_zero() {
        // 构造恰好半 tick：每单位 = 1/(2·TPS) 秒 → 1 单位 = 0.5 tick，远离零取 1。
        let half_tick_tb = Rational::new(1, 2 * TICKS_PER_SECOND).unwrap();
        assert_eq!(
            Timestamp::from_units(1, half_tick_tb),
            Some(Timestamp::from_ticks(1))
        );
        assert_eq!(
            Timestamp::from_units(-1, half_tick_tb),
            Some(Timestamp::from_ticks(-1))
        );
        assert_eq!(
            Timestamp::from_units(3, half_tick_tb),
            Some(Timestamp::from_ticks(2)) // 1.5 → 2
        );
        assert_eq!(
            Timestamp::from_units(-3, half_tick_tb),
            Some(Timestamp::from_ticks(-2))
        );
        // 不足一半归零。
        let third_tick_tb = Rational::new(1, 3 * TICKS_PER_SECOND).unwrap();
        assert_eq!(
            Timestamp::from_units(1, third_tick_tb),
            Some(Timestamp::ZERO)
        );
        assert_eq!(
            Timestamp::from_units(-1, third_tick_tb),
            Some(Timestamp::ZERO)
        );
        assert_eq!(
            Timestamp::from_units(2, third_tick_tb),
            Some(Timestamp::from_ticks(1)) // 0.667 → 1
        );

        // to_units 半单位：3 tick 按每单位 1/3528000 秒 = 1.5 单位 → 2，负数对称。
        let half_unit_tb = Rational::new(1, TICKS_PER_SECOND / 2).unwrap();
        assert_eq!(Timestamp::from_ticks(3).to_units(half_unit_tb), Some(2));
        assert_eq!(Timestamp::from_ticks(-3).to_units(half_unit_tb), Some(-2));
        assert_eq!(Timestamp::from_ticks(1).to_units(half_unit_tb), Some(1));
        assert_eq!(Timestamp::from_ticks(-1).to_units(half_unit_tb), Some(-1));
        // 毫秒时基下 3527 tick = 0.4999… → 0，3528 tick 恰 0.5 → 1。
        let ms = Rational::new(1, 1000).unwrap();
        assert_eq!(Timestamp::from_ticks(3527).to_units(ms), Some(0));
        assert_eq!(Timestamp::from_ticks(-3527).to_units(ms), Some(0));
        assert_eq!(Timestamp::from_ticks(3528).to_units(ms), Some(1));
        assert_eq!(Timestamp::from_ticks(-3528).to_units(ms), Some(-1));
    }

    #[test]
    fn units_invalid_timebase_returns_none() {
        // 零时基无法定义单位数。
        assert_eq!(Timestamp::from_ticks(5).to_units(Rational::ZERO), None);
        // 直接改 pub 字段破坏不变量（den == 0）时防御性返回 None。
        let broken = Rational { num: 1, den: 0 };
        assert_eq!(Timestamp::from_units(100, broken), None);
        assert_eq!(Timestamp::from_ticks(100).to_units(broken), None);
    }

    // —— 3) Rational ——

    #[test]
    fn rational_new_reduces_and_normalizes_den() {
        assert_eq!(Rational::new(6, 4), Some(Rational { num: 3, den: 2 }));
        assert_eq!(Rational::new(-6, 4), Some(Rational { num: -3, den: 2 }));
        assert_eq!(Rational::new(6, -4), Some(Rational { num: -3, den: 2 }));
        assert_eq!(Rational::new(-6, -4), Some(Rational { num: 3, den: 2 }));
        assert_eq!(Rational::new(0, -7), Some(Rational::ZERO));
        assert_eq!(Rational::new(5, 0), None);
        assert_eq!(Rational::new(0, 0), None);

        // i64::MIN 用 i128 中间量安全处理。
        assert_eq!(Rational::new(i64::MIN, i64::MIN), Some(Rational::ONE));
        assert_eq!(
            Rational::new(i64::MIN, 2),
            Some(Rational {
                num: i64::MIN / 2,
                den: 1
            })
        );
        assert_eq!(
            Rational::new(i64::MIN, 1),
            Some(Rational {
                num: i64::MIN,
                den: 1
            })
        );
        assert_eq!(Rational::new(i64::MIN, -1), None); // 真值 +2^63 装不进 i64
        assert_eq!(Rational::new(1, i64::MIN), None); // 分母 |2^63| 超出 i64
        assert_eq!(Rational::new(0, i64::MIN), Some(Rational::ZERO));
    }

    #[test]
    fn rational_reduce_recovers_invariant() {
        // 直接改 pub 字段破坏不变量后恢复。
        let r = Rational { num: 6, den: -4 };
        assert_eq!(r.reduce(), Rational { num: -3, den: 2 });
        assert_eq!(Rational { num: 0, den: -5 }.reduce(), Rational::ZERO);
        assert_eq!(
            Rational { num: -9, den: -6 }.reduce(),
            Rational { num: 3, den: 2 }
        );
    }

    #[test]
    #[should_panic(expected = "Rational::reduce")]
    fn rational_reduce_panics_on_zero_den() {
        let _ = Rational { num: 1, den: 0 }.reduce();
    }

    #[test]
    #[should_panic(expected = "Rational::reduce")]
    fn rational_reduce_panics_on_overflow() {
        let _ = Rational {
            num: i64::MIN,
            den: -1,
        }
        .reduce();
    }

    #[test]
    fn rational_inverse_mul_div() {
        let half = Rational::new(1, 2).unwrap();
        assert_eq!(half.inverse(), Rational::new(2, 1));
        assert_eq!(
            Rational::new(-3, 2).unwrap().inverse(),
            Rational::new(-2, 3)
        );
        assert_eq!(Rational::ZERO.inverse(), None);
        assert_eq!(Rational::ONE.inverse(), Some(Rational::ONE));
        assert_eq!(Rational::new(i64::MIN, 1).unwrap().inverse(), None); // 分母 2^63 溢出

        assert_eq!(half.mul(half), Rational::new(1, 4));
        assert_eq!(Rational::ZERO.mul(half), Some(Rational::ZERO));
        assert_eq!(half.mul(Rational::ZERO), Some(Rational::ZERO));
        // 分子分母交错约分：(2^40/3)·(3/2^40) == 1。
        let a = Rational::new(1 << 40, 3).unwrap();
        let b = Rational::new(3, 1 << 40).unwrap();
        assert_eq!(a.mul(b), Some(Rational::ONE));
        // 真溢出：2^80 与 2^126/3。
        let big = Rational::new(1 << 40, 1).unwrap();
        assert_eq!(big.mul(big), None);
        assert_eq!(
            Rational::new(i64::MIN, 1)
                .unwrap()
                .mul(Rational::new(i64::MIN, 3).unwrap()),
            None
        );

        assert_eq!(half.div(Rational::new(1, 4).unwrap()), Rational::new(2, 1));
        assert_eq!(half.div(Rational::new(4, 1).unwrap()), Rational::new(1, 8));
        assert_eq!(half.div(Rational::ZERO), None); // 除零
        assert_eq!(Rational::ZERO.div(half), Some(Rational::ZERO));
        assert_eq!(
            Rational::new(i64::MAX, 1).unwrap().div(half),
            None // 2·i64::MAX 溢出
        );
    }

    #[test]
    fn rational_ord_cross_denominator() {
        // 5/7 ≈ 0.714 > 7/10 = 0.7，跨分母须交叉相乘判定。
        assert_eq!(
            Rational::new(5, 7)
                .unwrap()
                .cmp(&Rational::new(7, 10).unwrap()),
            Ordering::Greater
        );
        assert!(Rational::new(1, 3).unwrap() < Rational::new(1, 2).unwrap());
        assert!(Rational::new(-1, 2).unwrap() < Rational::new(1, 3).unwrap());
        assert!(Rational::ZERO < Rational::ONE);

        // 排序后数值单调（期望序手算给出）。
        let mut v = [
            Rational::new(2, 3).unwrap(),
            Rational::new(-1, 2).unwrap(),
            Rational::new(1, 3).unwrap(),
            Rational::new(5, 7).unwrap(),
            Rational::new(7, 10).unwrap(),
            Rational::ZERO,
            Rational::ONE,
            Rational::new(-5, 3).unwrap(),
        ];
        v.sort();
        let got: Vec<(i64, i64)> = v.iter().map(|r| (r.num, r.den)).collect();
        assert_eq!(
            got,
            vec![
                (-5, 3),
                (-1, 2),
                (0, 1),
                (1, 3),
                (2, 3),
                (7, 10),
                (5, 7),
                (1, 1),
            ]
        );
    }

    #[test]
    fn rational_eq_hash_consistent_with_ord() {
        let vals: Vec<Rational> = [0i64, 1, -1, 2, 3, 6]
            .iter()
            .flat_map(|&n| {
                [1i64, 2, 3, 7]
                    .iter()
                    .map(move |&d| Rational::new(n, d).unwrap())
            })
            .collect();
        for (i, &a) in vals.iter().enumerate() {
            for &b in vals.iter().skip(i) {
                assert_eq!(a == b, a.cmp(&b) == Ordering::Equal, "{a} vs {b}");
            }
        }
        // 相等值（不同构造路径，构造即约分）哈希一致。
        let mut set = HashSet::new();
        set.insert(Rational::new(6, 4).unwrap()); // 存为 3/2
        assert!(set.contains(&Rational { num: 3, den: 2 }));
        assert!(!set.contains(&Rational { num: 2, den: 3 }));
    }

    #[test]
    fn rational_display_and_to_f64() {
        assert_eq!(Rational::new(1, 2).unwrap().to_f64(), 0.5);
        assert_eq!(Rational::new(-3, 2).unwrap().to_f64(), -1.5);
        assert_eq!(format!("{}", Rational::new(6, 4).unwrap()), "3/2");
        assert_eq!(format!("{}", Rational::ZERO), "0/1");
    }

    // —— 4) 溢出路径 ——

    #[test]
    fn overflow_paths_return_none() {
        assert_eq!(Timestamp::from_seconds(i64::MAX), None);
        assert_eq!(Timestamp::from_seconds(i64::MIN), None);
        assert_eq!(
            Timestamp::from_seconds(1),
            Some(Timestamp::from_ticks(TICKS_PER_SECOND))
        );
        assert_eq!(
            Timestamp::from_seconds(-3600),
            Some(Timestamp::from_ticks(-3600 * TICKS_PER_SECOND))
        );

        // units·num·TPS 三段 i64 连乘真溢出 i128。
        assert_eq!(
            Timestamp::from_units(i64::MAX, Rational::new(i64::MAX, 1).unwrap()),
            None
        );
        assert_eq!(Timestamp::from_units(i64::MIN, Rational::ONE), None);
        // to_units 结果超 i64。
        assert_eq!(
            Timestamp::from_ticks(i64::MAX).to_units(Rational::new(1, i64::MAX).unwrap()),
            None
        );
    }

    #[test]
    fn i128_intermediates_do_not_overflow_early() {
        // 朴素 i64 会先算 units·TPS = i64::MAX·7056000 而中途溢出；
        // i128 中间量 + 最后整除，精确还原。
        let tps = Rational::new(1, TICKS_PER_SECOND).unwrap();
        assert_eq!(
            Timestamp::from_units(i64::MAX, tps),
            Some(Timestamp::from_ticks(i64::MAX))
        );
        assert_eq!(
            Timestamp::from_units(i64::MIN, tps),
            Some(Timestamp::from_ticks(i64::MIN))
        );
        assert_eq!(
            Timestamp::from_ticks(i64::MAX).to_units(tps),
            Some(i64::MAX)
        );
        assert_eq!(
            Timestamp::from_ticks(i64::MIN).to_units(tps),
            Some(i64::MIN)
        );
    }

    // —— 5) Timestamp ——

    #[test]
    fn timestamp_negative_and_encoder_delay() {
        // 音频 encoder delay：-1024 样本 @48 kHz → -1024·147 tick。
        let delay = Timestamp::from_units(-1024, Rational::new(1, 48_000).unwrap());
        assert_eq!(delay, Some(Timestamp::from_ticks(-150_528)));
        let delay = delay.unwrap();
        assert_eq!(delay.as_ticks(), -150_528);
        assert!(delay < Timestamp::ZERO);
        assert_eq!(delay + Timestamp::from_ticks(150_528), Timestamp::ZERO);
    }

    #[test]
    fn timestamp_ord_matches_as_ticks() {
        let raw = [5i64, -3, 0, i64::MAX, i64::MIN, 294_000, -150_528];
        let mut ts: Vec<Timestamp> = raw.iter().map(|&t| Timestamp::from_ticks(t)).collect();
        ts.sort();
        let ticks: Vec<i64> = ts.iter().map(|t| t.as_ticks()).collect();
        let mut expect = raw.to_vec();
        expect.sort_unstable();
        assert_eq!(ticks, expect);
    }

    #[test]
    fn timestamp_checked_arithmetic() {
        let a = Timestamp::from_ticks(7);
        let b = Timestamp::from_ticks(-9);
        assert_eq!(a.checked_add(b), Some(Timestamp::from_ticks(-2)));
        assert_eq!(a.checked_sub(b), Some(Timestamp::from_ticks(16)));
        assert_eq!(a + b, Timestamp::from_ticks(-2));
        assert_eq!(a - b, Timestamp::from_ticks(16));
        assert_eq!(
            Timestamp::from_ticks(i64::MAX).checked_add(Timestamp::from_ticks(1)),
            None
        );
        assert_eq!(
            Timestamp::from_ticks(i64::MIN).checked_sub(Timestamp::from_ticks(1)),
            None
        );
    }

    #[test]
    #[should_panic(expected = "Timestamp 加法溢出")]
    fn timestamp_add_panics_on_overflow() {
        let _ = Timestamp::from_ticks(i64::MAX) + Timestamp::from_ticks(1);
    }

    #[test]
    #[should_panic(expected = "Timestamp 减法溢出")]
    fn timestamp_sub_panics_on_overflow() {
        let _ = Timestamp::from_ticks(i64::MIN) - Timestamp::from_ticks(1);
    }

    #[test]
    fn timestamp_display() {
        assert_eq!(
            format!("{}", Timestamp::from_ticks(294_000)),
            "294000 ticks"
        );
        assert_eq!(format!("{}", Timestamp::ZERO), "0 ticks");
        assert_eq!(format!("{}", Timestamp::from_ticks(-147)), "-147 ticks");
    }

    // —— 便捷函数 ——

    #[test]
    fn frame_duration_common_rates() {
        assert_eq!(
            frame_duration_ticks(Rational::new(24, 1).unwrap()),
            Some(Timestamp::from_ticks(294_000))
        );
        assert_eq!(
            frame_duration_ticks(Rational::new(25, 1).unwrap()),
            Some(Timestamp::from_ticks(282_240))
        );
        assert_eq!(
            frame_duration_ticks(Rational::new(1, 1).unwrap()),
            Some(Timestamp::from_ticks(TICKS_PER_SECOND))
        );
        // NTSC 30000/1001：7056000·1001/30000 = 235435.2 → 235435。
        assert_eq!(
            frame_duration_ticks(Rational::new(30_000, 1001).unwrap()),
            Some(Timestamp::from_ticks(235_435))
        );
        assert_eq!(frame_duration_ticks(Rational::ZERO), None);
        assert_eq!(frame_duration_ticks(Rational::new(-24, 1).unwrap()), None);
        // 溢出：TPS·i64::MAX 超 i64。
        assert_eq!(
            frame_duration_ticks(Rational::new(1, i64::MAX).unwrap()),
            None
        );
    }

    #[test]
    fn sample_duration_common_rates() {
        assert_eq!(
            sample_duration_ticks(48_000),
            Some(Timestamp::from_ticks(147))
        );
        assert_eq!(
            sample_duration_ticks(44_100),
            Some(Timestamp::from_ticks(160))
        );
        assert_eq!(
            sample_duration_ticks(1),
            Some(Timestamp::from_ticks(TICKS_PER_SECOND))
        );
        assert_eq!(sample_duration_ticks(0), None);
        assert_eq!(sample_duration_ticks(-48_000), None);
    }
}
