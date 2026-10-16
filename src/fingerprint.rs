//! ModelTrace 长整数指纹（移植自 rcell233/llm-checker）。
//!
//! 用 1..=355 整数的分布和位置特征比较参考库；分数只表示库内相似度。
//! 参考库没有 Fable 5.1 / GPT 6.1 Sol，且未按当前 CLI 校准，不能据排名判定换模。
//!
//! 指纹库内嵌在二进制里，运行时不需要联网。

use crate::collector::{Collector, SampleOutcome};
use anyhow::{Result, ensure};
use serde::Deserialize;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const BANK_JSON: &str = include_str!("../assets/unified_bank.json");

/// 取值范围 1..=355，与指纹库一致
pub const RANGE: u32 = 355;
/// 低于这个有效数字个数就不给结论（取自指纹库 minimum_valid_numbers）
pub const MIN_VALID: usize = 80;
pub const ROUNDS: usize = 3;
const MAX_NUMBERS: usize = 1000;
const MAX_ANSWER_BYTES: usize = 256 * 1024;

#[derive(Deserialize)]
struct BankFile {
    recommended_queries: usize,
    minimum_valid_numbers: usize,
    robust: Robust,
}

#[derive(Deserialize)]
struct Robust {
    model_order: Vec<String>,
    hellinger: Artifact,
    ordered_blocks: OrderedArtifact,
}

#[derive(Deserialize)]
struct Artifact {
    feature_mean: Vec<f64>,
    feature_scale: Vec<f64>,
    nuisance_basis: Vec<Vec<f64>>,
    centroids: Vec<Vec<f64>>,
}

#[derive(Deserialize)]
struct OrderedArtifact {
    weight: f64,
    feature_mean: Vec<f64>,
    feature_scale: Vec<f64>,
    nuisance_basis: Vec<Vec<f64>>,
    centroids: Vec<Vec<f64>>,
    /// 12 个环境 × 16 个模型 × 74 维
    environment_centroids: Vec<Vec<Vec<f64>>>,
}

pub struct Bank {
    pub model_order: Vec<String>,
    hellinger: Artifact,
    ordered: OrderedArtifact,
}

static BANK: OnceLock<Result<Bank, String>> = OnceLock::new();

pub fn bank() -> Result<&'static Bank> {
    let cached = BANK.get_or_init(|| {
        let file: BankFile =
            serde_json::from_str(BANK_JSON).map_err(|e| format!("解析指纹库失败: {e}"))?;
        if file.recommended_queries != ROUNDS || file.minimum_valid_numbers != MIN_VALID {
            return Err("整数指纹库采样设置不匹配".into());
        }
        let bank = Bank {
            model_order: file.robust.model_order,
            hellinger: file.robust.hellinger,
            ordered: file.robust.ordered_blocks,
        };
        bank.validate().map_err(|error| error.to_string())?;
        Ok(bank)
    });
    match cached {
        Ok(b) => Ok(b),
        Err(e) => anyhow::bail!("{e}"),
    }
}

impl Bank {
    fn validate(&self) -> Result<()> {
        let models = self.model_order.len();
        ensure!(models > 1 && models <= 1000, "整数指纹库候选数量无效");
        let mut names = std::collections::BTreeSet::new();
        ensure!(
            self.model_order
                .iter()
                .all(|name| !name.is_empty() && names.insert(name)),
            "整数指纹库候选名称无效"
        );
        validate_features(
            &self.hellinger.feature_mean,
            &self.hellinger.feature_scale,
            &self.hellinger.nuisance_basis,
            &self.hellinger.centroids,
            RANGE as usize,
            models,
        )?;
        validate_features(
            &self.ordered.feature_mean,
            &self.ordered.feature_scale,
            &self.ordered.nuisance_basis,
            &self.ordered.centroids,
            74,
            models,
        )?;
        ensure!(
            self.ordered.weight.is_finite() && (0.0..=1.0).contains(&self.ordered.weight),
            "整数指纹库权重无效"
        );
        ensure!(
            !self.ordered.environment_centroids.is_empty(),
            "整数指纹库缺少环境参考"
        );
        ensure!(
            self.ordered
                .environment_centroids
                .iter()
                .all(|environment| environment.len() == models
                    && environment
                        .iter()
                        .all(|row| row.len() == 74 && row.iter().all(|n| n.is_finite()))),
            "整数指纹库环境参考维度无效"
        );
        Ok(())
    }
}

