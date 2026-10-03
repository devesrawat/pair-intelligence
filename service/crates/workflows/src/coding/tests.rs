#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::testkit::*;
use super::*;
use pair_core::{
    error::ErrorCode,
    ids::{TaskId, TraceId},
    types::Decision,
};
use pair_policy::Gate;
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};

const SECRET_ENV_VALUE: &str = "tok_live_9f8e7d6c5b4a_do_not_leak";
const SECRET_FILE_VALUE: &str = "DB_PASSWORD=hunter2hunter2";

/// Temp repo + workspaces root, removed on drop.
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(acceptance: &str) -> Self {
        Self::with_class(acceptance, Some("personal"))
    }

    /// `class` is written into repo.json as `data_class`; `None` omits the key.
    fn with_class(acceptance: &str, class: Option<&str>) -> Self {
        let root =
            std::env::temp_dir().join(format!("pair_t_wf_{}", uuid::Uuid::new_v4().simple()));
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::create_dir_all(repo.join(".pair")).unwrap();
        std::fs::create_dir_all(root.join("ws")).unwrap();
        std::fs::write(repo.join("src/lib.txt"), "old\n").unwrap();
        std::fs::write(repo.join("README.md"), "readme\n").unwrap();
        std::fs::write(repo.join(".env"), format!("{SECRET_FILE_VALUE}\n")).unwrap();
        let class_field = class.map_or(String::new(), |c| format!(r#","data_class":"{c}""#));
        let cfg = format!(r#"{{"acceptance":[{acceptance}],"timeout_secs":2{class_field}}}"#);
        std::fs::write(repo.join(".pair/repo.json"), cfg).unwrap();
        let fx = Self { root };
        fx.git(&["init", "-q", "-b", "main"]);
        fx.git(&["add", "-A"]);
        fx.git(&["commit", "-q", "-m", "init"]);
        fx
    }
    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }
    fn git(&self, args: &[&str]) {
        let status = Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(self.repo())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }
    fn task(&self, scope: &[&str]) -> CodingTask {
        CodingTask {
            id: TaskId::new(),
            trace: TraceId::new(),
            issue: "change old to new".into(),
            repo: self.repo(),
            scope: Scope::new(scope.iter().map(|s| (*s).to_string()).collect()),
            workspaces_root: self.root.join("ws"),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

const PASS_ACCEPTANCE: &str = r#"["sh","-c","grep -q new src/lib.txt"]"#;

fn edit_json(files: &[(&str, &str)]) -> String {
    let edits: Vec<_> = files
        .iter()
        .map(|(p, c)| serde_json::json!({"path": p, "content": c}))
        .collect();
    serde_json::json!({ "edits": edits }).to_string()
}

fn provider_with_edit(edit: String) -> FnProvider {
    FnProvider::scripted(vec![
        "plan: edit lib".into(),
        edit,
        "review: looks fine".into(),
    ])
}

async fn run(
    fx_task: &CodingTask,
    provider: &FnProvider,
    policy: &Arc<FakePolicy>,
) -> pair_core::error::Result<CodingResult> {
    run_limited(
        fx_task,
        provider,
        policy,
        Arc::new(crate::limits::RunLimits::interactive()),
    )
    .await
}

async fn run_limited(
    fx_task: &CodingTask,
    provider: &FnProvider,
    policy: &Arc<FakePolicy>,
    limits: Arc<crate::limits::RunLimits>,
) -> pair_core::error::Result<CodingResult> {
    let budget = FakeBudget::default();
    let gate = Gate::unaudited_for_tests(policy.clone(), None);
    let deps = CodingDeps {
        provider,
        gate: &gate,
        budget: &budget,
        prices: &FixedPrices::standard(),
        planner: &FixedPlanner::single(),
        memory: &EmptyMemory,
        compiler: &PlainCompiler,
        policy: fake_ctx(&fx_task.workspaces_root),
        sandbox: host_sandbox(),
        limits,
    };
    run_coding_task(&deps, fx_task).await
}

#[tokio::test]
async fn coding_task_happy_path_yields_scoped_reviewed_patch() {
    let fx = Fixture::new(PASS_ACCEPTANCE);
    let task = fx.task(&["src/"]);
    let policy = Arc::new(FakePolicy::default());
    let provider = provider_with_edit(edit_json(&[("src/lib.txt", "new\n")]));
    let res = run(&task, &provider, &policy).await.unwrap();
    assert_eq!(res.status, CodingStatus::Succeeded);
    assert_eq!(res.changed_files, vec!["src/lib.txt".to_string()]);
    assert!(res.diff.contains("+new"));
    assert!(res.review.is_some());
    assert_eq!(res.stages.last(), Some(&Stage::Draft));
    assert!(res.branch.starts_with("pair/"));
    // every executed command went through policy, and none was a remote write
    let seen = policy.seen.lock().unwrap();
    assert!(seen.iter().all(|r| matches!(
        r.tool.as_str(),
        crate::tools::SHELL_EXEC | crate::tools::FS_WRITE
    )));
    assert!(seen.iter().all(|r| !r.args.iter().any(|a| a == "push")));
    // original checkout untouched
    assert_eq!(
        std::fs::read_to_string(fx.repo().join("src/lib.txt")).unwrap(),
        "old\n"
    );
}

#[tokio::test]
async fn coding_plan_and_diff_are_wrapped_as_data() {
    let fx = Fixture::new(PASS_ACCEPTANCE);
    let task = fx.task(&["src/"]);
    let provider = provider_with_edit(edit_json(&[("src/lib.txt", "new\n")]));
    run(&task, &provider, &Arc::new(FakePolicy::default()))
        .await
        .unwrap();
    let seen = provider.seen.lock().unwrap();
    let non_owner: Vec<_> = seen
        .iter()
        .flat_map(|r| r.messages.iter())
        .filter(|m| m.trust != pair_core::types::TrustClass::Owner)
        .collect();
    assert_eq!(
        non_owner.len(),
        2,
        "plan (edit step) and diff (review step)"
    );
    for m in non_owner {
        assert!(
            m.content.starts_with(pair_context::DATA_OPEN),
            "{}",
            m.content
        );
    }
}

#[tokio::test]
async fn repository_mutation_during_run_stops_task() {
    let fx = Fixture::new(PASS_ACCEPTANCE);
    let task = fx.task(&["src/"]);
    let repo = fx.repo();
    // The "model" call number 1 (edit) lands while someone commits to the checkout.
    let provider = FnProvider::new(Box::new(move |i, _| {
        if i == 1 {
            std::fs::write(repo.join("README.md"), "changed underneath\n").unwrap();
            for args in [vec!["add", "-A"], vec!["commit", "-q", "-m", "mutation"]] {
                let ok = Command::new("git")
                    .args([
                        "-c",
                        "user.name=t",
                        "-c",
                        "user.email=t@t",
                        "-c",
                        "commit.gpgsign=false",
                    ])
                    .args(&args)
                    .current_dir(&repo)
                    .status()
                    .unwrap()
                    .success();
                assert!(ok);
            }
        }
        Ok(if i == 1 {
            edit_json(&[("src/lib.txt", "new\n")])
        } else {
            "text".into()
        })
    }));
    let err = run(&task, &provider, &Arc::new(FakePolicy::default()))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::Conflict);
    // stopped before applying the edit or running anything further
    let wt_file = fx
        .root
        .join("ws")
        .join(task.id.to_string())
        .join("src/lib.txt");
    assert_eq!(std::fs::read_to_string(wt_file).unwrap(), "old\n");
    assert_eq!(provider.call_count(), 2);
}

/// Each mutation happens "underneath" the task while the model call for the edit step runs.
#[tokio::test]
async fn hooks_or_config_change_in_original_repo_stops_task() {
    type Mutation = fn(&Path);
    let mutations: [(&str, Mutation); 4] = [
        ("hook", |repo| {
            let hooks = repo.join(".git/hooks");
            std::fs::create_dir_all(&hooks).unwrap();
            std::fs::write(hooks.join("post-checkout"), "#!/bin/sh\ntouch /tmp/pwn\n").unwrap();
        }),
        ("config", |repo| {
            let mut cfg = std::fs::read_to_string(repo.join(".git/config")).unwrap();
            cfg.push_str("[core]\n\tfsmonitor = /tmp/pwn\n");
            std::fs::write(repo.join(".git/config"), cfg).unwrap();
        }),
        ("ref", |repo| {
            std::fs::write(
                repo.join(".git/refs/heads/planted"),
                std::fs::read_to_string(repo.join(".git/refs/heads/main")).unwrap(),
            )
            .unwrap();
        }),
        ("hook content", |repo| {
            let hooks = repo.join(".git/hooks");
            std::fs::create_dir_all(&hooks).unwrap();
            std::fs::write(hooks.join("pre-push.sample"), "changed\n").unwrap();
        }),
    ];
    for (name, mutate) in mutations {
        let fx = Fixture::new(PASS_ACCEPTANCE);
        let repo = fx.repo();
        let provider = FnProvider::new(Box::new(move |i, _| {
            if i == 1 {
                mutate(&repo);
            }
            Ok(if i == 1 {
                edit_json(&[("src/lib.txt", "new\n")])
            } else {
                "text".into()
            })
        }));
        let task = fx.task(&["src/"]);
        let err = run(&task, &provider, &Arc::new(FakePolicy::default()))
            .await
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::Conflict, "{name}: {}", err.message);
        // stopped before the edit was applied or anything ran
        let wt_file = fx
            .root
            .join("ws")
            .join(task.id.to_string())
            .join("src/lib.txt");
        assert_eq!(std::fs::read_to_string(wt_file).unwrap(), "old\n", "{name}");
    }
}

#[tokio::test]
async fn untouched_original_repo_passes_the_integrity_check_with_its_own_branch() {
    // creating the task worktree adds refs/heads/pair/<id>, which must not look like tampering
    let fx = Fixture::new(PASS_ACCEPTANCE);
    let provider = provider_with_edit(edit_json(&[("src/lib.txt", "new\n")]));
    let res = run(
        &fx.task(&["src/"]),
        &provider,
        &Arc::new(FakePolicy::default()),
    )
    .await
    .unwrap();
    assert_eq!(res.status, CodingStatus::Succeeded);
}

#[tokio::test]
async fn forged_git_pointer_in_worktree_is_never_followed() {
    let marker =
        std::env::temp_dir().join(format!("pair_t_fsmon_{}", uuid::Uuid::new_v4().simple()));
    let hook =
        std::env::temp_dir().join(format!("pair_t_hook_{}.sh", uuid::Uuid::new_v4().simple()));
    std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // the verify command (sandboxed code) swaps the worktree's .git pointer for a repo of its
    // own whose config defines a clean filter that would run a program on every host-side `git add`
    let acceptance = format!(
        r#"["sh","-c","git init -q --bare evil.git && git -C evil.git config filter.x.clean {hook} && git -C evil.git config core.bare false && rm .git && printf 'gitdir: %s\n' \"$PWD/evil.git\" > .git && echo '* filter=x' > .gitattributes; grep -q new src/lib.txt"]"#,
        hook = hook.display()
    );
    let fx = Fixture::new(&acceptance);
    let task = fx.task(&["src/"]);
    let provider = provider_with_edit(edit_json(&[("src/lib.txt", "new\n")]));
    let res = run(&task, &provider, &Arc::new(FakePolicy::default())).await;
    let _ = std::fs::remove_file(&hook);
    let ran = marker.exists();
    let _ = std::fs::remove_file(&marker);
    assert!(
        !ran,
        "host git followed the forged .git pointer and ran its clean filter"
    );
    // evil.git is untracked output of the verify command and so out of scope; either way the
    // patch is computed from the real repository, not the forged one
    match res {
        Ok(r) => assert!(r.diff.contains("+new")),
        Err(e) => assert!(
            matches!(e.code, ErrorCode::PolicyDenied | ErrorCode::Internal),
            "{}",
            e.message
        ),
    }
}

#[tokio::test]
async fn container_sandbox_without_image_never_runs_acceptance_on_the_host() {
    use crate::coding::{ContainerSandbox, ProcessExecutor};
    let fx = Fixture::new(r#"["sh","-c","touch ran.marker"]"#);
    let task = fx.task(&["src/"]);
    let provider = provider_with_edit(edit_json(&[("src/lib.txt", "new\n")]));
    let policy = Arc::new(FakePolicy::default());
    let budget = FakeBudget::default();
    let gate = Gate::unaudited_for_tests(policy, None);
    let deps = CodingDeps {
        provider: &provider,
        gate: &gate,
        budget: &budget,
        prices: &FixedPrices::standard(),
        planner: &FixedPlanner::single(),
        memory: &EmptyMemory,
        compiler: &PlainCompiler,
        policy: fake_ctx(&task.workspaces_root),
        sandbox: Arc::new(ContainerSandbox::new(Arc::new(ProcessExecutor), None)),
        limits: Arc::new(crate::limits::RunLimits::interactive()),
    };
    let err = run_coding_task(&deps, &task).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::PolicyDenied);
    assert!(!fx
        .root
        .join("ws")
        .join(task.id.to_string())
        .join("ran.marker")
        .exists());
}

#[tokio::test]
async fn edit_write_denied_by_gate_leaves_file_untouched() {
    let fx = Fixture::new(PASS_ACCEPTANCE);
    let task = fx.task(&["src/"]);
    let policy = Arc::new(FakePolicy {
        denied_tools: vec![crate::tools::FS_WRITE.into()],
        ..FakePolicy::default()
    });
    let provider = provider_with_edit(edit_json(&[
        ("src/lib.txt", "new\n"),
        ("src/other.txt", "also new\n"),
    ]));
    let err = run(&task, &provider, &policy).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::PolicyDenied);
    let wt = fx.root.join("ws").join(task.id.to_string());
    assert_eq!(
        std::fs::read_to_string(wt.join("src/lib.txt")).unwrap(),
        "old\n"
    );
    assert!(
        !wt.join("src/other.txt").exists(),
        "a denied batch writes nothing"
    );
    // the write was offered to policy as one fs.write request naming every target path
    let seen = policy.seen.lock().unwrap();
    let writes: Vec<_> = seen
        .iter()
        .filter(|r| r.tool == crate::tools::FS_WRITE)
        .collect();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].paths.len(), 2);
    assert!(writes[0]
        .paths
        .iter()
        .all(|p| p.starts_with(wt.to_str().unwrap())));
}

