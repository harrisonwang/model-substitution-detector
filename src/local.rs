//! 读取本地会话元数据，不启动客户端或发起模型请求。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const MAX_FILES: usize = 128;
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_LINE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Source {
    ChatGptDesktop,
    CodexCli,
    ClaudeDesktopCode,
    ClaudeCli,
}

impl Source {
    pub fn label(&self) -> &'static str {
        match self {
            Self::ChatGptDesktop => "ChatGPT",
            Self::CodexCli => "Codex CLI",
            Self::ClaudeDesktopCode => "Claude Desktop (Code)",
            Self::ClaudeCli => "Claude CLI",
        }
    }

    pub fn provider(&self) -> &'static str {
        match self {
            Self::ChatGptDesktop | Self::CodexCli => "openai",
            Self::ClaudeDesktopCode | Self::ClaudeCli => "anthropic",
        }
    }

    pub fn is_desktop(&self) -> bool {
        matches!(self, Self::ChatGptDesktop | Self::ClaudeDesktopCode)
    }

    pub fn is_claude(&self) -> bool {
        matches!(self, Self::ClaudeDesktopCode | Self::ClaudeCli)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Observation {
    pub source: Source,
    pub installed: bool,
    /// 记录中选择的模型不代表实际响应模型，也不能证明服务端的路由结果。
    pub selected_model: Option<String>,
    pub reported_model: Option<String>,
    pub timestamp: Option<String>,
    pub error: Option<String>,
}

pub struct LocalPaths {
    pub codex_home: PathBuf,
    pub claude_home: PathBuf,
    pub applications: Vec<PathBuf>,
    pub executable_dirs: Vec<PathBuf>,
}

impl LocalPaths {
    pub fn from_env() -> Self {
        let home_variable = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let home = std::env::var_os(home_variable)
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from);
        let directory = |variable, name| {
            std::env::var_os(variable)
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|home| home.join(name)))
                .unwrap_or_default()
        };
        let applications = std::env::var_os("MSD_APPLICATION_DIRS")
            .map(|paths| {
                std::env::split_paths(&paths)
                    .filter(|path| path.is_absolute())
                    .collect()
            })
            .unwrap_or_else(|| {
                let mut paths = vec![PathBuf::from("/Applications")];
                if let Some(home) = &home {
                    paths.push(home.join("Applications"));
                }
                paths
            });
        Self {
            codex_home: directory("CODEX_HOME", ".codex"),
            claude_home: directory("CLAUDE_CONFIG_DIR", ".claude"),
            applications,
            executable_dirs: std::env::var_os("PATH")
                .map(|path| {
                    std::env::split_paths(&path)
                        .filter(|path| path.is_absolute())
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

pub fn inspect(paths: &LocalPaths) -> Vec<Observation> {
    let app = |name: &str| paths.applications.iter().any(|dir| dir.join(name).is_dir());
    let binary = |name: &str| {
        paths
            .executable_dirs
            .iter()
            .any(|dir| crate::codex::is_executable(&crate::codex::executable_path(dir, name)))
    };
    let mut observations = vec![
        empty(Source::ChatGptDesktop, app("ChatGPT.app")),
        empty(Source::CodexCli, binary("codex")),
        empty(Source::ClaudeDesktopCode, app("Claude.app")),
        empty(Source::ClaudeCli, binary("claude")),
    ];
    for (home, folder, depth, codex) in [
        (&paths.codex_home, "sessions", 3, true),
        (&paths.claude_home, "projects", 1, false),
    ] {
        if home.as_os_str().is_empty() {
            continue;
        }
        let Ok(files) = recent_sessions(&home.join(folder), depth) else {
            continue;
        };
        for (_, path) in files {
            let Ok(found) = read_session(&path, codex) else {
                continue;
            };
            for item in found {
                let target = observations
                    .iter_mut()
                    .find(|target| target.source == item.source)
                    .expect("all session sources have an observation");
                if target.selected_model.is_none() && target.reported_model.is_none() {
                    target.selected_model = item.selected_model;
                    target.reported_model = item.reported_model;
                    target.timestamp = item.timestamp;
                    target.error = item.error;
                }
            }
            let complete = if codex {
                observations[..2]
                    .iter()
                    .all(|item| item.selected_model.is_some())
            } else {
                observations[2..]
                    .iter()
                    .all(|item| item.reported_model.is_some())
            };
            if complete {
                break;
            }
        }
    }
    observations
}

fn empty(source: Source, installed: bool) -> Observation {
    Observation {
        source,
        installed,
        selected_model: None,
        reported_model: None,
        timestamp: None,
        error: Some("未找到可用的本机会话模型元数据（最多读取最近 128 个会话）".into()),
    }
}

fn recent_sessions(root: &Path, depth: usize) -> io::Result<Vec<(SystemTime, PathBuf)>> {
    fn visit(root: &Path, depth: usize, files: &mut Vec<(SystemTime, PathBuf)>) -> io::Result<()> {
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = entry.path();
            if kind.is_dir() && depth > 0 {
                visit(&path, depth - 1, files)?;
            } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
                let metadata = entry.metadata()?;
                if metadata.len() <= MAX_FILE_BYTES {
                    files.push((metadata.modified()?, path));
                }
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    visit(root, depth, &mut files)?;
    files.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    files.truncate(MAX_FILES);
    Ok(files)
}

// serde 会忽略未知字段，包括提示词和回答内容。
#[derive(Default, Deserialize)]
struct Entry {
    #[serde(default, rename = "type")]
    kind: String,
    timestamp: Option<String>,
    #[serde(default)]
    payload: Payload,
    entrypoint: Option<String>,
    #[serde(rename = "isSidechain")]
    is_sidechain: Option<bool>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    #[serde(default)]
    message: Message,
}

#[derive(Default, Deserialize)]
struct Payload {
    id: Option<String>,
    source: Option<Value>,
    originator: Option<String>,
    thread_source: Option<String>,
    model: Option<String>,
}

#[derive(Default, Deserialize)]
struct Message {
    model: Option<String>,
}

fn read_session(path: &Path, codex: bool) -> io::Result<Vec<Observation>> {
    let mut reader = BufReader::new(File::open(path)?.take(MAX_FILE_BYTES + 1));
    let mut line = Vec::new();
    let mut source = None;
    let mut session_id = None;
    let mut found: Vec<Observation> = Vec::new();
    loop {
        line.clear();
        let count = reader
            .by_ref()
            .take(MAX_LINE_BYTES + 1)
            .read_until(b'\n', &mut line)?;
        if count == 0 {
            break;
        }
        if count as u64 > MAX_LINE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "session line too large",
            ));
        }
        let Ok(entry) = serde_json::from_slice::<Entry>(&line) else {
            continue;
        };
        if codex {
            if entry.kind == "session_meta" {
                let parsed_source = codex_source(&entry.payload);
                if session_id.is_some() && session_id != entry.payload.id {
                    return Ok(Vec::new());
                }
                session_id = entry.payload.id;
                source = parsed_source;
                if source.is_none() || session_id.is_none() {
                    return Ok(Vec::new());
                }
            } else if entry.kind == "turn_context"
                && let (Some(source), Some(model)) = (source, valid_model(entry.payload.model))
            {
                let mut item = empty(source, false);
                item.selected_model = Some(model);
                item.timestamp = valid_timestamp(entry.timestamp);
                item.error = Some("仅有会话所选模型，未获得服务端返回模型".into());
                found.clear();
                found.push(item);
            }
        } else if entry.kind == "assistant" && entry.is_sidechain == Some(false) {
            let source = match entry.entrypoint.as_deref() {
                Some("cli") => Source::ClaudeCli,
                Some("claude-desktop") => Source::ClaudeDesktopCode,
                _ => continue,
            };
            if entry.session_id.is_none() {
                continue;
            }
            if session_id.is_some() && session_id != entry.session_id {
                return Ok(Vec::new());
            }
            session_id = entry.session_id;
            if let Some(model) = valid_model(entry.message.model) {
                let mut item = empty(source, false);
                item.reported_model = Some(model);
                item.timestamp = valid_timestamp(entry.timestamp);
                item.error = Some("历史响应声明；未记录同次请求的所选模型".into());
                found.retain(|previous| previous.source != source);
                found.push(item);
            }
        }
    }
    Ok(found)
}

fn codex_source(payload: &Payload) -> Option<Source> {
    if payload
        .thread_source
        .as_deref()
        .is_some_and(|source| source != "user")
    {
        return None;
    }
    let source = payload.source.as_ref()?.as_str()?;
    match payload.originator.as_deref() {
        Some("Codex Desktop" | "ChatGPT Desktop") if matches!(source, "vscode" | "appServer") => {
            Some(Source::ChatGptDesktop)
        }
        Some("codex-tui" | "codex_exec") if matches!(source, "cli" | "exec" | "vscode") => {
            Some(Source::CodexCli)
        }
        _ => None,
    }
}

fn valid_model(model: Option<String>) -> Option<String> {
    model.filter(|model| {
        !model.is_empty()
            && model.len() <= 200
            && model
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:/".contains(&byte))
    })
}

fn valid_timestamp(timestamp: Option<String>) -> Option<String> {
    timestamp.filter(|timestamp| {
        timestamp.len() <= 64
            && timestamp
                .bytes()
                .all(|byte| byte.is_ascii_digit() || b"-:+.TZ".contains(&byte))
    })
}
