//! 由 tests/support/mod.rs 使用 rustc 编译，不作为产品二进制发布。
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::process::{self, Command};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const REFERENCES: &[(&str, &str, bool)] = &[/* KNOWLEDGE_REFERENCES */];

fn quoted(text: &str) -> String {
    let mut result = String::from("\"");
    for c in text.chars() {
        match c {
            '\\' => result.push_str("\\\\"),
            '"' => result.push_str("\\\""),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            c if c.is_control() => result.push_str(&format!("\\u{:04x}", c as u32)),
            c => result.push(c),
        }
    }
    result.push('"');
    result
}

fn append(variable: &str, value: &str) {
    if let Some(path) = env::var_os(variable) {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap()
            .write_all(format!("{value}\n").as_bytes())
            .unwrap();
    }
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--hold-pipes") {
        // 告知父测试后代 PID，保持继承的管道开启。
        fs::write(&args[1], process::id().to_string()).unwrap();
        thread::sleep(Duration::from_secs(10));
        return;
    }
    let executable = env::current_exe().unwrap();
    let claude = executable.file_stem().unwrap() == "claude";
    let desktop = executable
        .components()
        .any(|c| c.as_os_str() == "Applications");
    let source = if desktop {
        "desktop"
    } else if claude {
        "claude"
    } else {
        "cli"
    };
    let mode = env::var(if desktop {
        "MSD_DESKTOP_MODE"
    } else if claude {
        "MSD_MODE"
    } else {
        "MSD_CLI_MODE"
    })
    .unwrap_or_default();
    let first = args.first().map(String::as_str).unwrap_or("");
    append("MSD_LAUNCHES", &format!("{source}:{first}"));
    if first == "--version" {
        println!(
            "{}",
            if claude {
                "2.1.263 (Claude Code)"
            } else {
                "codex-cli test"
            }
        );
        return;
    }
    if first == "features" {
        println!("shell_tool stable true");
        return;
    }
    if args.iter().take(2).any(|arg| arg == "--help") {
        if mode == "incompatible" {
            println!("unsupported old client");
            return;
        }
        println!(
            "--ephemeral --json --output-schema --output-last-message --print --output-format --verbose --tools --safe-mode --strict-mcp-config --mcp-config --no-session-persistence --effort"
        );
        return;
    }
    if mode == "failure" {
        append("MSD_CALLS", "failed");
        if !claude {
            println!(
                r#"{{"type":"thread.started"}}
{{"type":"turn.started"}}
{{"type":"item.completed","item":{{"type":"error","message":"configuration warning"}}}}"#
            );
        }
        process::exit(1);
    }
    let argument = |name: &str| {
        args.windows(2)
            .find(|pair| pair[0] == name)
            .map(|pair| pair[1].as_str())
    };
    let mut model = argument("--model").unwrap_or("").to_string();
    let structured = argument("--output-schema").is_some()
        || argument("--system-prompt")
            .is_some_and(|s| s.contains("exactly one string field named answer"));
    let mut prompt = String::new();
    std::io::stdin().read_to_string(&mut prompt).unwrap();
    if let Some(path) = env::var_os("MSD_PROMPTS") {
        // Windows 会快速复用 PID，不能仅用 PID 命名采样记录。
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        fs::write(
            std::path::PathBuf::from(path).join(format!("{}-{stamp}", process::id())),
            &prompt,
        )
        .unwrap();
    }
    // 收集器回归用例：只由测试提示词触发，不依赖全局环境变量。
    if let Some(pid_file) = prompt
        .strip_prefix("fixture:timeout:")
        .or_else(|| prompt.strip_prefix("fixture:orphan:"))
    {
        let mut child = Command::new(&executable)
            .args(["--hold-pipes", pid_file])
            .spawn()
            .unwrap();
        while !std::path::Path::new(pid_file).exists() {
            thread::sleep(Duration::from_millis(5));
        }
        if prompt.starts_with("fixture:timeout:") {
            let _ = child.wait();
        }
        return;
    }
    if prompt == "fixture:tool" {
        println!(r#"{{"type":"item.started","item":{{"type":"command_execution"}}}}"#);
        std::io::stdout().flush().unwrap();
        thread::sleep(Duration::from_secs(10));
        return;
    }
    if prompt == "fixture:overflow" {
        std::io::stdout()
            .write_all(&vec![b'x'; 5 * 1024 * 1024])
            .unwrap();
        return;
    }
    let stage = if prompt.contains("独立的数值选择记录") {
        "integer"
    } else if prompt.contains("Closed-book numerical recall") {
        "knowledge"
    } else {
        "capability"
    };
    let reference = REFERENCES
        .iter()
        .find(|(question, _, _)| prompt.contains(question));
    let initial = prompt.contains("print(sum(i * i")
        || prompt.contains("n 除以 7 余 3")
        || stage == "integer"
        || reference.is_some_and(|(_, _, initial)| *initial);
    let mut answer = if stage == "integer" {
        let count = if mode == "confident" { 320 } else { 292 };
        (0..count)
            .map(|i| {
                let n = if mode == "confident" {
                    (i * 73 + 19) % 355 + 1
                } else {
                    (i * 37 + i * i * 11) % 355 + 1
                };
                format!(" {n}")
            })
            .collect::<String>()
    } else if stage == "knowledge" {
        if mode == "confident" {
            reference.expect("unknown reference").1.to_string()
        } else {
            "2".into()
        }
    } else {
        [
            ("terrarium refrigerator", "7"),
            ("The quick brown fox jumps", "fox"),
            ("ZX9-QY7-KP3", "ZX9-QY7-KP3"),
            ("banana", "A-N-A-N-A-B"),
            ("print(sum(i * i", "259"),
            ("2027-02-14", "星期日"),
            ("岛上只有骑士", "骑士"),
            ("小于 1000", "400"),
            ("n 除以 7 余 3", "654"),
        ]
        .into_iter()
        .find(|(question, _)| prompt.contains(question))
        .map_or("2", |(_, answer)| answer)
        .to_string()
    };
    append("MSD_CALLS", stage);
    if mode == "tool_event" {
        println!(r#"{{"type":"item.started","item":{{"type":"command_execution"}}}}"#);
        return;
    }
    if mode == "model_rerouted" || (mode == "late_rerouted" && answer == "654") {
        println!(
            r#"{{"type":"item.completed","item":{{"id":"item_0","type":"error","message":"Model rerouted: requested gpt-6-astra, using another model (private-reroute-detail)"}}}}"#
        );
    }
    if (mode == "late_failure" && answer == "654") || (mode == "additional_failure" && !initial) {
        process::exit(1);
    }
    if mode == "bad_capability" && answer == "654" {
        answer = "wrong".into();
    }
    if mode == "mismatch"
        || (mode == "additional_mismatch" && !initial)
        || (matches!(mode.as_str(), "integer_mismatch" | "invalid_mismatch") && stage == "integer")
        || (mode == "knowledge_mismatch" && stage == "knowledge")
    {
        model = "gpt-6-luna".into();
    }
    if mode == "no_declarations" {
        model.clear();
    }
    if mode == "empty_answer" {
        answer.clear();
    }
    if let Some(text) = prompt.strip_prefix("fixture:echo:") {
        answer = text.to_string();
    }
    if structured {
        answer = format!("{{\"answer\":{}}}", quoted(&answer));
    }
    if claude {
        println!(
            "{{\"type\":\"assistant\",\"message\":{{\"id\":\"msg_{}\",\"model\":{},\"content\":[{{\"type\":\"text\",\"text\":{}}}]}}}}",
            process::id(),
            quoted(&model),
            quoted(&answer)
        );
        println!(
            "{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":{}}}",
            quoted(&answer)
        );
    } else {
        fs::write(argument("--output-last-message").unwrap(), &answer).unwrap();
        eprintln!(
            "TRACE SSE event: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_{}\",\"model\":{},\"status\":\"completed\"}}}}",
            process::id(),
            quoted(&model)
        );
    }
    if mode == "invalid_mismatch" && stage == "integer" {
        process::exit(1);
    }
}
