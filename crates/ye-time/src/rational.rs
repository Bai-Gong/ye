//! 有理数 [`Rational`]：全库时基、帧率与速率的统一表示，对标 `AVRational`。
//!
//! 约定：时基以「秒 / tick」表示（`seconds = ticks · time_base`）；
//! 帧率/采样率常量（[`Rational::RATE_24`] 等）以「每秒次数」表示。

use core::cmp::Ordering;

use crate::error::TimeError;

/// 辗转相除求最大公约数（const 上下文不能用迭代器，手写循环）。
pub(crate) const fn gcd_u128(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// 取 `i64` 的绝对值放宽到 `u128`（调用方只传 i32 量级，`-v` 不会溢出）。
const fn abs_to_u128(v: i64) -> u128 {
    if v < 0 { (-v) as u128 } else { v as u128 }
}

/// i128 中间量归约：先 gcd 约分、再把符号翻到分子，最后检查落回 i32。
/// 约分后分子或分母落不回 `i32` → `Err(Overflow)`；`den == 0` → `Err(InvalidDenominator)`。
fn reduce_i128(num: i128, den: i128) -> Result<Rational, TimeError> {
    if den == 0 {
        return Err(TimeError::InvalidDenominator);
    }
    if num == 0 {
        return Ok(Rational::ZERO);
    }
    let g = gcd_u128(num.unsigned_abs(), den.unsigned_abs()) as i128;
    let mut n = num / g;
    let mut d = den / g;
    if d < 0 {
        n = -n;
        d = -d;
    }
    if n < i32::MIN as i128 || n > i32::MAX as i128 || d > i32::MAX as i128 {
        return Err(TimeError::Overflow);
    }
    Ok(Rational {
        num: n as i32,
        den: d as i32,
    })
}

/// 有理数：时基/帧率/速率的统一表示，对标 FFmpeg 的 `AVRational`。
///
/// ## 不变式（构造函数负责建立；字段私有，外部无法破坏）
///
/// - `den > 0`：符号收在分子 `num` 上；
/// - `|num|` 与 `den` 既约（`gcd(|num|, den) == 1`）；
/// - 0 恒表示为 `0/1`。
///
/// ## derive 清单
///
/// `Debug, Clone, Copy, PartialEq, Eq, Hash`（全字段 `i32` 值类型；
/// 既约形式保证相等当且仅当同值，派生 `Hash` 与此一致）。
/// `Ord`/`PartialOrd` 为**手工实现**（交叉相乘比较数值）——
/// 派生版会按 `(num, den)` 字典序比较，例如会把 1/2 排到 1/3 之前，语义错误。
///
/// ## panic 条件（文档写明的编程错误，仅 [`Rational::new`] 两处）
///
/// ① `den == 0`；② `num == i32::MIN` 且 `den` 为负奇数
/// （归一化先约分再翻符号：|i32::MIN| = 2^31 与负奇数既约，
/// 翻符号需表示 +2^31，落不回 `i32`；负偶数 `den` 因 gcd ≥ 2 安全，
/// 如 `new(i32::MIN, -2) == (2^30, 1)`；正 `den` 永不触发——
/// `num` 本身就是合法 `i32`）。数据驱动路径请改用 [`Rational::try_new`]（对应 `Err`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rational {
    /// 分子（符号收在此侧）。
    num: i32,
    /// 分母（恒为正）。
    den: i32,
}

