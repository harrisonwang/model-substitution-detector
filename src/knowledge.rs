//! 知识指纹：经校准的旧版基线或有限的外部参考。
//! 每次运行的数据均保留在内存中；外部参考比较不能认证模型身份。
use crate::collector::{Collector, SampleOutcome};
use crate::statistics::exact_paired_p_value;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs;
use std::io::{ErrorKind, Read};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const MODEL: &str = "gpt-6-astra";
pub const PROTOCOL: &str = "astra-knowledge/v1";
pub const MIN_PAIRS: usize = 30;
pub const ALPHA: f64 = 0.01;
pub const SCREEN_ROUNDS: u32 = 3;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unique_id() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!(
        "{}-{}-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        std::process::id(),
        COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

fn digest<T: Serialize>(value: &T) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(value)?)))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub id: String,
    pub domain: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeSet {
    pub schema_version: u32,
    pub model: String,
    pub id: String,
    pub created_at: u64,
    pub source: String,
    pub probes: Vec<Question>,
}

impl ProbeSet {
    pub fn new(probes: Vec<Question>, source: String) -> Result<Self> {
        let set = Self {
            schema_version: 1,
            model: MODEL.into(),
            id: digest(&probes)?,
            created_at: now(),
            source,
            probes,
        };
        set.validate()?;
        Ok(set)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1 && self.model == MODEL,
            "仅支持 Astra v1 探针格式"
        );
        ensure!(!self.source.trim().is_empty(), "探针来源不能为空");
        ensure!(
            !self.probes.is_empty() && self.probes.len() <= 1000,
            "题目数量须为 1–1000"
        );
        ensure!(self.id == digest(&self.probes)?, "探针内容摘要不匹配");
        let mut ids = BTreeSet::new();
        let mut prompts = BTreeSet::new();
        for q in &self.probes {
            ensure!(
                !q.id.is_empty()
                    && q.id.len() <= 80
                    && q.id
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "非法题目 ID: {}",
                q.id
            );
            ensure!(ids.insert(&q.id), "重复题目 ID: {}", q.id);
            ensure!(
                !q.domain.trim().is_empty() && q.domain.len() <= 100,
                "缺少或过长领域: {}",
                q.id
            );
            ensure!(
                !q.prompt.trim().is_empty() && q.prompt.len() <= 2000,
                "题目为空或过长: {}",
                q.id
            );
            ensure!(
                prompts.insert(
                    q.prompt
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .to_lowercase()
                ),
                "重复题目: {}",
                q.id
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Screening,
    Calibration,
    Validation,
    Audit,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContextKey {
    pub model: String,
    pub cli_version: String,
    pub effort: String,
    pub protocol: String,
    pub profile: Option<String>,
    pub source: String,
}

impl ContextKey {
    /// 通用校验：报文、能力、指纹等所有流程都适用。
    pub fn validate_common(&self) -> Result<()> {
        ensure!(
            !self.cli_version.trim().is_empty() && !self.source.trim().is_empty(),
            "CLI 版本和来源不能为空"
        );
        ensure!(
            ["low", "medium", "high", "xhigh"].contains(&self.effort.as_str()),
            "不支持的 effort"
        );
        Ok(())
    }
    /// 知识边界基线的完整校验：模型与协议必须与基线一致。
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.model == MODEL && self.protocol == PROTOCOL,
            "模型或采集协议不受支持"
        );
        self.validate_common()
    }
    pub fn comparable(&self, other: &Self) -> bool {
        self.model == other.model
            && self.cli_version == other.cli_version
            && self.effort == other.effort
            && self.protocol == other.protocol
            && self.profile == other.profile
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub probe_id: String,
    pub round: u32,
    pub outcome: SampleOutcome,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    pub schema_version: u32,
    pub id: String,
    pub started_at: u64,
    pub finished_at: u64,
    pub phase: Phase,
    pub context: ContextKey,
    pub probes: ProbeSet,
    pub rounds: u32,
    pub samples: Vec<Observation>,
}

impl Run {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1 && !self.id.is_empty(),
            "采样格式错误"
        );
        self.context.validate()?;
        self.probes.validate()?;
        ensure!(self.finished_at >= self.started_at, "采样时间错误");
        ensure!(self.rounds > 0 && self.rounds <= 10, "采样轮次须为 1–10");
        ensure!(
            self.samples.len() == self.probes.probes.len() * self.rounds as usize,
            "采样记录不完整；失败调用也须保留记录"
        );
        let ids: BTreeSet<_> = self.probes.probes.iter().map(|q| q.id.as_str()).collect();
        let mut seen = BTreeSet::new();
        let mut responses = BTreeSet::new();
        for s in &self.samples {
            ensure!(
                ids.contains(s.probe_id.as_str()) && s.round < self.rounds,
                "题目 ID 或轮次无效"
            );
            ensure!(seen.insert((&s.probe_id, s.round)), "重复题目／轮次记录");
            if let Some(id) = &s.outcome.response_id {
                ensure!(responses.insert(id), "重复 response_id，不是独立采样");
            }
        }
        Ok(())
    }
    pub fn observation(&self, id: &str, round: u32) -> Result<&Observation> {
        self.samples
            .iter()
            .find(|s| s.probe_id == id && s.round == round)
            .context("缺少题目采样")
    }
}

/// 采用保守规则，不进行模糊答案匹配，也不使用大模型判分。
pub fn normalize(answer: &str) -> Option<String> {
    let clean = answer
        .trim()
        .trim_matches(|c: char| "\"'`。.!！".contains(c))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let refusal = [
        "i don't know",
        "i do not know",
        "i cannot answer",
        "i can't answer",
        "i cannot determine",
        "i can't determine",
        "i cannot provide",
        "i can't provide",
        "i am unable to",
        "i'm unable to",
        "i am not sure",
        "i'm not sure",
        "i am uncertain",
        "i'm uncertain",
        "sorry,",
        "sorry，",
        "抱歉",
        "对不起",
        "我不知道",
        "我不确定",
        "我无法",
        "无法回答",
        "无法确定",
        "拒绝回答",
    ]
    .iter()
    .any(|prefix| clean.starts_with(prefix));
    if clean.is_empty()
        || clean.chars().count() > 160
        || refusal
        || ["unknown", "n/a", "null", "不知道", "不确定"].contains(&clean.as_str())
        || clean.starts_with("unknown:")
        || clean.starts_with("unknown —")
    {
        None
    } else {
        Some(clean)
    }
}

