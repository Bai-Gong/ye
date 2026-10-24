//! 运行报告与摘要行（终验硬断言的接口）。
//!
//! 摘要行是全框架对外的主要机器可读输出，格式**冻结**：
//!
//! ```text
//! CONFORMANCE ran=N skipped=K failed=M sha_ok=P
//! ```
//!
//! - `ran`：实际执行了对拍的**就位集**向量数（含失败）；
//! - `skipped`：未执行的就位集向量（not_fetched / oracle_missing /
//!   unsupported_codec；sha 不符**不算** skip——见 `sha_ok`）——开发期
//!   向量取不到允许 skip，但**必须**落在本行里，漏跑不许静默；
//! - `failed`：执行了且对拍不通过的向量数（`failed ≤ ran`）；
//! - `sha_ok`：sha256 对账通过的向量数（`≤ ran`；缓存文件在而 sha 不符
//!   时向量**照跑不 skip**、计 `sha_ok=false`——sha 漂移是信号
//!   （如换 ffmpeg 版本重生成），不是可跳过的常态）。
//!
//! 待网络清册（`[[pending_vector]]`）**不计入**本行；runner 在摘要行后
//! 另落一行 [`Report::pending_line`]（2026-10-24 施工图①裁决：清册是
//! 「网络恢复后补齐」的登记项，不参与终验断言）。

/// 单个向量的结局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// 对拍已执行。`passed=false` 计入 failed。
    Ran {
        /// 向量名。
        name: String,
        /// 是否全部口径通过。
        passed: bool,
        /// sha256 是否对账通过（不通过也照跑，落摘要行计数）。
        sha_ok: bool,
        /// 人类可读明细（失败原因/通过项）。
        detail: String,
    },
    /// 未执行（原因落摘要行与明细输出）。
    Skipped {
        /// 向量名。
        name: String,
        /// skip 原因（not_fetched / oracle_missing / unsupported_codec）。
        reason: String,
        /// sha256 是否已对账通过（对账先于 oracle 可用性检查）。
        sha_ok: bool,
    },
}

/// 一次运行的全部结局（就位集）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// 逐向量结局（清单顺序）。
    pub outcomes: Vec<Outcome>,
    /// 待网络清册条数（[[pending_vector]]，不参与 ran/skipped/failed 计数）。
    pub pending: usize,
}

impl Report {
    /// (ran, skipped, failed, sha_ok)。
    pub fn counts(&self) -> (usize, usize, usize, usize) {
        let mut ran = 0usize;
        let mut skipped = 0usize;
        let mut failed = 0usize;
        let mut sha_ok = 0usize;
        for o in &self.outcomes {
            match o {
                Outcome::Ran {
                    passed,
                    sha_ok: ran_sha,
                    ..
                } => {
                    ran += 1;
                    if *ran_sha {
                        sha_ok += 1;
                    }
                    if !passed {
                        failed += 1;
                    }
                }
                Outcome::Skipped {
                    sha_ok: skip_sha, ..
                } => {
                    skipped += 1;
                    if *skip_sha {
                        sha_ok += 1;
                    }
                }
            }
        }
        (ran, skipped, failed, sha_ok)
    }

    /// 摘要行（格式冻结，终验 grep 本行）。
    pub fn summary_line(&self) -> String {
        let (ran, skipped, failed, sha_ok) = self.counts();
        format!("CONFORMANCE ran={ran} skipped={skipped} failed={failed} sha_ok={sha_ok}")
    }

    /// 待网络清册行（清册不参与断言；网络恢复后实测 sha256 升入就位集）。
    pub fn pending_line(&self) -> String {
        format!(
            "CONFORMANCE pending={}（待网络清册，不参与断言）",
            self.pending
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_line_format_is_frozen() {
        let r = Report {
            outcomes: vec![
                Outcome::Ran {
                    name: "a".into(),
                    passed: true,
                    sha_ok: true,
                    detail: "nal-sequence PASS | sps-fields PASS".into(),
                },
                Outcome::Ran {
                    name: "b".into(),
                    passed: false,
                    sha_ok: false, // sha 漂移：照跑，不计通过
                    detail: "nal-sequence FAIL".into(),
                },
                Outcome::Skipped {
                    name: "c".into(),
                    reason: "not_fetched".into(),
                    sha_ok: false,
                },
                Outcome::Skipped {
                    name: "d".into(),
                    reason: "oracle_missing".into(),
                    sha_ok: true,
                },
            ],
            pending: 4,
        };
        assert_eq!(r.counts(), (2, 2, 1, 2));
        assert_eq!(
            r.summary_line(),
            "CONFORMANCE ran=2 skipped=2 failed=1 sha_ok=2"
        );
        assert_eq!(
            r.pending_line(),
            "CONFORMANCE pending=4（待网络清册，不参与断言）"
        );
    }

    /// 摘要行机制的行为锁定：抽走向量（Ran→not_fetched Skip）后
    /// ran 减一、skipped 增一且打印（施工图①验收点，端到端现场演示
    /// 见 README「摘要行机制验证」；此处锁计数语义）。
    #[test]
    fn removing_a_vector_shifts_ran_to_skipped() {
        let with = Report {
            outcomes: vec![
                Outcome::Ran {
                    name: "v".into(),
                    passed: true,
                    sha_ok: true,
                    detail: String::new(),
                },
                Outcome::Skipped {
                    name: "w".into(),
                    reason: "not_fetched".into(),
                    sha_ok: false,
                },
            ],
            pending: 0,
        };
        assert_eq!(
            with.summary_line(),
            "CONFORMANCE ran=1 skipped=1 failed=0 sha_ok=1"
        );
        // 人为抽走向量 v（缓存文件被删 → not_fetched）
        let without = Report {
            outcomes: vec![
                Outcome::Skipped {
                    name: "v".into(),
                    reason: "not_fetched".into(),
                    sha_ok: false,
                },
                Outcome::Skipped {
                    name: "w".into(),
                    reason: "not_fetched".into(),
                    sha_ok: false,
                },
            ],
            pending: 0,
        };
        assert_eq!(
            without.summary_line(),
            "CONFORMANCE ran=0 skipped=2 failed=0 sha_ok=0"
        );
    }

    #[test]
    fn empty_report() {
        let r = Report::default();
        assert_eq!(
            r.summary_line(),
            "CONFORMANCE ran=0 skipped=0 failed=0 sha_ok=0"
        );
        assert_eq!(
            r.pending_line(),
            "CONFORMANCE pending=0（待网络清册，不参与断言）"
        );
    }
}