impl Rational {
    /// 字面量/已校验路径的构造，`const fn`，内部完成归一化
    /// （先 gcd 约分、再翻符号到分子；const 上下文手写循环）。
    ///
    /// # Panics（文档写明的编程错误）
    ///
    /// - ① `den == 0`；
    /// - ② `num == i32::MIN` 且 `den` 为负奇数：归一化翻符号得 +2^31，
    ///   落不回 `i32`。负偶数 `den` 因 gcd ≥ 2 安全；正 `den` 永不触发。
    ///   解析容器字段等数据驱动路径请改用 [`Rational::try_new`]。
    pub const fn new(num: i32, den: i32) -> Rational {
        if den == 0 {
            panic!("ye-time: Rational::new: denominator must not be zero");
        }
        let g = gcd_u128(abs_to_u128(num as i64), abs_to_u128(den as i64)) as i64;
        let mut n = (num as i64) / g;
        let mut d = (den as i64) / g;
        if d < 0 {
            if n == i32::MIN as i64 {
                panic!(
                    "ye-time: Rational::new: |num| = 2^31 with negative odd denominator cannot be normalized into i32"
                );
            }
            n = -n;
            d = -d;
        }
        Rational {
            num: n as i32,
            den: d as i32,
        }
    }

    /// 数据驱动路径（解析容器字段等不可信输入）的安全构造，归一化同 [`Rational::new`]。
    ///
    /// - `den == 0` → `Err(InvalidDenominator)`；
    /// - `num == i32::MIN` 且 `den` 为负奇数 → `Err(Overflow)`
    ///   （归一化翻符号得 +2^31 落不回 `i32`；负偶数 `den` 因 gcd ≥ 2 安全，
    ///   如 `try_new(i32::MIN, -2) == Ok((2^30, 1))`）。
    pub fn try_new(num: i32, den: i32) -> Result<Rational, TimeError> {
        if den == 0 {
            return Err(TimeError::InvalidDenominator);
        }
        let g = gcd_u128(abs_to_u128(num as i64), abs_to_u128(den as i64)) as i64;
        let mut n = (num as i64) / g;
        let mut d = (den as i64) / g;
        if d < 0 {
            if n == i32::MIN as i64 {
                return Err(TimeError::Overflow);
            }
            n = -n;
            d = -d;
        }
        Ok(Rational {
            num: n as i32,
            den: d as i32,
        })
    }

    /// 分子（符号在此侧）。
    pub fn num(&self) -> i32 {
        self.num
    }

    /// 分母（恒为正）。
    pub fn den(&self) -> i32 {
        self.den
    }

    /// 0/1。
    pub const ZERO: Rational = Rational::new(0, 1);
    /// 1/1。
    pub const ONE: Rational = Rational::new(1, 1);
    /// 23.976…（24 ÷ 1.001），千分位写法 23976/1000（归一化为 2997/125）。
    pub const RATE_23_976: Rational = Rational::new(23_976, 1_000);
    /// 24 fps。
    pub const RATE_24: Rational = Rational::new(24, 1);
    /// 25 fps。
    pub const RATE_25: Rational = Rational::new(25, 1);
    /// 29.97…（NTSC），30000/1001。
    pub const RATE_29_97: Rational = Rational::new(30_000, 1_001);
    /// 30 fps。
    pub const RATE_30: Rational = Rational::new(30, 1);
    /// 50 fps。
    pub const RATE_50: Rational = Rational::new(50, 1);
    /// 59.94…（NTSC），60000/1001。
    pub const RATE_59_94: Rational = Rational::new(60_000, 1_001);
    /// 60 fps。
    pub const RATE_60: Rational = Rational::new(60, 1);
    /// 120 fps。
    pub const RATE_120: Rational = Rational::new(120, 1);
    /// 44.1 kHz。
    pub const RATE_44100: Rational = Rational::new(44_100, 1);
    /// 48 kHz。
    pub const RATE_48000: Rational = Rational::new(48_000, 1);

    /// 加法：`a/b + c/d = (ad + cb)/(bd)`。i128 中间量，归约后须落回 i32，
    /// 否则 `Err(Overflow)`。不实现 `std::ops`（杜绝隐藏 panic），全部显式
    /// `Result`，故按 `checked_*` 命名（与整数 `checked_add` 的可失败语义对齐）。
    pub fn checked_add(self, rhs: Rational) -> Result<Rational, TimeError> {
        let n = self.num as i128 * rhs.den as i128 + rhs.num as i128 * self.den as i128;
        let d = self.den as i128 * rhs.den as i128;
        reduce_i128(n, d)
    }

