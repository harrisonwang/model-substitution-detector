use crate::codex::{self, Client};
use crate::collector::{Collector, CollectorConfig, SampleOutcome};
use crate::fingerprint;
use crate::knowledge::{self, Decision};
use crate::local::LocalPaths;
use crate::probe::{self, Outcome};
use crate::verdict::{Check, Finding, Report, Status};
use anyhow::{Result, bail};
use std::collections::BTreeSet;
use std::path::Path;

#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub source: &'static str,
    /// 当前尝试的候选入口；整套检测只由一个可用入口执行。
    pub entry: usize,
    pub entries: usize,
    pub stage: &'static str,
    pub completed: usize,
    pub total: usize,
}

pub fn run(model: &str, progress: impl FnMut(Progress)) -> Report {
    run_with_paths(model, &LocalPaths::from_env(), progress)
}

pub fn run_with_paths(
    model: &str,
    paths: &LocalPaths,
    mut progress: impl FnMut(Progress),
) -> Report {
    let mut report = Report {
        model: model.into(),
        findings: Vec::new(),
    };
    let clients = codex::discover(paths, model);
    if clients.is_empty() {
        let (source, error) = if model.starts_with("claude-") {
            ("Claude", "未找到可用于检测的 Claude 入口")
        } else {
            ("ChatGPT", "未找到 ChatGPT 应用内置引擎或 codex")
        };
        report.findings = unavailable(source, error);
    }
    for (index, client) in clients.iter().enumerate() {
        let mut update = |stage, completed, total| {
            progress(Progress {
                source: client.source.label(),
                entry: index + 1,
                entries: clients.len(),
                stage,
                completed,
                total,
            });
        };
        match check_client(client, model, &mut update) {
            Ok(findings) => {
                // 完成的检测结果包含首次响应后的异常和失败。
                // 保留该结果，不再尝试其他客户端，并丢弃此前不可用入口的失败记录。
                report.findings = findings;
                break;
            }
            Err(error) => report
                .findings
                .extend(unavailable(client.source.label(), &error.to_string())),
        }
    }
    report
}

fn unavailable(source: &str, error: &str) -> Vec<Finding> {
    vec![
        Finding::new(source, Check::Declaration, Status::Error, error),
        Finding::new(source, Check::Capability, Status::Skipped, "连接失败"),
        Finding::new(source, Check::Integer, Status::Skipped, "连接失败"),
        Finding::new(source, Check::Knowledge, Status::Skipped, "连接失败"),
    ]
}

