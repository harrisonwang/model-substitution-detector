//! 通过已安装的 Codex 或 Claude CLI 进行隔离采样，并限制资源用量。
//!
//! 身份验证仍由 CLI 管理。返回的模型名称仅作为元数据，
//! 不能证明模型的真实身份。

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const OUTPUT_LIMIT: usize = 4 * 1024 * 1024;
const ANSWER_LIMIT: usize = 256 * 1024;
const PROMPT_LIMIT: usize = 128 * 1024;
const SCHEMA: &str = r#"{"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}"#;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct CollectorConfig {
    pub binary: PathBuf,
    pub profile: Option<String>,
    pub effort: String,
    pub timeout_secs: u64,
    pub model: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SampleOutcome {
    pub answer: Option<String>,
    pub error: Option<String>,
    pub duration_ms: u64,
    pub reported_model: Option<String>,
    pub response_id: Option<String>,
    pub service_tier: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    /// 模型已开始输出；后续验证错误不属于连接失败。
    #[serde(default, skip_serializing_if = "is_false")]
    pub response_started: bool,
    /// CLI 明确报告了模型重路由；即使回答无效，该证据仍然有效。
    #[serde(default, skip_serializing_if = "is_false")]
    pub model_rerouted: bool,
    /// 仅记录事件类型，不包含命令参数、提示词、原始日志或配置。
    #[serde(default)]
    pub evidence: Vec<String>,
}

fn is_false(value: &bool) -> bool {
    !value
}

pub struct Collector {
    config: CollectorConfig,
    version: String,
    disabled_features: Vec<String>,
}

#[derive(Clone, Copy)]
enum AnswerFormat {
    Structured,
    Text,
}

pub fn discover_cli(model: &str) -> Result<PathBuf> {
    crate::codex::find_binary(model)
}