#[tokio::test]
async fn coding_task_stops_at_the_tool_call_cap_and_the_deadline() {
    use crate::limits::{RunLimits, MAX_TOOL_CALLS};
    // 25 passing commands + the write batch exceed the 20-call cap
    let many = vec![r#"["true"]"#; 25].join(",");
    let fx = Fixture::new(&many);
    let provider = provider_with_edit(edit_json(&[("src/lib.txt", "new\n")]));
    let err = run(
        &fx.task(&["src/"]),
        &provider,
        &Arc::new(FakePolicy::default()),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::LimitExceeded, "{}", err.message);

    // a deadline that has already passed stops the task before any model call
    let fx = Fixture::new(PASS_ACCEPTANCE);
    let provider = provider_with_edit(edit_json(&[("src/lib.txt", "new\n")]));
    let expired = Arc::new(RunLimits::with_deadline(
        MAX_TOOL_CALLS,
        std::time::Instant::now(),
    ));
    let err = run_limited(
        &fx.task(&["src/"]),
        &provider,
        &Arc::new(FakePolicy::default()),
        expired,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::LimitExceeded);
    assert_eq!(provider.call_count(), 0);
}

#[tokio::test]
async fn hidden_credentials_not_exposed() {
    std::env::set_var("PAIR_TEST_API_TOKEN", SECRET_ENV_VALUE);
    let acceptance =
        r#"["sh","-c","env; ls -a; cat .env 2>&1; ls ~/.ssh 2>&1; grep -q new src/lib.txt"]"#;
    let fx = Fixture::new(acceptance);
    let task = fx.task(&["src/"]);
    let provider = provider_with_edit(edit_json(&[("src/lib.txt", "new\n")]));
    let res = run(&task, &provider, &Arc::new(FakePolicy::default()))
        .await
        .unwrap();
    let out = format!("{}{}", res.commands[0].stdout, res.commands[0].stderr);
    assert!(
        !out.contains(SECRET_ENV_VALUE),
        "env secret value leaked into command output"
    );
    assert!(
        !out.contains("PAIR_TEST_API_TOKEN"),
        "secret variable present in command env"
    );
    assert!(
        !out.contains("hunter2"),
        "credential file contents reachable from workspace"
    );
    assert!(!res.worktree.join(".env").exists());
    // the withheld file must not show up as a deletion in the patch
    assert_eq!(res.changed_files, vec!["src/lib.txt".to_string()]);
    // and the model cannot write credential files even when the scope names them
    let fx2 = Fixture::new(PASS_ACCEPTANCE);
    let task2 = fx2.task(&["src/", ".env"]);
    let provider2 = provider_with_edit(edit_json(&[(".env", "X=1\n")]));
    let err = run(&task2, &provider2, &Arc::new(FakePolicy::default()))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PolicyDenied);
}