    /// 减法：`a/b − c/d = (ad − cb)/(bd)`。溢出语义同 [`Rational::checked_add`]。
    pub fn checked_sub(self, rhs: Rational) -> Result<Rational, TimeError> {
        let n = self.num as i128 * rhs.den as i128 - rhs.num as i128 * self.den as i128;
        let d = self.den as i128 * rhs.den as i128;
        reduce_i128(n, d)
    }

    /// 乘法：`(ac)/(bd)`。i128 中间量**先乘后约**——即使乘积超出 i32，
    /// 约分后仍可能落回（如 `(i32::MAX/1)·(1/i32::MAX) == 1/1`）；归约后落不回 → `Err(Overflow)`。
    pub fn checked_mul(self, rhs: Rational) -> Result<Rational, TimeError> {
        let n = self.num as i128 * rhs.num as i128;
        let d = self.den as i128 * rhs.den as i128;
        reduce_i128(n, d)
    }

    /// 除法：`(a·d)/(b·c)`。`rhs` 为 0（即 `0/1`）→ `Err(DivideByZero)`。
    pub fn checked_div(self, rhs: Rational) -> Result<Rational, TimeError> {
        if rhs.num == 0 {
            return Err(TimeError::DivideByZero);
        }
        let n = self.num as i128 * rhs.den as i128;
        let d = self.den as i128 * rhs.num as i128;
        reduce_i128(n, d)
    }

    /// 取负。`num == i32::MIN` 时 `−num` 落不回 `i32` → `Err(Overflow)`。
    pub fn checked_neg(self) -> Result<Rational, TimeError> {
        if self.num == i32::MIN {
            return Err(TimeError::Overflow);
        }
        Ok(Rational {
            num: -self.num,
            den: self.den,
        })
    }

    /// 求倒：`den/num`（结果归一化，符号仍在分子）。
    /// `self` 为 0（即 `0/1`）→ `Err(DivideByZero)`。
    pub fn invert(self) -> Result<Rational, TimeError> {
        if self.num == 0 {
            return Err(TimeError::DivideByZero);
        }
        reduce_i128(self.den as i128, self.num as i128)
    }

    /// 显示用浮点近似：`num as f64 / den as f64`。
    ///
    /// infallible：|num/den| ≤ 2^31 << f64 上限，不会产生无穷。
    /// **仅用于 UI/日志**，不作权威值、不参与累积运算（全库约定）。
    pub fn to_f64(self) -> f64 {
        self.num as f64 / self.den as f64
    }