impl Collector {
    pub fn new(mut config: CollectorConfig) -> Result<Self> {
        if !matches!(config.effort.as_str(), "low" | "medium" | "high" | "xhigh") {
            bail!("unsupported reasoning effort");
        }
        if config.model.trim().is_empty() {
            bail!("请求的模型名不能为空");
        }
        if config.timeout_secs == 0 {
            bail!("collector timeout must be positive");
        }
        config.binary = fs::canonicalize(&config.binary).context("CLI binary not found")?;
        let version_output = metadata_command(&config.binary, &["--version"])?;
        if config.model.starts_with("claude-") {
            if config.profile.is_some() {
                bail!("Claude CLI does not support Codex profiles");
            }
            let version = version_output
                .lines()
                .map(str::trim)
                .find(|line| line.ends_with(" (Claude Code)") && line.len() <= 200)
                .context("unrecognized Claude CLI version output")?
                .to_string();
            let help = metadata_command(&config.binary, &["--help"])?;
            for flag in [
                "--print",
                "--output-format",
                "--verbose",
                "--tools",
                "--safe-mode",
                "--strict-mcp-config",
                "--mcp-config",
                "--no-session-persistence",
                "--effort",
            ] {
                if !help.contains(flag) {
                    bail!("installed Claude CLI does not support required flag {flag}");
                }
            }
            return Ok(Self {
                config,
                version,
                disabled_features: Vec::new(),
            });
        }
        let version = version_output
            .lines()
            .find(|line| line.trim().starts_with("codex-cli "))
            .map(str::trim)
            .filter(|line| line.len() <= 200)
            .context("unrecognized Codex CLI version output")?
            .to_string();
        let help = metadata_command(&config.binary, &["exec", "--help"])?;
        for flag in [
            "--ephemeral",
            "--json",
            "--output-schema",
            "--output-last-message",
        ] {
            if !help.contains(flag) {
                bail!("installed Codex CLI does not support required flag {flag}");
            }
        }
        // 仅枚举功能名称，不输出或保留功能状态及用户配置。
        // 不添加旧版 CLI 可能无法识别的功能开关。
        let features = metadata_command(&config.binary, &["features", "list"])?;
        let disabled_features = features
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .filter(|name| {
                matches!(
                    *name,
                    "shell_tool"
                        | "unified_exec"
                        | "apps"
                        | "plugins"
                        | "hooks"
                        | "browser_use"
                        | "browser_use_external"
                        | "computer_use"
                        | "in_app_browser"
                        | "image_generation"
                        | "view_image"
                        | "multi_agent"
                        | "multi_agent_v2"
                        | "goals"
                        | "memories"
                        | "shell_snapshot"
                        | "shell_snapshot_v2"
                        | "code_mode_host"
                        | "code_mode"
                        | "skill_mcp_dependency_install"
                        | "skill_search"
                        | "tool_suggest"
                        | "sleep_tool"
                        | "workspace_dependencies"
                        | "remote_plugin"
                )
            })
            .map(str::to_owned)
            .collect();
        Ok(Self {
            config,
            version,
            disabled_features,
        })
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn collect(&self, prompt: &str) -> SampleOutcome {
        self.collect_with_format(prompt, AnswerFormat::Structured)
    }

    /// 收集完整的最终文本，不附加模式定义或 JSON 格式指令。
    /// 用于能力探测。
    pub fn collect_text(&self, prompt: &str) -> SampleOutcome {
        self.collect_with_format(prompt, AnswerFormat::Text)
    }

    /// 最多同时采样三个提示词，按索引顺序返回已完成的结果。
    pub fn collect_many(
        &self,
        prompts: &[String],
        progress: &mut dyn FnMut(usize, usize),
    ) -> Vec<(usize, SampleOutcome)> {
        collect_parallel(prompts, |prompt| self.collect(prompt), progress)
    }

    pub fn collect_text_many(
        &self,
        prompts: &[String],
        progress: &mut dyn FnMut(usize, usize),
    ) -> Vec<(usize, SampleOutcome)> {
        collect_parallel(prompts, |prompt| self.collect_text(prompt), progress)
    }

    fn collect_with_format(&self, prompt: &str, format: AnswerFormat) -> SampleOutcome {
        let started = Instant::now();
        let mut result = match self.collect_inner(prompt, format) {
            Ok(result) => result,
            Err(error) => SampleOutcome {
                error: Some(error.to_string()),
                ..Default::default()
            },
        };
        result.duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        result
    }

    fn collect_inner(&self, prompt: &str, format: AnswerFormat) -> Result<SampleOutcome> {
        if prompt.is_empty() || prompt.len() > PROMPT_LIMIT {
            bail!("prompt is empty or exceeds the collector limit");
        }
        let scratch = Scratch::new()?;
        let work = scratch.0.join("work");
        fs::create_dir(&work).context("cannot create isolated working directory")?;
        if self.config.model.starts_with("claude-") {
            return self.collect_claude(prompt, format, &work);
        }
        let final_path = scratch.0.join(match format {
            AnswerFormat::Structured => "answer.json",
            AnswerFormat::Text => "answer.txt",
        });
        let mut command = Command::new(&self.config.binary);
        command.args([
            "exec",
            "--skip-git-repo-check",
            "--ephemeral",
            "--json",
            "--color",
            "never",
        ]);
        command.args(["--sandbox", "read-only", "--model", &self.config.model]);
        if matches!(format, AnswerFormat::Structured) {
            let schema = scratch.0.join("answer-schema.json");
            fs::write(&schema, SCHEMA).context("cannot write answer schema")?;
            command.arg("--output-schema").arg(&schema);
        }
        command.arg("--output-last-message").arg(&final_path);
        command.arg("--cd").arg(&work).current_dir(&work);
        if let Some(profile) = &self.config.profile {
            command.arg("--profile").arg(profile);
        }
        // 保留选定的服务提供方、端点及 Codex 管理的身份验证。
        // CLI 配置覆盖仅影响当前子进程，不修改用户文件。
        for setting in [
            "approval_policy=\"never\"",
            "web_search=\"disabled\"",
            "project_doc_max_bytes=0",
            "project_doc_fallback_filenames=[]",
            "history.persistence=\"none\"",
            "mcp_servers={}",
        ] {
            command.args(["-c", setting]);
        }
        command.args(["-c", match format {
            AnswerFormat::Structured => "developer_instructions=\"Answer the supplied question directly from your own knowledge. Do not call tools, browse, inspect files, or delegate. Return only the final answer matching the supplied JSON schema.\"",
            AnswerFormat::Text => "developer_instructions=\"Answer the supplied question directly from your own knowledge. Do not call tools, browse, inspect files, or delegate. Return only the final answer in the format requested by the user.\"",
        }]);
        command.args([
            "-c",
            &format!("model_reasoning_effort=\"{}\"", self.config.effort),
        ]);
        for feature in &self.disabled_features {
            command.arg("--disable").arg(feature);
        }
        // 仅跟踪响应事件，不保存原始标准错误输出。
        // 这些事件提供元数据，并捕获 CLI JSONL 中遗漏的工具调用。
        command.env("RUST_LOG", "codex_api::sse::responses=trace");
        command.arg("-"); // 提示词仅通过标准输入传递，不放入命令行参数或 shell。
        let output = run_bounded(
            command,
            prompt.as_bytes().to_vec(),
            Duration::from_secs(self.config.timeout_secs),
            true,
        )?;
        let (mut result, completed) = parse_events(&output.stdout, &output.stderr);
        if let Some(error) = output.error {
            result.error = Some(error);
        } else if !output.status.is_some_and(|status| status.success()) {
            result.error = Some("Codex exited unsuccessfully; raw diagnostics withheld".into());
        }
        if result.error.is_none() && !completed {
            result.error = Some("Codex did not emit a completed response event".into());
        }
        if result.error.is_none() {
            // 以专用的最终消息文件为准，不使用中间的
            // 智能体消息或推理内容作为备用结果。
            match read_answer(&final_path, format) {
                Ok(answer) => result.answer = Some(answer),
                Err(error) => result.error = Some(error.to_string()),
            }
        }
        if result.error.is_some() {
            result.answer = None;
        }
        Ok(result)
    }

    fn collect_claude(
        &self,
        prompt: &str,
        format: AnswerFormat,
        work: &Path,
    ) -> Result<SampleOutcome> {
        let instruction = match format {
            AnswerFormat::Structured => {
                "Answer from your own knowledge without tools. Return only a JSON object with exactly one string field named answer."
            }
            AnswerFormat::Text => {
                "Answer from your own knowledge without tools. Return only the final answer in the format requested by the user."
            }
        };
        let mut command = Command::new(&self.config.binary);
        command.current_dir(work).args([
            "--print",
            "--verbose",
            "--output-format",
            "stream-json",
            "--model",
            &self.config.model,
            "--effort",
            &self.config.effort,
            "--tools",
            "",
            "--safe-mode",
            "--strict-mcp-config",
            "--mcp-config",
            "{\"mcpServers\":{}}",
            "--no-session-persistence",
            "--system-prompt",
            instruction,
        ]);
        let output = run_bounded(
            command,
            prompt.as_bytes().to_vec(),
            Duration::from_secs(self.config.timeout_secs),
            true,
        )?;
        let (mut result, completed) = parse_claude_events(&output.stdout, format);
        if let Some(error) = output.error {
            result.error = Some(error);
        } else if !output.status.is_some_and(|status| status.success()) {
            result.error = Some("Claude exited unsuccessfully; raw diagnostics withheld".into());
        } else if !completed {
            result.error = Some("Claude did not emit a completed result".into());
        }
        if result.error.is_some() {
            result.answer = None;
        }
        Ok(result)
    }
}

fn collect_parallel(
    prompts: &[String],
    collect: impl Fn(&str) -> SampleOutcome + Sync,
    progress: &mut dyn FnMut(usize, usize),
) -> Vec<(usize, SampleOutcome)> {
    progress(0, prompts.len());
    // 领取任务与停止后续任务领取共用一把锁。
    // 采样或调用进度回调时不持有该锁。
    let dispatch = Mutex::new((0_usize, false));
    let (sender, receiver) = mpsc::channel();
    let mut results = Vec::with_capacity(prompts.len());
    thread::scope(|scope| {
        for _ in 0..prompts.len().min(3) {
            let sender = sender.clone();
            let dispatch = &dispatch;
            let collect = &collect;
            scope.spawn(move || {
                loop {
                    let index = {
                        let mut state = dispatch.lock().expect("collector dispatch lock poisoned");
                        if state.1 || state.0 == prompts.len() {
                            break;
                        }
                        let index = state.0;
                        state.0 += 1;
                        index
                    };
                    let sample = collect(&prompts[index]);
                    if sample.error.is_some() || sample.model_rerouted {
                        dispatch.lock().expect("collector dispatch lock poisoned").1 = true;
                    }
                    if sender.send((index, sample)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(sender);
        for result in receiver {
            results.push(result);
            progress(results.len(), prompts.len());
        }
    });
    results.sort_unstable_by_key(|(index, _)| *index);
    results
}

fn read_answer(path: &Path, format: AnswerFormat) -> Result<String> {
    let mut bytes = Vec::new();
    File::open(path)
        .context("Codex did not produce a final-answer file")?
        .take((ANSWER_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .context("cannot read final-answer file")?;
    decode_answer(&bytes, format)
}

fn decode_answer(bytes: &[u8], format: AnswerFormat) -> Result<String> {
    if bytes.len() > ANSWER_LIMIT {
        bail!("final answer exceeds the collector limit");
    }
    if matches!(format, AnswerFormat::Text) {
        let text = std::str::from_utf8(bytes).context("final answer is not UTF-8 text")?;
        if text.trim().is_empty() {
            bail!("final answer is empty");
        }
        return Ok(text.to_string());
    }
    let value: Value =
        serde_json::from_slice(bytes).context("final answer does not match the JSON schema")?;
    let object = value
        .as_object()
        .context("final answer is not a JSON object")?;
    let answer = object
        .get("answer")
        .and_then(Value::as_str)
        .context("final answer has no string answer field")?;
    if object.len() != 1 || answer.trim().is_empty() {
        bail!("final answer does not match the JSON schema");
    }
    Ok(answer.trim().to_string())
}

fn event_value(line: &[u8]) -> Option<Value> {
    let text = std::str::from_utf8(line).ok()?.trim();
    let json = text
        .split_once("SSE event: ")
        .map_or(text, |(_, rest)| rest);
    serde_json::from_str(json).ok()
}

fn has_tool(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            if let Some(kind) = object.get("type").and_then(Value::as_str) {
                if kind.contains("tool")
                    || kind.contains("function_call")
                    || kind.contains("computer_call")
                    || matches!(
                        kind,
                        "command_execution"
                            | "file_change"
                            | "web_search"
                            | "web_search_call"
                            | "todo_list"
                            | "image_generation_call"
                            | "local_shell_call"
                    )
                {
                    return true;
                }
                // 遇到未来新增的 CLI 项目类型时拒绝处理，
                // 避免默许新增的执行能力。
                if matches!(kind, "item.started" | "item.updated" | "item.completed")
                    && let Some(item_kind) = object
                        .get("item")
                        .and_then(|v| v.get("type"))
                        .and_then(Value::as_str)
                    && !matches!(
                        item_kind,
                        "agent_message" | "reasoning" | "message" | "error"
                    )
                {
                    return true;
                }
            }
            object.values().any(has_tool)
        }
        Value::Array(values) => values.iter().any(has_tool),
        _ => false,
    }
}

fn parse_events(stdout: &[u8], stderr: &[u8]) -> (SampleOutcome, bool) {
    let mut result = SampleOutcome::default();
    let mut completed = false;
    for line in stdout
        .split(|byte| *byte == b'\n')
        .chain(stderr.split(|byte| *byte == b'\n'))
    {
        let Some(event) = event_value(line) else {
            continue;
        };
        if has_tool(&event) {
            result.response_started = true;
            result.error = Some("tool event detected; sample invalid".into());
        }
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        if matches!(
            kind,
            "response.created" | "response.completed" | "turn.completed"
        ) {
            result.response_started = true;
        }
        // 项目子类型名称用于排查 CLI 数据结构变更，
        // 无需保留消息文本、工具参数或原始标准错误输出。
        if matches!(kind, "item.started" | "item.updated" | "item.completed")
            && let Some(item) = event.get("item")
            && let Some(item_kind) = safe_metadata(item, "type")
        {
            let label = format!("{kind}/{item_kind}");
            if result.evidence.len() < 24 && !result.evidence.contains(&label) {
                result.evidence.push(label);
            }
            if item_kind != "error" {
                result.response_started = true;
            }
            if item_kind == "error" {
                let message = item.get("message").and_then(Value::as_str).unwrap_or("");
                // Codex exec 的 ErrorItem 是非致命诊断信息（包括
                // ConfigWarning/Warning）；致命错误以顶层事件报告。
                // ModelRerouted 使用相同的项目类型，但即使本轮成功，
                // 也意味着采样未遵守使用所请求模型的约定。
                // 参见 codex-rs/exec/src/event_processor_with_jsonl_output.rs。
                if message
                    .trim()
                    .to_ascii_lowercase()
                    .starts_with("model rerouted:")
                {
                    result.model_rerouted = true;
                    result.error = Some("Codex reported model rerouting; sample invalid".into());
                }
            }
        }
        if matches!(kind, "turn.completed" | "response.completed") {
            completed = true;
        }
        if result.evidence.len() < 24
            && !kind.is_empty()
            && kind.len() <= 80
            && kind
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b))
            && !result.evidence.iter().any(|old| old == kind)
        {
            result.evidence.push(kind.to_string());
        }
        if matches!(
            kind,
            "turn.failed" | "error" | "response.failed" | "response.incomplete"
        ) {
            result
                .error
                .get_or_insert_with(|| "Codex reported a failed or incomplete response".into());
        }
        if matches!(kind, "response.created" | "response.completed")
            && let Some(response) = event.get("response")
        {
            update_metadata(&mut result, response);
            if kind == "response.completed"
                && response
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s != "completed")
            {
                result
                    .error
                    .get_or_insert_with(|| "response did not complete".into());
            }
        }
        if kind == "turn.completed" {
            update_usage(&mut result, &event);
        }
    }
    (result, completed)
}

fn parse_claude_events(stdout: &[u8], format: AnswerFormat) -> (SampleOutcome, bool) {
    let mut result = SampleOutcome::default();
    let mut completed = false;
    for line in stdout.split(|byte| *byte == b'\n') {
        let Some(event) = event_value(line) else {
            continue;
        };
        if has_tool(&event) {
            result.response_started = true;
            result.error = Some("tool event detected; sample invalid".into());
        }
        match event.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                result.response_started = true;
                if let Some(message) = event.get("message") {
                    update_metadata(&mut result, message);
                }
                if !result.evidence.iter().any(|kind| kind == "assistant") {
                    result.evidence.push("assistant".into());
                }
            }
            Some("result") => {
                completed = true;
                result.evidence.push("result".into());
                update_usage(&mut result, &event);
                if event.get("is_error").and_then(Value::as_bool) != Some(false)
                    || event.get("subtype").and_then(Value::as_str) != Some("success")
                    || event
                        .get("stop_reason")
                        .and_then(Value::as_str)
                        .is_some_and(|reason| reason != "end_turn")
                {
                    result.error = Some("Claude reported a failed or incomplete result".into());
                    continue;
                }
                result.response_started = true;
                if event
                    .get("permission_denials")
                    .and_then(Value::as_array)
                    .is_some_and(|denials| !denials.is_empty())
                {
                    result.error = Some("tool permission requested; sample invalid".into());
                    continue;
                }
                match event
                    .get("result")
                    .and_then(Value::as_str)
                    .context("Claude did not produce a final answer")
                    .and_then(|answer| decode_answer(answer.as_bytes(), format))
                {
                    Ok(answer) => result.answer = Some(answer),
                    Err(error) => result.error = Some(error.to_string()),
                }
            }
            Some("error") => {
                result.error = Some("Claude reported an error; raw diagnostics withheld".into());
            }
            _ => {}
        }
    }
    (result, completed)
}

fn safe_metadata(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 200
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b))
        })
        .map(str::to_owned)
}

