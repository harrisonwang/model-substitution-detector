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
    // 已有校准基线必须保持完整题集，不能套用外部参考的初测停止规则。
    let legacy = model == knowledge::MODEL
        && !matches!(std::fs::symlink_metadata(".msd/active.json"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound);
    let plan = Plan::new(model, legacy)?;
    let mut samples = Vec::with_capacity(plan.tasks.len());
    progress("初测", 0, plan.initial);
    let first = collect_task(&collector, &plan.tasks[0]);
    progress("初测", 1, plan.initial);
    if !first.response_started
        && !first.model_rerouted
        && let Some(error) = &first.error
    {
        bail!("首次采集失败：{error}");
    }
    samples.push((0, first));
    if !should_stop(model, &samples) {
        collect_tasks(
            &collector,
            model,
            &plan.tasks,
            1..plan.initial,
            &mut samples,
            &mut |done| {
                progress("初测", done, plan.initial);
            },
        );
    }
    if !legacy && !should_stop(model, &samples) && !plan.agrees(model, &samples) {
        // 初测一致只是减少采样的实验规则，不是校准后的模型身份认证。
        // 所有原题保留，存疑时每批最多追加三题；不重复询问已经测过的题。
        for start in (plan.initial..plan.tasks.len()).step_by(3) {
            let end = (start + 3).min(plan.tasks.len());
            collect_tasks(
                &collector,
                model,
                &plan.tasks,
                start..end,
                &mut samples,
                &mut |done| {
                    progress("加测", done, plan.tasks.len());
                },
            );
            if should_stop(model, &samples) {
                break;
            }
        }
    }

    let all: Vec<_> = samples.iter().map(|(_, sample)| sample.clone()).collect();
    let declaration = declarations(model, source, &all);
    let stopped = should_stop(model, &samples);
    let reason = match declaration.status {
        Status::Rerouted => "服务已切换模型",
        Status::Anomaly => "模型声明不符",
        _ => "采集失败",
    };
    let mut findings = vec![declaration];
    let capability = plan.samples(Check::Capability, &samples);
    if capability.len() == 1 && capability[0].1.model_rerouted {
        findings.push(Finding::new(
            source,
            Check::Capability,
            Status::Skipped,
            reason,
        ));
    } else {
        let answers: Vec<_> = capability
            .iter()
            .map(|(_, sample)| sample.clone())
            .collect();
        findings.push(capability_finding(
            source,
            capability.iter().map(|(index, _)| &plan.questions[*index]),
            &answers,
        ));
    }
    let integers = plan.samples(Check::Integer, &samples);
    if integers.is_empty() {
        findings.push(Finding::new(
            source,
            Check::Integer,
            Status::Skipped,
            reason,
        ));
    } else {
        match plan.integer_report(model, &samples) {
            Ok(result) => findings.push(integer_finding(source, result)),
            Err(error) => findings.push(Finding::new(
                source,
                Check::Integer,
                Status::Error,
                error.to_string(),
            )),
        }
    }
    let knowledge_samples = plan.samples(Check::Knowledge, &samples);
    let knowledge_result = if legacy && !stopped {
        knowledge::check(
            &collector,
            Path::new(".msd"),
            model,
            source,
            &mut |done, total| {
                progress("知识基线", done, total);
            },
        )
    } else {
        Ok(plan
            .reference
            .as_ref()
            .filter(|_| !knowledge_samples.is_empty())
            .map(|reference| reference.report(knowledge_samples)))
    };
    match knowledge_result {
        Ok(Some(result)) => {
            if legacy {
                let mut all = all;
                all.extend(result.samples.iter().cloned());
                findings[0] = declarations(model, source, &all);
            }
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
        }
        Ok(None) => findings.push(Finding::new(
            source,
            Check::Knowledge,
            Status::Skipped,
            if stopped {
                reason
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
    Ok(findings)
}

struct Task {
    check: Check,
    index: usize,
    prompt: String,
}

struct Plan {
    tasks: Vec<Task>,
    initial: usize,
    questions: &'static [probe::Probe],
    numbers: Vec<usize>,
    reference: Option<knowledge::ReferencePlan>,
}

impl Plan {
    fn new(model: &str, legacy: bool) -> Result<Self> {
        let questions = probe::probes()?;
        // 初测覆盖代码推演与约束推理，尚未按目标/替代模型的样本做区分度校准。
        let mut capabilities: Vec<_> = (0..questions.len()).collect();
        capabilities.sort_by_key(|&index| match questions[index].id.as_str() {
            "code_trace" => (0, 0),
            "crt" => (1, 0),
            _ => (2, index),
        });
        anyhow::ensure!(capabilities.len() >= 2, "能力题库不足两题");
        let reference = if legacy {
            None
        } else {
            knowledge::reference_plan(model)?
        };
        let order = reference
            .as_ref()
            .map_or_else(Vec::new, |reference| reference.order());
        let (integer_prompts, numbers): (Vec<_>, Vec<_>) =
            fingerprint::challenges()?.into_iter().unzip();
        let capability = |index| Task {
            check: Check::Capability,
            index,
            prompt: questions[index].prompt_zh.clone(),
        };
        let knowledge = |index| Task {
            check: Check::Knowledge,
            index,
            prompt: reference
                .as_ref()
                .expect("knowledge indices require a reference")
                .prompt(index),
        };
        let mut tasks = vec![capability(capabilities[0]), capability(capabilities[1])];
        // 混合队列共享三个 worker，避免能力、整数、知识各自等上一阶段完成。
        for (index, prompt) in integer_prompts.into_iter().enumerate() {
            tasks.push(Task {
                check: Check::Integer,
                index,
                prompt,
            });
            if let Some(&index) = order.get(index) {
                tasks.push(knowledge(index));
            }
        }
        if let Some(&index) = order.get(3) {
            tasks.push(knowledge(index));
        }
        let mut initial = tasks.len();
        for index in 0..capabilities
            .len()
            .saturating_sub(2)
            .max(order.len().saturating_sub(4))
        {
            if let Some(&index) = capabilities.get(index + 2) {
                tasks.push(capability(index));
            }
            if let Some(&index) = order.get(index + 4) {
                tasks.push(knowledge(index));
            }
        }
        if legacy {
            initial = tasks.len();
        }
        Ok(Self {
            tasks,
            initial,
            questions,
            numbers,
            reference,
        })
    }

    fn samples(
        &self,
        check: Check,
        samples: &[(usize, SampleOutcome)],
    ) -> Vec<(usize, SampleOutcome)> {
        samples
            .iter()
            .filter_map(|(index, sample)| {
                let task = &self.tasks[*index];
                (task.check == check).then(|| (task.index, sample.clone()))
            })
            .collect()
    }

    fn integer_report(
        &self,
        model: &str,
        samples: &[(usize, SampleOutcome)],
    ) -> Result<fingerprint::FingerprintReport> {
        fingerprint::summarize(
            self.samples(Check::Integer, samples)
                .into_iter()
                .map(|(index, sample)| (sample, self.numbers[index]))
                .collect(),
            model,
        )
    }

    fn agrees(&self, model: &str, samples: &[(usize, SampleOutcome)]) -> bool {
        let Some(reference) = &self.reference else {
            return false;
        };
        let all: Vec<_> = samples.iter().map(|(_, sample)| sample.clone()).collect();
        let capability = self.samples(Check::Capability, samples);
        let knowledge = self.samples(Check::Knowledge, samples);
        samples.len() >= self.initial
            && declarations(model, "", &all).status == Status::Clear
            && capability.len() >= 2
            && capability.iter().all(|(index, sample)| {
                probe::judge(
                    sample.answer.as_deref().unwrap_or_default(),
                    &self.questions[*index].check,
                ) == Outcome::Pass
            })
            && knowledge.len() >= 4
            && reference.matches(&knowledge)
            && self
                .integer_report(model, samples)
                .is_ok_and(|report| report.agrees_with_target())
    }
}

fn collect_task(collector: &Collector, task: &Task) -> SampleOutcome {
    if task.check == Check::Knowledge {
        collector.collect(&task.prompt)
    } else {
        collector.collect_text(&task.prompt)
    }
}

fn collect_tasks(
    collector: &Collector,
    model: &str,
    tasks: &[Task],
    range: std::ops::Range<usize>,
    samples: &mut Vec<(usize, SampleOutcome)>,
    progress: &mut dyn FnMut(usize),
) {
    let start = range.start;
    let completed = samples.len();
    let batch = crate::collector::collect_parallel_until(
        &tasks[range],
        |task| collect_task(collector, task),
        |sample| sample_should_stop(model, sample),
        &mut |done, _| {
            progress(completed + done);
        },
    );
    samples.extend(
        batch
            .into_iter()
            .map(|(index, sample)| (start + index, sample)),
    );
}

fn should_stop(model: &str, samples: &[(usize, SampleOutcome)]) -> bool {
    samples
        .iter()
        .any(|(_, sample)| sample_should_stop(model, sample))
}

fn sample_should_stop(model: &str, sample: &SampleOutcome) -> bool {
    sample.error.is_some()
        || sample.model_rerouted
        || sample.reported_model.as_deref().is_some_and(|reported| {
            !reported.trim().is_empty() && !same_declaration(model, reported)
        })
}

fn integer_finding(source: &str, result: fingerprint::FingerprintReport) -> Finding {
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
    finding
}

fn capability_finding<'a>(
    source: &str,
    questions: impl IntoIterator<Item = &'a probe::Probe>,
    samples: &[SampleOutcome],
) -> Finding {
    let mut passed = 0;
    let mut failed = Vec::new();
    let mut invalid = 0;
    for (question, sample) in questions.into_iter().zip(samples) {
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
