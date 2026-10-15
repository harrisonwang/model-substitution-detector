use msd::codex::discover;
use msd::local::{LocalPaths, Observation, Source, inspect};
use serde_json::json;
use std::fs::{self, File, FileTimes};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture {
    root: PathBuf,
    paths: LocalPaths,
}

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "msd-local-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self {
            paths: LocalPaths {
                codex_home: root.join("codex"),
                claude_home: root.join("claude"),
                applications: vec![root.join("Applications")],
                executable_dirs: vec![root.join("bin")],
            },
            root,
        }
    }

    fn log(&self, relative: &str, entries: &[serde_json::Value], modified: u64) {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let content = entries
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, content + "\n").unwrap();
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(modified)))
            .unwrap();
    }

    fn observation(&self, source: Source) -> Observation {
        inspect(&self.paths)
            .into_iter()
            .find(|item| item.source == source)
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn codex_meta(source: serde_json::Value, originator: &str, id: &str) -> serde_json::Value {
    json!({"type":"session_meta","payload":{"id":id,"source":source,"originator":originator}})
}

fn selected(model: &str) -> serde_json::Value {
    json!({"type":"turn_context","timestamp":"2026-10-14T02:00:00Z","payload":{"model":model}})
}

fn response(entrypoint: &str, model: &str, sidechain: bool) -> serde_json::Value {
    json!({
        "type":"assistant", "entrypoint":entrypoint, "sessionId":"session-a",
        "isSidechain":sidechain, "timestamp":"2026-10-14T02:00:00Z",
        "message":{"model":model,"content":[{"type":"text","text":"private answer"}]}
    })
}

#[test]
fn no_installations_still_reports_each_channel_without_fabricating_evidence() {
    let fixture = Fixture::new();
    let observations = inspect(&fixture.paths);
    assert_eq!(observations.len(), 4);
    assert!(observations.iter().all(|item| !item.installed
        && item.selected_model.is_none()
        && item.reported_model.is_none()
        && item.error.is_some()));
}

#[test]
fn chatgpt_codex_sessions_and_cli_with_same_source_stay_distinct() {
    let fixture = Fixture::new();
    fixture.log(
        "codex/sessions/2026/10/14/desktop.jsonl",
        &[
            codex_meta(json!("vscode"), "Codex Desktop", "desktop"),
            selected("gpt-6-astra"),
        ],
        2,
    );
    fixture.log(
        "codex/sessions/2026/10/14/cli.jsonl",
        &[
            codex_meta(json!("vscode"), "codex-tui", "cli"),
            selected("gpt-6.1-sol"),
        ],
        1,
    );
    let desktop = fixture.observation(Source::ChatGptDesktop);
    let cli = fixture.observation(Source::CodexCli);
    assert_eq!(desktop.selected_model.as_deref(), Some("gpt-6-astra"));
    assert_eq!(cli.selected_model.as_deref(), Some("gpt-6.1-sol"));
    assert!(desktop.reported_model.is_none() && cli.reported_model.is_none());
    assert!(desktop.error.is_some() && cli.error.is_some());
}

#[test]
fn subagent_copied_parent_metadata_is_not_desktop_evidence() {
    let fixture = Fixture::new();
    fixture.log(
        "codex/sessions/2026/10/14/child.jsonl",
        &[
            codex_meta(
                json!({"subagent":{"thread_spawn":{}}}),
                "Codex Desktop",
                "child",
            ),
            codex_meta(json!("vscode"), "Codex Desktop", "parent"),
            selected("gpt-6-astra"),
        ],
        1,
    );
    assert!(
        fixture
            .observation(Source::ChatGptDesktop)
            .selected_model
            .is_none()
    );
}

#[test]
fn conflicting_session_ids_are_rejected() {
    let fixture = Fixture::new();
    fixture.log(
        "codex/sessions/2026/10/14/mixed.jsonl",
        &[
            codex_meta(json!("exec"), "codex_exec", "one"),
            selected("gpt-6-astra"),
            codex_meta(json!("exec"), "codex_exec", "two"),
            selected("gpt-6.1-sol"),
        ],
        1,
    );
    assert!(
        fixture
            .observation(Source::CodexCli)
            .selected_model
            .is_none()
    );
}

#[test]
fn latest_codex_selection_is_never_a_response_model() {
    let fixture = Fixture::new();
    fixture.log(
        "codex/sessions/2026/10/14/turns.jsonl",
        &[
            codex_meta(json!("exec"), "codex_exec", "one"),
            selected("gpt-6-astra"),
            selected("gpt-6.1-sol"),
        ],
        1,
    );
    let item = fixture.observation(Source::CodexCli);
    assert_eq!(item.selected_model.as_deref(), Some("gpt-6.1-sol"));
    assert!(item.reported_model.is_none());
}

#[test]
fn claude_desktop_code_and_cli_responses_do_not_mix() {
    let fixture = Fixture::new();
    fixture.log(
        "claude/projects/project/session.jsonl",
        &[
            response("claude-desktop", "claude-fable-5.1", false),
            response("cli", "claude-opus-5.5", false),
            response("cli", "auxiliary-model", true),
            response("cli", "<synthetic>", false),
            response("unknown", "unattributed-model", false),
        ],
        1,
    );
    let desktop = fixture.observation(Source::ClaudeDesktopCode);
    let cli = fixture.observation(Source::ClaudeCli);
    assert_eq!(desktop.reported_model.as_deref(), Some("claude-fable-5.1"));
    assert_eq!(cli.reported_model.as_deref(), Some("claude-opus-5.5"));
    assert!(desktop.selected_model.is_none() && cli.selected_model.is_none());
    let output = serde_json::to_string(&desktop).unwrap();
    assert!(!output.contains("private answer"));
}

#[test]
fn latest_session_wins_without_merging_older_selection() {
    let fixture = Fixture::new();
    fixture.log(
        "codex/sessions/2026/10/14/old.jsonl",
        &[
            codex_meta(json!("exec"), "codex_exec", "old"),
            selected("gpt-6-astra"),
        ],
        1,
    );
    fixture.log(
        "codex/sessions/2026/10/14/new.jsonl",
        &[
            codex_meta(json!("exec"), "codex_exec", "new"),
            selected("gpt-6.1-sol"),
        ],
        2,
    );
    assert_eq!(
        fixture
            .observation(Source::CodexCli)
            .selected_model
            .as_deref(),
        Some("gpt-6.1-sol")
    );
}

#[test]
fn app_installation_does_not_mean_desktop_was_measured() {
    let fixture = Fixture::new();
    fs::create_dir_all(
        fixture
            .root
            .join("Applications/ChatGPT.app/Contents/Resources/codex-cli"),
    )
    .unwrap();
    let item = fixture.observation(Source::ChatGptDesktop);
    assert!(item.installed);
    assert!(item.selected_model.is_none() && item.reported_model.is_none());
    assert!(item.error.is_some());
    assert!(!fixture.observation(Source::CodexCli).installed);
}

#[test]
fn terminal_control_sequences_in_models_are_discarded() {
    let fixture = Fixture::new();
    fixture.log(
        "claude/projects/project/session.jsonl",
        &[response("cli", "claude-fable-5.1\u{1b}[2J", false)],
        1,
    );
    assert!(
        fixture
            .observation(Source::ClaudeCli)
            .reported_model
            .is_none()
    );
}

#[test]
fn oversized_sessions_are_skipped() {
    let fixture = Fixture::new();
    fixture.log(
        "claude/projects/project/large.jsonl",
        &[response("cli", "claude-fable-5.1", false)],
        1,
    );
    File::options()
        .write(true)
        .open(fixture.root.join("claude/projects/project/large.jsonl"))
        .unwrap()
        .set_len(33 * 1024 * 1024)
        .unwrap();
    assert!(
        fixture
            .observation(Source::ClaudeCli)
            .reported_model
            .is_none()
    );
}

fn executable(path: &std::path::Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn native_cli_binaries_are_discovered_and_reported_as_installed() {
    let fixture = Fixture::new();
    for (name, model, source) in [
        ("codex", "gpt-6-astra", Source::CodexCli),
        ("claude", "claude-fable-5.1", Source::ClaudeCli),
    ] {
        let binary = fixture
            .root
            .join("bin")
            .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        executable(&binary);
        let clients = discover(&fixture.paths, model);
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[0].source, source);
        assert_eq!(clients[0].binary, binary);
        assert!(fixture.observation(source).installed);
    }
}

#[test]
fn batch_wrappers_are_not_discovered_as_native_clients() {
    let fixture = Fixture::new();
    for name in ["codex", "claude"] {
        executable(&fixture.root.join(format!("bin/{name}.cmd")));
        executable(&fixture.root.join(format!("bin/{name}.bat")));
        #[cfg(windows)]
        executable(&fixture.root.join(format!("bin/{name}")));
    }
    assert!(discover(&fixture.paths, "gpt-6-astra").is_empty());
    assert!(discover(&fixture.paths, "claude-fable-5.1").is_empty());
    assert!(!fixture.observation(Source::CodexCli).installed);
    assert!(!fixture.observation(Source::ClaudeCli).installed);
}

#[test]
#[cfg(windows)]
fn windows_userprofile_is_used_without_home_or_config_overrides() {
    const CHILD: &str = "MSD_TEST_WINDOWS_USERPROFILE";
    if let Some(root) = std::env::var_os(CHILD) {
        let root = PathBuf::from(root);
        let paths = LocalPaths::from_env();
        assert_eq!(paths.codex_home, root.join(".codex"));
        assert_eq!(paths.claude_home, root.join(".claude"));
        return;
    }
    let fixture = Fixture::new();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "windows_userprofile_is_used_without_home_or_config_overrides",
            "--nocapture",
        ])
        .env(CHILD, &fixture.root)
        .env("USERPROFILE", &fixture.root)
        .env_remove("HOME")
        .env_remove("CODEX_HOME")
        .env_remove("CLAUDE_CONFIG_DIR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[cfg(unix)]
fn chatgpt_app_is_a_runnable_client_without_a_separate_cli_or_sessions() {
    let fixture = Fixture::new();
    let engine = fixture.root.join(
        "Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex",
    );
    executable(&engine);
    let clients = discover(&fixture.paths, "gpt-6-astra");
    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0].source, Source::ChatGptDesktop);
    assert_eq!(clients[0].binary, engine);
    assert!(!fixture.observation(Source::CodexCli).installed);
    assert!(
        fixture
            .observation(Source::ChatGptDesktop)
            .selected_model
            .is_none()
    );
}