fn validate_features(
    mean: &[f64],
    scale: &[f64],
    basis: &[Vec<f64>],
    centroids: &[Vec<f64>],
    dimensions: usize,
    models: usize,
) -> Result<()> {
    ensure!(
        mean.len() == dimensions
            && mean.iter().all(|n| n.is_finite())
            && scale.len() == dimensions
            && scale.iter().all(|n| n.is_finite() && *n > 0.0)
            && centroids.len() == models
            && basis
                .iter()
                .chain(centroids)
                .all(|row| row.len() == dimensions && row.iter().all(|n| n.is_finite())),
        "整数指纹库特征维度或数值无效"
    );
    Ok(())
}

#[derive(Debug)]
pub struct FingerprintSample {
    pub expected_numbers: usize,
    pub valid_numbers: usize,
    pub sample: SampleOutcome,
}

#[derive(Debug)]
pub struct FingerprintReport {
    pub valid_samples: usize,
    pub requested_rank: Option<usize>,
    pub ranking: Vec<Score>,
    pub samples: Vec<FingerprintSample>,
}

impl FingerprintReport {
    /// 只用于决定是否继续加测；一致排名不代表模型身份已经验证。
    pub(crate) fn agrees_with_target(&self) -> bool {
        if self.valid_samples != ROUNDS
            || self.samples.len() != ROUNDS
            || self.requested_rank != Some(1)
        {
            return false;
        }
        let Some(target) = unique_winner(&self.ranking) else {
            return false;
        };
        self.samples.iter().all(|sample| {
            if sample.sample.error.is_some() {
                return false;
            }
            let numbers = numbers_from(sample.sample.answer.as_deref().unwrap_or_default());
            score(&numbers).is_ok_and(|scores| {
                unique_winner(&scores).is_some_and(|winner| same_label(winner, target))
            })
        })
    }
}

fn unique_winner(scores: &[Score]) -> Option<&str> {
    let (index, best) = scores
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.score.total_cmp(&b.score))?;
    scores
        .iter()
        .enumerate()
        .all(|(other, score)| {
            score.score.is_finite() && (other == index || best.score > score.score)
        })
        .then_some(best.model.as_str())
}

pub(crate) fn challenges() -> Result<Vec<(String, usize)>> {
    bank()?;
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    Ok((0..ROUNDS).map(|round| challenge(round, seed)).collect())
}

/// 通过采集器的限并发采样执行三次独立 CLI 调用。
/// 样本和报告均不写入磁盘。
/// 即使请求的模型已收录在库中，排名也仅供辅助参考。
pub fn collect(
    collector: &Collector,
    requested_model: &str,
    progress: &mut dyn FnMut(usize, usize),
) -> Result<FingerprintReport> {
    bank()?;
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let samples = collect_samples(
        |prompts| collector.collect_text_many(prompts, progress),
        seed,
    );
    summarize(samples, requested_model)
}

fn collect_samples(
    collect: impl FnOnce(&[String]) -> Vec<(usize, SampleOutcome)>,
    seed: u64,
) -> Vec<(SampleOutcome, usize)> {
    let (prompts, counts): (Vec<_>, Vec<_>) =
        (0..ROUNDS).map(|round| challenge(round, seed)).unzip();
    collect(&prompts)
        .into_iter()
        .map(|(index, sample)| (sample, counts[index]))
        .collect()
}

