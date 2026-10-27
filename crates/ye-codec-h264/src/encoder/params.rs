//! 参数集定形（M3 施工图 T5）：level 查表与 avcC 写侧。
//!
//! - [`level_for`]：按 帧尺寸×帧率 取满足约束的**最小** level_idc
//!   （H.264 表 A-1/A-3 的 MaxMBPS/MaxFS/单边宏块数上限子集，见
//!   `LEVELS` 表头注释）；查不到（超出 5.2 上限）返回 `None`，由
//!   调用方（编码器工厂）转 `InvalidArguments`；
//! - [`build_avcc`]：avcC（AVCDecoderConfigurationRecord，
//!   ISO 14496-15 §5.3.3.1）**写侧**构造——SPS/PPS 各一条、长度前缀
//!   4 字节（lengthSizeMinusOne=3）；读侧 [`AvcConfigRecord::parse`]
//!   （`nal.rs`）的回环由测试保证。
//!
//! 表与查表均为纯数据/纯函数（无状态、`Send + Sync` 天然满足）。

use ye_time::Rational;

/// 一个 level 的吞吐/帧尺寸限额（H.264 表 A-1 的子集）。
struct LevelSpec {
    /// level_idc（×10 记法：30 = level 3.0）。
    idc: u8,
    /// MaxMBPS：每秒最大宏块处理量。
    max_mbps: u64,
    /// MaxFS：单帧最大宏块数。
    max_fs: u64,
}

/// level 限额表（H.264 表 A-1，升序；**不含 1b**——1b 与 1.1 共用
/// level_idc=11、靠 constraint_set3_flag 区分，Baseline 收紧下不值得
/// 引入该特例，1b 与 1.1 的限额本身相同，跳过不损失选择）。
const LEVELS: [LevelSpec; 16] = [
    LevelSpec {
        idc: 10,
        max_mbps: 1_485,
        max_fs: 99,
    },
    LevelSpec {
        idc: 11,
        max_mbps: 3_000,
        max_fs: 396,
    },
    LevelSpec {
        idc: 12,
        max_mbps: 6_000,
        max_fs: 396,
    },
    LevelSpec {
        idc: 13,
        max_mbps: 11_880,
        max_fs: 396,
    },
    LevelSpec {
        idc: 20,
        max_mbps: 11_880,
        max_fs: 396,
    },
    LevelSpec {
        idc: 21,
        max_mbps: 18_900,
        max_fs: 792,
    },
    LevelSpec {
        idc: 22,
        max_mbps: 20_250,
        max_fs: 1_620,
    },
    LevelSpec {
        idc: 30,
        max_mbps: 40_500,
        max_fs: 1_620,
    },
    LevelSpec {
        idc: 31,
        max_mbps: 108_000,
        max_fs: 3_600,
    },
    LevelSpec {
        idc: 32,
        max_mbps: 216_000,
        max_fs: 5_120,
    },
    LevelSpec {
        idc: 40,
        max_mbps: 245_760,
        max_fs: 8_192,
    },
    LevelSpec {
        idc: 41,
        max_mbps: 245_760,
        max_fs: 8_192,
    },
    LevelSpec {
        idc: 42,
        max_mbps: 522_240,
        max_fs: 8_704,
    },
    LevelSpec {
        idc: 50,
        max_mbps: 589_824,
        max_fs: 22_080,
    },
    LevelSpec {
        idc: 51,
        max_mbps: 983_040,
        max_fs: 36_864,
    },
    LevelSpec {
        idc: 52,
        max_mbps: 2_073_600,
        max_fs: 36_864,
    },
];