/// 仅在初始化出错或模型尚未响应就失败时，才尝试其他候选入口。
fn check_client(
    client: &Client,
    model: &str,
    progress: &mut dyn FnMut(&'static str, usize, usize),
) -> Result<Vec<Finding>> {
    let source = client.source.label();
    progress("准备", 0, 0);
    let collector = Collector::new(CollectorConfig {
        binary: client.binary.clone(),
        profile: None,
        effort: "low".into(),
        timeout_secs: 120,
        model: model.into(),
    })?;
    let (capability, mut samples) = check_capability(&collector, source, &mut |done, total| {
        progress("能力评测", done, total);
    })?;
    let rerouted = samples.iter().any(|sample| sample.model_rerouted);
    if rerouted || samples.iter().any(|sample| sample.error.is_some()) {
        let reason = if rerouted {
            "服务已切换模型"
        } else {
            "采集失败"
        };
        return Ok(vec![
            declarations(model, source, &samples),
            if rerouted && samples.len() == 1 {
                Finding::new(source, Check::Capability, Status::Skipped, reason)
            } else {
                capability
            },
            Finding::new(source, Check::Integer, Status::Skipped, reason),
            Finding::new(source, Check::Knowledge, Status::Skipped, reason),
        ]);
    }
    let mut findings = vec![capability];
    match fingerprint::collect(&collector, model, &mut |done, total| {
        progress("整数指纹", done, total);
    }) {
        Ok(result) => {
            let error = result
                .samples
                .iter()
                .find_map(|sample| sample.sample.error.clone());
            let status = if error.is_some() {
                Status::Error
            } else if result.valid_samples == 0 {
                Status::Inconclusive
            } else {
                Status::Observation
            };
            let mut detail = format!("有效 {}/{} 轮", result.valid_samples, result.samples.len());
            if let Some(rank) = result.requested_rank {
                detail.push_str(&format!("，目标排名 {}/{}", rank, result.ranking.len()));
            } else if result.valid_samples > 0 {
                detail.push_str("，参考库未覆盖目标");
            }
            if let Some(top) = result.ranking.first() {
                detail.push_str(&format!("，最相似 {}；仅供参考", top.model));
            }
            let mut finding = Finding::new(source, Check::Integer, status, detail);
            finding.error = error;
            findings.push(finding);
            samples.extend(result.samples.into_iter().map(|sample| sample.sample));
        }
        Err(error) => findings.push(Finding::new(
            source,
            Check::Integer,
            Status::Error,
            error.to_string(),
        )),
    }
    let failed = samples
        .iter()
        .any(|sample| sample.model_rerouted || sample.error.is_some())
        || findings
            .iter()
            .any(|finding| finding.status == Status::Error);
    let knowledge_result = if failed {
        Ok(None)
    } else {
        knowledge::check(
            &collector,
            Path::new(".msd"),
            model,
            source,
            &mut |done, total| {
                progress("知识边界", done, total);
            },
        )
    };
    match knowledge_result {
        Ok(Some(result)) => {
            let status = if result.calibrated && result.decision == Decision::BehaviorShift {
                Status::Anomaly
            } else if result.error.is_some() {
                Status::Error
            } else if !result.calibrated {
                Status::Observation
            } else {
                match result.decision {
                    Decision::NoDetectedShift => Status::Clear,
                    Decision::BehaviorShift => Status::Anomaly,
                    Decision::Inconclusive => Status::Inconclusive,
                }
            };
            let mut finding = Finding::new(source, Check::Knowledge, status, result.summary);
            finding.error = result.error;
            findings.push(finding);
            samples.extend(result.samples);
        }
        Ok(None) => findings.push(Finding::new(
            source,
            Check::Knowledge,
            Status::Skipped,
            if samples.iter().any(|sample| sample.model_rerouted) {
                "服务已切换模型"
            } else if failed {
                "采集失败"
            } else {
                "暂无该模型的参考数据"
            },
        )),
        Err(error) => findings.push(Finding::new(
            source,
            Check::Knowledge,
            Status::Error,
            error.to_string(),
        )),
    }
    // 汇总所有检测方法的响应元数据，统一检查模型声明。
    findings.insert(0, declarations(model, source, &samples));
    Ok(findings)
}

fn check_capability(
    collector: &Collector,
    source: &str,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<(Finding, Vec<SampleOutcome>)> {
    let questions = probe::probes()?;
    let Some(first) = questions.first() else {
        bail!("能力题库为空");
    };
    let mut samples = Vec::with_capacity(questions.len());
    progress(0, questions.len());
    // 发起并发请求前先验证连接。模型一旦开始输出，即使答案无效，
    // 也应计入当前入口的检测结果。
    let sample = collector.collect_text(&first.prompt_zh);
    progress(1, questions.len());
    if !sample.response_started
        && !sample.model_rerouted
        && let Some(error) = &sample.error
    {
        bail!("首次采集失败：{error}");
    }
    let failed = sample.model_rerouted || sample.error.is_some();
    samples.push(sample);
    if !failed {
        let prompts: Vec<_> = questions[1..].iter().map(|q| q.prompt_zh.clone()).collect();
        samples.extend(
            collector
                .collect_text_many(&prompts, &mut |done, _| {
                    progress(done + 1, questions.len());
                })
                .into_iter()
                .map(|(_, sample)| sample),
        );
    }
    Ok((capability_finding(source, questions, &samples), samples))
}

fn capability_finding(
    source: &str,
    questions: &[probe::Probe],
    samples: &[SampleOutcome],
) -> Finding {
    let mut passed = 0;
    let mut failed = Vec::new();
    let mut invalid = 0;
    for (question, sample) in questions.iter().zip(samples) {
        let outcome = if sample.error.is_some() {
            Outcome::Unknown
        } else {
            probe::judge(
                sample.answer.as_deref().unwrap_or_default(),
                &question.check,
            )
        };
        match outcome {
            Outcome::Pass => passed += 1,
            Outcome::Fail => failed.push(question.id.as_str()),
            Outcome::Unknown => invalid += 1,
        }
    }
    let error = samples.iter().find_map(|sample| sample.error.clone());
    let status = if error.is_some() {
        Status::Error
    } else if invalid > 0 {
        Status::Inconclusive
    } else if !failed.is_empty() {
        Status::Observation
    } else {
        Status::Clear
    };
    let mut detail = format!("{passed}/{} 题答对", samples.len());
    if !failed.is_empty() {
        detail.push_str(&format!("；未通过 {}，不据此判定换模", failed.join(", ")));
    }
    if invalid > 0 {
        detail.push_str(&format!("；{invalid} 题未取得有效回答"));
    }
    let mut finding = Finding::new(source, Check::Capability, status, detail);
    finding.error = error;
    finding
}

pub fn declarations(model: &str, source: &str, samples: &[SampleOutcome]) -> Finding {
    let mut declared = 0;
    let mut mismatches = BTreeSet::new();
    for sample in samples {
        if sample.error.is_some() {
            continue;
        }
        if let Some(reported) = sample
            .reported_model
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            declared += 1;
            if !same_declaration(model, reported) {
                mismatches.insert(reported);
            }
        }
    }
    let reported_models: Vec<String> = mismatches.into_iter().map(str::to_owned).collect();
    let (status, detail) = if !reported_models.is_empty() {
        (
            Status::Anomaly,
            format!("返回 {}", reported_models.join(", ")),
        )
    } else if samples.iter().any(|sample| sample.model_rerouted) {
        (Status::Rerouted, "服务端报告模型切换".into())
    } else if declared == samples.len() && declared > 0 {
        (
            Status::Clear,
            format!("{declared}/{} 次声明一致", samples.len()),
        )
    } else {
        (
            Status::Inconclusive,
            format!("仅 {declared}/{} 次取得有效模型声明", samples.len()),
        )
    };
    let mut finding = Finding::new(source, Check::Declaration, status, detail);
    finding.reported_models = reported_models;
    finding
}

fn same_declaration(requested: &str, reported: &str) -> bool {
    fn canonical(model: &str) -> &str {
        match model {
            "claude-fable-5.1" => "claude-fable-5-1",
            "claude-opus-5.5" => "claude-opus-5-5",
            _ => model,
        }
    }
    canonical(requested) == canonical(reported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incomplete_samples_do_not_hide_a_valid_mismatch() {
        let samples = [
            SampleOutcome {
                reported_model: Some("gpt-6-luna".into()),
                ..Default::default()
            },
            SampleOutcome {
                error: Some("timeout".into()),
                ..Default::default()
            },
        ];
        assert_eq!(
            declarations("gpt-6-astra", "CLI", &samples).status,
            Status::Anomaly
        );
    }

    #[test]
    fn failed_or_empty_declarations_are_not_substitution_evidence() {
        for sample in [
            SampleOutcome {
                reported_model: Some("gpt-6-luna".into()),
                error: Some("tool used".into()),
                ..Default::default()
            },
            SampleOutcome {
                reported_model: Some("".into()),
                ..Default::default()
            },
        ] {
            assert_eq!(
                declarations("gpt-6-astra", "CLI", &[sample]).status,
                Status::Inconclusive
            );
        }
        assert_eq!(
            declarations("gpt-6-astra", "CLI", &[]).status,
            Status::Inconclusive
        );
    }

    #[test]
    fn capability_errors_are_observations_not_model_identification() {
        let questions = probe::probes().unwrap();
        let samples = vec![
            SampleOutcome {
                answer: Some("wrong".into()),
                ..Default::default()
            };
            questions.len()
        ];
        let finding = capability_finding("ChatGPT", questions, &samples);
        assert_eq!(finding.status, Status::Observation);
        assert!(finding.detail.contains("不据此判定换模"));
    }

    #[test]
    fn aliases_do_not_conflate_different_models() {
        assert!(same_declaration("claude-fable-5.1", "claude-fable-5-1"));
        assert!(!same_declaration("claude-fable-5.1", "claude-opus-5-5"));
        assert!(!same_declaration("gpt-6-astra", "gpt-6-luna"));
    }
}