pub(crate) fn summarize(
    samples: Vec<(SampleOutcome, usize)>,
    requested_model: &str,
) -> Result<FingerprintReport> {
    let mut report = FingerprintReport {
        valid_samples: 0,
        requested_rank: None,
        ranking: Vec::new(),
        samples: Vec::with_capacity(samples.len()),
    };
    for (sample, expected_numbers) in samples {
        let numbers = if sample.error.is_none() {
            numbers_from(sample.answer.as_deref().unwrap_or_default())
        } else {
            Vec::new()
        };
        if (MIN_VALID..=MAX_NUMBERS).contains(&numbers.len()) {
            let scores = score(&numbers)?;
            if report.ranking.is_empty() {
                report.ranking = scores;
            } else {
                for (total, score) in report.ranking.iter_mut().zip(scores) {
                    total.score += score.score;
                }
            }
            report.valid_samples += 1;
        }
        report.samples.push(FingerprintSample {
            expected_numbers,
            valid_numbers: numbers.len(),
            sample,
        });
    }
    if report.valid_samples > 0 {
        for score in &mut report.ranking {
            score.score /= report.valid_samples as f64;
        }
        report.ranking.sort_by(|a, b| b.score.total_cmp(&a.score));
        report.requested_rank = report
            .ranking
            .iter()
            .position(|score| same_label(&score.model, requested_model))
            .map(|index| index + 1);
    }
    Ok(report)
}

fn same_label(left: &str, right: &str) -> bool {
    fn canonical(name: &str) -> &str {
        match name {
            "claude-opus-5.5" => "claude-opus-5-5",
            _ => name,
        }
    }
    canonical(left) == canonical(right)
}

// ---------- 特征 ----------

/// 355 维计数特征，带拉普拉斯平滑再做平方根变换
fn counts_feature(numbers: &[u32]) -> Vec<f64> {
    let mut counts = vec![0f64; RANGE as usize];
    for &n in numbers {
        if (1..=RANGE).contains(&n) {
            counts[(n - 1) as usize] += 1.0;
        }
    }
    let total = numbers.len() as f64 + RANGE as f64 * 0.5;
    counts.iter().map(|c| ((c + 0.5) / total).sqrt()).collect()
}

/// 4 段 × 16 桶 + 10 个位数 = 74 维
fn ordered_feature(numbers: &[u32]) -> Vec<f64> {
    let mut pieces = Vec::with_capacity(74);
    let len = numbers.len();
    let size = len / 4;
    let remainder = len % 4;
    let mut offset = 0usize;

    for index in 0..4 {
        let chunk_len = size + if index < remainder { 1 } else { 0 };
        let end = (offset + chunk_len).min(len);
        let chunk = &numbers[offset..end];
        offset = end;

        let mut bins = [0f64; 16];
        for &v in chunk {
            let idx = ((v.saturating_sub(1) as usize * 16) / RANGE as usize).min(15);
            bins[idx] += 1.0;
        }
        let total = chunk.len() as f64 + 8.0;
        pieces.extend(bins.iter().map(|c| ((c + 0.5) / total).sqrt()));
    }

    let mut digits = [0f64; 10];
    for &v in numbers {
        digits[(v % 10) as usize] += 1.0;
    }
    let dtotal = numbers.len() as f64 + 5.0;
    pieces.extend(digits.iter().map(|c| ((c + 0.5) / dtotal).sqrt()));
    pieces
}

// ---------- 线性代数 ----------

