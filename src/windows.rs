//! Windows 桌面应用与 npm 原生 CLI 的安装布局；不执行 cmd/PowerShell 包装脚本。

use std::path::{Path, PathBuf};

pub(crate) fn application_dirs() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    // WindowsApps 通常不允许列目录。查询当前用户注册的包，兼容商店更新和其他磁盘。
    // 固定命令不插入路径、模型名或其他用户输入，输出只包含安装位置。
    if let Some(root) = std::env::var_os("SystemRoot") {
        let powershell = PathBuf::from(root).join("System32/WindowsPowerShell/v1.0/powershell.exe");
        if powershell.is_absolute()
            && let Ok(output) = crate::collector::metadata_command(
                &powershell,
                &[
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "$ErrorActionPreference='Stop'; [Console]::OutputEncoding=[System.Text.UTF8Encoding]::new($false); Get-AppxPackage | Where-Object { $_.Name -in @('OpenAI.Codex', 'OpenAI.ChatGPT', 'OpenAI.ChatGPT-Desktop') } | ForEach-Object { $_.InstallLocation }",
                ],
            )
        {
            paths.extend(
                output
                    .lines()
                    .map(str::trim)
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute()),
            );
        }
    }
    for variable in ["LOCALAPPDATA", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(root) = std::env::var_os(variable)
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
        {
            for name in ["Codex", "ChatGPT"] {
                paths.push(root.join(name));
                paths.push(root.join("Programs").join(name));
                paths.push(root.join("OpenAI").join(name));
            }
        }
    }
    paths
}

pub(crate) fn desktop_binaries(directories: &[PathBuf]) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let bundled: Vec<_> = directories
        .iter()
        .filter_map(|directory| bundled_binary(directory))
        .collect();
    let cached = directories
        .iter()
        .filter_map(|directory| cached_binary(directory))
        .collect();
    // 缓存是桌面引擎的副本。有已安装的引擎时不将缓存当作另一个备用客户端，
    // 即使应用将它加入 PATH，也应保留桌面来源归属。
    if !bundled.is_empty() {
        (bundled, cached)
    } else {
        (cached, Vec::new())
    }
}

fn bundled_binary(directory: &Path) -> Option<PathBuf> {
    // 只接受资源目录内的引擎；顶层 Codex.exe / ChatGPT.exe 是图形应用。
    for relative in [
        "app/resources/codex.exe",
        "resources/codex.exe",
        "ChatGPT/resources/codex.exe",
        "Codex/resources/codex.exe",
    ] {
        let binary = directory.join(relative);
        if crate::codex::is_executable(&binary) {
            return Some(binary);
        }
    }
    None
}

fn cached_binary(directory: &Path) -> Option<PathBuf> {
    // 桌面应用启动后会将引擎解压到 OpenAI/{Codex,ChatGPT}/bin/<hash>。
    // 优先最近更新的缓存，不按不具备版本含义的哈希目录名排序。
    let bin = directory.join("bin");
    let mut cached: Vec<_> = std::fs::read_dir(&bin)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            if !entry.file_type().ok()?.is_dir() {
                return None;
            }
            let binary = entry.path().join("codex.exe");
            let modified = binary.metadata().ok()?.modified().ok()?;
            crate::codex::is_executable(&binary).then_some((modified, binary))
        })
        .collect();
    cached.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    cached
        .into_iter()
        .next()
        .map(|(_, binary)| binary)
        .or_else(|| {
            let binary = bin.join("codex.exe");
            crate::codex::is_executable(&binary).then_some(binary)
        })
}

pub(crate) fn npm_binary(directory: &Path) -> Option<PathBuf> {
    let modules = directory.join("node_modules/@openai");
    let package = modules.join("codex");
    let targets = if cfg!(target_arch = "aarch64") {
        [("arm64", "aarch64"), ("x64", "x86_64")]
    } else {
        [("x64", "x86_64"), ("arm64", "aarch64")]
    };
    for (arch, target) in targets {
        for root in [
            package.join(format!("node_modules/@openai/codex-win32-{arch}")),
            modules.join(format!("codex-win32-{arch}")),
            package.clone(),
        ] {
            let binary = root.join(format!("vendor/{target}-pc-windows-msvc/codex/codex.exe"));
            if crate::codex::is_executable(&binary) {
                return Some(binary);
            }
        }
    }
    None
}
