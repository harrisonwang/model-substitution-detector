use std::{collections::BTreeSet, fmt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    Declaration,
    Capability,
    Integer,
    Knowledge,
}

impl Check {
    fn label(self) -> &'static str {
        match self {
            Self::Declaration => "报文取证",
            Self::Capability => "能力评测",
            Self::Integer => "整数指纹",
            Self::Knowledge => "知识边界指纹",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Clear,
    /// 未经校准的行为观察不能作为模型身份的证据。
    Observation,
    Anomaly,
    Rerouted,
    Inconclusive,
    Skipped,
    Error,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Self::Clear => "通过",
            Self::Observation => "参考",
            Self::Anomaly => "不符",
            Self::Rerouted => "模型切换",
            Self::Inconclusive => "无法判定",
            Self::Skipped => "未运行",
            Self::Error => "失败",
        }
    }
}

#[derive(Debug)]
pub struct Finding {
    pub source: String,
    pub check: Check,
    pub status: Status,
    pub detail: String,
    pub error: Option<String>,
    /// 响应中的模型名称，与供人阅读的诊断信息分开存储。
    pub reported_models: Vec<String>,
}

impl Finding {
    pub fn new(
        source: impl Into<String>,
        check: Check,
        status: Status,
        detail: impl Into<String>,
    ) -> Self {
        let detail = detail.into();
        Self {
            source: source.into(),
            check,
            status,
            error: (status == Status::Error).then(|| detail.clone()),
            reported_models: Vec::new(),
            detail,
        }
    }
}

pub struct Report {
    pub model: String,
    pub findings: Vec<Finding>,
}

impl Report {
    pub fn exit_code(&self) -> u8 {
        if self
            .findings
            .iter()
            .any(|finding| matches!(finding.status, Status::Anomaly | Status::Rerouted))
        {
            return 2;
        }
        // 可选参考数据的覆盖情况和应用历史记录不属于执行失败。
        // 实际执行失败或缺少响应证据才属于执行失败。
        if self.findings.iter().any(|finding| {
            finding.error.is_some()
                || finding.status == Status::Error
                || (finding.check == Check::Declaration && finding.status == Status::Inconclusive)
        }) || !self.findings.iter().any(|finding| {
            matches!(finding.check, Check::Declaration | Check::Knowledge)
                && finding.status == Status::Clear
        }) {
            1
        } else {
            0
        }
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if f.alternate() {
            writeln!(f, "目标模型：{}", self.model)?;
            let mut source = None;
            for finding in &self.findings {
                if source != Some(finding.source.as_str()) {
                    writeln!(f, "\n{}", finding.source)?;
                    source = Some(finding.source.as_str());
                }
                writeln!(
                    f,
                    "  {}：{}（{}）",
                    finding.check.label(),
                    finding.status.label(),
                    finding.detail
                )?;
            }
            write!(f, "\n结论：")?;
        }
        write!(f, "{} ", self.model)?;
        match self.exit_code() {
            0 => write!(f, "✓"),
            2 => {
                let models: BTreeSet<_> = self
                    .findings
                    .iter()
                    .filter(|finding| {
                        finding.check == Check::Declaration && finding.status == Status::Anomaly
                    })
                    .flat_map(|finding| finding.reported_models.iter().map(String::as_str))
                    .collect();
                // 模型声明相互冲突时，无法确定唯一的替代模型。
                let replacement = if models.len() == 1 {
                    models.first().copied().unwrap_or("?")
                } else {
                    "?"
                };
                write!(f, "✗ → {replacement}")
            }
            _ => write!(f, "?"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(items: &[(Check, Status)]) -> Report {
        Report {
            model: "gpt-6-astra".into(),
            findings: items
                .iter()
                .map(|(check, status)| {
                    let mut finding = Finding::new("ChatGPT", *check, *status, "返回 gpt-6-luna");
                    if *check == Check::Declaration && *status == Status::Anomaly {
                        finding.reported_models.push("gpt-6-luna".into());
                    }
                    finding
                })
                .collect(),
        }
    }

    #[test]
    fn behavioral_variation_and_optional_coverage_do_not_claim_substitution() {
        let report = report(&[
            (Check::Declaration, Status::Clear),
            (Check::Capability, Status::Observation),
            (Check::Integer, Status::Inconclusive),
            (Check::Knowledge, Status::Skipped),
        ]);
        assert_eq!(report.exit_code(), 0);
        assert_eq!(report.to_string(), "gpt-6-astra ✓");
        assert!(format!("{report:#}").ends_with(&format!("结论：{report}")));
    }

    #[test]
    fn response_mismatch_takes_precedence_over_collection_failure() {
        let report = report(&[
            (Check::Declaration, Status::Anomaly),
            (Check::Knowledge, Status::Error),
        ]);
        assert_eq!(report.exit_code(), 2);
        assert_eq!(report.to_string(), "gpt-6-astra ✗ → gpt-6-luna");
    }

    #[test]
    fn failures_and_absent_identity_evidence_cannot_pass() {
        for items in [
            vec![],
            vec![(Check::Capability, Status::Clear)],
            vec![(Check::Declaration, Status::Inconclusive)],
            vec![
                (Check::Declaration, Status::Clear),
                (Check::Knowledge, Status::Error),
            ],
        ] {
            let report = report(&items);
            assert_eq!(report.exit_code(), 1);
            assert_eq!(report.to_string(), "gpt-6-astra ?");
        }
    }

    #[test]
    fn calibrated_behavior_shift_does_not_invent_a_replacement_model() {
        let report = report(&[(Check::Knowledge, Status::Anomaly)]);
        assert_eq!(report.exit_code(), 2);
        assert_eq!(report.to_string(), "gpt-6-astra ✗ → ?");
    }

    #[test]
    fn conflicting_declarations_do_not_pick_an_arbitrary_replacement() {
        let mut report = report(&[
            (Check::Declaration, Status::Anomaly),
            (Check::Declaration, Status::Anomaly),
        ]);
        assert_eq!(report.to_string(), "gpt-6-astra ✗ → gpt-6-luna");
        report.findings[1].reported_models = vec!["gpt-6.1-sol".into()];
        assert_eq!(report.to_string(), "gpt-6-astra ✗ → ?");
        assert_eq!(report.exit_code(), 2);
        assert!(format!("{report:#}").ends_with(&format!("结论：{report}")));
    }
}
