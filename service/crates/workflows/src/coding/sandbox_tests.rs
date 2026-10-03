#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::sandbox::{
    ContainerSandbox, HostSandbox, Sandbox, CPUS, MEMORY_LIMIT, PIDS_LIMIT, TMPFS, ULIMITS,
    WORKER_USER,
};
use super::testkit::RecordingExecutor;
use super::{ExecOutput, ExecSpec, ProcessExecutor};
use pair_core::error::ErrorCode;
use std::{path::Path, process::Command, sync::Arc, time::Duration};

const IMAGE: &str = "pair-worker:test";
const COMPOSE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../deploy/worker/compose.worker.yaml"
);

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("pair_t_{tag}_{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn spec(cwd: &Path) -> ExecSpec {
    ExecSpec {
        argv: vec!["cargo".into(), "test".into()],
        cwd: cwd.to_path_buf(),
        env: vec![
            ("HOME".into(), "/host/home".into()),
            ("PATH".into(), "/host/bin".into()),
            ("LANG".into(), "C.UTF-8".into()),
        ],
        timeout: Duration::from_secs(5),
        max_output_bytes: 1024,
    }
}

fn container(exec: &Arc<RecordingExecutor>) -> ContainerSandbox {
    ContainerSandbox::new(exec.clone(), Some(IMAGE.into()))
}

fn has_pair(args: &[String], a: &str, b: &str) -> bool {
    args.windows(2).any(|w| w[0] == a && w[1] == b)
}

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

#[tokio::test]
async fn container_sandbox_args_have_no_network_and_single_mount() {
    let wt = temp_dir("wt");
    let exec = Arc::new(RecordingExecutor::ok());
    container(&exec).run(&spec(&wt)).await.unwrap();
    let calls = exec.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let argv = &calls[0].argv;
    assert_eq!(argv[0], "docker");
    assert_eq!(argv[1], "run");
    assert!(argv.contains(&"--rm".to_string()));
    assert!(has_pair(argv, "--network", "none"));
    assert!(argv.contains(&"--read-only".to_string()));
    assert!(has_pair(argv, "--cap-drop", "ALL"));
    assert!(has_pair(argv, "--security-opt", "no-new-privileges:true"));
    assert!(has_pair(argv, "--user", WORKER_USER));
    assert!(has_pair(argv, "--pids-limit", PIDS_LIMIT));
    assert!(has_pair(argv, "--memory", MEMORY_LIMIT));
    for forbidden in [
        "--privileged",
        "--pid",
        "--ipc",
        "--device",
        "--volumes-from",
    ] {
        assert!(!argv.iter().any(|a| a == forbidden), "{forbidden}");
    }
    // exactly one mount: the task directory, read-write
    let mounts: Vec<&String> = argv
        .windows(2)
        .filter(|w| w[0] == "-v" || w[0] == "--volume" || w[0] == "--mount")
        .map(|w| &w[1])
        .collect();
    let dir = wt.display().to_string();
    assert_eq!(mounts, vec![&format!("{dir}:{dir}:rw")]);
    assert!(has_pair(argv, "-w", &dir));
    // the host's HOME and PATH never enter the container; the image name precedes the command
    assert!(!argv
        .iter()
        .any(|a| a.contains("/host/home") || a.contains("/host/bin")));
    assert!(argv.contains(&"HOME=/tmp/home".to_string()));
    let image_at = argv.iter().position(|a| a == IMAGE).unwrap();
    assert_eq!(&argv[image_at + 1..], ["cargo", "test"]);
    // the docker CLI itself runs with an explicit environment, not the whole host env
    assert!(!calls[0]
        .env
        .iter()
        .any(|(k, _)| k.contains("TOKEN") || k.contains("SECRET")));
}

#[test]
fn host_sandbox_refused_by_default() {
    let exec = Arc::new(RecordingExecutor::ok());
    for lookup in [None, Some("0"), Some("true"), Some("")] {
        let err = HostSandbox::with_lookup(exec.clone(), |_| lookup.map(String::from))
            .err()
            .unwrap_or_else(|| panic!("host sandbox allowed with {lookup:?}"));
        assert_eq!(err.code, ErrorCode::PolicyDenied);
    }
    assert!(HostSandbox::with_lookup(exec, |k| {
        (k == "PAIR_ALLOW_HOST_EXEC").then(|| "1".to_string())
    })
    .is_ok());
}