    /// `av_d2q` 等价：按 FFmpeg 同款管线实现——先把 `value` 缩放成精确整数比
    /// `num/2^shift`（`num = floor(|value|·2^shift + 0.5)`，`shift = 62 −
    /// max(ilog2(|value|), 0)`），再做整数连分式展开，在分母不超过 `max_den`
    /// 的收敛子与半收敛子（中间分数）中取误差最小者。误差用整数交叉相乘比较
    /// （f64 减法会抹掉次 ULP 差异导致平手误判），判据同 `av_reduce`：
    /// 下一收敛子超预算时按分母预算收缩部分商，仅当半收敛子**严格更近**时采用
    /// （平手保留收敛子）。与 `av_d2q` 的差异：`av_d2q` 的 `max` 同时约束分子，
    /// 本接口只约束分母（分子以 `i32` 表示能力为界，超界报 `Overflow`）。
    /// 返回值满足 [`Rational`] 全部不变式（收敛子与半收敛子天然既约）。
    ///
    /// - `max_den <= 0` → `Err(InvalidDenominator)`；
    /// - `value` 非有限（NaN/±∞）→ `Err(NotFinite)`；
    /// - 最佳逼近的分子落不回 `i32`（如 |value| ≥ 2^31）→ `Err(Overflow)`；
    /// - 极小 `|value|`（如 `0.01` 配 `max_den = 5`）可能返回 `0/1`——
    ///   此时它确实是分母预算内的最优近似，非零输入的常用量级不会坍缩到零。
    pub fn from_f64(value: f64, max_den: i32) -> Result<Rational, TimeError> {
        if !value.is_finite() {
            return Err(TimeError::NotFinite);
        }
        if max_den <= 0 {
            return Err(TimeError::InvalidDenominator);
        }
        if value.abs() >= 2147483648.0 {
            // |value| ≥ 2^31：任何 i32 分子的有理数都表示不了该量级
            return Err(TimeError::Overflow);
        }
        let negative = value < 0.0;
        let abs_value = value.abs();
        if abs_value == 0.0 {
            return Ok(Rational::ZERO);
        }
        let max = max_den as i128;

        // av_d2q 的取整管线：缩放 2 的幂只动指数、恒精确（最小次正规数 ×2^62
        // = 2^-1012 仍在正规范围）；num ≥ 2^52 时 +0.5 被 ulp 吸收，等价精确取整。
        // |value| < 2^31 保证 num < 2^63，i128 全程无溢出。
        // 二进制指数直接从位模式取（< 1 时取 0，与 FFmpeg 的 FFMAX(e-1,0) 等价）
        let shift = if abs_value < 1.0 {
            62
        } else {
            62 - (((abs_value.to_bits() >> 52) & 0x7ff) as i32 - 1023)
        };
        let num = ((abs_value * (1u64 << shift) as f64) + 0.5).floor() as i128;
        let mut den: i128 = 1i128 << shift;

        // gcd 预约分（av_reduce 同款；den 是 2 的幂，gcd 为 2 的幂）
        let g = gcd_u128(num.unsigned_abs(), den.unsigned_abs()) as i128;
        let mut num = num / g;
        den /= g;
        // 已在预算内 → 精确表示，直接返回（av_reduce 的 fast path）
        if num <= max && den <= max {
            return Ok(Rational {
                num: if negative { -(num as i32) } else { num as i32 },
                den: den as i32,
            });
        }

        // 整数连分式（欧几里得辗转）：尾部 num/den，
        // 收敛子 h0/k0（上一个）、h1/k1（当前，已入预算）
        let mut h0: i128 = 0;
        let mut k0: i128 = 1;
        let mut h1: i128 = 1;
        let mut k1: i128 = 0;
        // 精确值（约分后）留作误差比较基准
        let (vnum, vden) = (num, den);
        // 半收敛子候选 (sn, sd)：因「下一收敛子分母超预算」退出时存在
        let mut semi: Option<(i128, i128)> = None;
        while den > 0 {
            let q = num / den; // 部分商（首圈 num < den 时为 0）
            let a2d = q * k1 + k0;
            if a2d > max {
                // 下一收敛子分母超预算：按分母预算收缩部分商得半收敛子
                // s_t = (t·h1 + h0)/(t·k1 + k0)，误差随 t 单调下降，取最大 t
                let t = (max - k0) / k1;
                if t >= 1 {
                    semi = Some((t * h1 + h0, t * k1 + k0));
                }
                break;
            }
            let a2n = q * h1 + h0;
            h0 = h1;
            h1 = a2n;
            k0 = k1;
            k1 = a2d;
            let r = num - q * den;
            num = den;
            den = r; // r = 0 → 已被收敛子精确捕获，下一轮退出
        }

        let mut h = h1;
        let mut k = k1;
        if let Some((sn, sd)) = semi {
            // 仅当严格更近才采用（av_reduce 判据）。
            // err = |vnum/vden − p/q| 的交叉相乘整数比较：
            // |vnum·sd − sn·vden|·k  vs  |vnum·k − h·vden|·sd（正分母约去）。
            // 量级 ≤ 2^63·2^31·2^31 = 2^125 < i128::MAX，无溢出。
            let e_semi = (vnum * sd - sn * vden).abs() * k;
            let e_conv = (vnum * k - h * vden).abs() * sd;
            if e_semi < e_conv && sn <= i32::MAX as i128 {
                h = sn;
                k = sd;
            }
        }
        if h > i32::MAX as i128 {
            return Err(TimeError::Overflow);
        }
        let num = if negative {
            -(h as i32)
        } else {
            h as i32
        };
        // 收敛子与半收敛子均天然既约（相邻行列式 h·k' − h'·k = ±1），直接构造；
        // negative 时 h ≥ 1（value < 0 且非零），取负不溢出
        Ok(Rational {
            num,
            den: k as i32,
        })
    }
}

