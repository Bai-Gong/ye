//! 容差声明（按 codec 逐条**写死**）。
//!
//! ## 只准收紧
//!
//! 容差表是入库资产：向**更严**方向修改（收紧/删条）免评审；任何
//! **放宽**（bit-exact → PSNR 下限、提高 PSNR 下限、放宽比较维度）
//! 都必须回施工图重新定口径，不允许在对拍失败的压力下就地放宽。

/// 单条容差口径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tolerance {
    /// 逐位一致（最严口径；PSNR 下限属放宽，禁止出现在本表）。
    BitExact {
        /// 比较维度描述（如 "nal-sequence"）。
        dimension: &'static str,
    },
}

/// 政策说明（runner 每次运行打印，防止口径漂移）。
pub const POLICY_NOTE: &str =
    "容差按 codec 逐条写死；只准收紧（向更严方向），放宽须回施工图重新评审";

/// 取某 codec 的容差表（空表 = 该 codec 尚无对拍口径，runner 会 skip）。
pub fn tolerances(codec: &str) -> &'static [Tolerance] {
    match codec {
        // h264（施工图①码流层对拍；M2 解码落地后追加
        // "yuv420p frames" bit-exact 条目——那也是收紧方向的补条）。
        "h264" => &[
            Tolerance::BitExact {
                dimension: "nal-sequence vs ffmpeg h264_mp4toannexb",
            },
            Tolerance::BitExact {
                dimension: "sps-fields profile/level/width/height vs ffprobe",
            },
        ],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_declared_bit_exact_only() {
        let t = tolerances("h264");
        assert_eq!(t.len(), 2);
        assert!(t.iter().all(|x| matches!(x, Tolerance::BitExact { .. })));
        // 未定义 codec → 空（runner skip）
        assert!(tolerances("hevc").is_empty());
    }
}