#[tokio::test]
async fn employer_repo_refused() {
    let fx = Fixture::with_class(PASS_ACCEPTANCE, Some("employer"));
    let provider = provider_with_edit(edit_json(&[("src/lib.txt", "new\n")]));
    let task = fx.task(&["src/"]);
    let err = run(&task, &provider, &Arc::new(FakePolicy::default()))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PolicyDenied);
    assert_eq!(provider.call_count(), 0, "no model call for refused data");
    assert!(!fx.root.join("ws").join(task.id.to_string()).exists());
    // unknown classes are refused the same way
    let fx = Fixture::with_class(PASS_ACCEPTANCE, Some("top_secret"));
    let err = run(
        &fx.task(&["src/"]),
        &provider,
        &Arc::new(FakePolicy::default()),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::PolicyDenied);
}

#[tokio::test]
async fn missing_data_class_refused() {
    let fx = Fixture::with_class(PASS_ACCEPTANCE, None);
    let provider = provider_with_edit(edit_json(&[("src/lib.txt", "new\n")]));
    let err = run(
        &fx.task(&["src/"]),
        &provider,
        &Arc::new(FakePolicy::default()),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::PolicyDenied);
    assert!(err.message.contains("data_class"), "{}", err.message);
    assert_eq!(provider.call_count(), 0);
}

