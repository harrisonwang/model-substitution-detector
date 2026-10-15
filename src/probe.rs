//! 能力评测（题库来自 HoneyMeta/ccfingerprint）。
//!
//! 让模型答有确定答案的题，本地按确定性规则判分。抓的是"明显变弱"。
//! 题库内嵌在二进制里，运行时不需要联网。

use anyhow::Result;
use serde::Deserialize;
use std::sync::OnceLock;

const DATASET: &str = include_str!("../assets/ccfp-dataset.json");

#[derive(Deserialize)]
struct Dataset {
    capability: Vec<Probe>,
}

#[derive(Deserialize)]
pub struct Probe {
    pub id: String,
    pub tier: u32,
    pub weight: u32,
    pub prompt_zh: String,
    pub check: Check,
}

#[derive(Deserialize)]
pub struct Check {
    #[serde(default)]
    reject: Vec<String>,
    #[serde(default)]
    case_sensitive: bool,
    exact: Option<String>,
    numeric: Option<f64>,
    contains: Option<String>,
    any: Option<Vec<String>>,
}

static PROBES: OnceLock<Result<Vec<Probe>, String>> = OnceLock::new();

pub fn probes() -> Result<&'static [Probe]> {
    let cached = PROBES.get_or_init(|| {
        let ds: Dataset =
            serde_json::from_str(DATASET).map_err(|e| format!("解析题库失败: {e}"))?;
        Ok(ds.capability)
    });
    match cached {
        Ok(p) => Ok(p),
        Err(e) => anyhow::bail!("{e}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    Fail,
    /// 证据不足：空回答或规则无法判定
    Unknown,
}

fn norm(s: &str) -> String {
    s.to_lowercase().trim().to_string()
}

/// ASCII 关键词按词边界匹配（"no" 不该命中 "not"），CJK 按子串匹配。
fn has_token(answer: &str, token: &str) -> bool {
    let t = norm(token);
    if t.is_empty() {
        return false;
    }
    let ascii_word = t
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '\'' | '-'));
    if !ascii_word {
        return answer.contains(&t);
    }

    let bytes = answer.as_bytes();
    let tlen = t.len();
    let mut start = 0usize;
    while start <= answer.len() {
        let Some(pos) = answer[start..].find(&t) else {
            break;
        };
        let abs = start + pos;
        let before_ok = abs == 0 || !bytes[abs - 1].is_ascii_alphanumeric();
        let after = abs + tlen;
        let after_ok = after >= bytes.len() || !bytes[after].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

/// 只接受一个数值；逗号必须符合每三位一组的标准分隔格式。
fn extract_number(s: &str) -> Option<f64> {
    let start = s.find(|c: char| c.is_ascii_digit())?;
    let end = s.rfind(|c: char| c.is_ascii_digit())? + 1;
    let prefix = s[..start].trim_end();
    let sign = if prefix.ends_with('-') { "-" } else { "" };
    if s[..start].ends_with(['.', ',']) {
        return None;
    }
    let mut parts = s[start..end].split('.');
    let integer = parts.next()?;
    let fraction = parts.next();
    if parts.next().is_some()
        || fraction.is_some_and(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let groups: Vec<_> = integer.split(',').collect();
    if groups
        .iter()
        .any(|group| group.is_empty() || !group.bytes().all(|b| b.is_ascii_digit()))
        || groups.len() > 1
            && (groups[0].len() > 3 || groups[1..].iter().any(|group| group.len() != 3))
    {
        return None;
    }
    let number = format!("{sign}{}", s[start..end].replace(',', ""));
    number.parse::<f64>().ok().filter(|value| value.is_finite())
}

pub fn judge(answer: &str, check: &Check) -> Outcome {
    let a = norm(answer);
    if a.is_empty() {
        return Outcome::Unknown;
    }
    if !check.reject.is_empty() && check.reject.iter().any(|k| has_token(&a, k)) {
        return Outcome::Fail;
    }
    if let Some(exact) = &check.exact {
        let (answer, expected) = if check.case_sensitive {
            (answer, exact.clone())
        } else {
            (a.as_str(), norm(exact))
        };
        let cleaned = answer.trim_matches(|c: char| c.is_whitespace() || "\"'`.,:;".contains(c));
        return if cleaned == expected {
            Outcome::Pass
        } else {
            Outcome::Fail
        };
    }
    if let Some(n) = check.numeric {
        return match extract_number(&a) {
            Some(v) if (v - n).abs() < 1e-6 => Outcome::Pass,
            _ => Outcome::Fail,
        };
    }
    if let Some(c) = &check.contains {
        return if a.contains(&norm(c)) {
            Outcome::Pass
        } else {
            Outcome::Fail
        };
    }
    if let Some(any) = &check.any {
        return if any.iter().any(|k| has_token(&a, k)) {
            Outcome::Pass
        } else {
            Outcome::Fail
        };
    }
    Outcome::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(json: &str) -> Check {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn numeric_and_reject() {
        let c = check(r#"{"contains":"ZX9-QY7-KP3","reject":["ZX9-QY7-KP2"]}"#);
        assert_eq!(judge("ZX9-QY7-KP3", &c), Outcome::Pass);
        assert_eq!(judge("ZX9-QY7-KP2", &c), Outcome::Fail);
    }

    #[test]
    fn exact_matching() {
        let c = check(r#"{"exact":"fox"}"#);
        assert_eq!(judge("fox", &c), Outcome::Pass);
        assert_eq!(judge("  \"fox\".  ", &c), Outcome::Pass);
        assert_eq!(judge("quick brown fox", &c), Outcome::Fail);
    }

    #[test]
    fn number_extraction() {
        let c = check(r#"{"numeric":259}"#);
        assert_eq!(judge("259", &c), Outcome::Pass);
        assert_eq!(judge("答案是 259。", &c), Outcome::Pass);
        assert_eq!(judge("1,234", &c), Outcome::Fail);
    }

    #[test]
    fn numeric_answers_cannot_hide_alternatives_or_join_separate_numbers() {
        let seven = check(r#"{"numeric":7}"#);
        for answer in ["7或8", "7,8", "7 8", "7/8", "7.0.0", ".7", "-7"] {
            assert_eq!(judge(answer, &seven), Outcome::Fail, "{answer}");
        }
        let four_hundred = check(r#"{"numeric":400}"#);
        for answer in ["4 00", "4,00", "40,0", "400 或 401", "400,000"] {
            assert_eq!(judge(answer, &four_hundred), Outcome::Fail, "{answer}");
        }
        let grouped = check(r#"{"numeric":1234.5}"#);
        for answer in ["1,234.5", "答案是 1,234.50。", "1234.5"] {
            assert_eq!(judge(answer, &grouped), Outcome::Pass, "{answer}");
        }
        assert_eq!(judge("12,34.5", &grouped), Outcome::Fail);
    }

    #[test]
    fn embedded_rules_accept_weekday_aliases_and_require_exact_case() {
        let probes = probes().unwrap();
        let rule = |id: &str| &probes.iter().find(|probe| probe.id == id).unwrap().check;
        for answer in [
            "星期日",
            "星期天",
            "周日",
            "周天",
            "礼拜日",
            "礼拜天",
            "Sunday",
        ] {
            assert_eq!(
                judge(answer, rule("weekday_calc")),
                Outcome::Pass,
                "{answer}"
            );
        }
        assert_eq!(
            judge("A-N-A-N-A-B", rule("string_transform")),
            Outcome::Pass
        );
        assert_eq!(
            judge("a-n-a-n-a-b", rule("string_transform")),
            Outcome::Fail
        );
        assert_eq!(judge("ZX9-QY7-KP3", rule("needle")), Outcome::Pass);
        for answer in [
            "zx9-qy7-kp3",
            "密钥是 ZX9-QY7-KP3",
            "ZX9-QY7-KP3，请使用此密钥",
        ] {
            assert_eq!(judge(answer, rule("needle")), Outcome::Fail, "{answer}");
        }
    }

    #[test]
    fn ascii_word_boundary() {
        assert!(!has_token("i do not know", "no"));
        assert!(has_token("no idea", "no"));
        assert!(has_token("骑士", "骑士"));
    }
}
