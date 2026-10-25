//! 容差声明（按 codec 逐条**写死**）。
//!
//! ## 只准收紧
//!
//! 容差表是入库资产：向**更严**方向修改（收紧/删条）免评审；任何
//! **放宽**（bit-exact → PSNR 下限、提高 PSNR 下限、放宽比较维度）
//! 都必须回施工图重新定口径，不允许在对拍失败的压力下就地放宽。
//!
//! ## 施工图②逐帧容差（2026-10-24 定死）
//!
//! 原文：「容差（定死，只准收紧）：100% 像素 |Δ|≤1 且 ≥99.9% 像素
//! Δ=0」。落地口径为 [`Tolerance::MaxAbs`]，**逐帧判定**：每帧内全部
//! 样本 |Δ|≤1 且该帧 Δ=0 样本占比 ≥99.9%（比全程合并判定更严——
//! 单帧劣化不能被其他帧摊薄）。收紧方向 = 降 `max_abs`、升
//! `zero_ratio_permille_min` 或收为 [`Tolerance::BitExact`]；放宽
//! （升 max_abs、降占比下限、改全程合并判定）须回施工图重新评审。

use crate::compare::FrameDiffReport;

/// 单条容差口径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tolerance {
    /// 逐位一致（最严口径；PSNR 下限属放宽，禁止出现在本表）。
    BitExact {
        /// 比较维度描述（如 "nal-sequence"）。
        dimension: &'static str,
    },
    /// 逐帧最大绝对差 + 全等占比下限（施工图② YUV 逐帧对拍口径）。
    MaxAbs {
        /// 比较维度描述。
        dimension: &'static str,
        /// 单样本最大绝对差上界（全部样本，即 100% 像素 |Δ|≤max_abs）。
        max_abs: u32,
        /// 每帧 Δ=0 样本占比下限（千分比；999 = ≥99.9%）。
        zero_ratio_permille_min: u64,
    },
}

impl Tolerance {
    /// 对逐帧对拍报告下裁决（[`FrameDiffReport`] 由比较器量出，本方法
    /// 只按口径判）。`Ok(())` = 通过；`Err(归档行)` = 超容差，错误串
    /// 即失败归档的容差行（首差帧/宏块位置/max-abs 由
    /// [`FrameDiffReport::archive_lines`] 提供）。
    ///
    /// # Errors
    /// 超容差 → `Err`（人类可读明细）。
    pub fn check_yuv_frames(&self, report: &FrameDiffReport) -> Result<(), String> {
        match *self {
            Tolerance::BitExact { dimension } => {
                if report.max_abs == 0 {
                    Ok(())
                } else {
                    Err(format!(
                        "{dimension}: FAIL（bit-exact 口径下 max_abs={} 须为 0）",
                        report.max_abs
                    ))
                }
            }
            Tolerance::MaxAbs {
                dimension,
                max_abs,
                zero_ratio_permille_min,
            } => {
                let mut fails: Vec<String> = Vec::new();
                if report.max_abs > max_abs {
                    fails.push(format!(
                        "max_abs={} 超 {}（位于 {}）",
                        report.max_abs,
                        max_abs,
                        report
                            .max_abs_at
                            .as_ref()
                            .map(FrameDiffReport::spot_str)
                            .unwrap_or_else(|| "?".into())
                    ));
                }
                // 每帧 Δ=0 占比 ≥ min‰ ⟺ 差异样本数×1000 ≤ (1000−min)×帧样本数
                let over: Vec<usize> = report
                    .frame_diff_counts
                    .iter()
                    .enumerate()
                    .filter(|&(_f, &d)| {
                        d * 1000 > (1000 - zero_ratio_permille_min) * report.frame_samples as u64
                    })
                    .map(|(f, _)| f)
                    .collect();
                if !over.is_empty() {
                    fails.push(format!(
                        "Δ=0 帧占比不达 {}‰ 的帧共 {} 个（首帧 {}，该帧差异样本 {}/{}）",
                        zero_ratio_permille_min,
                        over.len(),
                        over[0],
                        report.frame_diff_counts[over[0]],
                        report.frame_samples
                    ));
                }
                if fails.is_empty() {
                    Ok(())
                } else {
                    Err(format!("{dimension}: FAIL（{}）", fails.join("；")))
                }
            }
        }
    }
}

/// 政策说明（runner 每次运行打印，防止口径漂移）。
pub const POLICY_NOTE: &str =
    "容差按 codec 逐条写死；只准收紧（向更严方向），放宽须回施工图重新评审";