#[test]
#[cfg(unix)]
fn app_engine_and_path_alias_are_only_sampled_once() {
    let fixture = Fixture::new();
    let engine = fixture.root.join(
        "Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex",
    );
    executable(&engine);
    fs::create_dir_all(fixture.root.join("bin")).unwrap();
    std::os::unix::fs::symlink(&engine, fixture.root.join("bin/codex")).unwrap();
    let clients = discover(&fixture.paths, "gpt-6-astra");
    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0].source, Source::ChatGptDesktop);
}

#[test]
#[cfg(unix)]
fn app_and_a_separate_cli_are_both_discovered() {
    let fixture = Fixture::new();
    executable(&fixture.root.join(
        "Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex",
    ));
    executable(&fixture.root.join("bin/codex"));
    let clients = discover(&fixture.paths, "gpt-6-astra");
    assert_eq!(clients.len(), 2);
    assert_eq!(clients[0].source, Source::ChatGptDesktop);
    assert_eq!(clients[1].source, Source::CodexCli);
}

#[test]
#[cfg(unix)]
fn user_applications_are_supported_and_model_family_filters_clients() {
    let mut fixture = Fixture::new();
    let applications = fixture.root.join("home/Applications");
    fixture.paths.applications.push(applications.clone());
    executable(
        &applications
            .join("ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex"),
    );
    executable(&fixture.root.join("bin/claude"));
    let openai = discover(&fixture.paths, "gpt-6-astra");
    let claude = discover(&fixture.paths, "claude-fable-5.1");
    assert_eq!(openai.len(), 1);
    assert_eq!(openai[0].source, Source::ChatGptDesktop);
    assert_eq!(claude.len(), 1);
    assert_eq!(claude[0].source, Source::ClaudeCli);
}

#[test]
fn duplicate_path_entries_are_deduplicated() {
    let mut fixture = Fixture::new();
    executable(
        &fixture
            .root
            .join(format!("bin/codex{}", std::env::consts::EXE_SUFFIX)),
    );
    fixture.paths.executable_dirs.push(fixture.root.join("bin"));
    let clients = discover(&fixture.paths, "gpt-6-astra");
    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0].source, Source::CodexCli);
}

#[test]
fn app_directory_without_an_executable_is_not_a_sampling_client() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.root.join("Applications/ChatGPT.app")).unwrap();
    assert!(discover(&fixture.paths, "gpt-6-astra").is_empty());
}
