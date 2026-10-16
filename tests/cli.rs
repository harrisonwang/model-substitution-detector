//! 端到端测试使用临时客户端可执行文件，不调用真实模型。
mod support;

use std::collections::BTreeSet;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "msd-cli 空格 &-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::create_dir(path.join("Applications")).unwrap();
        fs::create_dir(path.join("prompts")).unwrap();
        for name in ["codex", "claude"] {
            support::install_client(&path.join(format!("{name}{}", std::env::consts::EXE_SUFFIX)));
        }
        Self(path)
    }

    fn run(&self, args: &[&str], mode: &str) -> Output {
        self.run_client_modes(args, mode, mode)
    }

    fn run_client_modes(&self, args: &[&str], desktop_mode: &str, cli_mode: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_msd"))
            .current_dir(&self.0)
            .env_clear()
            .envs(
                ["SystemRoot", "TEMP", "TMP"]
                    .into_iter()
                    .filter_map(|key| std::env::var_os(key).map(|value| (key, value))),
            )
            .env("PATH", &self.0)
            .env("CODEX_HOME", self.0.join("codex-home"))
            .env("CLAUDE_CONFIG_DIR", self.0.join("claude-home"))
            // 避免发现真实安装的应用或发起付费模型请求。
            .env("MSD_APPLICATION_DIRS", self.0.join("Applications"))
            .env("MSD_CALLS", self.0.join("calls"))
            .env("MSD_LAUNCHES", self.0.join("launches"))
            .env("MSD_PROMPTS", self.0.join("prompts"))
            .env("MSD_MODE", cli_mode)
            .env("MSD_DESKTOP_MODE", desktop_mode)
            .env("MSD_CLI_MODE", cli_mode)
            .args(args)
            .output()
            .unwrap()
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.0.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn launches(&self) -> Vec<String> {
        fs::read_to_string(self.0.join("launches"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn prompts(&self) -> Vec<String> {
        fs::read_dir(self.0.join("prompts"))
            .unwrap()
            .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect()
    }

    fn assert_no_repeated_questions(&self, capability: usize, knowledge: usize) {
        let prompts = self.prompts();
        let questions: Vec<_> = prompts
            .iter()
            .filter(|prompt| !prompt.contains("独立的数值选择记录"))
            .collect();
        assert_eq!(questions.len(), capability + knowledge);
        assert_eq!(
            questions.iter().copied().collect::<BTreeSet<_>>().len(),
            questions.len(),
            "capability or knowledge question was repeated"
        );
        assert_eq!(
            questions
                .iter()
                .filter(|prompt| prompt.contains("Closed-book numerical recall"))
                .count(),
            knowledge
        );
    }

    fn assert_only_desktop_invoked(&self) {
        let launches = self.launches();
        assert!(!launches.is_empty());
        assert!(
            launches.iter().all(|entry| entry.starts_with("desktop:")),
            "backup client must not even be checked for its version: {launches:?}"
        );
    }

    fn assert_rounds(&self, capability: usize, integer: usize, knowledge: usize) {
        let calls = self.calls();
        assert_eq!(calls.len(), capability + integer + knowledge, "{calls:?}");
        for (stage, expected) in [
            ("capability", capability),
            ("integer", integer),
            ("knowledge", knowledge),
        ] {
            assert_eq!(
                calls.iter().filter(|value| value.as_str() == stage).count(),
                expected,
                "wrong request count for {stage}: {calls:?}"
            );
        }
    }

    fn install_app_engine(&self, keep_cli: bool) -> PathBuf {
        #[cfg(not(windows))]
        let engine = self.0.join(
            "Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex",
        );
        #[cfg(windows)]
        let engine = self.0.join("Applications/ChatGPT/resources/codex.exe");
        fs::create_dir_all(engine.parent().unwrap()).unwrap();
        if keep_cli {
            fs::copy(
                self.0
                    .join(format!("codex{}", std::env::consts::EXE_SUFFIX)),
                &engine,
            )
            .unwrap();
        } else {
            fs::rename(
                self.0
                    .join(format!("codex{}", std::env::consts::EXE_SUFFIX)),
                &engine,
            )
            .unwrap();
        }
        engine
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn assert_summary(output: &Output, expected: &str, exit_code: i32) {
    assert_eq!(output.status.code(), Some(exit_code));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("{expected}\n")
    );
    assert!(
        output.stderr.is_empty(),
        "default stderr must be empty: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_four_checks(text: &str) {
    for check in ["报文取证", "能力评测", "整数指纹", "知识边界指纹"] {
        assert!(
            text.lines()
                .any(|line| line.trim_start().starts_with(&format!("{check}："))),
            "missing {check}: {text}"
        );
    }
    assert!(!text.contains("当前版本未实现"), "{text}");
}

#[test]
fn verbosity_changes_only_presentation_and_shows_all_four_checks() {
    let fixture = Fixture::new();
    let expected_summary = "gpt-6.1-sol ✓";
    let output = fixture.run(&["-m", "gpt-6.1-sol"], "matching");
    assert_summary(&output, expected_summary, 0);
    fixture.assert_rounds(9, 3, 0);
    for (index, flag) in ["-v", "--verbose"].iter().enumerate() {
        let verbose = fixture.run(&["-m", "gpt-6.1-sol", flag], "matching");
        let text = String::from_utf8_lossy(&verbose.stdout);
        assert_four_checks(&text);
        assert!(text.contains("Codex CLI"), "{text}");
        assert!(
            text.contains("12/12") && text.contains("9/9 题答对"),
            "{text}"
        );
        assert!(text.contains("知识边界指纹：未运行"), "{text}");
        assert!(
            text.lines().last().unwrap().ends_with(expected_summary),
            "{text}"
        );
        assert!(verbose.status.success());
        assert!(verbose.stderr.is_empty(), "{:?}", verbose.stderr);
        fixture.assert_rounds(9 * (index + 2), 3 * (index + 2), 0);
    }
    assert!(!fixture.0.join(".msd").exists());
}

#[test]
fn supported_models_run_integer_and_knowledge_checks_in_memory() {
    for model in ["gpt-6-astra", "claude-fable-5.1"] {
        let fixture = Fixture::new();
        let output = fixture.run(&["-m", model], "matching");
        assert_summary(&output, &format!("{model} ✓"), 0);
        fixture.assert_rounds(9, 3, 30);
        fixture.assert_no_repeated_questions(9, 30);
        let verbose = fixture.run(&["-m", model, "-v"], "matching");
        let text = String::from_utf8_lossy(&verbose.stdout);
        assert!(verbose.status.success(), "{text}");
        assert_four_checks(&text);
        assert!(
            text.contains("42/42") && text.contains("9/9 题答对"),
            "{text}"
        );
        assert!(!text.contains("整数指纹：未运行"), "{text}");
        assert!(!text.contains("知识边界指纹：未运行"), "{text}");
        assert!(text.contains("30/30"), "{text}");
        if model.starts_with("claude-") {
            assert!(text.contains("Claude CLI"), "{text}");
            assert!(!text.contains("Codex CLI"), "{text}");
        }
        fixture.assert_rounds(18, 6, 60);
        assert!(!fixture.0.join(".msd").exists());
    }
}

#[test]
fn matching_initial_evidence_finishes_after_nine_calls() {
    let fixture = Fixture::new();
    let output = fixture.run(&["-m", "gpt-6-astra", "-v"], "confident");
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(0),
        "{text}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_four_checks(&text);
    assert!(text.trim_end().ends_with("gpt-6-astra ✓"), "{text}");
    fixture.assert_rounds(2, 3, 4);
    fixture.assert_no_repeated_questions(2, 4);
    let prompts = fixture.prompts();
    for required in ["print(sum(i * i", "n 除以 7 余 3"] {
        assert!(prompts.iter().any(|prompt| prompt.contains(required)));
    }
    let bank: serde_json::Value =
        serde_json::from_str(include_str!("../assets/knowledge-reference.json")).unwrap();
    let reference = bank["references"]
        .as_array()
        .unwrap()
        .iter()
        .find(|reference| reference["model"] == "gpt-6-astra")
        .unwrap();
    for index in [0, 8, 12, 20] {
        let expected = reference["probes"][index]["prompt"].as_str().unwrap();
        assert!(
            prompts.iter().any(|prompt| prompt.ends_with(expected)),
            "missing initial reference {index}"
        );
    }
    assert!(!fixture.0.join(".msd").exists());
}

#[test]
fn missing_target_fingerprint_coverage_cannot_end_after_initial_sampling() {
    for (model, knowledge) in [("gpt-6.1-sol", 0), ("claude-fable-5.1", 30)] {
        let fixture = Fixture::new();
        let output = fixture.run(&["-m", model], "confident");
        assert_summary(&output, &format!("{model} ✓"), 0);
        fixture.assert_rounds(9, 3, knowledge);
        fixture.assert_no_repeated_questions(9, knowledge);
    }
}

#[test]
fn additional_sampling_stops_after_the_first_failed_or_mismatched_batch() {
    for (mode, summary, code) in [
        ("additional_mismatch", "gpt-6-astra ✗ → gpt-6-luna", 2),
        ("additional_failure", "gpt-6-astra ?", 1),
    ] {
        let fixture = Fixture::new();
        let output = fixture.run(&["-m", "gpt-6-astra"], mode);
        assert_summary(&output, summary, code);
        let count = fixture.calls().len();
        assert!(
            (10..=12).contains(&count),
            "only the stopped batch may be in flight: {count}"
        );
        let prompts = fixture.prompts();
        let questions: Vec<_> = prompts
            .iter()
            .filter(|prompt| !prompt.contains("独立的数值选择记录"))
            .collect();
        assert_eq!(
            questions.iter().copied().collect::<BTreeSet<_>>().len(),
            questions.len()
        );
    }
}

#[test]
fn declarations_are_checked_across_all_methods() {
    for mode in ["mismatch", "integer_mismatch", "knowledge_mismatch"] {
        let fixture = Fixture::new();
        let expected = "gpt-6-astra ✗ → gpt-6-luna";
        let output = fixture.run(&["-m", "gpt-6-astra"], mode);
        assert_summary(&output, expected, 2);
        let initial_calls = fixture.calls().len();
        if mode == "mismatch" {
            fixture.assert_rounds(1, 0, 0);
        } else {
            assert!((2..=9).contains(&initial_calls));
            assert!(fixture.calls().iter().any(|stage| stage
                == if mode == "integer_mismatch" {
                    "integer"
                } else {
                    "knowledge"
                }));
        }
        let verbose = fixture.run(&["-m", "gpt-6-astra", "-v"], mode);
        let text = String::from_utf8_lossy(&verbose.stdout);
        assert_four_checks(&text);
        assert_eq!(verbose.status.code(), Some(2), "{text}");
        assert!(text.lines().last().unwrap().ends_with(expected), "{text}");
        let additional_calls = fixture.calls().len() - initial_calls;
        assert!((1..=9).contains(&additional_calls));
        if mode == "mismatch" {
            assert_eq!(additional_calls, 1);
        }
    }
}

#[test]
fn wrong_capability_answers_and_uncalibrated_references_are_only_observations() {
    let fixture = Fixture::new();
    let output = fixture.run(&["-m", "gpt-6-astra"], "bad_capability");
    assert_summary(&output, "gpt-6-astra ✓", 0);
    fixture.assert_rounds(9, 3, 30);
    let verbose = fixture.run(&["-m", "gpt-6-astra", "-v"], "bad_capability");
    let text = String::from_utf8_lossy(&verbose.stdout);
    assert!(text.contains("8/9 题答对"), "{text}");
    assert!(verbose.status.success(), "{text}");
}

#[test]
fn invalid_sample_declarations_do_not_become_mismatch_evidence() {
    let fixture = Fixture::new();
    let output = fixture.run(&["-m", "gpt-6-astra"], "invalid_mismatch");
    assert_summary(&output, "gpt-6-astra ?", 1);
    assert!(fixture.calls().iter().any(|stage| stage == "integer"));
    assert!(fixture.calls().len() <= 9);
}

#[test]
fn explicit_model_reroute_stops_after_one_request_and_keeps_details_private() {
    let fixture = Fixture::new();
    let output = fixture.run(&["-m", "gpt-6-astra"], "model_rerouted");
    assert_summary(&output, "gpt-6-astra ✗ → ?", 2);
    fixture.assert_rounds(1, 0, 0);

    let verbose = fixture.run(&["-m", "gpt-6-astra", "-v"], "model_rerouted");
    let text = String::from_utf8_lossy(&verbose.stdout);
    let errors = String::from_utf8_lossy(&verbose.stderr);
    assert_eq!(verbose.status.code(), Some(2), "{text}");
    assert_four_checks(&text);
    for check in ["能力评测", "整数指纹", "知识边界指纹"] {
        assert!(text.contains(&format!("{check}：未运行")), "{text}");
    }
    assert!(
        text.lines().last().unwrap().ends_with("gpt-6-astra ✗ → ?"),
        "{text}"
    );
    for private_detail in ["Model rerouted:", "private-reroute-detail"] {
        assert!(!text.contains(private_detail), "{text}");
        assert!(!errors.contains(private_detail), "{errors}");
    }
    fixture.assert_rounds(2, 0, 0);
}

#[test]
fn later_reroute_keeps_inflight_results_and_stops_before_additional_batches() {
    let fixture = Fixture::new();
    let output = fixture.run(&["-m", "gpt-6-astra", "-v"], "late_rerouted");
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(2), "{text}");
    assert!(text.contains("能力评测：失败"), "{text}");
    assert_four_checks(&text);
    assert!(text.trim_end().ends_with("gpt-6-astra ✗ → ?"), "{text}");
    let calls = fixture.calls();
    assert!((2..=9).contains(&calls.len()));
    assert_eq!(
        calls
            .iter()
            .filter(|stage| stage.as_str() == "capability")
            .count(),
        2
    );
}

#[test]
fn missing_declarations_cannot_produce_a_clear_conclusion() {
    for model in ["gpt-6-astra", "claude-fable-5.1"] {
        let fixture = Fixture::new();
        let output = fixture.run(&["-m", model], "no_declarations");
        assert_summary(&output, &format!("{model} ?"), 1);
        fixture.assert_rounds(9, 3, 30);
    }
}

#[test]
fn chatgpt_bundled_engine_works_without_a_path_cli() {
    let fixture = Fixture::new();
    fixture.install_app_engine(false);
    let output = fixture.run(&["-m", "gpt-6-astra", "-v"], "matching");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("ChatGPT"), "{text}");
    assert!(text.lines().last().unwrap().ends_with("gpt-6-astra ✓"));
    fixture.assert_rounds(9, 3, 30);
}

#[test]
#[cfg(unix)]
fn app_engine_and_symlinked_cli_are_sampled_once() {
    let fixture = Fixture::new();
    let engine = fixture.install_app_engine(false);
    symlink(
        engine,
        fixture
            .0
            .join(format!("codex{}", std::env::consts::EXE_SUFFIX)),
    )
    .unwrap();
    let output = fixture.run(&["-m", "gpt-6-astra"], "matching");
    assert_summary(&output, "gpt-6-astra ✓", 0);
    fixture.assert_rounds(9, 3, 30);
}

#[test]
fn app_engine_is_preferred_without_starting_a_separate_cli() {
    let fixture = Fixture::new();
    fixture.install_app_engine(true);
    let output = fixture.run(&["-m", "gpt-6-astra", "-v"], "matching");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("ChatGPT"), "{text}");
    assert!(!text.contains("Codex CLI"), "{text}");
    fixture.assert_rounds(9, 3, 30);
    fixture.assert_only_desktop_invoked();
}

#[test]
fn unavailable_app_falls_back_to_cli_without_retaining_failed_findings() {
    for mode in ["failure", "incompatible"] {
        let fixture = Fixture::new();
        fixture.install_app_engine(true);
        let output = fixture.run_client_modes(&["-m", "gpt-6-astra", "-v"], mode, "matching");
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success(), "{mode}: {text}");
        assert!(text.contains("Codex CLI"), "{text}");
        assert!(!text.contains("ChatGPT"), "{text}");
        assert_four_checks(&text);
        assert!(text.trim_end().ends_with("gpt-6-astra ✓"), "{text}");
        assert!(output.stderr.is_empty(), "{:?}", output.stderr);

        let launches = fixture.launches();
        let desktop_launches = if mode == "failure" {
            vec![
                "desktop:--version",
                "desktop:exec",
                "desktop:features",
                "desktop:exec",
            ]
        } else {
            vec!["desktop:--version", "desktop:exec"]
        };
        assert_eq!(
            &launches[..desktop_launches.len()],
            desktop_launches.as_slice()
        );
        assert_eq!(launches[desktop_launches.len()], "cli:--version");
        assert!(
            launches[desktop_launches.len()..]
                .iter()
                .all(|entry| entry.starts_with("cli:")),
            "{launches:?}"
        );
        let calls = fixture.calls();
        if mode == "failure" {
            assert_eq!(calls.first().map(String::as_str), Some("failed"));
            assert_eq!(calls.len(), 43, "{calls:?}");
            for (stage, count) in [("capability", 9), ("integer", 3), ("knowledge", 30)] {
                assert_eq!(
                    calls.iter().filter(|call| call.as_str() == stage).count(),
                    count
                );
            }
        } else {
            fixture.assert_rounds(9, 3, 30);
        }
    }
}

#[test]
fn all_unavailable_clients_keep_their_failure_reasons() {
    let fixture = Fixture::new();
    fixture.install_app_engine(true);
    let output = fixture.run_client_modes(&["-m", "gpt-6-astra", "-v"], "failure", "failure");
    let text = String::from_utf8_lossy(&output.stdout);
    let errors = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{text}");
    assert!(
        text.contains("ChatGPT") && text.contains("Codex CLI"),
        "{text}"
    );
    assert!(
        errors.contains("ChatGPT") && errors.contains("Codex CLI"),
        "{errors}"
    );
    assert_eq!(text.matches("首次采集失败").count(), 2, "{text}");
    assert!(text.trim_end().ends_with("gpt-6-astra ?"), "{text}");
    assert_eq!(fixture.calls(), ["failed", "failed"]);
    assert_eq!(
        fixture.launches(),
        [
            "desktop:--version",
            "desktop:exec",
            "desktop:features",
            "desktop:exec",
            "cli:--version",
            "cli:exec",
            "cli:features",
            "cli:exec",
        ]
    );
}

#[test]
fn app_results_and_later_failures_never_switch_to_a_matching_backup() {
    for (mode, expected, exit_code) in [
        ("mismatch", "gpt-6-astra ✗ → gpt-6-luna", 2),
        ("integer_mismatch", "gpt-6-astra ✗ → gpt-6-luna", 2),
        ("knowledge_mismatch", "gpt-6-astra ✗ → gpt-6-luna", 2),
        ("model_rerouted", "gpt-6-astra ✗ → ?", 2),
        ("late_rerouted", "gpt-6-astra ✗ → ?", 2),
        ("bad_capability", "gpt-6-astra ✓", 0),
        ("no_declarations", "gpt-6-astra ?", 1),
        ("late_failure", "gpt-6-astra ?", 1),
        ("invalid_mismatch", "gpt-6-astra ?", 1),
        ("empty_answer", "gpt-6-astra ?", 1),
        ("tool_event", "gpt-6-astra ?", 1),
    ] {
        let fixture = Fixture::new();
        fixture.install_app_engine(true);
        let output = fixture.run_client_modes(&["-m", "gpt-6-astra", "-v"], mode, "matching");
        let text = String::from_utf8_lossy(&output.stdout);
        assert_eq!(output.status.code(), Some(exit_code), "{mode}: {text}");
        assert!(text.trim_end().ends_with(expected), "{mode}: {text}");
        assert!(text.contains("ChatGPT"), "{text}");
        assert!(!text.contains("Codex CLI"), "{text}");
        assert_four_checks(&text);
        fixture.assert_only_desktop_invoked();
        match mode {
            "mismatch" | "model_rerouted" | "empty_answer" | "tool_event" => {
                fixture.assert_rounds(1, 0, 0)
            }
            "late_rerouted" | "late_failure" => {
                let calls = fixture.calls();
                assert!((2..=9).contains(&calls.len()), "{calls:?}");
                assert_eq!(
                    calls
                        .iter()
                        .filter(|stage| stage.as_str() == "capability")
                        .count(),
                    2
                );
            }
            "integer_mismatch" | "knowledge_mismatch" | "invalid_mismatch" => {
                let calls = fixture.calls();
                assert!((2..=9).contains(&calls.len()), "{calls:?}");
                let expected_stage = if mode == "knowledge_mismatch" {
                    "knowledge"
                } else {
                    "integer"
                };
                assert!(
                    calls.iter().any(|stage| stage == expected_stage),
                    "{calls:?}"
                );
            }
            _ => fixture.assert_rounds(9, 3, 30),
        }
    }
}

#[test]
fn api_placeholder_does_not_fall_back_to_local_clients() {
    let fixture = Fixture::new();
    let args = ["--api", "https://example.invalid/v1", "-m", "gpt-6-astra"];
    let output = fixture.run(&args, "matching");
    assert_summary(&output, "gpt-6-astra ?", 1);
    let verbose = fixture.run(&[args.as_slice(), &["-v"]].concat(), "matching");
    assert_eq!(verbose.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&verbose.stderr).contains("尚未实现"));
    assert!(fixture.calls().is_empty());
}