fn update_metadata(result: &mut SampleOutcome, response: &Value) {
    if let Some(model) = safe_metadata(response, "model") {
        result.reported_model = Some(model);
    }
    if let Some(id) = safe_metadata(response, "id") {
        result.response_id = Some(id);
    }
    if let Some(tier) = safe_metadata(response, "service_tier") {
        result.service_tier = Some(tier);
    }
    update_usage(result, response);
}

fn update_usage(result: &mut SampleOutcome, response: &Value) {
    let Some(usage) = response.get("usage") else {
        return;
    };
    if let Some(n) = usage.get("input_tokens").and_then(Value::as_u64) {
        result.input_tokens = Some(n);
    }
    if let Some(n) = usage.get("output_tokens").and_then(Value::as_u64) {
        result.output_tokens = Some(n);
    }
    if let Some(n) = usage
        .pointer("/output_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64)
    {
        result.reasoning_tokens = Some(n);
    }
}

struct Captured {
    status: Option<ExitStatus>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    error: Option<String>,
}

fn metadata_command(binary: &Path, args: &[&str]) -> Result<String> {
    let mut command = Command::new(binary);
    command.args(args).env("RUST_LOG", "off");
    let captured = run_bounded(command, Vec::new(), Duration::from_secs(10), false)?;
    if let Some(error) = captured.error {
        bail!("CLI capability check failed: {error} (raw diagnostics withheld)");
    }
    if !captured.status.is_some_and(|s| s.success()) {
        bail!(
            "CLI capability check failed: {} (raw diagnostics withheld)",
            captured
                .status
                .map_or_else(|| "missing exit status".into(), |status| status.to_string())
        );
    }
    String::from_utf8(captured.stdout).context("CLI capability output is not UTF-8")
}

fn run_bounded(
    mut command: Command,
    input: Vec<u8>,
    timeout: Duration,
    detect_tools: bool,
) -> Result<Captured> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .context("collector timeout is too large")?;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().context("failed to start CLI")?;
    let stdout = child.stdout.take().context("missing CLI stdout")?;
    let stderr = child.stderr.take().context("missing CLI stderr")?;
    let mut stdin = child.stdin.take().context("missing CLI stdin")?;
    let limit_hit = Arc::new(AtomicBool::new(false));
    let tool_seen = Arc::new(AtomicBool::new(false));
    let pipe_failed = Arc::new(AtomicBool::new(false));
    let out_rx = read_pipe(
        stdout,
        Arc::clone(&limit_hit),
        Arc::clone(&tool_seen),
        Arc::clone(&pipe_failed),
        detect_tools,
    );
    let err_rx = read_pipe(
        stderr,
        Arc::clone(&limit_hit),
        Arc::clone(&tool_seen),
        Arc::clone(&pipe_failed),
        detect_tools,
    );
    let write_rx = if input.is_empty() {
        drop(stdin);
        None
    } else {
        let (write_tx, write_rx) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let result = stdin.write_all(&input);
            drop(stdin);
            let _ = write_tx.send(result.is_ok());
        });
        Some(write_rx)
    };
    let mut error = None;
    let status;
    loop {
        if tool_seen.load(Ordering::Relaxed) {
            error = Some("tool event detected; sample invalid".into());
        } else if limit_hit.load(Ordering::Relaxed) {
            error = Some("CLI output exceeded the collector limit".into());
        } else if pipe_failed.load(Ordering::Relaxed) {
            error = Some("CLI event stream could not be read completely".into());
        } else if Instant::now() >= deadline {
            error = Some("CLI collection timed out".into());
        }
        if error.is_some() {
            kill_group(&mut child);
            status = child.wait().ok();
            break;
        }
        match child.try_wait() {
            Ok(Some(exit)) => {
                status = Some(exit);
                break;
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(_) => {
                error = Some("could not observe CLI process status".into());
                kill_group(&mut child);
                status = child.wait().ok();
                break;
            }
        }
    }
    // 主进程退出后，后代进程可能仍持有管道。
    kill_group(&mut child);
    // 读取线程可能在子进程退出后才获得调度。使用调用剩余的时间，
    // 超时子进程被终止后，所有读取线程共用一秒清理时间。
    let cleanup_deadline = deadline
        .checked_add(Duration::from_secs(1))
        .unwrap_or(deadline);
    let stdout = receive_until(&out_rx, cleanup_deadline);
    let stderr = receive_until(&err_rx, cleanup_deadline);
    if let Err(pipe_error) = &stdout {
        error.get_or_insert_with(|| format!("CLI stdout did not close cleanly: {pipe_error}"));
    }
    if let Err(pipe_error) = &stderr {
        error.get_or_insert_with(|| format!("CLI stderr did not close cleanly: {pipe_error}"));
    }
    if let Some(write_rx) = write_rx {
        match receive_until(&write_rx, cleanup_deadline) {
            Ok(true) => {}
            Ok(false) => {
                error.get_or_insert_with(|| "CLI stdin write failed".into());
            }
            Err(pipe_error) => {
                error.get_or_insert_with(|| {
                    format!("CLI stdin did not close cleanly: {pipe_error}")
                });
            }
        }
    }
    if limit_hit.load(Ordering::Relaxed) {
        error.get_or_insert_with(|| "CLI output exceeded the collector limit".into());
    }
    if tool_seen.load(Ordering::Relaxed) {
        error = Some("tool event detected; sample invalid".into());
    }
    if pipe_failed.load(Ordering::Relaxed) {
        error.get_or_insert_with(|| "CLI event stream could not be read completely".into());
    }
    Ok(Captured {
        status,
        stdout: stdout.unwrap_or_default(),
        stderr: stderr.unwrap_or_default(),
        error,
    })
}