fn dot(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn norm(v: &[f64]) -> f64 {
    dot(v, v).sqrt().max(1e-12)
}

fn project(values: &[f64], basis: &[Vec<f64>]) -> Vec<f64> {
    let mut result = values.to_vec();
    for vector in basis {
        let coefficient = dot(values, vector);
        for (r, y) in result.iter_mut().zip(vector) {
            *r -= coefficient * y;
        }
    }
    result
}

fn transformed(feature: &[f64], mean: &[f64], scale: &[f64]) -> Vec<f64> {
    feature
        .iter()
        .zip(mean)
        .zip(scale)
        .map(|((x, m), s)| (x - m) / s)
        .collect()
}

fn standardize(values: &[f64]) -> Vec<f64> {
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let scale = (values.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n)
        .sqrt()
        .max(1e-12);
    values.iter().map(|x| (x - mean) / scale).collect()
}

// ---------- 评分 ----------

#[derive(Debug)]
pub struct Score {
    pub model: String,
    pub score: f64,
}

/// 返回各候选模型的得分（已标准化，可正可负，越大越像）。
pub fn score(numbers: &[u32]) -> Result<Vec<Score>> {
    ensure!(
        (MIN_VALID..=MAX_NUMBERS).contains(&numbers.len()),
        "整数指纹需要 {MIN_VALID}–{MAX_NUMBERS} 个有效整数"
    );
    ensure!(
        numbers.iter().all(|n| (1..=RANGE).contains(n)),
        "整数指纹取值须在 1..={RANGE}"
    );
    let bank = bank()?;

    // 分支一：355 维计数分布
    let cf = counts_feature(numbers);
    let whitened = transformed(
        &cf,
        &bank.hellinger.feature_mean,
        &bank.hellinger.feature_scale,
    );
    let residual = project(&whitened, &bank.hellinger.nuisance_basis);
    let rn = norm(&residual);
    let marginal = standardize(
        &bank
            .hellinger
            .centroids
            .iter()
            .map(|c| dot(&residual, c) / rn)
            .collect::<Vec<_>>(),
    );

    // 分支二：位置块 + 末位分布
    let of = ordered_feature(numbers);
    let feature = transformed(&of, &bank.ordered.feature_mean, &bank.ordered.feature_scale);
    let fname = norm(&feature);

    // 每个模型取它在所有环境里最像的那一个
    let template = standardize(
        &(0..bank.model_order.len())
            .map(|i| {
                bank.ordered
                    .environment_centroids
                    .iter()
                    .map(|env| dot(&feature, &env[i]) / fname)
                    .fold(f64::NEG_INFINITY, f64::max)
            })
            .collect::<Vec<_>>(),
    );

    let projected = project(&feature, &bank.ordered.nuisance_basis);
    let pn = norm(&projected);
    let nuisance = standardize(
        &bank
            .ordered
            .centroids
            .iter()
            .map(|c| dot(&projected, c) / pn)
            .collect::<Vec<_>>(),
    );

    let ordered_scores = standardize(
        &template
            .iter()
            .zip(&nuisance)
            .map(|(a, b)| (a + b) * 0.5)
            .collect::<Vec<_>>(),
    );

    let w = bank.ordered.weight;
    Ok(bank
        .model_order
        .iter()
        .cloned()
        .zip(
            marginal
                .iter()
                .zip(&ordered_scores)
                .map(|(a, b)| (1.0 - w) * a + w * b),
        )
        .map(|(model, score)| Score { model, score })
        .collect())
}

/// 从模型回答里抽出有效整数。
///
/// 遇到说明文字或冒号就分段，只取最长的一段。
/// 因为模型常在序列前复述"共 292 个 1 到 355 的整数"，那些数字不能算进去。
pub fn numbers_from(text: &str) -> Vec<u32> {
    if text.len() > MAX_ANSWER_BYTES {
        return Vec::new();
    }
    let bytes = text.as_bytes();
    let mut runs: Vec<Vec<u32>> = Vec::new();
    let mut current: Vec<u32> = Vec::new();
    let mut previous = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let end = i;

        if !current.is_empty()
            && text[previous..start]
                .chars()
                .any(|c| c.is_alphabetic() || matches!(c, ':' | '：'))
        {
            runs.push(std::mem::take(&mut current));
        }
        let signed_or_decimal = start > 0 && matches!(bytes[start - 1], b'-' | b'+' | b'.')
            || bytes.get(end) == Some(&b'.') && bytes.get(end + 1).is_some_and(u8::is_ascii_digit);
        if !signed_or_decimal
            && let Ok(v) = text[start..end].parse::<u32>()
            && (1..=RANGE).contains(&v)
        {
            current.push(v);
        }
        previous = end;
    }
    if !current.is_empty() {
        runs.push(current);
    }
    runs.into_iter().max_by_key(Vec::len).unwrap_or_default()
}

