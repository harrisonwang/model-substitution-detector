//! 查找可运行的本地客户端，包括 ChatGPT 内置引擎。

use crate::local::{LocalPaths, Source};
use anyhow::{Result, anyhow};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[cfg(not(windows))]
const EMBEDDED: &str = "ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex";

#[derive(Debug, Clone)]
pub struct Client {
    pub source: Source,
    pub binary: PathBuf,
}

/// 优先查找应用内置引擎，再查找 PATH，确保别名仍归属于原应用。
pub fn discover(paths: &LocalPaths, model: &str) -> Vec<Client> {
    let claude = model.starts_with("claude-");
    let mut clients = Vec::new();
    let mut seen = HashSet::new();
    #[cfg(windows)]
    let (desktop, aliases) = if claude {
        (Vec::new(), Vec::new())
    } else {
        crate::windows::desktop_binaries(&paths.applications)
    };
    #[cfg(windows)]
    seen.extend(aliases.iter().filter_map(|path| path.canonicalize().ok()));
    let mut add = |source, binary: PathBuf| {
        if is_executable(&binary)
            && let Ok(canonical) = binary.canonicalize()
            && seen.insert(canonical)
        {
            clients.push(Client { source, binary });
        }
    };
    if !claude {
        #[cfg(not(windows))]
        for directory in &paths.applications {
            add(Source::ChatGptDesktop, directory.join(EMBEDDED));
        }
        #[cfg(windows)]
        for binary in desktop {
            add(Source::ChatGptDesktop, binary);
        }
    }
    let (source, name) = if claude {
        (Source::ClaudeCli, "claude")
    } else {
        (Source::CodexCli, "codex")
    };
    for directory in &paths.executable_dirs {
        add(source, executable_path(directory, name));
        #[cfg(windows)]
        if !claude && let Some(binary) = crate::windows::npm_binary(directory) {
            add(source, binary);
        }
    }
    clients
}

pub fn find_binary(model: &str) -> Result<PathBuf> {
    discover(&LocalPaths::from_env(), model)
        .into_iter()
        .next()
        .map(|client| client.binary)
        .ok_or_else(|| {
            if model.starts_with("claude-") {
                anyhow!("未找到可采样的 Claude 入口；当前支持 PATH 中的 claude")
            } else {
                anyhow!("未找到 ChatGPT/Codex 桌面应用内置引擎或 PATH 中的 Codex CLI")
            }
        })
}

pub(crate) fn executable_path(directory: &Path, name: &str) -> PathBuf {
    // 直接调用 Windows 原生可执行文件；.cmd 包装脚本会让模型名和采集配置
    // 经过 cmd.exe 的参数解析。
    directory.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
}

pub(crate) fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}
