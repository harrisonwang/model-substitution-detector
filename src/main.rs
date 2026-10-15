use clap::Parser;
use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(version, about = "检测指定模型是否被替换")]
struct Cli {
    /// 待测模型（必填）；只检测一个可用入口，GPT 优先使用 ChatGPT 内置引擎
    #[arg(short, long, value_parser = model_name)]
    model: String,
    /// API 地址（预留，尚未实现）
    #[arg(long, value_name = "URL")]
    api: Option<String>,
    /// 显示四类检测的状态、原因及最终结论
    #[arg(short, long)]
    verbose: bool,
}

fn model_name(value: &str) -> Result<String, String> {
    if !(value.starts_with("gpt-") && value.len() > 4
        || value.starts_with("claude-") && value.len() > 7)
        || value.len() > 200
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:/".contains(&byte))
    {
        return Err("请填写完整的 gpt-* 或 claude-* 模型名，只能包含字母、数字和 -_.:/".into());
    }
    Ok(value.into())
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let failed = error.use_stderr();
            let _ = error.print();
            return if failed {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    if cli.api.is_some() {
        if cli.verbose {
            eprintln!("msd: API 检测尚未实现");
        }
        let _ = writeln!(io::stdout().lock(), "{} ?", cli.model);
        return ExitCode::FAILURE;
    }
    let report = detect(&cli.model);
    for finding in &report.findings {
        if cli.verbose
            && let Some(error) = &finding.error
        {
            eprintln!("msd: {}：{error}", finding.source);
        }
    }
    let mut stdout = io::stdout().lock();
    let output = if cli.verbose {
        writeln!(stdout, "{report:#}")
    } else {
        writeln!(stdout, "{report}")
    };
    if output.is_err() {
        return ExitCode::FAILURE;
    }
    ExitCode::from(report.exit_code())
}

fn detect(model: &str) -> msd::verdict::Report {
    if !io::stderr().is_terminal() {
        return msd::detect::run(model, |_| {});
    }
    thread::scope(|scope| {
        let (sender, receiver) = mpsc::channel();
        let display = scope.spawn(|| show_progress(receiver));
        let report = msd::detect::run(model, |progress| {
            let _ = sender.send(progress);
        });
        drop(sender);
        let _ = display.join();
        report
    })
}

fn show_progress(receiver: mpsc::Receiver<msd::detect::Progress>) {
    let started = Instant::now();
    let mut stderr = io::stderr().lock();
    let _ = write!(stderr, "正在检测…");
    let _ = stderr.flush();
    let mut current = None;
    loop {
        match receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(progress) => current = Some(progress),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        let seconds = started.elapsed().as_secs();
        let mut line = match current {
            Some(progress) => {
                let count = if progress.total == 0 {
                    String::new()
                } else {
                    format!(" {}/{}", progress.completed, progress.total)
                };
                if progress.stage == "准备" && progress.entries > 1 {
                    format!(
                        "{} · 准备（尝试入口 {}/{}）",
                        progress.source, progress.entry, progress.entries
                    )
                } else {
                    format!("{} · {}{}", progress.source, progress.stage, count)
                }
            }
            None => "正在检测…".into(),
        };
        line.push_str(&format!(" · {:02}:{:02}", seconds / 60, seconds % 60));
        if write!(stderr, "\r\x1b[2K{line}")
            .and_then(|_| stderr.flush())
            .is_err()
        {
            break;
        }
    }
    let _ = write!(stderr, "\r\x1b[2K");
    let _ = stderr.flush();
}