/// 按 帧尺寸（像素）×帧率 取满足约束的最小 level_idc。
///
/// 约束（H.264 表 A-1/A-3 子集，全整数交叉相乘、无浮点）：
///
/// - `MaxFS ≥ mb_w × mb_h`（单帧宏块总数）；
/// - `mb_w、mb_h ≤ ⌊√(8 × MaxFS)⌋`（单边宏块数上限，表 A-3）；
/// - `mb_w × mb_h × frame_rate ≤ MaxMBPS`（吞吐；`frame_rate` 为
///   「每秒次数」的有理数，`num > 0` 由调用方保证）。
///
/// 无满足项（超出 5.2 上限）→ `None`。
#[must_use]
pub fn level_for(width: u32, height: u32, frame_rate: Rational) -> Option<u8> {
    if width == 0 || height == 0 || frame_rate.num() <= 0 {
        return None;
    }
    let mb_w = u64::from(width.div_ceil(16));
    let mb_h = u64::from(height.div_ceil(16));
    let mb_total = mb_w * mb_h;
    // 吞吐比较：mb_total × fr.num ≤ max_mbps × fr.den（i128 防乘法溢出）
    let lhs = i128::from(mb_total) * i128::from(frame_rate.num());
    let fr_den = i128::from(frame_rate.den());
    for level in &LEVELS {
        let max_fs = level.max_fs;
        let side = (8u64.saturating_mul(max_fs)).isqrt();
        let throughput_ok = lhs <= i128::from(level.max_mbps) * fr_den;
        if mb_total <= max_fs && mb_w <= side && mb_h <= side && throughput_ok {
            return Some(level.idc);
        }
    }
    None
}

/// avcC（AVCDecoderConfigurationRecord）写侧：SPS/PPS 各一条、长度
/// 前缀 4 字节（`lengthSizeMinusOne = 3`，与编码器包体 AVCC 形态一致）。
///
/// `profile_indication`/`level_indication` 从 `sps_nal` 头后三字节取
/// （写侧 SPS 的 profile/constraint/level 即权威值），保证 avcC 与
/// 码流内参数集逐字节一致；读侧 [`AvcConfigRecord::parse`] 回环。
#[must_use]
pub(crate) fn build_avcc(sps_nal: &[u8], pps_nal: &[u8]) -> Vec<u8> {
    debug_assert!(
        sps_nal.len() >= 4 && !pps_nal.is_empty(),
        "完整 NAL（含头字节）"
    );
    let mut out = Vec::with_capacity(11 + sps_nal.len() + pps_nal.len());
    out.push(1); // configurationVersion
    out.push(sps_nal[1]); // AVCProfileIndication = SPS profile_idc
    out.push(sps_nal[2]); // profile_compatibility = SPS constraint 字节
    out.push(sps_nal[3]); // AVCLevelIndication = SPS level_idc
    out.push(0xFF); // 保留位 '111111' + lengthSizeMinusOne='11'（4 字节）
    out.push(0xE1); // 保留位 '111' + numOfSequenceParameterSets=1
    out.extend_from_slice(&(sps_nal.len() as u16).to_be_bytes());
    out.extend_from_slice(sps_nal);
    out.push(1); // numOfPictureParameterSets
    out.extend_from_slice(&(pps_nal.len() as u16).to_be_bytes());
    out.extend_from_slice(pps_nal);
    out
}