#[tokio::test]
async fn data_class_flows_to_model_request() {
    use pair_core::types::DataClass;
    let fx = Fixture::with_class(PASS_ACCEPTANCE, Some("sensitive"));
    let provider = provider_with_edit(edit_json(&[("src/lib.txt", "new\n")]));
    let policy = Arc::new(FakePolicy::default());
    run(&fx.task(&["src/"]), &provider, &policy).await.unwrap();
    let seen = provider.seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert!(seen.iter().all(|r| r.data_class == DataClass::Sensitive));
    let actions = policy.seen.lock().unwrap();
    assert!(!actions.is_empty());
    assert!(actions.iter().all(|a| a.data_class == DataClass::Sensitive));
}

#[tokio::test]
async fn failed_acceptance_tests_reported_not_hidden() {
    let fx = Fixture::new(r#"["sh","-c","echo ASSERTION_FAILED_MARKER >&2; exit 1"]"#);
    let task = fx.task(&["src/"]);
    let provider = provider_with_edit(edit_json(&[("src/lib.txt", "new\n")]));
    let res = run(&task, &provider, &Arc::new(FakePolicy::default()))
        .await
        .unwrap();
    assert_eq!(res.status, CodingStatus::Failed);
    assert_eq!(res.failure_class, Some(FailureClass::ImplementationDefect));
    assert!(res
        .commands
        .last()
        .unwrap()
        .stderr
        .contains("ASSERTION_FAILED_MARKER"));
    assert!(
        res.review.is_none(),
        "a failed result must not be reviewed into a pass"
    );
    assert!(res.summary.contains("failed"));
}

#[tokio::test]
async fn missing_command_and_timeout_are_environment_failures() {
    let fx = Fixture::new(r#"["pair-no-such-binary-xyz"]"#);
    let provider = provider_with_edit(edit_json(&[("src/lib.txt", "new\n")]));
    let res = run(
        &fx.task(&["src/"]),
        &provider,
        &Arc::new(FakePolicy::default()),
    )
    .await
    .unwrap();
    assert_eq!(res.failure_class, Some(FailureClass::Environment));

    let fx = Fixture::new(r#"["sleep","30"]"#);
    let provider = provider_with_edit(edit_json(&[("src/lib.txt", "new\n")]));
    let res = run(
        &fx.task(&["src/"]),
        &provider,
        &Arc::new(FakePolicy::default()),
    )
    .await
    .unwrap();
    assert!(res.commands[0].timed_out);
    assert_eq!(res.failure_class, Some(FailureClass::Environment));
}

#[tokio::test]
async fn policy_denied_command_never_executes() {
    let fx = Fixture::new(r#"["sh","-c","touch ran.marker"]"#);
    let policy = Arc::new(FakePolicy {
        denied_exes: vec!["sh".into()],
        ..FakePolicy::default()
    });
    let provider = provider_with_edit(edit_json(&[("src/lib.txt", "new\n")]));
    let task = fx.task(&["src/"]);
    let err = run(&task, &provider, &policy).await.unwrap_err();
    assert_eq!(err.code, ErrorCode::PolicyDenied);
    assert!(!fx
        .root
        .join("ws")
        .join(task.id.to_string())
        .join("ran.marker")
        .exists());
}

fn test_runner<'a>(gate: &'a Gate, dir: &Path) -> Runner<'a> {
    Runner::new(
        gate,
        RunnerSetup {
            task: TaskId::new(),
            trace: TraceId::new(),
            ctx: fake_ctx(dir),
            home: dir.join("home"),
            timeout: std::time::Duration::from_secs(2),
            passthrough: Vec::new(),
            data_class: pair_core::types::DataClass::Personal,
        },
    )
    .with_sandbox(host_sandbox())
}

#[tokio::test]
async fn runner_cannot_execute_without_gate_allow() {
    let dir = std::env::temp_dir().join(format!("pair_t_gate_{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let argv: Vec<String> = ["sh", "-c", "touch ran.marker"]
        .iter()
        .map(ToString::to_string)
        .collect();
    let denied = Decision::Deny {
        reason: "no".into(),
    };
    let needs_approval = Decision::NeedsApproval {
        payload_hash: "h".into(),
    };
    for (decision, code) in [
        (denied, ErrorCode::PolicyDenied),
        (needs_approval, ErrorCode::ApprovalRequired),
    ] {
        let gate = Gate::unaudited_for_tests(Arc::new(FixedPolicy(decision)), None);
        let err = test_runner(&gate, &dir).run(&argv, &dir).await.unwrap_err();
        assert_eq!(err.code, code);
        assert!(!dir.join("ran.marker").exists());
    }
    let gate = Gate::unaudited_for_tests(Arc::new(FixedPolicy(Decision::Allow)), None);
    let report = test_runner(&gate, &dir).run(&argv, &dir).await.unwrap();
    assert!(report.passed());
    assert!(dir.join("ran.marker").exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn patch_output_is_scoped() {
    // 1. one out-of-scope file rejects the whole edit set, nothing is written
    let fx = Fixture::new(PASS_ACCEPTANCE);
    let task = fx.task(&["src/"]);
    let provider = provider_with_edit(edit_json(&[
        ("src/lib.txt", "new\n"),
        ("README.md", "pwned\n"),
    ]));
    let err = run(&task, &provider, &Arc::new(FakePolicy::default()))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::PolicyDenied);
    assert!(err.message.contains("README.md"));
    let wt = fx.root.join("ws").join(task.id.to_string());
    assert_eq!(
        std::fs::read_to_string(wt.join("src/lib.txt")).unwrap(),
        "old\n"
    );

    // 2. traversal and absolute paths are rejected
    for bad in ["../escape.txt", "/etc/passwd", "src/../../x"] {
        let fx = Fixture::new(PASS_ACCEPTANCE);
        let provider = provider_with_edit(edit_json(&[(bad, "x")]));
        let err = run(
            &fx.task(&["src/"]),
            &provider,
            &Arc::new(FakePolicy::default()),
        )
        .await
        .unwrap_err();
        assert_eq!(err.code, ErrorCode::PolicyDenied, "{bad}");
    }

    // 3. a verify command that writes outside scope is caught in the final patch check
    let fx = Fixture::new(r#"["sh","-c","echo x > stray.txt; grep -q new src/lib.txt"]"#);
    let provider = provider_with_edit(edit_json(&[("src/lib.txt", "new\n")]));
    let err = run(
        &fx.task(&["src/"]),
        &provider,
        &Arc::new(FakePolicy::default()),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::PolicyDenied);
    assert!(err.message.contains("stray.txt"));
}

#[test]
fn scope_permits_only_declared_paths() {
    let s = Scope::new(vec!["src/".into(), "Cargo.toml".into()]);
    assert!(s.permits("src/a/b.rs"));
    assert!(s.permits("Cargo.toml"));
    assert!(!s.permits("Cargo.toml.bak"));
    assert!(!s.permits("srcx/a.rs"));
    assert!(!s.permits("src/.env"));
    assert!(!s.permits("src/../etc"));
    assert!(
        is_secret_path("config/id_rsa")
            && is_secret_path("a/server.pem")
            && !is_secret_path("src/env.rs")
    );
}

#[test]
fn dot_git_check_is_case_insensitive_and_more_secret_files_are_withheld() {
    for p in [".GIT/config", "a/.Git/hooks/x", ".gIt"] {
        assert!(!Scope::new(vec!["".into()]).permits(p), "{p}");
        assert_eq!(
            super::scope::normalize_rel(p).unwrap_err().code,
            ErrorCode::PolicyDenied
        );
    }
    for secret in [
        ".envrc",
        "sub/.git-credentials",
        "keys/id_ecdsa",
        "id_dsa",
        "kubeconfig",
        "deploy/prod.tfvars",
        "release.jks",
        "x/KUBECONFIG",
    ] {
        assert!(is_secret_path(secret), "{secret}");
    }
    assert!(!is_secret_path("src/environment.rs") && !is_secret_path("docs/envrc.md"));
}

#[test]
fn env_passthrough_rejects_host_service_variables() {
    for name in ["DOCKER_HOST", "KUBECONFIG", "SSH_AUTH_SOCK", "docker_host"] {
        let json = format!(
            r#"{{"acceptance":[["true"]],"data_class":"public","env_passthrough":["{name}"]}}"#
        );
        assert!(RepoConfig::parse(&json).is_err(), "{name}");
    }
}

#[test]
fn repo_config_requires_acceptance_and_rejects_secret_passthrough() {
    assert!(RepoConfig::parse(r#"{"acceptance":[],"data_class":"public"}"#).is_err());
    assert!(RepoConfig::parse(
        r#"{"acceptance":[["true"]],"data_class":"public","env_passthrough":["AWS_SECRET_ACCESS_KEY"]}"#
    )
    .is_err());
    assert!(RepoConfig::parse(
        r#"{"acceptance":[["true"]],"data_class":"public","env_passthrough":["CARGO_HOME"]}"#
    )
    .is_ok());
    let _ = Path::new(".");
}
