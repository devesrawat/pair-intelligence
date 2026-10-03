#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::exec::{CommandExecutor, ExecSpec, ProcessExecutor};
use std::{path::PathBuf, time::Duration};

fn spec(script: &str, timeout: Duration, cap: usize) -> ExecSpec {
    ExecSpec {
        argv: vec!["sh".into(), "-c".into(), script.into()],
        cwd: std::env::temp_dir(),
        env: vec![(
            "PATH".into(),
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        )],
        timeout,
        max_output_bytes: cap,
    }
}

fn alive(pid: i32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn temp_file() -> PathBuf {
    std::env::temp_dir().join(format!("pair_t_pid_{}", uuid::Uuid::new_v4().simple()))
}

#[tokio::test]
async fn timeout_kills_grandchildren() {
    let pidfile = temp_file();
    // the shell backgrounds a long sleeper (a grandchild of the executor) and waits on it
    let script = format!("sleep 60 & echo $! > {}; wait", pidfile.display());
    let started = std::time::Instant::now();
    let out = ProcessExecutor
        .exec(&spec(&script, Duration::from_millis(700), 1024))
        .await;
    assert!(out.timed_out);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "must not wait for the sleeper"
    );
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    std::fs::remove_file(&pidfile).unwrap();
    // give the kernel a moment to reap
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!alive(pid), "grandchild {pid} survived the timeout");
}

#[tokio::test]
async fn background_children_do_not_outlive_a_finished_command() {
    let pidfile = temp_file();
    let script = format!("sleep 60 & echo $! > {}", pidfile.display());
    let out = ProcessExecutor
        .exec(&spec(&script, Duration::from_secs(10), 1024))
        .await;
    assert_eq!(out.exit_code, Some(0));
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    std::fs::remove_file(&pidfile).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!alive(pid));
}

#[tokio::test]
async fn output_capped_while_streaming() {
    const CAP: usize = 4096;
    const PRODUCED: u64 = 20 * 1024 * 1024;
    let script = format!(
        "head -c {PRODUCED} /dev/zero | tr '\\0' x; head -c {PRODUCED} /dev/zero | tr '\\0' y >&2"
    );
    let out = ProcessExecutor
        .exec(&spec(&script, Duration::from_secs(60), CAP))
        .await;
    assert_eq!(out.exit_code, Some(0));
    assert_eq!(out.stdout.len(), CAP, "only the tail is kept");
    assert_eq!(out.stderr.len(), CAP);
    assert_eq!(out.stdout_total, PRODUCED);
    assert_eq!(out.stderr_total, PRODUCED);
    assert!(out.truncated());
    assert!(out.stdout.iter().all(|b| *b == b'x'));
}

#[tokio::test]
async fn missing_binary_is_a_spawn_error_not_a_panic() {
    let mut s = spec("true", Duration::from_secs(1), 16);
    s.argv = vec!["pair-no-such-binary-xyz".into()];
    let out = ProcessExecutor.exec(&s).await;
    assert!(out.spawn_error.is_some());
    assert_eq!(out.exit_code, None);
}
