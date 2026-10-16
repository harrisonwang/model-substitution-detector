#![cfg(windows)]

mod support;

use msd::collector::{Collector, CollectorConfig};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "msd Windows 中文 &-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        support::install_client(&directory.join("codex.exe"));
        Self(directory)
    }

    fn collector(&self, timeout_secs: u64) -> Collector {
        Collector::new(CollectorConfig {
            binary: self.0.join("codex.exe"),
            profile: None,
            effort: "low".into(),
            timeout_secs,
            model: "gpt-6-astra".into(),
        })
        .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore = "需要本机已安装客户端；只检查版本和能力，不发起模型请求"]
fn installed_clients_support_collector_protocol() {
    let paths = msd::local::LocalPaths::from_env();
    let clients = msd::codex::discover(&paths, "gpt-6-astra");
    assert!(!clients.is_empty(), "no installed Codex client was found");
    for client in clients {
        let collector = Collector::new(CollectorConfig {
            binary: client.binary.clone(),
            profile: None,
            effort: "low".into(),
            timeout_secs: 5,
            model: "gpt-6-astra".into(),
        })
        .unwrap_or_else(|error| panic!("{}: {error:#}", client.binary.display()));
        println!(
            "{} | {} | {}",
            client.source.label(),
            collector.version(),
            client.binary.display()
        );
    }
}

#[test]
fn native_collector_preserves_unicode_quotes_and_shell_metacharacters() {
    let fixture = Fixture::new();
    let collector = fixture.collector(5);
    let answer = "中文回答\nC:\\带 空格\\路径 & | < > %PATH% \"quoted\"";
    for structured in [false, true] {
        let prompt = format!("fixture:echo:{answer}");
        let sample = if structured {
            collector.collect(&prompt)
        } else {
            collector.collect_text(&prompt)
        };
        assert_eq!(sample.error, None);
        assert_eq!(sample.answer.as_deref(), Some(answer));
        assert_eq!(sample.reported_model.as_deref(), Some("gpt-6-astra"));
    }
}

#[test]
fn timeout_and_normal_parent_exit_both_terminate_descendants_holding_pipes() {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    for mode in ["timeout", "orphan"] {
        let fixture = Fixture::new();
        let collector = fixture.collector(2);
        let pid_file = fixture.0.join("child.pid");
        let started = Instant::now();
        let sample = collector.collect_text(&format!("fixture:{mode}:{}", pid_file.display()));
        assert!(sample.answer.is_none());
        let error = sample.error.unwrap();
        if mode == "timeout" {
            assert!(error.contains("timed out"), "{error}");
        }
        assert!(!error.contains("did not close cleanly"), "{error}");
        assert!(started.elapsed().as_secs_f32() < 3.5);
        let pid = fs::read_to_string(pid_file).unwrap().parse().unwrap();
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if !handle.is_null() {
            let status = unsafe { WaitForSingleObject(handle, 1000) };
            unsafe { CloseHandle(handle) };
            assert_eq!(status, WAIT_OBJECT_0, "descendant is still running");
        }
    }
}

#[test]
fn tool_events_and_output_limits_stop_the_windows_client() {
    let fixture = Fixture::new();
    let collector = fixture.collector(5);
    for (prompt, expected) in [
        ("fixture:tool", "tool event"),
        ("fixture:overflow", "exceeded"),
    ] {
        let sample = collector.collect_text(prompt);
        assert!(sample.answer.is_none());
        assert!(
            sample.error.as_deref().unwrap().contains(expected),
            "{sample:?}"
        );
        assert!(sample.duration_ms < 4000);
    }
}