impl Ord for Rational {
    /// 数值全序：交叉相乘 `num_lhs·den_rhs` vs `num_rhs·den_lhs`（i64）。
    ///
    /// `den > 0` 保证不等号方向不变；|num|·den ≤ 2^31·2^31 = 2^62 恒不溢出 i64；
    /// 既约形式保证相等当且仅当同值（与派生 `PartialEq`/`Hash` 一致）。
    fn cmp(&self, other: &Rational) -> Ordering {
        let lhs = self.num as i64 * other.den as i64;
        let rhs = other.num as i64 * self.den as i64;
        lhs.cmp(&rhs)
    }
}

impl PartialOrd for Rational {
    fn partial_cmp(&self, other: &Rational) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    fn r(num: i32, den: i32) -> Rational {
        Rational::new(num, den)
    }

    fn parts(x: Rational) -> (i32, i32) {
        (x.num(), x.den())
    }

    fn hash_of(x: Rational) -> u64 {
        let mut h = DefaultHasher::new();
        x.hash(&mut h);
        h.finish()
    }

    // ---- 归一化 ----

    #[test]
    fn new_normalizes() {
        assert_eq!(parts(r(6, 4)), (3, 2));
        assert_eq!(parts(r(2, -4)), (-1, 2));
        assert_eq!(parts(r(0, -7)), (0, 1));
        // 负偶数 den：先约分 gcd=2 再翻符号，(-2^31)/(-2) = +2^30
        assert_eq!(parts(r(i32::MIN, 2)), (-1 << 30, 1));
        assert_eq!(parts(r(i32::MIN, -2)), (1 << 30, 1));
        // 正 den 对照：num 本身是合法 i32，不触发
        assert_eq!(parts(r(i32::MIN, 3)), (i32::MIN, 3));
    }

    #[test]
    fn try_new_invalid_and_overflow() {
        assert_eq!(Rational::try_new(1, 0), Err(TimeError::InvalidDenominator));
        assert_eq!(Rational::try_new(0, 0), Err(TimeError::InvalidDenominator));
        // num == i32::MIN × 负奇数 den：gcd=1、翻符号得 +2^31 > i32::MAX
        assert_eq!(Rational::try_new(i32::MIN, -1), Err(TimeError::Overflow));
        assert_eq!(Rational::try_new(i32::MIN, -3), Err(TimeError::Overflow));
        // 负偶数 den 安全
        assert_eq!(Rational::try_new(i32::MIN, -2).map(parts), Ok((1 << 30, 1)));
        // 正常路径与 new 一致
        assert_eq!(Rational::try_new(6, 4).map(parts), Ok((3, 2)));
    }

    #[test]
    #[should_panic(expected = "denominator must not be zero")]
    fn new_panics_on_zero_den() {
        let _ = Rational::new(1, 0);
    }

    #[test]
    #[should_panic(expected = "cannot be normalized into i32")]
    fn new_panics_on_min_with_minus_one() {
        let _ = Rational::new(i32::MIN, -1);
    }

    #[test]
    #[should_panic(expected = "cannot be normalized into i32")]
    fn new_panics_on_min_with_minus_three() {
        let _ = Rational::new(i32::MIN, -3);
    }

    // ---- 四则 ----