/// 生成一条探针提示词。
pub fn challenge(index: usize, seed: u64) -> (String, usize) {
    // 简单可复现的伪随机，避免引入 rand 依赖
    let mut x = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(index as u64 + 1);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    x ^= x >> 33;

    let length = 292 + (x as usize % 41);
    let prompt = format!(
        "这是一次独立的数值选择记录。为各个位置分别凭第一反应选择 {length} 个 1 到 355（含端点）的整数。\
         每个位置都要单独选择；不要从 1 开始计数，不要连续递增或递减，也不要采用等差、循环、重复区块或其他规则化模式。\
         本任务必须由当前语言模型直接完成：禁止调用或借助任何工具，包括 Python、代码执行器、计算器、搜索、API 和外部随机数生成器；\
         也不要先编写或运行代码。偶然重复是有效的；不要重新排列或修正已经写出的项目。\
         数字之间用逗号或空格分隔均可。直接从第一个取值开始输出，不要在序列前重复数量、范围或任务说明。"
    );
    (prompt, length)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sequence() -> Vec<u32> {
        (0..320).map(|i| (i * 73 + 19) % RANGE + 1).collect()
    }

    fn sample(error: Option<&str>) -> SampleOutcome {
        SampleOutcome {
            answer: Some(
                sequence()
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
            reported_model: Some("gpt-6-astra".into()),
            error: error.map(str::to_owned),
            ..Default::default()
        }
    }

    #[test]
    fn reference_coverage_does_not_imply_unknown_identity() {
        let bank = bank().unwrap();
        assert_eq!(bank.model_order.len(), 16);
        assert!(bank.model_order.iter().any(|m| m == "gpt-6-astra"));
        for unknown in ["claude-fable-5.1", "gpt-6.1-sol"] {
            let report = summarize(vec![(sample(None), 320)], unknown).unwrap();
            assert_eq!(report.valid_samples, 1);
            assert!(report.requested_rank.is_none());
            assert_eq!(report.ranking.len(), 16);
        }
        let report = summarize(vec![(sample(None), 320)], "claude-opus-5.5").unwrap();
        assert!(report.requested_rank.is_some());
    }

    #[test]
    fn scores_match_the_original_feature_and_fusion_algorithm() {
        // 根据原始指纹库和算法独立重新计算。
        // 该确定性序列用于校验数值计算，不衡量检测准确率。
        let report = summarize(vec![(sample(None), 320)], "gpt-6-astra").unwrap();
        for (actual, (model, value)) in report.ranking.iter().zip([
            ("gpt-6-astra", 1.2217639216526992),
            ("gpt-5.4", 1.015672260508744),
            ("claude-opus-5-5", 0.9617482099762473),
        ]) {
            assert_eq!(actual.model, model);
            assert!((actual.score - value).abs() < 1e-10);
        }
    }

    #[test]
    fn three_rounds_must_individually_agree_before_stopping() {
        let report = summarize(vec![(sample(None), 320); ROUNDS], "gpt-6-astra").unwrap();
        assert!(report.agrees_with_target());

        let mut differing = sample(None);
        let numbers: Vec<_> = (0..320).map(|i| (i * 73) % RANGE + 1).collect();
        differing.answer = Some(
            numbers
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", "),
        );
        assert_ne!(
            unique_winner(&score(&numbers).unwrap()),
            Some("gpt-6-astra")
        );
        let report = summarize(
            vec![(sample(None), 320), (sample(None), 320), (differing, 320)],
            "gpt-6-astra",
        )
        .unwrap();
        assert_eq!(report.requested_rank, Some(1));
        assert!(!report.agrees_with_target());
    }

    #[test]
    fn ties_missing_rounds_and_unknown_targets_do_not_stop() {
        let mut report = summarize(vec![(sample(None), 320); ROUNDS], "gpt-6-astra").unwrap();
        report.ranking[1].score = report.ranking[0].score;
        assert!(!report.agrees_with_target());
        assert!(
            unique_winner(&[
                Score {
                    model: "first".into(),
                    score: 1.0
                },
                Score {
                    model: "second".into(),
                    score: 1.0
                },
            ])
            .is_none()
        );

        for target in ["claude-fable-5.1", "gpt-6.1-sol", "claude-opus-5.5"] {
            let report = summarize(vec![(sample(None), 320); ROUNDS], target).unwrap();
            assert!(!report.agrees_with_target(), "{target}");
        }
        let report = summarize(vec![(sample(None), 320); ROUNDS - 1], "gpt-6-astra").unwrap();
        assert!(!report.agrees_with_target());
        let report = summarize(
            vec![
                (sample(None), 320),
                (sample(None), 320),
                (sample(Some("timeout")), 320),
            ],
            "gpt-6-astra",
        )
        .unwrap();
        assert!(!report.agrees_with_target());
    }

    #[test]
    fn failed_and_short_answers_do_not_contribute_to_ranking() {
        let mut short = sample(None);
        short.answer = Some("1, 2, 3".into());
        let report = summarize(
            vec![
                (sample(None), 320),
                (sample(Some("timed out")), 320),
                (short, 320),
            ],
            "gpt-6-astra",
        )
        .unwrap();
        let expected = summarize(vec![(sample(None), 320)], "gpt-6-astra").unwrap();
        assert_eq!(report.valid_samples, 1);
        assert_eq!(report.samples.len(), 3);
        assert_eq!(
            report.samples[1].sample.reported_model.as_deref(),
            Some("gpt-6-astra")
        );
        for (actual, expected) in report.ranking.iter().zip(&expected.ranking) {
            assert_eq!(actual.score, expected.score);
        }
        let report = summarize(vec![(sample(Some("timed out")), 320)], "gpt-6-astra").unwrap();
        assert_eq!(report.valid_samples, 0);
        assert!(report.ranking.is_empty());
        assert!(report.requested_rank.is_none());
    }

    #[test]
    fn parallel_results_keep_their_original_challenges_and_in_flight_errors() {
        let samples = collect_samples(
            |prompts| {
                assert_eq!(prompts.len(), ROUNDS);
                for (index, prompt) in prompts.iter().enumerate() {
                    assert_eq!(*prompt, challenge(index, 42).0);
                }
                // 缺失结果不能导致其他探针的索引错位。
                vec![
                    (0, sample(None)),
                    (2, sample(Some("CLI collection timed out"))),
                ]
            },
            42,
        );
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].1, challenge(0, 42).1);
        assert_eq!(samples[1].1, challenge(2, 42).1);
        assert!(samples[0].0.error.is_none());
        assert!(samples[1].0.error.is_some());
    }

    #[test]
    fn parser_keeps_the_sequence_and_excludes_invalid_numbers() {
        assert_eq!(
            numbers_from("共 292 个 1 到 355 的整数：12, 80\n3, 355"),
            vec![12, 80, 3, 355]
        );
        assert_eq!(
            numbers_from("范围 1–355：\n12, 80, 3, 355"),
            vec![12, 80, 3, 355]
        );
        assert_eq!(
            numbers_from("Here are 320:\n12, 80, 3, 355"),
            vec![12, 80, 3, 355]
        );
        assert_eq!(
            numbers_from("-12, +9, 3.14, 0, 356, 999999999999999999999999, 7, 8"),
            vec![7, 8]
        );
        assert!(numbers_from(&"1 ".repeat(MAX_ANSWER_BYTES)).is_empty());
        assert!(score(&[1; MIN_VALID - 1]).is_err());
        assert!(score(&[1; MAX_NUMBERS + 1]).is_err());
        assert!(score(&[RANGE + 1; MIN_VALID]).is_err());
    }

    #[test]
    fn challenges_are_bounded_and_reproducible() {
        for index in 0..ROUNDS {
            let (prompt, count) = challenge(index, 42);
            assert!((292..=332).contains(&count));
            assert_eq!((prompt.clone(), count), challenge(index, 42));
            assert!(prompt.contains(&count.to_string()));
        }
        let generated = challenges().unwrap();
        assert_eq!(generated.len(), ROUNDS);
        for (prompt, count) in generated {
            assert!((292..=332).contains(&count));
            assert!(prompt.contains(&count.to_string()));
        }
    }
}