fn receive_until<T>(
    receiver: &mpsc::Receiver<T>,
    deadline: Instant,
) -> std::result::Result<T, mpsc::RecvTimeoutError> {
    receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
}

fn read_pipe<R: Read + Send + 'static>(
    mut pipe: R,
    limit_hit: Arc<AtomicBool>,
    tool_seen: Arc<AtomicBool>,
    pipe_failed: Arc<AtomicBool>,
    detect_tools: bool,
) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut checked_until = 0;
        let mut search_from = 0;
        let mut buffer = [0_u8; 8192];
        loop {
            let n = match pipe.read(&mut buffer) {
                Ok(0) => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    pipe_failed.store(true, Ordering::Relaxed);
                    break;
                }
                Ok(n) => n,
            };
            if bytes.len() + n > OUTPUT_LIMIT {
                limit_hit.store(true, Ordering::Relaxed);
                break;
            }
            bytes.extend_from_slice(&buffer[..n]);
            if detect_tools {
                while let Some(relative) = bytes[search_from..].iter().position(|b| *b == b'\n') {
                    let end = search_from + relative;
                    if event_value(&bytes[checked_until..end]).is_some_and(|value| has_tool(&value))
                    {
                        tool_seen.store(true, Ordering::Relaxed);
                    }
                    checked_until = end + 1;
                    search_from = checked_until;
                }
                search_from = bytes.len();
            }
        }
        if detect_tools
            && event_value(&bytes[checked_until..]).is_some_and(|value| has_tool(&value))
        {
            tool_seen.store(true, Ordering::Relaxed);
        }
        let _ = tx.send(bytes);
    });
    rx
}