#[test]
fn help_is_small_and_removed_commands_do_not_invoke_a_client() {
    let fixture = Fixture::new();
    let help = fixture.run(&["--help"], "matching");
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    assert!(help.contains("--model") && help.contains("--api") && help.contains("--verbose"));
    assert!(!help.contains("gpt-6-astra"), "{help}");
    for old in ["--all", "--probe", "--fp", "baseline", "--store"] {
        assert!(!help.contains(old), "{help}");
        assert_eq!(fixture.run(&[old], "matching").status.code(), Some(1));
    }
    assert!(fixture.calls().is_empty());
}

#[test]
fn runtime_failures_are_concise_and_verbose_explains_the_error() {
    for mode in ["failure", "late_failure"] {
        let fixture = Fixture::new();
        let output = fixture.run(&["-m", "gpt-6-astra"], mode);
        assert_summary(&output, "gpt-6-astra ?", 1);
        if mode == "failure" {
            assert_eq!(fixture.calls().len(), 1);
        } else {
            assert!(fixture.calls().len() <= 9);
            assert_eq!(
                fixture
                    .calls()
                    .iter()
                    .filter(|stage| stage.as_str() == "capability")
                    .count(),
                2
            );
        }
        let verbose = fixture.run(&["-m", "gpt-6-astra", "-v"], mode);
        let text = String::from_utf8_lossy(&verbose.stdout);
        assert_four_checks(&text);
        assert_eq!(verbose.status.code(), Some(1), "{text}");
        assert!(
            !verbose.stderr.is_empty(),
            "verbose output needs an error reason"
        );
        assert!(text.lines().last().unwrap().ends_with("gpt-6-astra ?"));
        if mode == "failure" {
            assert!(text.contains("能力评测：未运行"), "{text}");
            assert!(text.contains("整数指纹：未运行"), "{text}");
            assert!(text.contains("知识边界指纹：未运行"), "{text}");
            assert_eq!(fixture.calls().len(), 2);
        } else {
            assert!(fixture.calls().len() <= 18);
            assert_eq!(
                fixture
                    .calls()
                    .iter()
                    .filter(|stage| stage.as_str() == "capability")
                    .count(),
                4
            );
        }
    }
}

#[test]
fn missing_client_does_not_invoke_another_model_family() {
    let fixture = Fixture::new();
    fs::remove_file(
        fixture
            .0
            .join(format!("codex{}", std::env::consts::EXE_SUFFIX)),
    )
    .unwrap();
    let output = fixture.run(&["-m", "gpt-6-astra"], "matching");
    assert_summary(&output, "gpt-6-astra ?", 1);
    assert!(fixture.calls().is_empty());
}

#[test]
fn missing_model_does_not_invoke_a_client() {
    let fixture = Fixture::new();
    let output = fixture.run(&[], "matching");
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--model <MODEL>"));
    assert!(fixture.calls().is_empty());
}

#[test]
fn ambiguous_short_model_names_do_not_route_to_the_wrong_client() {
    let fixture = Fixture::new();
    for model in ["opus", "fable", "", "gpt-", "claude-"] {
        assert_eq!(
            fixture.run(&["-m", model], "matching").status.code(),
            Some(1)
        );
    }
    assert!(fixture.calls().is_empty());
}
