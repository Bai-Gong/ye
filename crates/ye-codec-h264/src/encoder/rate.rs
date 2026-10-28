//! 单程码控（虚拟缓冲反馈）：M3 首轮登记（m3-plan §5 裁决 5「真码控
//! 不做」）→ 后续批次施工（m3-plan §7.2）。
//!
//! ## 语义（随 `EncoderConfig.bit_rate` 接线，`adapter` 逐帧调用）
//!
//! - `bit_rate = None`：码控缺席，编码器行为与首轮完全一致（恒定 QP /
//!   启发式 CRF），本模块不参与；
//! - `bit_rate = Some(bps)` + [`QualityMode::Qp`]：**CBR 风味**——QP 自
//!   基值起步、受虚拟缓冲反馈**游走**（积分式控制：累计偏差决定偏移，
//!   可远离基值以追上目标码率），输出平均码率趋近 bps；
//! - `bit_rate = Some(bps)` + [`QualityMode::Crf`]：**A-VBR**——基 QP 走
//!   帧内活动度启发式（`adapter::qp_for`），码控只作**上限约束**：游走
//!   QP 仅在高于基 QP 时生效（富余不降质、越线才加粗），对标 ffmpeg
//!   `-crf c -maxrate b -bufsize 2b`。
//!
//! ## 虚拟缓冲口径
//!
//! - 帧预算 `bits_per_frame = bps × fr.den / fr.num`（帧率取
//!   `VideoEncoderConfig.frame_rate`，纯整数截断）；
//! - 缓冲容量 `capacity_bits = 2 × bps`（2 秒），对齐 duan 侧 ffmpeg
//!   引擎 `-bufsize 2 × maxrate` 的口径；目标水位 = 容量一半；
//! - 每帧编码后水位推进 `+ 帧实际位数 − 帧预算`，钳在
//!   `[0, capacity_bits]`；
//! - 首帧（IDR）用基 QP 起步，反馈从第二帧生效。
//!
//! ## 偏差 → ΔQP 映射的依据
//!
//! `ΔQP = clamp(偏差 / quantum, −4, +4)`，`quantum = capacity / 8`：
//!
//! - **满刻度对齐**：偏差值域为 ±capacity/2（水位钳制所至），除以
//!   capacity/8 恰映射 ±4——缓冲打满/放空是单帧 ±4 的极限场景，中间
//!   线性内插；
//! - **增益余量**：码率-QP 经验模型 `bits ∝ 2^(−QP/6)`（QP +6 码率
//!   减半，每 1 档 ≈ 11%）。本比例反馈环的每帧无量纲增益
//!   `g = (ln2/6) × 预算/(capacity/8) = (ln2/6) × 4/fps`，25fps 下
//!   ≈ 0.018，距离散环发散上界（g > 4）余量两个数量级以上；
//! - **防振荡**：单帧 |ΔQP| ≤ 4 + 整数除法死区（|偏差| < quantum 时
//!   ΔQP = 0，水位偏离不足缓冲 1/8 不动档）+ 总 QP 钳 `1..=51`。
//!
//! 全模块纯整数 `i64` 运算，无浮点无 unsafe；构造校验失败走 `Err`。

use ye_codec::QualityMode;
use ye_time::Rational;

/// ΔQP 的单帧上限（绝对值）。防止单帧水位脉冲（如 IDR）引发 QP 跳变
/// 过冲；±4 ≈ 码率 ±40% 的单帧纠偏权限，缓冲口径下足够追目标。
const MAX_QP_STEP: i16 = 4;

/// QP 游走值域下界（H.264 QP 0 显式拒绝的库风，对齐工厂校验）。
const QP_MIN: i16 = 1;

/// QP 游走值域上界。
const QP_MAX: i16 = 51;

/// 码控风味（由 [`QualityMode`] 推导，见模块文档的语义节）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RateFlavor {
    /// CBR：QP 游走值直接作为下一帧 QP（积分式追目标码率）。
    Cbr,
    /// A-VBR 封顶：游走值仅在与基 QP 取大时生效（上限约束）。
    AvbrCap,
}