// ---------------------------------------------------------------------------
// 测试：level 查表边界 + avcC 写读回环
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::frame::{write_pps_nal, write_sps_nal};
    use crate::nal::AvcConfigRecord;
    use crate::sps::Sps;

    /// 表升序且覆盖常见档位（结构性质，防手改表破坏查表前提）。
    #[test]
    fn level_table_is_sorted_and_covers_common_tiers() {
        let idcs: Vec<u8> = LEVELS.iter().map(|l| l.idc).collect();
        let mut sorted = idcs.clone();
        sorted.sort_unstable();
        assert_eq!(idcs, sorted, "表须升序（查表取首个满足项）");
        assert!(idcs.contains(&30) && idcs.contains(&31) && idcs.contains(&40));
        assert_eq!(*idcs.last().unwrap(), 52, "最高档 5.2");
    }

    /// 已知档位锚点（对齐 FFmpeg/x264 常见选择）：
    /// 小尺寸低帧率 → 10；QCIF@30 → 11 附近；CIF@30 → 13；
    /// 480p30 → 30；720p30 → 31；1080p30 → 40；1080p60 → 42。
    #[test]
    fn level_for_known_anchors() {
        let fr = |n: i32, d: i32| Rational::new(n, d);
        assert_eq!(level_for(96, 64, fr(10, 1)), Some(10));
        assert_eq!(level_for(176, 144, fr(30, 1)), Some(11));
        assert_eq!(level_for(352, 288, fr(30, 1)), Some(13));
        assert_eq!(level_for(720, 480, fr(30, 1)), Some(30));
        assert_eq!(level_for(1280, 720, fr(30, 1)), Some(31));
        assert_eq!(level_for(1920, 1080, fr(30, 1)), Some(40));
        assert_eq!(level_for(1920, 1080, fr(60, 1)), Some(42));
        assert_eq!(level_for(3840, 2160, fr(60, 1)), Some(52));
    }

    /// 边界：恰好压线取本档、超线升档；单边宏块数上限（表 A-3）独立
    /// 生效；NTSC 有理帧率走整数交叉相乘。
    #[test]
    fn level_for_boundaries() {
        let fr = |n: i32, d: i32| Rational::new(n, d);
        // 99 MB（11×9）在 level 1 的 MaxFS=99 恰好压线；吞吐 99×30=2970
        // ≤ 3000 → level 11 吞吐过、但 11 也满足——99 MB < 396。两档谁先？
        // level 10：MaxFS 99 ≥ 99 ✓、吞吐 2970 > 1485 ✗ → 升 11。
        assert_eq!(level_for(176, 144, fr(30, 1)), Some(11));
        // 单边上限：level 1 的 √(8×99)=28——80×16 宽 5 MB 高 1 MB，总 5
        // 个 MB 远小于 99，但宽 5 ≤ 28 ✓ → level 10（吞吐 5×30=150 ≤
        // 1485 ✓）。构造真正的单边越界：560px 宽 = 35 MB > 28 → 至少
        // level 1.1（√(8×396)=56 ≥ 35）。
        assert_eq!(level_for(560, 16, fr(30, 1)), Some(11));
        // 超出 5.2：8K@120（1638400 MB × 120 ≈ 1.97e8 MB/s > 2.07e6）
        assert_eq!(level_for(7680, 4320, fr(120, 1)), None);
        // 非法入参
        assert_eq!(level_for(0, 64, fr(30, 1)), None);
        assert_eq!(level_for(64, 0, fr(30, 1)), None);
        assert_eq!(level_for(64, 64, fr(0, 1)), None);
        // NTSC：30000/1001
        assert_eq!(level_for(640, 480, fr(30000, 1001)), Some(30));
    }

    /// avcC 写侧 → [`AvcConfigRecord::parse`] 回环：字段逐项相等、
    /// SPS/PPS 字节原样、profile/level 与 SPS 一致。
    #[test]
    fn avcc_write_parse_roundtrip() {
        for (w, h, level) in [(96usize, 64usize, 10u8), (1920, 1080, 40)] {
            let sps = write_sps_nal(w, h, level).unwrap();
            let pps = write_pps_nal().unwrap();
            let avcc = build_avcc(&sps, &pps);

            let rec = AvcConfigRecord::parse(&avcc).unwrap();
            assert_eq!(rec.configuration_version, 1);
            assert_eq!(rec.profile_indication, 66);
            assert_eq!(rec.profile_compatibility, 0x80, "SPS constraint 字节透传");
            assert_eq!(rec.level_indication, level);
            assert_eq!(rec.length_size, 4);
            assert_eq!(rec.sps_nals, vec![sps.clone()]);
            assert_eq!(rec.pps_nals, vec![pps.clone()]);

            // avcC 里的 SPS 可直接经解析器还原尺寸（容器侧消费形态）
            let parsed = Sps::parse(&rec.sps_nals[0]).unwrap();
            assert_eq!(parsed.level_idc, level);
            assert_eq!(parsed.width() as usize, w);
            assert_eq!(parsed.height() as usize, h);
        }
    }
}