pub fn valid_answer(out: &SampleOutcome) -> Option<String> {
    if out.error.is_some() {
        return None;
    }
    normalize(out.answer.as_deref()?)
}

pub fn question_prompt(q: &Question) -> String {
    format!(
        "You are completing a closed-book factual recall probe. Answer only from your internal knowledge. Do not browse, call tools, execute code, inspect files, or ask follow-up questions. Give only the shortest unambiguous answer (one name, number, date, or short phrase). If uncertain or unable to answer, use UNKNOWN. Return JSON with exactly one string field named answer.\n\nQuestion:\n{}",
        q.prompt
    )
}

fn select_stable(screening: &Run) -> Result<Vec<(Question, String)>> {
    screening.validate()?;
    ensure!(
        screening.phase == Phase::Screening && screening.rounds == SCREEN_ROUNDS,
        "筛选须为独立 3 轮 screening"
    );
    let mut stable = Vec::new();
    for q in &screening.probes.probes {
        let answers: Vec<_> = (0..SCREEN_ROUNDS)
            .map(|r| {
                screening
                    .observation(&q.id, r)
                    .ok()
                    .and_then(|s| valid_answer(&s.outcome))
            })
            .collect();
        if let Some(expected) = &answers[0]
            && answers.iter().all(|a| a.as_ref() == Some(expected))
        {
            stable.push((q.clone(), expected.clone()));
        }
    }
    ensure!(
        stable.len() >= MIN_PAIRS,
        "仅 {} 道稳定题，至少需要 {MIN_PAIRS} 道；补充候选题后重新筛选",
        stable.len()
    );
    Ok(stable)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StableProbe {
    pub question: Question,
    pub expected: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Quality {
    pub total: usize,
    pub valid: usize,
    pub mismatches: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    pub schema_version: u32,
    pub id: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub parent_id: Option<String>,
    /// 用户提供的来源说明，不代表经过远程认证的身份。
    pub reference_label: String,
    pub context: ContextKey,
    pub probes: Vec<StableProbe>,
    pub screening: Run,
    pub calibration: Run,
    pub validation: Run,
    pub quality: Quality,
}

impl Baseline {
    pub fn probe_set(&self) -> Result<ProbeSet> {
        ProbeSet::new(
            self.probes.iter().map(|p| p.question.clone()).collect(),
            format!("baseline:{}", self.id),
        )
    }
    pub fn hash(&self) -> Result<String> {
        let mut b = self.clone();
        b.id.clear();
        digest(&b)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1 && self.id == self.hash()?,
            "基线格式或内容摘要不匹配"
        );
        ensure!(!self.reference_label.trim().is_empty(), "缺少参考来源说明");
        ensure!(
            self.expires_at > self.created_at
                && self.expires_at.saturating_sub(self.screening.started_at) <= 90 * 86400,
            "基线有效期无效"
        );
        independent(&[&self.screening, &self.calibration, &self.validation])?;
        ensure!(
            self.context == self.screening.context
                && self.context == self.calibration.context
                && self.context == self.validation.context,
            "基线与各阶段的通道或配置不一致"
        );
        ensure!(
            self.screening.finished_at <= self.calibration.started_at
                && self.calibration.finished_at <= self.validation.started_at
                && self.created_at >= self.validation.finished_at,
            "基线各阶段时间顺序无效"
        );
        ensure!(
            self.calibration.phase == Phase::Calibration
                && self.validation.phase == Phase::Validation
                && self.calibration.rounds == 1
                && self.validation.rounds == 1,
            "校准和验证均须为独立单轮"
        );
        let selected = select_stable(&self.screening)?;
        let expected: Vec<_> = selected
            .into_iter()
            .map(|(question, expected)| StableProbe { question, expected })
            .collect();
        ensure!(self.probes == expected, "基线与原始筛选记录不一致");
        let questions: Vec<_> = self.probes.iter().map(|p| p.question.clone()).collect();
        ensure!(
            self.calibration.probes.probes == questions
                && self.validation.probes.probes == questions,
            "校准／验证题集与筛选结果不一致"
        );
        let mut quality = Quality {
            total: self.probes.len(),
            valid: 0,
            mismatches: 0,
        };
        let mut calibration_valid = 0;
        for probe in &self.probes {
            calibration_valid += usize::from(
                valid_answer(&self.calibration.observation(&probe.question.id, 0)?.outcome)
                    .is_some(),
            );
            if let Some(answer) =
                valid_answer(&self.validation.observation(&probe.question.id, 0)?.outcome)
            {
                quality.valid += 1;
                quality.mismatches += usize::from(answer != probe.expected);
            }
        }
        ensure!(
            calibration_valid >= MIN_PAIRS
                && (quality.total - calibration_valid) * 10 <= quality.total,
            "校准有效样本不足或无效率超过 10%"
        );
        ensure!(
            quality.valid >= MIN_PAIRS && (quality.total - quality.valid) * 10 <= quality.total,
            "独立验证有效样本不足或无效率超过 10%"
        );
        ensure!(
            quality.mismatches * 10 <= quality.valid,
            "独立验证失配率超过 10%"
        );
        ensure!(self.quality == quality, "基线质量摘要与原始验证记录不一致");
        Ok(())
    }
}

fn independent(runs: &[&Run]) -> Result<()> {
    let mut ids = BTreeSet::new();
    let mut responses = BTreeSet::new();
    for run in runs {
        run.validate()?;
        ensure!(
            ids.insert(&run.id),
            "各阶段必须独立采样，不能复用同一采样记录"
        );
        for s in &run.samples {
            if let Some(id) = &s.outcome.response_id {
                ensure!(responses.insert(id), "不同阶段复用了 response_id");
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    NoDetectedShift,
    BehaviorShift,
    Inconclusive,
}
impl Decision {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::NoDetectedShift => 0,
            Self::BehaviorShift => 2,
            Self::Inconclusive => 1,
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Self::NoDetectedShift => "未检出显著行为变化",
            Self::BehaviorShift => "发现行为不一致，需独立复核",
            Self::Inconclusive => "证据不足",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairResult {
    pub probe_id: String,
    pub reference_answer: Option<String>,
    pub target_answer: Option<String>,
    pub reference_mismatch: Option<bool>,
    pub target_mismatch: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditReport {
    pub schema_version: u32,
    pub model: String,
    pub baseline_id: String,
    pub run_id: String,
    pub evaluated_at: u64,
    pub decision: Decision,
    pub reasons: Vec<String>,
    pub total: usize,
    pub valid_pairs: usize,
    pub reference_only_mismatches: usize,
    pub target_only_mismatches: usize,
    pub reference_mismatches: usize,
    pub target_mismatches: usize,
    pub p_value: Option<f64>,
    pub alpha: f64,
    pub declaration_mismatches: Vec<String>,
    pub pairs: Vec<PairResult>,
    pub notes: Vec<String>,
}

pub fn score(baseline: &Baseline, run: &Run, evaluated_at: u64) -> Result<AuditReport> {
    baseline.validate()?;
    run.validate()?;
    ensure!(
        run.phase == Phase::Audit && run.rounds == 1,
        "检测须使用单轮 audit 采样"
    );
    independent(&[
        &baseline.screening,
        &baseline.calibration,
        &baseline.validation,
        run,
    ])?;
    let mut report = AuditReport {
        schema_version: 1,
        model: MODEL.into(),
        baseline_id: baseline.id.clone(),
        run_id: run.id.clone(),
        evaluated_at,
        decision: Decision::Inconclusive,
        reasons: Vec::new(),
        total: baseline.probes.len(),
        valid_pairs: 0,
        reference_only_mismatches: 0,
        target_only_mismatches: 0,
        reference_mismatches: 0,
        target_mismatches: 0,
        p_value: None,
        alpha: ALPHA,
        declaration_mismatches: Vec::new(),
        pairs: Vec::new(),
        notes: vec![
            "结果仅描述与用户指定可信参考的行为一致性，不证明真实权重身份。".into(),
            "p 值适用于本轮检验；题目相关性、动态路由及重复检测会影响解释，异常需新采样复核。"
                .into(),
        ],
    };
    if !baseline.context.comparable(&run.context) {
        report
            .reasons
            .push("模型、effort、CLI 版本、profile 或采集协议不一致".into());
    }
    if evaluated_at >= baseline.expires_at || run.finished_at >= baseline.expires_at {
        report
            .reasons
            .push("基线已过期，需从可信参考刷新并激活".into());
    }
    if run.started_at < baseline.created_at || run.finished_at > evaluated_at {
        report.reasons.push("检测采样时间不在有效时间范围内".into());
    }
    if run.probes.probes
        != baseline
            .probes
            .iter()
            .map(|p| p.question.clone())
            .collect::<Vec<_>>()
    {
        report.reasons.push("题集或题目顺序与基线不一致".into());
    }
    if !report.reasons.is_empty() {
        return Ok(report);
    }
    for p in &baseline.probes {
        let reference = valid_answer(&baseline.calibration.observation(&p.question.id, 0)?.outcome);
        let target_sample = &run.observation(&p.question.id, 0)?.outcome;
        let target = valid_answer(target_sample);
        if target_sample.error.is_none()
            && let Some(model) = &target_sample.reported_model
            && !model.trim().is_empty()
            && model != MODEL
            && !report.declaration_mismatches.contains(model)
        {
            report.declaration_mismatches.push(model.clone());
        }
        let r = reference.as_ref().map(|a| a != &p.expected);
        let t = target.as_ref().map(|a| a != &p.expected);
        if let (Some(r), Some(t)) = (r, t) {
            report.valid_pairs += 1;
            report.reference_mismatches += usize::from(r);
            report.target_mismatches += usize::from(t);
            report.reference_only_mismatches += usize::from(r && !t);
            report.target_only_mismatches += usize::from(t && !r);
        }
        report.pairs.push(PairResult {
            probe_id: p.question.id.clone(),
            reference_answer: reference,
            target_answer: target,
            reference_mismatch: r,
            target_mismatch: t,
        });
    }
    if report.valid_pairs < MIN_PAIRS {
        report.reasons.push(format!(
            "有效配对 {}，至少需要 {MIN_PAIRS}",
            report.valid_pairs
        ));
    }
    if (report.total - report.valid_pairs) * 10 > report.total {
        report
            .reasons
            .push("无效配对超过 10%；失败、拒答及 UNKNOWN 不计为答错".into());
    }
    if !report.reasons.is_empty() {
        return Ok(report);
    }
    let p = exact_paired_p_value(
        report.target_only_mismatches as u64,
        report.reference_only_mismatches as u64,
    );
    ensure!(p.is_finite() && (0.0..=1.0).contains(&p), "统计计算失败");
    report.p_value = Some(p);
    report.decision = if p < ALPHA {
        Decision::BehaviorShift
    } else {
        Decision::NoDetectedShift
    };
    if !report.declaration_mismatches.is_empty() {
        report
            .notes
            .push("另有服务端模型声明差异；此字段不作为真实身份或行为检验的替代证据。".into());
    }
    Ok(report)
}

const MAX_JSON_BYTES: u64 = 32 * 1024 * 1024;

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .with_context(|| format!("无法读取 {}", path.display()))?
        .take(MAX_JSON_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= MAX_JSON_BYTES, "JSON 文件超过 32 MiB");
    serde_json::from_slice(&bytes).with_context(|| format!("JSON 格式错误: {}", path.display()))
}

fn active_baseline(
    store: &Path,
    context: &ContextKey,
    evaluated_at: u64,
) -> Result<Option<Baseline>> {
    let path = store.join("active.json");
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        result => {
            result.with_context(|| format!("无法检查 {}", path.display()))?;
        }
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Active {
        baseline_id: String,
        activated_at: u64,
        #[serde(rename = "previous_id")]
        _previous_id: Option<String>,
    }
    let pointer: Active = read_json(&path)?;
    ensure!(
        pointer.baseline_id.len() == 64
            && pointer.baseline_id.bytes().all(|b| b.is_ascii_hexdigit()),
        "非法基线 ID"
    );
    let baseline: Baseline = read_json(
        &store
            .join("baselines")
            .join(format!("{}.json", pointer.baseline_id)),
    )?;
    baseline.validate()?;
    ensure!(baseline.id == pointer.baseline_id, "基线文件名与内容不匹配");
    ensure!(evaluated_at < baseline.expires_at, "基线已过期");
    ensure!(
        baseline.created_at <= pointer.activated_at && pointer.activated_at <= evaluated_at,
        "基线激活时间无效"
    );
    ensure!(
        baseline.context.comparable(context),
        "基线模型、effort、CLI 版本、profile 或采集协议不一致"
    );
    Ok(Some(baseline))
}

/// 提供给 CLI 的结果。保留所有采样中的声明及其有效性标记，
/// 以便执行与其他探针相同的协议检查。
pub struct CheckReport {
    pub calibrated: bool,
    pub decision: Decision,
    pub summary: String,
    pub samples: Vec<SampleOutcome>,
    pub error: Option<String>,
}

#[derive(Deserialize)]
struct ReferenceBank {
    references: Vec<Reference>,
}

#[derive(Deserialize)]
struct Reference {
    model: String,
    date: String,
    probes: Vec<ReferenceProbe>,
}

#[derive(Deserialize)]
struct ReferenceProbe {
    id: String,
    domain: String,
    prompt: String,
    expected: f64,
    tolerance: f64,
    relative: bool,
}

/// 仅按领域分散安排采样，未经模型区分度校准，不代表最优或最小题量。
const REFERENCE_DOMAIN_ORDER: [&str; 15] = [
    "chemistry_bp",
    "biology",
    "programming",
    "chinese_history",
    "chemistry_mp",
    "physics",
    "astronomy",
    "math",
    "medical",
    "pop_culture",
    "internet_culture",
    "chinese_geography",
    "chinese_internet",
    "chinese_literature",
    "crypto_params",
];

/// 外部知识参考的采样计划；比较结果仍未校准，不能认证模型身份。
pub(crate) struct ReferencePlan {
    reference: Reference,
}

pub(crate) fn reference_plan(model: &str) -> Result<Option<ReferencePlan>> {
    let Some(reference) = reference_for(model)? else {
        return Ok(None);
    };
    let plan = ReferencePlan { reference };
    ensure!(plan.order().len() == plan.len(), "知识参考领域顺序不完整");
    Ok(Some(plan))
}

impl ReferencePlan {
    pub(crate) fn len(&self) -> usize {
        self.reference.probes.len()
    }

    /// 先覆盖全部 15 个领域，再取各领域第二题；每个领域内保持原题序。
    pub(crate) fn order(&self) -> Vec<usize> {
        (0..2)
            .flat_map(|round| {
                REFERENCE_DOMAIN_ORDER.iter().filter_map(move |domain| {
                    self.reference
                        .probes
                        .iter()
                        .enumerate()
                        .filter(|(_, probe)| probe.domain == *domain)
                        .nth(round)
                        .map(|(index, _)| index)
                })
            })
            .collect()
    }

    pub(crate) fn prompt(&self, index: usize) -> String {
        reference_prompt(&self.reference.probes[index])
    }

    /// 仅用于决定是否继续采样；UNKNOWN、失败或任何不匹配均不能提前结束。
    pub(crate) fn matches(&self, samples: &[(usize, SampleOutcome)]) -> bool {
        !samples.is_empty()
            && samples.iter().all(|(index, sample)| {
                !sample.model_rerouted
                    && self.reference.probes.get(*index).is_some_and(|probe| {
                        numeric_answer(sample)
                            .is_some_and(|answer| reference_matches(probe, answer))
                    })
            })
    }

    pub(crate) fn report(&self, samples: Vec<(usize, SampleOutcome)>) -> CheckReport {
        compare_reference(&self.reference, samples)
    }
}

fn reference_for(model: &str) -> Result<Option<Reference>> {
    let canonical = match model {
        "claude-fable-5-1" => "claude-fable-5.1",
        other => other,
    };
    let bank: ReferenceBank =
        serde_json::from_str(include_str!("../assets/knowledge-reference.json"))?;
    let Some(reference) = bank.references.into_iter().find(|r| r.model == canonical) else {
        return Ok(None);
    };
    ensure!(reference.probes.len() == 30, "知识参考题数无效");
    let mut ids = BTreeSet::new();
    let mut domains = std::collections::BTreeMap::new();
    for probe in &reference.probes {
        ensure!(
            ids.insert(&probe.id)
                && !probe.prompt.trim().is_empty()
                && probe.expected.is_finite()
                && probe.tolerance.is_finite()
                && probe.tolerance >= 0.0,
            "知识参考数据无效"
        );
        *domains.entry(&probe.domain).or_insert(0) += 1;
    }
    ensure!(
        domains.len() == 15 && domains.values().all(|count| *count == 2),
        "知识参考领域分布无效"
    );
    Ok(Some(reference))
}

fn numeric_answer(sample: &SampleOutcome) -> Option<f64> {
    if sample.error.is_some() {
        return None;
    }
    let number: f64 = sample.answer.as_deref()?.trim().parse().ok()?;
    number.is_finite().then_some(number)
}

fn reference_matches(probe: &ReferenceProbe, answer: f64) -> bool {
    if probe.relative {
        if probe.expected == 0.0 {
            answer == 0.0
        } else {
            (answer - probe.expected).abs() / probe.expected.abs() <= probe.tolerance
        }
    } else {
        // 绝对容差沿用 KBF 的整数舍入约定。
        (answer.round() - probe.expected.round()).abs() <= probe.tolerance
    }
}

fn reference_prompt(probe: &ReferenceProbe) -> String {
    format!(
        "Closed-book numerical recall. Do not browse or use tools. Return JSON with exactly one string field named answer. Its value must contain only one number without units or separators; use UNKNOWN if uncertain.\n\n{}",
        probe.prompt
    )
}

fn reference_prompts(reference: &Reference) -> Vec<String> {
    reference.probes.iter().map(reference_prompt).collect()
}

fn compare_reference(
    reference: &Reference,
    mut indexed_samples: Vec<(usize, SampleOutcome)>,
) -> CheckReport {
    indexed_samples.sort_by_key(|(index, _)| *index);
    let mut samples = Vec::with_capacity(indexed_samples.len());
    let mut valid = 0;
    let mut matched = 0;
    let mut error = None;
    for (index, sample) in indexed_samples {
        let probe = &reference.probes[index];
        if let Some(answer) = numeric_answer(&sample) {
            valid += 1;
            matched += usize::from(reference_matches(probe, answer));
        }
        if error.is_none() {
            error = sample
                .error
                .clone()
                .or_else(|| sample.model_rerouted.then(|| "服务端报告模型切换".into()));
        }
        samples.push(sample);
    }
    CheckReport {
        calibrated: false,
        decision: Decision::Inconclusive,
        summary: format!(
            "已测 {}/{}，匹配 {}/{}，KBF {}，未校准",
            samples.len(),
            reference.probes.len(),
            matched,
            valid,
            reference.date,
        ),
        samples,
        error,
    }
}

fn collect_baseline_samples(
    probes: &[Question],
    mut collect: impl FnMut(&str) -> SampleOutcome,
    progress: &mut dyn FnMut(usize, usize),
) -> Vec<Observation> {
    let mut samples = Vec::with_capacity(probes.len());
    progress(0, probes.len());
    for question in probes {
        let outcome = collect(&question_prompt(question));
        let stopped = outcome.error.is_some() || outcome.model_rerouted;
        samples.push(Observation {
            probe_id: question.id.clone(),
            round: 0,
            outcome,
        });
        progress(samples.len(), probes.len());
        if stopped {
            break;
        }
    }
    samples
}

fn baseline_report(baseline: &Baseline, run: Run) -> CheckReport {
    let mut error = run.samples.iter().find_map(|sample| {
        sample.outcome.error.clone().or_else(|| {
            sample
                .outcome
                .model_rerouted
                .then(|| "服务端报告模型切换".into())
        })
    });
    let report = if error.is_some() {
        // 已停止的运行按设计不会采集完整数据，不能进入 score()。
        None
    } else {
        let scored = (|| -> Result<AuditReport> {
            ensure!(
                run.finished_at < baseline.expires_at,
                "基线在采样过程中已过期"
            );
            score(baseline, &run, run.finished_at)
        })();
        match scored {
            Ok(report) => Some(report),
            Err(reason) => {
                error = Some(reason.to_string());
                None
            }
        }
    };
    let calibrated = baseline.context.source == run.context.source;
    let (decision, mut summary) = match report {
        Some(report) => (
            report.decision.clone(),
            format!(
                "{}，有效对比 {}/{} 题",
                report.decision.label(),
                report.valid_pairs,
                report.total,
            ),
        ),
        None => (
            Decision::Inconclusive,
            format!(
                "已测 {}/{}，无法判定",
                run.samples.len(),
                baseline.probes.len()
            ),
        ),
    };
    if !calibrated {
        summary.push_str("；参考入口不同，仅供参考");
    }
    CheckReport {
        calibrated,
        decision,
        summary,
        // 即使评分失败，也保留声明和明确的路由切换证据。
        samples: run
            .samples
            .into_iter()
            .map(|sample| sample.outcome)
            .collect(),
        error,
    }
}

/// 优先使用适用的现有基线；没有基线时，对支持的模型执行固定外部参考比较，
/// 不在目标模型上进行校准。
/// 现有基线损坏或不兼容时仍然报错，不回退到外部参考。
pub fn check(
    collector: &Collector,
    store: &Path,
    model: &str,
    source: &str,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<Option<CheckReport>> {
    let started_at = now();
    if model == MODEL {
        let context = ContextKey {
            model: model.into(),
            cli_version: collector.version().into(),
            effort: "low".into(),
            protocol: PROTOCOL.into(),
            profile: None,
            source: source.into(),
        };
        if let Some(baseline) = active_baseline(store, &context, started_at)? {
            let probes = baseline.probe_set()?;
            let samples = collect_baseline_samples(
                &probes.probes,
                |prompt| collector.collect(prompt),
                progress,
            );
            let run = Run {
                schema_version: 1,
                id: unique_id(),
                started_at,
                finished_at: now(),
                phase: Phase::Audit,
                context,
                probes,
                rounds: 1,
                samples,
            };
            return Ok(Some(baseline_report(&baseline, run)));
        }
    }
    let Some(reference) = reference_for(model)? else {
        return Ok(None);
    };
    let samples = collector.collect_many(&reference_prompts(&reference), progress);
    Ok(Some(compare_reference(&reference, samples)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const CREATED: u64 = 1_006;
    const EVALUATED: u64 = 1_010;

    #[test]
    fn external_reference_coverage_is_explicit_and_balanced() {
        for model in [MODEL, "claude-fable-5.1", "claude-fable-5-1"] {
            assert_eq!(reference_for(model).unwrap().unwrap().probes.len(), 30);
        }
        for model in ["gpt-6.1-sol", "claude-opus-5.5", "unknown-model"] {
            assert!(reference_for(model).unwrap().is_none());
        }
    }

    #[test]
    fn reference_plan_covers_domains_before_repeating_and_keeps_original_prompts() {
        for model in [MODEL, "claude-fable-5.1", "claude-fable-5-1"] {
            let plan = reference_plan(model).unwrap().unwrap();
            let order = plan.order();
            assert_eq!(plan.len(), 30);
            assert_eq!(&order[..4], &[0, 8, 12, 20]);
            assert_eq!(
                order.iter().copied().collect::<BTreeSet<_>>(),
                (0..30).collect()
            );
            for indices in order.as_chunks::<15>().0 {
                let domains: BTreeSet<_> = indices
                    .iter()
                    .map(|index| plan.reference.probes[*index].domain.as_str())
                    .collect();
                assert_eq!(domains.len(), 15);
            }
            for (first, second) in order[..15].iter().zip(&order[15..]) {
                assert!(first < second);
                assert_eq!(
                    plan.reference.probes[*first].domain,
                    plan.reference.probes[*second].domain
                );
            }
            for (index, probe) in plan.reference.probes.iter().enumerate() {
                assert_eq!(
                    plan.prompt(index),
                    format!(
                        "Closed-book numerical recall. Do not browse or use tools. Return JSON with exactly one string field named answer. Its value must contain only one number without units or separators; use UNKNOWN if uncertain.\n\n{}",
                        probe.prompt
                    )
                );
            }
        }
        assert!(reference_plan("gpt-6.1-sol").unwrap().is_none());
    }

    #[test]
    fn reference_plan_requires_valid_matching_answers_without_claiming_identity() {
        let plan = reference_plan(MODEL).unwrap().unwrap();
        let samples: Vec<_> = plan
            .order()
            .into_iter()
            .take(4)
            .map(|index| {
                (
                    index,
                    SampleOutcome {
                        answer: Some(plan.reference.probes[index].expected.to_string()),
                        reported_model: Some(MODEL.into()),
                        ..Default::default()
                    },
                )
            })
            .collect();
        assert!(plan.matches(&samples));
        assert!(!plan.matches(&[]));
        for answer in [None, Some("UNKNOWN"), Some("NaN"), Some("1e99")] {
            let mut invalid = samples.clone();
            invalid[0].1.answer = answer.map(str::to_owned);
            assert!(!plan.matches(&invalid));
        }
        let mut invalid = samples.clone();
        invalid[0].1.error = Some("fixture timeout".into());
        assert!(!plan.matches(&invalid));
        invalid[0].1.error = None;
        invalid[0].1.model_rerouted = true;
        assert!(!plan.matches(&invalid));
        invalid[0].1.model_rerouted = false;
        invalid[0].0 = plan.len();
        assert!(!plan.matches(&invalid));

        let report = plan.report(samples);
        assert!(!report.calibrated);
        assert_eq!(report.decision, Decision::Inconclusive);
        assert!(report.summary.contains("已测 4/30，匹配 4/4"));
        assert_eq!(report.samples.len(), 4);
    }

    #[test]
    fn external_samples_are_compared_without_claiming_calibrated_identity() {
        let reference = reference_for(MODEL).unwrap().unwrap();
        let prompts = reference_prompts(&reference);
        assert_eq!(prompts.len(), reference.probes.len());
        for (prompt, probe) in prompts.iter().zip(&reference.probes) {
            assert!(prompt.ends_with(&probe.prompt));
        }
        for matches in [true, false] {
            // 逆序完成不能导致答案与其他题目错误配对。
            let samples = reference
                .probes
                .iter()
                .enumerate()
                .rev()
                .map(|(index, probe)| {
                    let answer = if matches {
                        probe.expected
                    } else {
                        probe.expected + 1e15
                    };
                    (
                        index,
                        SampleOutcome {
                            answer: Some(answer.to_string()),
                            response_id: Some(format!("response-{index}")),
                            reported_model: Some(MODEL.into()),
                            ..Default::default()
                        },
                    )
                })
                .collect();
            let report = compare_reference(&reference, samples);
            assert_eq!(report.samples.len(), 30);
            for (index, sample) in report.samples.iter().enumerate() {
                assert_eq!(
                    sample.response_id.as_deref(),
                    Some(format!("response-{index}").as_str())
                );
            }
            assert!(!report.calibrated);
            assert_eq!(report.decision, Decision::Inconclusive);
            assert!(report.error.is_none());
            assert!(report.summary.contains(if matches {
                "匹配 30/30"
            } else {
                "匹配 0/30"
            }));
            assert!(report.summary.contains("未校准"));
        }
    }

    #[test]
    fn unknowns_are_not_mismatches_and_collection_errors_preserve_in_flight_results() {
        let reference = reference_for(MODEL).unwrap().unwrap();
        let unknown = compare_reference(
            &reference,
            (0..reference.probes.len())
                .map(|index| {
                    (
                        index,
                        SampleOutcome {
                            answer: Some("UNKNOWN".into()),
                            reported_model: Some("gpt-6.1-sol".into()),
                            ..Default::default()
                        },
                    )
                })
                .collect(),
        );
        assert_eq!(unknown.samples.len(), 30);
        assert!(unknown.summary.contains("匹配 0/0"));
        assert_eq!(
            unknown.samples[0].reported_model.as_deref(),
            Some("gpt-6.1-sol")
        );
        let failed = compare_reference(
            &reference,
            vec![
                (
                    5,
                    SampleOutcome {
                        answer: Some("78".into()),
                        reported_model: Some("gpt-6.1-sol".into()),
                        error: Some("fixture timeout".into()),
                        model_rerouted: true,
                        ..Default::default()
                    },
                ),
                (
                    17,
                    SampleOutcome {
                        answer: Some(reference.probes[17].expected.to_string()),
                        reported_model: Some(MODEL.into()),
                        ..Default::default()
                    },
                ),
                (
                    0,
                    SampleOutcome {
                        answer: Some(reference.probes[0].expected.to_string()),
                        reported_model: Some(MODEL.into()),
                        ..Default::default()
                    },
                ),
            ],
        );
        assert_eq!(failed.samples.len(), 3);
        assert!(failed.summary.contains("已测 3/30，匹配 2/2"));
        assert_eq!(failed.error.as_deref(), Some("fixture timeout"));
        assert!(failed.samples[1].error.is_some());
        assert!(failed.samples[1].model_rerouted);
        assert!(failed.samples[2].error.is_none());
    }

    #[test]
    fn numeric_reference_answers_reject_ambiguous_and_nonfinite_values() {
        for answer in ["UNKNOWN", "78 degrees", "1 or 2", "NaN", "inf", "1e999"] {
            assert!(
                numeric_answer(&SampleOutcome {
                    answer: Some(answer.into()),
                    ..Default::default()
                })
                .is_none()
            );
        }
        let mut probe = ReferenceProbe {
            id: "fixture".into(),
            domain: "fixture".into(),
            prompt: "fixture".into(),
            expected: 100.0,
            tolerance: 0.05,
            relative: true,
        };
        assert!(reference_matches(&probe, 105.0));
        assert!(!reference_matches(&probe, 106.0));
        probe.expected = 0.0;
        assert!(reference_matches(&probe, 0.0));
        assert!(!reference_matches(&probe, 0.01));
        probe.relative = false;
        probe.tolerance = 0.0;
        probe.expected = -2.5;
        assert!(reference_matches(&probe, -3.0));
        assert!(!reference_matches(&probe, -2.0));
    }

    fn fixture(count: usize) -> (Baseline, Run) {
        let mut set = ProbeSet::new(
            (0..count)
                .map(|i| Question {
                    id: format!("probe-{i}"),
                    domain: format!("domain-{}", i % 5),
                    prompt: format!("Synthetic fixture question {i}?"),
                })
                .collect(),
            "synthetic fixture, not a real model".into(),
        )
        .unwrap();
        set.created_at = 1_000;
        let context = ContextKey {
            model: MODEL.into(),
            cli_version: "codex-cli fixture-1".into(),
            effort: "high".into(),
            protocol: PROTOCOL.into(),
            profile: None,
            source: "synthetic fixture".into(),
        };
        let make_run = |phase, started_at, rounds| Run {
            schema_version: 1,
            id: format!("fixture-{phase:?}"),
            started_at,
            finished_at: started_at + 1,
            phase,
            context: context.clone(),
            probes: set.clone(),
            rounds,
            samples: set
                .probes
                .iter()
                .flat_map(|q| {
                    (0..rounds).map(|round| Observation {
                        probe_id: q.id.clone(),
                        round,
                        outcome: SampleOutcome {
                            answer: Some(format!("answer-{}", q.id)),
                            ..Default::default()
                        },
                    })
                })
                .collect(),
        };
        let mut baseline = Baseline {
            schema_version: 1,
            id: String::new(),
            created_at: CREATED,
            expires_at: CREATED + 30 * 86_400,
            parent_id: None,
            reference_label: "synthetic fixture".into(),
            context: context.clone(),
            probes: set
                .probes
                .iter()
                .cloned()
                .map(|question| StableProbe {
                    expected: format!("answer-{}", question.id),
                    question,
                })
                .collect(),
            screening: make_run(Phase::Screening, 1_000, 3),
            calibration: make_run(Phase::Calibration, 1_002, 1),
            validation: make_run(Phase::Validation, 1_004, 1),
            quality: Quality {
                total: count,
                valid: count,
                mismatches: 0,
            },
        };
        baseline.id = baseline.hash().unwrap();
        baseline.validate().unwrap();
        (baseline, make_run(Phase::Audit, 1_008, 1))
    }

    #[test]
    fn legacy_sampling_stops_on_error_or_reroute_and_preserves_evidence() {
        for failed_at in [0, 2] {
            for rerouted in [false, true] {
                let (baseline, mut run) = fixture(40);
                let mut calls = 0;
                let mut progress = Vec::new();
                run.samples = collect_baseline_samples(
                    &run.probes.probes,
                    |_| {
                        let failed = calls == failed_at;
                        let sample = SampleOutcome {
                            answer: (!failed).then(|| format!("answer-probe-{calls}")),
                            reported_model: Some("gpt-6-luna".into()),
                            error: (failed && !rerouted).then(|| "fixture timeout".into()),
                            model_rerouted: failed && rerouted,
                            ..Default::default()
                        };
                        calls += 1;
                        sample
                    },
                    &mut |completed, total| progress.push((completed, total)),
                );
                assert_eq!(calls, failed_at + 1);
                assert_eq!(
                    progress,
                    (0..=calls)
                        .map(|completed| (completed, 40))
                        .collect::<Vec<_>>()
                );
                let report = baseline_report(&baseline, run);
                assert_eq!(report.decision, Decision::Inconclusive);
                assert!(report.error.is_some());
                assert_eq!(report.samples.len(), calls);
                let last = report.samples.last().unwrap();
                assert_eq!(last.model_rerouted, rerouted);
                assert_eq!(last.reported_model.as_deref(), Some("gpt-6-luna"));
            }
        }
    }

    #[test]
    fn legacy_expiry_and_scoring_failures_keep_collected_declarations() {
        for expired in [false, true] {
            let (baseline, mut run) = fixture(40);
            run.samples[0].outcome.reported_model = Some("gpt-6-luna".into());
            if expired {
                run.finished_at = baseline.expires_at;
            } else {
                // 重复响应会使 score 的独立性检查失败。
                for sample in run.samples.iter_mut().take(2) {
                    sample.outcome.response_id = Some("duplicate-response".into());
                }
            }
            let report = baseline_report(&baseline, run);
            assert_eq!(report.decision, Decision::Inconclusive);
            assert!(report.error.is_some());
            assert_eq!(report.samples.len(), 40);
            assert_eq!(
                report.samples[0].reported_model.as_deref(),
                Some("gpt-6-luna")
            );
        }
    }

    #[test]
    fn retained_ablation_contracts() {
        // 原始实现也运行过相同的八个用例。
        // 这些用例用于验证软件行为，不衡量实际模型的检测准确率。
        for (name, count, changed, missing, effort, expired, expected) in [
            ("unchanged", 40, 0, 0, false, false, 0),
            ("six changes below alpha", 40, 6, 0, false, false, 0),
            ("seven changes above alpha", 40, 7, 0, false, false, 2),
            ("10% missing", 40, 0, 4, false, false, 0),
            ("12.5% missing", 40, 0, 5, false, false, 1),
            ("under 30 valid", 30, 0, 1, false, false, 1),
            ("effort mismatch", 40, 0, 0, true, false, 1),
            ("expired", 40, 0, 0, false, true, 1),
        ] {
            let (baseline, mut run) = fixture(count);
            for sample in run.samples.iter_mut().take(changed) {
                sample.outcome.answer = Some("changed fixture answer".into());
            }
            for sample in run.samples.iter_mut().take(missing) {
                sample.outcome.error = Some("fixture transport failure".into());
            }
            if effort {
                run.context.effort = "low".into();
            }
            let evaluated_at = if expired {
                baseline.expires_at
            } else {
                EVALUATED
            };
            let report = score(&baseline, &run, evaluated_at).unwrap();
            assert_eq!(report.decision.exit_code(), expected, "{name}");
            if changed == 6 || changed == 7 {
                assert!((report.p_value.unwrap() - 0.5_f64.powi(changed as i32)).abs() < 1e-14);
            }
        }
    }

    #[test]
    fn baseline_derivations_and_independence_are_checked_even_with_a_new_hash() {
        let (baseline, _) = fixture(40);
        for mutation in 0..6 {
            let mut changed = baseline.clone();
            match mutation {
                0 => changed.probes[0].expected = "edited answer".into(),
                1 => changed.quality.mismatches += 1,
                2 => changed.calibration.id = changed.screening.id.clone(),
                3 => changed.validation.samples[0].outcome.error = Some("failed".into()),
                4 => changed.screening.samples[0].outcome.answer = Some("UNKNOWN".into()),
                _ => changed.context.cli_version = "edited version".into(),
            }
            changed.id = changed.hash().unwrap();
            assert!(changed.validate().is_err(), "mutation {mutation}");
        }
        let mut tampered = baseline.clone();
        tampered.reference_label.push_str(" edited without rehash");
        assert!(tampered.validate().is_err());
        let roundtrip: Baseline =
            serde_json::from_slice(&serde_json::to_vec(&baseline).unwrap()).unwrap();
        roundtrip.validate().unwrap();
    }

    #[test]
    fn paired_test_uses_discordance_and_keeps_missing_answers_out_of_votes() {
        let (mut baseline, mut run) = fixture(40);
        for sample in baseline.calibration.samples.iter_mut().take(3) {
            sample.outcome.answer = Some("reference mismatch".into());
        }
        baseline.id = baseline.hash().unwrap();
        for sample in run.samples.iter_mut().skip(3).take(8) {
            sample.outcome.answer = Some("target mismatch".into());
        }
        run.samples[39].outcome.answer = Some("UNKNOWN".into());
        let report = score(&baseline, &run, EVALUATED).unwrap();
        assert_eq!(
            (
                report.reference_only_mismatches,
                report.target_only_mismatches
            ),
            (3, 8)
        );
        assert_eq!(report.valid_pairs, 39);
        assert_eq!(report.p_value, Some(exact_paired_p_value(8, 3)));
        run.samples[0].outcome.response_id = Some("duplicated response".into());
        run.samples[1].outcome.response_id = Some("duplicated response".into());
        assert!(score(&baseline, &run, EVALUATED).is_err());
    }

    #[test]
    fn declarations_require_valid_sampling_but_not_a_known_answer() {
        for (reported, error, mismatch) in [
            (
                Some("gpt-6.1-sol"),
                Some("tool event; sample invalid"),
                false,
            ),
            (Some(""), None, false),
            (Some(" \t"), None, false),
            (None, None, false),
            (Some(MODEL), None, false),
            (Some("gpt-6.1-sol"), None, true),
            (Some("gpt-6-astra "), None, true),
        ] {
            let (baseline, mut run) = fixture(40);
            run.samples[0].outcome = SampleOutcome {
                answer: Some("UNKNOWN".into()),
                reported_model: reported.map(str::to_owned),
                error: error.map(str::to_owned),
                ..Default::default()
            };
            let report = score(&baseline, &run, EVALUATED).unwrap();
            assert_eq!(report.valid_pairs, 39);
            assert_eq!(report.decision, Decision::NoDetectedShift);
            let expected: Vec<_> = reported.into_iter().filter(|_| mismatch).collect();
            assert_eq!(report.declaration_mismatches, expected);
        }
    }

    struct Store(PathBuf);
    impl Store {
        fn new() -> Self {
            let store =
                Self(std::env::temp_dir().join(format!("msd-baseline-test-{}", unique_id())));
            fs::create_dir_all(store.0.join("baselines")).unwrap();
            store
        }
        fn save(&self, baseline: &Baseline) {
            fs::write(
                self.0
                    .join("baselines")
                    .join(format!("{}.json", baseline.id)),
                serde_json::to_vec(baseline).unwrap(),
            )
            .unwrap();
            fs::write(
                self.0.join("active.json"),
                serde_json::to_vec(&serde_json::json!({
                    "baseline_id": baseline.id, "activated_at": CREATED, "previous_id": null,
                }))
                .unwrap(),
            )
            .unwrap();
        }
    }
    impl Drop for Store {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn only_an_absent_active_pointer_is_optional() {
        let store = Store::new();
        let (baseline, _) = fixture(40);
        let load = || active_baseline(&store.0, &baseline.context, EVALUATED);
        assert!(load().unwrap().is_none());
        fs::write(store.0.join("active.json"), "malformed").unwrap();
        assert!(load().is_err());
        store.save(&baseline);
        assert_eq!(load().unwrap().unwrap().id, baseline.id);
        assert!(active_baseline(&store.0, &baseline.context, baseline.expires_at).is_err());
        let mut context = baseline.context.clone();
        context.effort = "low".into();
        assert!(active_baseline(&store.0, &context, EVALUATED).is_err());
        fs::remove_file(
            store
                .0
                .join("baselines")
                .join(format!("{}.json", baseline.id)),
        )
        .unwrap();
        assert!(load().is_err());
        fs::write(
            store.0.join("active.json"),
            r#"{"baseline_id":"../outside","activated_at":1006,"previous_id":null}"#,
        )
        .unwrap();
        assert!(load().is_err());
    }
}