/// 单程码控控制器（虚拟缓冲反馈；口径与映射依据见模块文档）。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq`（纯整数小状态，
/// 可判等支持测试断言）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RateController {
    /// 码控风味。
    flavor: RateFlavor,
    /// 帧预算（bits/帧 = bps × fr.den / fr.num）。
    bits_per_frame: i64,
    /// 虚拟缓冲容量（bits = 2 × bps）。
    capacity_bits: i64,
    /// 目标水位（bits = 容量一半）。
    target_fill: i64,
    /// 偏差 → ΔQP 换算量子（bits = 容量 1/8；满刻度 ±容量/2 ↔ ±4）。
    quantum: i64,
    /// 当前水位（bits；构造时半满起步）。
    fill_bits: i64,
    /// QP 游走值（反馈累计偏移的载体；未起步时无意义）。
    qp_walk: i16,
    /// 是否已编码首帧（首帧用基 QP，反馈从第二帧生效）。
    started: bool,
}

impl RateController {
    /// 构造：`bit_rate` 单位 bit/s；帧率与质量模式取自
    /// `VideoEncoderConfig`（风味推导见 [`RateFlavor`]）。
    ///
    /// # Errors
    /// `bit_rate` 为 0 / 超出 i64 / 缓冲容量（2×bps）溢出 / 帧预算
    /// （bps × fr.den）溢出 / 帧率非正 → `Err(理由)`（`adapter` 映射为
    /// `InvalidArguments`）。
    pub(crate) fn new(
        bit_rate: u64,
        frame_rate: Rational,
        quality: QualityMode,
    ) -> Result<Self, &'static str> {
        if bit_rate == 0 {
            return Err("bit rate must be > 0");
        }
        let bps = i64::try_from(bit_rate).map_err(|_| "bit rate exceeds i64")?;
        // 容量 = 2×bps 必须落在 i64 内（同时挡住 bps × den 的极端项）
        if bps > i64::MAX / 2 {
            return Err("bit rate too large for the 2-second virtual buffer");
        }
        let capacity_bits = bps * 2;
        let target_fill = capacity_bits / 2;
        let quantum = capacity_bits / 8;
        if frame_rate.num() <= 0 {
            return Err("frame rate must be positive");
        }
        let bits_per_frame = bps
            .checked_mul(i64::from(frame_rate.den()))
            .ok_or("bit rate x frame rate overflows i64")?
            / i64::from(frame_rate.num());
        let flavor = match quality {
            QualityMode::Qp(_) => RateFlavor::Cbr,
            QualityMode::Crf(_) => RateFlavor::AvbrCap,
        };
        Ok(RateController {
            flavor,
            bits_per_frame,
            capacity_bits,
            target_fill,
            quantum,
            fill_bits: target_fill,
            qp_walk: 0,
            started: false,
        })
    }

    /// 下一帧应使用的 QP（编码前调用）。
    ///
    /// 未起步（首帧）→ `base_qp` 原样返回；此后 CBR 返回游走值、A-VBR
    /// 返回 `max(基 QP, 游走值)`（上限约束语义）。
    #[must_use]
    pub(crate) fn next_qp(&self, base_qp: u8) -> u8 {
        if !self.started {
            return base_qp;
        }
        let qp = match self.flavor {
            RateFlavor::Cbr => self.qp_walk,
            RateFlavor::AvbrCap => self.qp_walk.max(i16::from(base_qp)),
        };
        qp.clamp(QP_MIN, QP_MAX) as u8
    }

    /// 一帧编码完成后的回灌（编码后调用）：`frame_bits` 为该帧实际输出
    /// 位数（AVCC 包体位数），`qp_used` 为该帧**实际使用**的 QP——游走
    /// 值以此为锚（A-VBR 下实际值 ≥ 基 QP，游走不会在基 QP 之下累积）。
    pub(crate) fn report_frame(&mut self, frame_bits: i64, qp_used: u8) {
        if !self.started {
            self.started = true;
            self.qp_walk = i16::from(qp_used);
        }
        // 水位推进：入 − 出，钳在缓冲容量内（打满/放空即极限偏差）。
        // 帧位数与水位均为非负、理论上可接近 i64 上界——饱和运算保
        // 证极端和值下钳制结果仍正确
        self.fill_bits = self
            .fill_bits
            .saturating_add(frame_bits)
            .saturating_sub(self.bits_per_frame)
            .clamp(0, self.capacity_bits);
        // 偏差 → ΔQP（映射依据见模块文档）；整数除法死区天然吸收微幅
        // 振荡（|偏差| < quantum → ΔQP = 0）
        let deviation = self.fill_bits - self.target_fill;
        let delta = (deviation / self.quantum)
            .clamp(-i64::from(MAX_QP_STEP), i64::from(MAX_QP_STEP)) as i16;
        self.qp_walk = (i16::from(qp_used) + delta).clamp(QP_MIN, QP_MAX);
    }
}