    #[test]
    fn arithmetic_basics() {
        assert_eq!(r(1, 2).checked_add(r(1, 3)).map(parts), Ok((5, 6)));
        assert_eq!(r(1, 2).checked_sub(r(1, 3)).map(parts), Ok((1, 6)));
        assert_eq!(r(2, 3).checked_mul(r(3, 4)).map(parts), Ok((1, 2)));
        assert_eq!(r(1, 2).checked_div(r(3, 4)).map(parts), Ok((2, 3)));
        assert_eq!(r(30000, 1001).invert().map(parts), Ok((1001, 30000)));
        assert_eq!(r(0, 1).invert(), Err(TimeError::DivideByZero));
        assert_eq!(r(1, 2).checked_div(r(0, 1)), Err(TimeError::DivideByZero));
        assert_eq!(r(5, 1).checked_neg().map(parts), Ok((-5, 1)));
        // 0 参与运算保持 0/1
        assert_eq!(r(0, 5).checked_mul(r(7, 3)).map(parts), Ok((0, 1)));
    }

    #[test]
    fn arithmetic_overflow_paths() {
        // 和 2^32−2 落不回 i32
        assert_eq!(
            r(i32::MAX, 1).checked_add(r(i32::MAX, 1)),
            Err(TimeError::Overflow)
        );
        // 分子积 2^60，既约后仍超 i32
        assert_eq!(
            r(1 << 30, 3).checked_mul(r(1 << 30, 5)),
            Err(TimeError::Overflow)
        );
        // 取负 i32::MIN
        assert_eq!(r(i32::MIN, 3).checked_neg(), Err(TimeError::Overflow));
    }

    #[test]
    fn arithmetic_reduce_rescue() {
        // i128 中间量先乘后约：(2^31−1)·1 / 1·(2^31−1) 归约回 1/1
        assert_eq!(
            r(i32::MAX, 1).checked_mul(r(1, i32::MAX)).map(parts),
            Ok((1, 1))
        );
    }

    // ---- Ord / Hash ----

    #[test]
    fn ord_by_value_and_hash_consistent() {
        assert!(r(1, 3) < r(1_000_000, 2_999_999));
        assert!(r(-1, 2) < r(-1, 3));
        assert_eq!(r(2, 4), r(1, 2));
        assert_eq!(hash_of(r(2, 4)), hash_of(r(1, 2)));
        // 字典序陷阱防回归：数值上 1/2 > 1/3，但 (1,2) < (1,3) 按字段字典序
        assert!(r(1, 2) > r(1, 3));
        assert!(r(-2, 3) < r(-1, 3));
    }

    // ---- from_f64 ----

    #[test]
    fn from_f64_known_values() {
        assert_eq!(Rational::from_f64(0.1, 65536).map(parts), Ok((1, 10)));
        assert_eq!(
            Rational::from_f64(29.97, 100_000).map(parts),
            Ok((2997, 100))
        );
        assert_eq!(
            Rational::from_f64(-29.97, 100_000).map(parts),
            Ok((-2997, 100))
        );
        assert_eq!(Rational::from_f64(0.0, 100).map(parts), Ok((0, 1)));
        assert_eq!(Rational::from_f64(-0.0, 100).map(parts), Ok((0, 1)));
        // 0.6 在 max_den=4 下取半收敛子 2/3（err≈0.067 < 收敛子 1/2 的 0.1），
        // 与 av_d2q 一致；0.6/5 精确捕获收敛子 3/5
        assert_eq!(Rational::from_f64(0.6, 4).map(parts), Ok((2, 3)));
        assert_eq!(Rational::from_f64(-0.6, 4).map(parts), Ok((-2, 3)));
        assert_eq!(Rational::from_f64(0.6, 5).map(parts), Ok((3, 5)));
        // 半收敛子取代「坍缩为 0/1」的旧病：非零输入不再退回零值哨兵
        assert_eq!(Rational::from_f64(0.1, 5).map(parts), Ok((1, 5)));
        assert_eq!(Rational::from_f64(0.2, 4).map(parts), Ok((1, 4)));
        // 极小值 0.01 配 max_den=5：0/1 确为分母预算内最优（1/5 误差 0.19）
        assert_eq!(Rational::from_f64(0.01, 5).map(parts), Ok((0, 1)));
    }