#[tokio::test]
async fn repo_git_dir_not_writable_in_sandbox() {
    let root = temp_dir("repo");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join(".env"), "SECRET=1\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "i"]);
    let wt = root.join("wt");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "pair/x",
            wt.to_str().unwrap(),
            "HEAD",
        ],
    );
    assert!(
        wt.join(".git").is_file(),
        "a worktree's .git is only a pointer"
    );

    let exec = Arc::new(RecordingExecutor::ok());
    container(&exec).run(&spec(&wt)).await.unwrap();
    {
        let calls = exec.calls.lock().unwrap();
        let joined = calls[0].argv.join("\n");
        // neither the repo, its .git, nor the worktree's admin dir is reachable
        assert!(!joined.contains(repo.to_str().unwrap()), "{joined}");
        assert!(!joined.contains(".git"), "{joined}");
        for m in calls[0].argv.windows(2).filter(|w| w[0] == "-v") {
            assert!(m[1].ends_with(":rw") && m[1].starts_with(wt.to_str().unwrap()));
        }
    }

    // pointing the sandbox at the repository itself (a .git directory) is refused outright
    let err = container(&exec).run(&spec(&repo)).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::PolicyDenied);
    assert_eq!(exec.calls.lock().unwrap().len(), 1, "nothing was executed");
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn unconfigured_container_sandbox_fails_closed() {
    let wt = temp_dir("wt");
    let exec = Arc::new(RecordingExecutor::ok());
    for image in [None, Some("  ".to_string())] {
        let err = ContainerSandbox::new(exec.clone(), image)
            .run(&spec(&wt))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::PolicyDenied);
    }
    assert!(exec.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn container_timeout_kills_the_container_by_name() {
    let wt = temp_dir("wt");
    let timed_out = ExecOutput {
        timed_out: true,
        ..ExecOutput::default()
    };
    let exec = Arc::new(RecordingExecutor::new(vec![
        timed_out,
        ExecOutput::default(),
    ]));
    let out = container(&exec).run(&spec(&wt)).await.unwrap();
    assert!(out.timed_out);
    let calls = exec.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    let name = &calls[0].argv[calls[0].argv.iter().position(|a| a == "--name").unwrap() + 1];
    assert_eq!(
        calls[1].argv,
        vec!["docker".to_string(), "kill".into(), name.clone()]
    );
}

#[tokio::test]
async fn docker_failure_is_an_environment_failure_not_a_test_failure() {
    let wt = temp_dir("wt");
    let failed = ExecOutput {
        exit_code: Some(125),
        stderr: b"Cannot connect to the Docker daemon".to_vec(),
        ..ExecOutput::default()
    };
    let exec = Arc::new(RecordingExecutor::new(vec![failed]));
    let out = container(&exec).run(&spec(&wt)).await.unwrap();
    assert_eq!(out.exit_code, None);
    assert!(out.spawn_error.unwrap().contains("Docker daemon"));
}

/// The sandbox profile must equal deploy/worker/compose.worker.yaml (same pattern as the
/// policy tool-registry test): a change to one without the other fails here.
#[tokio::test]
async fn compose_profile_matches_sandbox_args() {
    let yaml: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&std::fs::read_to_string(COMPOSE).unwrap()).unwrap();
    let svc = &yaml["services"]["worker"];
    assert_eq!(svc["user"].as_str(), Some(WORKER_USER));
    assert_eq!(svc["read_only"].as_bool(), Some(true));
    assert_eq!(svc["network_mode"].as_str(), Some("none"));
    assert_eq!(svc["cap_drop"][0].as_str(), Some("ALL"));
    assert_eq!(
        svc["security_opt"][0].as_str(),
        Some("no-new-privileges:true")
    );
    assert_eq!(
        svc["pids_limit"].as_u64().map(|v| v.to_string()).as_deref(),
        Some(PIDS_LIMIT)
    );
    assert_eq!(svc["mem_limit"].as_str(), Some(MEMORY_LIMIT));
    assert_eq!(svc["memswap_limit"].as_str(), Some(MEMORY_LIMIT));
    assert_eq!(svc["cpus"].as_f64(), CPUS.parse::<f64>().ok());
    assert_eq!(svc["tmpfs"][0].as_str(), Some(TMPFS));
    let nofile = &svc["ulimits"]["nofile"];
    let ulimit_nofile = format!(
        "nofile={}:{}",
        nofile["soft"].as_u64().unwrap(),
        nofile["hard"].as_u64().unwrap()
    );
    let ulimit_nproc = format!("nproc={}", svc["ulimits"]["nproc"].as_u64().unwrap());
    let ulimit_fsize = format!("fsize={}", svc["ulimits"]["fsize"].as_u64().unwrap());
    assert_eq!(
        ULIMITS.to_vec(),
        vec![ulimit_nofile, ulimit_nproc, ulimit_fsize]
    );
    // and the args really carry every one of those settings
    let wt = temp_dir("wt");
    let exec = Arc::new(RecordingExecutor::ok());
    container(&exec).run(&spec(&wt)).await.unwrap();
    let argv = exec.calls.lock().unwrap()[0].argv.clone();
    assert!(has_pair(&argv, "--tmpfs", TMPFS));
    assert!(has_pair(&argv, "--cpus", CPUS));
    assert!(has_pair(&argv, "--memory-swap", MEMORY_LIMIT));
    for u in ULIMITS {
        assert!(has_pair(&argv, "--ulimit", u), "{u}");
    }
}

#[test]
fn default_runner_uses_the_container_sandbox() {
    use super::{Runner, RunnerSetup};
    use pair_core::ids::{TaskId, TraceId};
    use pair_policy::Gate;
    let gate = Gate::new(Arc::new(super::testkit::FakePolicy::default()), None);
    let runner = Runner::new(
        &gate,
        RunnerSetup {
            task: TaskId::new(),
            trace: TraceId::new(),
            ctx: super::testkit::fake_ctx(&std::env::temp_dir()),
            home: std::env::temp_dir(),
            timeout: Duration::from_secs(1),
            passthrough: Vec::new(),
            data_class: pair_core::types::DataClass::Personal,
        },
    );
    assert_eq!(runner.sandbox_name(), "container");
    let _ = ProcessExecutor;
}