// ---------------------------------------------------------------------------
// 单元测试：预算换算 / 方向 / 钳制 / 死区 / A-VBR 语义 / 构造校验
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 基准控制器：300kbps @ 25fps、Qp(24) → 预算 12000、容量 600000、
    /// 目标水位 300000、量子 75000。
    fn cbr() -> RateController {
        RateController::new(300_000, Rational::new(25, 1), QualityMode::Qp(24)).unwrap()
    }

    /// 预算与缓冲换算：整数算术逐项断言（含非整帧率）。
    #[test]
    fn budget_and_buffer_conversion() {
        let rc = cbr();
        assert_eq!(rc.bits_per_frame, 12_000, "300k×1/25");
        assert_eq!(rc.capacity_bits, 600_000, "2 秒容量 = 2×bps");
        assert_eq!(rc.target_fill, 300_000, "目标水位 = 容量一半");
        assert_eq!(rc.quantum, 75_000, "量子 = 容量 1/8");
        assert_eq!(rc.fill_bits, 300_000, "起始水位半满");

        // 非整帧率 30000/1001：bps×den/num 精确整除
        let rc = RateController::new(300_000, Rational::new(30_000, 1_001), QualityMode::Qp(24))
            .unwrap();
        assert_eq!(rc.bits_per_frame, 10_010, "300000×1001/30000");
    }

    /// 首帧用基 QP 起步（两种风味一致）：反馈从第二帧生效。
    #[test]
    fn first_frame_uses_base_qp() {
        let rc = cbr();
        assert_eq!(rc.next_qp(24), 24);
        assert_eq!(rc.next_qp(37), 37);

        let rc = RateController::new(300_000, Rational::new(25, 1), QualityMode::Crf(18)).unwrap();
        assert_eq!(rc.next_qp(22), 22);
    }

    /// 超支升 QP：单帧大超支（水位打满 → 满偏差 +300k）恰好 +4；连续
    /// 超支逐帧 +4 爬升，QP 钳 51。
    #[test]
    fn overshoot_raises_qp_by_at_most_4() {
        let mut rc = cbr();
        rc.report_frame(600_000, 24);
        assert_eq!(rc.fill_bits, 600_000, "水位钳在容量");
        assert_eq!(rc.next_qp(24), 28, "满偏差 → ΔQP = +4");

        // 中等超支：偏差 188k → +2
        let mut rc = cbr();
        rc.report_frame(200_000, 24);
        assert_eq!(rc.next_qp(24), 26);

        // 持续超支：+4/帧爬到 51 钳住（24→28→32→36→40→44→48→51）
        let mut rc = cbr();
        let mut qp = rc.next_qp(24);
        let mut seen = vec![qp];
        for _ in 0..10 {
            rc.report_frame(600_000, qp);
            qp = rc.next_qp(24);
            seen.push(qp);
        }
        assert_eq!(seen, vec![24, 28, 32, 36, 40, 44, 48, 51, 51, 51, 51]);
    }

    /// 低支降 QP：零位回灌先死区（偏差 < 75k 不动档），越过死区后逐帧
    /// 下调，水位钳 0、QP 钳 1。
    #[test]
    fn undershoot_lowers_qp_with_deadband() {
        let mut rc = cbr();
        // 前 6 帧：偏差 −12k×k ∈ (−75k, 0) → 死区
        for k in 1..=6 {
            rc.report_frame(0, 24);
            assert_eq!(rc.next_qp(24), 24, "k={k} 死区内不动");
        }
        // 第 7 帧起：偏差 −84k/−96k → −1/帧（以实际使用 QP 为锚）
        rc.report_frame(0, 24);
        assert_eq!(rc.next_qp(24), 23);
        rc.report_frame(0, 23);
        assert_eq!(rc.next_qp(24), 22);

        // 长期零支：水位放空、QP 钳 1
        for _ in 0..40 {
            let qp = rc.next_qp(24);
            rc.report_frame(0, qp);
        }
        assert_eq!(rc.fill_bits, 0, "水位钳在 0");
        assert_eq!(rc.next_qp(24), 1, "QP 钳在下界 1");
    }

    /// 恰好达预算 → 水位不动、QP 不动（确定性稳态）。
    #[test]
    fn on_budget_holds_steady() {
        let mut rc = cbr();
        for _ in 0..10 {
            let qp = rc.next_qp(24);
            rc.report_frame(12_000, qp);
        }
        assert_eq!(rc.fill_bits, 300_000);
        assert_eq!(rc.next_qp(24), 24);

        // 死区内的中等盈余（+38k < 75k）同样不动
        let mut rc = cbr();
        rc.report_frame(50_000, 24);
        assert_eq!(rc.next_qp(24), 24);
    }

    /// A-VBR 语义：封顶生效时 QP 高于基值；富余（水位低）时不降质——
    /// 实际 QP 恒 ≥ 基 QP，游走值以实际 QP 为锚，稳态沉到基 − 单步上限。
    #[test]
    fn avbr_caps_above_base_and_never_below() {
        let mut rc =
            RateController::new(300_000, Rational::new(25, 1), QualityMode::Crf(18)).unwrap();
        // 超支：封顶拉高
        rc.report_frame(600_000, 22);
        assert_eq!(rc.next_qp(22), 26, "游走值 26 > 基 22 → 生效");
        assert_eq!(rc.next_qp(30), 30, "基 QP 更高时取基值");

        // 长期富余（全新控制器、零位回灌）：实际 QP 恒 = 基 18（游走值
        // 低于基即失效），锚定使 walk = 基 + Δ；水位放空（2 秒缓冲 =
        // 50 帧预算）后 Δ 稳态 −4 → walk 稳态 14，仍高于更低基值时生效
        let mut rc =
            RateController::new(300_000, Rational::new(25, 1), QualityMode::Crf(18)).unwrap();
        for _ in 0..100 {
            let qp = rc.next_qp(18);
            rc.report_frame(0, qp);
        }
        assert_eq!(rc.fill_bits, 0, "水位放空");
        assert_eq!(rc.qp_walk, 14, "稳态 = 基 18 − 单步上限 4（锚定语义）");
        assert_eq!(rc.next_qp(22), 22, "富余不降质：不低于基 QP");
        assert_eq!(rc.next_qp(12), 14, "基更低时游走值仍生效（连续性）");
    }

    /// 构造校验：0 / 超界 / 帧预算溢出 / 帧率非正 → Err。
    #[test]
    fn invalid_inputs_rejected() {
        let fps = Rational::new(25, 1);
        let mk = |bps: u64, fr: Rational| RateController::new(bps, fr, QualityMode::Qp(24));
        assert_eq!(mk(0, fps), Err("bit rate must be > 0"));
        assert_eq!(mk(u64::MAX, fps), Err("bit rate exceeds i64"));
        assert_eq!(
            mk(u64::try_from(i64::MAX / 2 + 1).unwrap(), fps),
            Err("bit rate too large for the 2-second virtual buffer")
        );
        // 帧预算 bps × den 溢出 i64
        assert_eq!(
            mk(4_000_000_000_000, Rational::new(1, 2_000_000_000)),
            Err("bit rate x frame rate overflows i64")
        );
        assert_eq!(
            mk(300_000, Rational::new(0, 1)),
            Err("frame rate must be positive")
        );
        assert!(mk(300_000, fps).is_ok());
    }
}