    #[test]
    fn from_f64_matches_bruteforce_best() {
        // 半收敛子搜索后应达到「分母 ≤ max_den 的全局最优近似」：
        // 逐分母暴力搜索（p = round(x·q)，含 p = 0 即 0/1）取最小 f64 误差作基准
        let values = [
            0.01f64, 0.05, 0.1, 0.1234567, 0.15, 0.2, 0.3333, 0.5, 0.6, 0.615, 0.85, 0.9, 1.37,
            2.71, 3.16, 12.3456789, 29.97, 59.94, 123.456,
        ];
        for &x in &values {
            for &max_den in &[1i32, 2, 3, 4, 5, 6, 7, 8, 9, 10, 13, 20, 50, 100, 1000] {
                let q = Rational::from_f64(x, max_den).unwrap();
                let got_err = (x - q.to_f64()).abs();
                let best = (1..=max_den)
                    .map(|d| {
                        let p = (x * d as f64).round();
                        (x - p / d as f64).abs()
                    })
                    .fold(f64::INFINITY, f64::min);
                assert!(
                    got_err <= best + 1e-12,
                    "x = {x}, max_den = {max_den}, q = {q:?}, err = {got_err}, best = {best}"
                );
            }
        }
    }

    #[test]
    fn from_f64_errors() {
        assert_eq!(Rational::from_f64(f64::NAN, 100), Err(TimeError::NotFinite));
        assert_eq!(
            Rational::from_f64(f64::INFINITY, 100),
            Err(TimeError::NotFinite)
        );
        assert_eq!(
            Rational::from_f64(f64::NEG_INFINITY, 100),
            Err(TimeError::NotFinite)
        );
        assert_eq!(
            Rational::from_f64(0.5, 0),
            Err(TimeError::InvalidDenominator)
        );
        assert_eq!(
            Rational::from_f64(0.5, -1),
            Err(TimeError::InvalidDenominator)
        );
        // |value| ≥ 2^31：分子落不回 i32
        assert_eq!(Rational::from_f64(3e9, 1000), Err(TimeError::Overflow));
    }

    #[test]
    fn from_f64_accuracy_den_1000() {
        for &x in &[
            0.0,
            -0.1,
            0.1,
            0.125,
            0.25,
            1.0 / 3.0,
            2.5,
            29.97,
            59.94,
            123.456,
        ] {
            let q = Rational::from_f64(x, 1000).unwrap();
            assert!(q.den() >= 1 && q.den() <= 1000, "x = {x}, q = {q:?}");
            let err = (q.to_f64() - x).abs();
            assert!(err < 1e-9, "x = {x}, q = {q:?}, err = {err}");
        }
    }

    // ---- 常量 ----

    #[test]
    fn rate_constants_values() {
        assert_eq!(parts(Rational::ZERO), (0, 1));
        assert_eq!(parts(Rational::ONE), (1, 1));
        assert_eq!(parts(Rational::RATE_23_976), (2997, 125));
        assert_eq!(parts(Rational::RATE_24), (24, 1));
        assert_eq!(parts(Rational::RATE_25), (25, 1));
        assert_eq!(parts(Rational::RATE_29_97), (30000, 1001));
        assert_eq!(parts(Rational::RATE_30), (30, 1));
        assert_eq!(parts(Rational::RATE_50), (50, 1));
        assert_eq!(parts(Rational::RATE_59_94), (60000, 1001));
        assert_eq!(parts(Rational::RATE_60), (60, 1));
        assert_eq!(parts(Rational::RATE_120), (120, 1));
        assert_eq!(parts(Rational::RATE_44100), (44100, 1));
        assert_eq!(parts(Rational::RATE_48000), (48000, 1));
    }

    #[test]
    fn to_f64_display_value() {
        assert_eq!(r(1, 2).to_f64(), 0.5);
        assert_eq!(r(-1, 2).to_f64(), -0.5);
        assert_eq!(r(30000, 1001).to_f64(), 30000.0 / 1001.0);
    }
}