fn kill_group(child: &mut Child) {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        if let Ok(pid) = i32::try_from(child.id()) {
            // 子进程通过 process_group(0) 启动；负 PID 仅指向
            // 这个新进程组，包括常规派生的后代进程。
            unsafe {
                kill(-pid, 9);
            }
        }
    }
    let _ = child.kill();
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Result<Self> {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for _ in 0..16 {
            let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("msd-collector-{}-{stamp}-{id}", std::process::id()));
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => bail!("cannot create private temporary collector directory"),
            }
        }
        bail!("cannot allocate private temporary collector directory")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const MODEL: &str = "gpt-6-astra";
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn parallel_collection_uses_three_workers_and_preserves_prompt_indices() {
        use std::sync::{Barrier, atomic::AtomicUsize};
        let prompts: Vec<_> = (0..9).map(|index| format!("prompt-{index}")).collect();
        let barrier = Barrier::new(3);
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let calling_thread = thread::current().id();
        let mut updates = Vec::new();
        let samples = collect_parallel(
            &prompts,
            |prompt| {
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(count, Ordering::SeqCst);
                barrier.wait();
                active.fetch_sub(1, Ordering::SeqCst);
                SampleOutcome {
                    answer: Some(prompt.into()),
                    ..Default::default()
                }
            },
            &mut |completed, total| {
                assert_eq!(thread::current().id(), calling_thread);
                updates.push((completed, total));
            },
        );
        assert_eq!(peak.load(Ordering::SeqCst), 3);
        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert_eq!(
            updates,
            (0..=9).map(|completed| (completed, 9)).collect::<Vec<_>>()
        );
        assert_eq!(samples.len(), prompts.len());
        for (expected, (index, sample)) in samples.into_iter().enumerate() {
            assert_eq!(index, expected);
            assert_eq!(sample.answer.as_deref(), Some(prompts[index].as_str()));
        }
    }

    #[test]
    fn parallel_collection_stops_dispatch_on_error_or_reroute_and_keeps_inflight_results() {
        use std::sync::{Barrier, Condvar};
        for rerouted in [false, true] {
            let prompts: Vec<_> = (0..10).map(|index| index.to_string()).collect();
            let barrier = Barrier::new(3);
            let gate = (Mutex::new(false), Condvar::new());
            let samples = collect_parallel(
                &prompts,
                |prompt| {
                    let index: usize = prompt.parse().unwrap();
                    assert!(
                        index < 3,
                        "a new request was dispatched after collection stopped"
                    );
                    barrier.wait();
                    if index == 0 {
                        return SampleOutcome {
                            error: (!rerouted).then(|| "collection failed".into()),
                            model_rerouted: rerouted,
                            ..Default::default()
                        };
                    }
                    let mut released = gate.0.lock().unwrap();
                    while !*released {
                        released = gate.1.wait(released).unwrap();
                    }
                    SampleOutcome {
                        answer: Some(prompt.into()),
                        ..Default::default()
                    }
                },
                &mut |completed, total| {
                    assert_eq!(total, 10);
                    if completed == 1 {
                        *gate.0.lock().unwrap() = true;
                        gate.1.notify_all();
                    }
                },
            );
            assert_eq!(samples.len(), 3);
            assert_eq!(
                samples.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
                vec![0, 1, 2]
            );
            assert_eq!(samples[0].1.model_rerouted, rerouted);
            assert_eq!(samples[1].1.answer.as_deref(), Some("1"));
            assert_eq!(samples[2].1.answer.as_deref(), Some("2"));
        }
    }

    #[test]
    fn parallel_collection_handles_empty_and_single_prompt_batches() {
        let mut updates = Vec::new();
        let empty = collect_parallel(
            &[],
            |_| panic!("no requests expected"),
            &mut |done, total| updates.push((done, total)),
        );
        assert!(empty.is_empty());
        assert_eq!(updates, vec![(0, 0)]);
        updates.clear();
        let samples = collect_parallel(
            &["one".into()],
            |prompt| SampleOutcome {
                answer: Some(prompt.into()),
                ..Default::default()
            },
            &mut |done, total| updates.push((done, total)),
        );
        assert_eq!(updates, vec![(0, 1), (1, 1)]);
        assert_eq!(samples[0].0, 0);
        assert_eq!(samples[0].1.answer.as_deref(), Some("one"));
    }

    #[test]
    fn pipe_completion_uses_the_remaining_call_deadline() {
        struct DelayedReader(bool);
        impl Read for DelayedReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if self.0 {
                    return Ok(0);
                }
                self.0 = true;
                thread::sleep(Duration::from_millis(1100));
                buffer[..2].copy_from_slice(b"ok");
                Ok(2)
            }
        }
        let receiver = read_pipe(
            DelayedReader(false),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicBool::new(false)),
            false,
        );
        let deadline = Instant::now() + Duration::from_secs(3);
        assert_eq!(receive_until(&receiver, deadline).unwrap(), b"ok");
        let (_sender, pending) = mpsc::channel::<()>();
        assert_eq!(
            receive_until(&pending, Instant::now()),
            Err(mpsc::RecvTimeoutError::Timeout)
        );
    }

    #[test]
    fn metadata_failure_exposes_exit_status_without_raw_diagnostics() {
        let directory = Scratch::new().unwrap();
        let binary = directory.0.join("failing-cli");
        fs::write(
            &binary,
            "#!/bin/sh\necho secret-do-not-retain >&2\nexit 17\n",
        )
        .unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let error = metadata_command(&binary, &["--version"])
            .unwrap_err()
            .to_string();
        assert!(error.contains("17"));
        assert!(!error.contains("secret-do-not-retain"));
    }

    fn mock(body: &str, timeout_secs: u64) -> (Scratch, Collector) {
        let directory = Scratch::new().unwrap();
        let binary = directory.0.join("codex");
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in\n--version) echo 'codex-cli mock'; exit 0;;\nfeatures) echo 'shell_tool stable true'; exit 0;;\nesac\nif [ \"$2\" = '--help' ]; then echo '--ephemeral --json --output-schema --output-last-message'; exit 0; fi\noutput=''\nschema=''\ndeveloper=''\nwhile [ $# -gt 0 ]; do\ncase \"$1\" in\n--output-last-message) shift; output=$1;;\n--output-schema) shift; schema=$1;;\n-c) shift; case \"$1\" in developer_instructions=*) developer=$1;; esac;;\nesac\nshift\ndone\ncat >/dev/null\n{body}\n"
        );
        fs::write(&binary, script).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let collector = Collector::new(CollectorConfig {
            binary,
            profile: None,
            effort: "low".into(),
            timeout_secs,
            model: MODEL.into(),
        })
        .unwrap();
        (directory, collector)
    }

    fn mock_claude(body: &str) -> (Scratch, Collector) {
        let directory = Scratch::new().unwrap();
        let binary = directory.0.join("claude");
        let script = format!(
            r#"#!/bin/sh
case "$1" in
--version) echo '2.1.263 (Claude Code)'; exit 0;;
--help) echo '--print --output-format --verbose --tools --safe-mode --strict-mcp-config --mcp-config --no-session-persistence --effort'; exit 0;;
esac
tools=missing
safe=missing
strict=missing
persistence=missing
while [ $# -gt 0 ]; do
case "$1" in
--tools) shift; tools=$1;;
--safe-mode) safe=yes;;
--strict-mcp-config) strict=yes;;
--no-session-persistence) persistence=no;;
esac
shift
done
[ -z "$tools" ] && [ "$safe" = yes ] && [ "$strict" = yes ] && [ "$persistence" = no ] || exit 31
cat >/dev/null
{body}
"#
        );
        fs::write(&binary, script).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let collector = Collector::new(CollectorConfig {
            binary,
            profile: None,
            effort: "low".into(),
            timeout_secs: 2,
            model: "claude-fable-5.1".into(),
        })
        .unwrap();
        (directory, collector)
    }

    #[test]
    fn claude_collects_final_result_and_response_metadata() {
        let (_directory, collector) = mock_claude(
            r#"
echo '{"type":"system","subtype":"init","model":"claude-fable-5.1"}'
echo '{"type":"assistant","message":{"id":"msg_123","model":"claude-opus-5.5","content":[{"type":"text","text":"intermediate"}]}}'
printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn","result":"42\n43","usage":{"input_tokens":12,"output_tokens":7}}'
"#,
        );
        let result = collector.collect_text("question");
        assert_eq!(result.error, None);
        assert_eq!(result.answer.as_deref(), Some("42\n43"));
        assert_eq!(result.reported_model.as_deref(), Some("claude-opus-5.5"));
        assert_eq!(result.response_id.as_deref(), Some("msg_123"));
        assert_eq!(result.input_tokens, Some(12));
        assert_eq!(result.output_tokens, Some(7));
    }

    #[test]
    fn claude_structured_answers_are_validated() {
        let (_directory, collector) = mock_claude(
            r#"
echo '{"type":"result","subtype":"success","is_error":false,"result":"{\"answer\":\"42\"}"}'
"#,
        );
        assert_eq!(collector.collect("question").answer.as_deref(), Some("42"));
        let (_directory, collector) = mock_claude(
            r#"
echo '{"type":"result","subtype":"success","is_error":false,"result":"42"}'
"#,
        );
        let result = collector.collect("question");
        assert!(result.error.is_some());
        assert!(result.answer.is_none());
    }

    #[test]
    fn claude_rejects_tools_before_waiting_for_completion() {
        let (_directory, collector) = mock_claude(
            r#"
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"secret-do-not-retain"}}]}}'
sleep 10
"#,
        );
        let result = collector.collect_text("question");
        assert!(result.answer.is_none());
        assert!(result.error.as_deref().unwrap().contains("tool event"));
        assert!(result.duration_ms < 2000);
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("secret-do-not-retain")
        );
    }

    #[test]
    fn claude_rejects_failed_missing_and_truncated_results() {
        for event in [
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"42"}]}}"#,
            r#"{"type":"result","subtype":"success","is_error":true,"result":"secret-do-not-retain"}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"stop_reason":"max_tokens","result":"42"}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"42","permission_denials":[{"tool_name":"Bash"}]}"#,
        ] {
            let (_directory, collector) = mock_claude(&format!("echo '{event}'"));
            let result = collector.collect_text("question");
            assert!(result.answer.is_none());
            assert!(result.error.is_some());
            assert!(
                !serde_json::to_string(&result)
                    .unwrap()
                    .contains("secret-do-not-retain")
            );
        }
    }

    #[test]
    fn collects_only_schema_final_and_metadata() {
        let (_directory, collector) = mock(
            r#"
    echo '{"type":"item.completed","item":{"type":"reasoning","text":"not the answer"}}'
    echo '{"type":"item.completed","item":{"type":"agent_message","text":"also not the final file"}}'
    echo 'TRACE SSE event: {"type":"response.completed","response":{"id":"resp_123","model":"gpt-6-astra","status":"completed","service_tier":"default","usage":{"input_tokens":12,"output_tokens":7,"output_tokens_details":{"reasoning_tokens":3}}}}' >&2
    echo '{"answer":"42"}' > "$output"
    "#,
            2,
        );
        let result = collector.collect("question");
        assert_eq!(result.error, None);
        assert_eq!(result.answer.as_deref(), Some("42"));
        assert_eq!(result.reported_model.as_deref(), Some(MODEL));
        assert_eq!(result.reasoning_tokens, Some(3));
    }

    #[test]
    fn free_text_preserves_multiline_output_without_affecting_schema_mode() {
        let (_directory, collector) = mock(
            r#"
    if [ -n "$schema" ]; then
      case "$developer" in *'JSON schema'*) ;; *) exit 21;; esac
      echo '{"answer":"42"}' > "$output"
    else
      case "$developer" in *JSON*) exit 22;; esac
      printf '12\n34\n56\n' > "$output"
    fi
    echo '{"type":"turn.completed"}'
    "#,
            2,
        );
        let structured = collector.collect("question");
        assert_eq!(structured.error, None);
        assert_eq!(structured.answer.as_deref(), Some("42"));
        let text = collector.collect_text("question");
        assert_eq!(text.error, None);
        assert_eq!(text.answer.as_deref(), Some("12\n34\n56\n"));
        let structured_again = collector.collect("question");
        assert_eq!(structured_again.error, None);
        assert_eq!(structured_again.answer.as_deref(), Some("42"));
    }

    #[test]
    fn free_text_does_not_unwrap_json_and_rejects_tools() {
        let (_directory, collector) = mock(
            "echo '{\"answer\":\"42\"}' > \"$output\"\necho '{\"type\":\"turn.completed\"}'",
            2,
        );
        let result = collector.collect_text("question");
        assert_eq!(result.error, None);
        assert_eq!(result.answer.as_deref(), Some("{\"answer\":\"42\"}\n"));

        let (_directory, collector) = mock(
            "echo '12' > \"$output\"\necho '{\"type\":\"item.started\",\"item\":{\"type\":\"command_execution\"}}'\nsleep 10",
            2,
        );
        let result = collector.collect_text("question");
        assert_eq!(result.answer, None);
        assert!(result.error.unwrap().contains("tool event"));
    }

    #[test]
    fn rejects_tools_even_with_valid_final_answer() {
        let (_directory, collector) = mock(
            r#"
    echo '{"answer":"42"}' > "$output"
    echo '{"type":"item.started","item":{"type":"mcp_tool_call","arguments":{"token":"secret"}}}'
    sleep 10
    "#,
            2,
        );
        let result = collector.collect("question");
        assert!(!result.model_rerouted);
        assert_eq!(result.answer, None);
        assert!(result.error.unwrap().contains("tool event"));
        assert!(result.duration_ms < 2000);
    }

    #[test]
    fn timeout_kills_children_holding_output_pipes() {
        let (_directory, collector) = mock("sleep 10 &\nwait", 1);
        let result = collector.collect("question");
        assert_eq!(result.answer, None);
        assert!(result.error.unwrap().contains("timed out"));
        assert!(result.duration_ms < 3000);
    }

    #[test]
    fn does_not_fallback_to_reasoning_or_status() {
        let (_directory, collector) = mock(
            "echo '{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"42\"}}'",
            2,
        );
        let result = collector.collect("question");
        assert_eq!(result.answer, None);
        assert!(result.error.is_some());
    }

    #[test]
    fn output_limit_rejects_run_without_deadlocking() {
        let (_directory, collector) = mock("dd if=/dev/zero bs=65536 count=80 2>/dev/null", 2);
        let result = collector.collect("question");
        assert_eq!(result.answer, None);
        assert!(result.error.unwrap().contains("exceeded"));
        assert!(result.duration_ms < 3000);
    }

    #[test]
    fn raw_diagnostics_are_not_returned() {
        let (_directory, collector) = mock(
            "echo 'Authorization: Bearer do-not-retain-this' >&2\nexit 1",
            2,
        );
        let result = collector.collect("question");
        assert!(result.error.is_some());
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("do-not-retain")
        );
    }

    #[test]
    fn catches_sse_tools_and_ignores_tool_words_in_answer() {
        assert!(has_tool(
            &json!({"type":"response.output_item.added","item":{"type":"function_call"}})
        ));
        assert!(!has_tool(
            &json!({"type":"item.completed","item":{"type":"agent_message","text":"the word tool is harmless"}})
        ));
    }

    #[test]
    fn nonfatal_cli_diagnostic_is_not_a_tool_call_or_an_answer() {
        let (_directory, collector) = mock(
            r#"
    echo '{"type":"item.completed","item":{"type":"error","message":"configuration warning with secret-do-not-retain"}}'
    echo '{"type":"turn.completed"}'
    echo '{"answer":"42"}' > "$output"
    "#,
            2,
        );
        let result = collector.collect("question");
        assert!(!result.model_rerouted);
        assert_eq!(result.error, None);
        assert_eq!(result.answer.as_deref(), Some("42"));
        assert!(result.evidence.contains(&"item.completed/error".into()));
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("secret-do-not-retain")
        );
    }

    #[test]
    fn rerouting_and_fatal_errors_invalidate_even_a_completed_answer() {
        for (event, rerouted) in [
            (
                r#"{"type":"item.completed","item":{"type":"error","message":"Model rerouted: secret-do-not-retain"}}"#,
                true,
            ),
            (r#"{"type":"error","message":"fatal failure"}"#, false),
            (
                r#"{"type":"turn.failed","error":{"message":"fatal failure"}}"#,
                false,
            ),
            (
                r#"{"type":"error","message":"model rerouted: arbitrary top-level error"}"#,
                false,
            ),
        ] {
            let body = format!(
                "echo '{event}'\necho '{{\"type\":\"turn.completed\"}}'\necho '{{\"answer\":\"42\"}}' > \"$output\""
            );
            let (_directory, collector) = mock(&body, 2);
            let result = collector.collect("question");
            assert!(result.error.is_some());
            assert_eq!(result.answer, None);
            assert_eq!(result.model_rerouted, rerouted);
            assert!(
                !serde_json::to_string(&result)
                    .unwrap()
                    .contains("secret-do-not-retain")
            );
        }
    }

    #[test]
    fn absent_rerouting_flag_preserves_existing_sample_serialization() {
        let original = r#"{"answer":null,"error":null,"duration_ms":0,"reported_model":null,"response_id":null,"service_tier":null,"input_tokens":null,"output_tokens":null,"reasoning_tokens":null,"evidence":[]}"#;
        assert_eq!(
            serde_json::to_string(&SampleOutcome::default()).unwrap(),
            original
        );
        let mut sample: SampleOutcome = serde_json::from_str(original).unwrap();
        assert!(!sample.model_rerouted);
        sample.model_rerouted = true;
        let encoded = serde_json::to_value(&sample).unwrap();
        assert_eq!(encoded["model_rerouted"], true);
    }
}