/// 取某 codec 的容差表（空表 = 该 codec 尚无对拍口径，runner 会 skip）。
pub fn tolerances(codec: &str) -> &'static [Tolerance] {
    match codec {
        // h264：施工图①码流层两条 + 施工图②逐帧 YUV 一条（原文
        // 「100% 像素 |Δ|≤1 且 ≥99.9% 像素 Δ=0」，逐帧判定）。
        "h264" => &[
            Tolerance::BitExact {
                dimension: "nal-sequence vs ffmpeg h264_mp4toannexb",
            },
            Tolerance::BitExact {
                dimension: "sps-fields profile/level/width/height vs ffprobe",
            },
            Tolerance::MaxAbs {
                dimension: "yuv420p frames vs ffmpeg rawvideo（逐帧）",
                max_abs: 1,
                zero_ratio_permille_min: 999,
            },
        ],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compare::{SampleSpot, diff_yuv_frames};

    fn report_from(ours: &[u8], ref_: &[u8]) -> FrameDiffReport {
        diff_yuv_frames(ours, ref_, 4, 4, crate::compare::ChromaSampling::Yuv420).unwrap()
    }

    fn report_at(ours: &[u8], ref_: &[u8], w: usize, h: usize) -> FrameDiffReport {
        diff_yuv_frames(ours, ref_, w, h, crate::compare::ChromaSampling::Yuv420).unwrap()
    }

    #[test]
    fn h264_declared_entries() {
        let t = tolerances("h264");
        assert_eq!(t.len(), 3);
        assert!(
            t.iter()
                .take(2)
                .all(|x| matches!(x, Tolerance::BitExact { .. }))
        );
        // 施工图②逐帧条目：max_abs=1、Δ=0 占比 ≥999‰，写死
        match t[2] {
            Tolerance::MaxAbs {
                max_abs,
                zero_ratio_permille_min,
                ..
            } => {
                assert_eq!(max_abs, 1);
                assert_eq!(zero_ratio_permille_min, 999);
            }
            other => panic!("h264 第 3 条应为 MaxAbs：{other:?}"),
        }
        // 未定义 codec → 空（runner skip）
        assert!(tolerances("hevc").is_empty());
    }

    /// 4×4 420 单帧 24 样本：999‰ 下限 ⟸ 允许差异 ≤ ⌊24×1/1000⌋=0 个…
    /// 实际 (1000-999)*24/1000 = 0.024 → 差 1 个样本即 1000‰>999‰ 超限。
    /// 用两帧（48 样本/帧差数阈值仍 0）验证逐帟能力与 max_abs 维度。
    #[test]
    fn yuv_frame_tolerance_verdicts() {
        let frame = [100u8; 24];
        let tol = tolerances("h264")[2];
        // 完全一致 → Ok
        let both: Vec<u8> = frame.iter().chain(frame.iter()).cloned().collect();
        assert!(tol.check_yuv_frames(&report_from(&both, &both)).is_ok());
        // 单样本 +1（max_abs 合法）但小帧占比超限 → Err（占比维度）
        let mut ours = both.clone();
        ours[5] += 1;
        let r = report_from(&ours, &both);
        assert_eq!(r.max_abs, 1);
        match tol.check_yuv_frames(&r) {
            Err(msg) => assert!(msg.contains("占比"), "{msg}"),
            Ok(()) => panic!("单帧占比超限应 FAIL"),
        }
        // 单样本 +2 → max_abs 维度 FAIL（即使占比维度也超，两因都报）
        let mut ours = both.clone();
        ours[5] += 2;
        match tol.check_yuv_frames(&report_from(&ours, &both)) {
            Err(msg) => assert!(msg.contains("max_abs=2"), "{msg}"),
            Ok(()) => panic!("max_abs=2 应 FAIL"),
        }
        // bit-exact 口径对非零 max_abs 报错
        let be = Tolerance::BitExact { dimension: "t" };
        match be.check_yuv_frames(&report_from(&ours, &both)) {
            Err(msg) => assert!(msg.contains("bit-exact"), "{msg}"),
            Ok(()) => panic!("bit-exact 应 FAIL"),
        }
    }

    /// 占比边界：两帧 48 样本/帧 → 帧内差异 ≤0.048 ⟹ 全等才过；
    /// 用 640×360 大帧造「57 个 ±1 过、58 个不过」的施工图边界。
    #[test]
    fn ratio_boundary_on_640x360_frame() {
        // 单帧 640×360 420：Y=230400 + Cb/Cr 各 57600 → 345600 样本；
        // (1000-999)*345600/1000 = 345.6 → 允许差异 ≤345 个。
        let w = 640usize;
        let h = 360usize;
        let frame_len = w * h + 2 * (w / 2) * (h / 2);
        let base = vec![100u8; frame_len];
        let tol = tolerances("h264")[2];
        let mut ours = base.clone();
        for b in ours.iter_mut().take(345) {
            *b += 1; // 345 个 ±1：345*1000 = 345000 ≤ 345600 → 过
        }
        assert!(tol.check_yuv_frames(&report_at(&ours, &base, w, h)).is_ok());
        let mut ours = base.clone();
        for b in ours.iter_mut().take(346) {
            *b += 1; // 346 个 → 346000 > 345600 → 超限
        }
        match tol.check_yuv_frames(&report_at(&ours, &base, w, h)) {
            Err(msg) => assert!(msg.contains("346"), "{msg}"),
            Ok(()) => panic!("346/345600 应超 999‰ 下限"),
        }
    }

    /// 归档字段联动：首差帧/宏块位置由报告给出（见 compare 模块测试），
    /// 此处锁容差行不与归档字段冲突（容差行只含判据数字）。
    #[test]
    fn failure_detail_uses_report_spots() {
        let frame = [7u8; 24];
        let both: Vec<u8> = frame.iter().chain(frame.iter()).cloned().collect();
        let mut ours = both.clone();
        ours[24 + 3] += 9; // 帧 1（第 2 帧）首样本 +9
        let r = report_from(&ours, &both);
        assert_eq!(r.max_abs, 9);
        assert_eq!(
            r.max_abs_at,
            Some(SampleSpot {
                frame: 1,
                plane: 0,
                x: 3,
                y: 0,
                abs_diff: 9
            })
        );
    }
}
