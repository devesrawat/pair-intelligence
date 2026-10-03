#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::testkit::{fake_ctx, host_sandbox, FakePolicy};
use super::*;
use pair_core::{
    error::ErrorCode,
    ids::{ApprovalId, TaskId, TraceId},
    types::DataClass,
};
use pair_policy::Gate;
use std::{path::Path, sync::Arc};

const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

fn action(url: &str) -> RemoteAction {
    RemoteAction {
        kind: RemoteKind::Push,
        remote_url: url.into(),
        branch: "pair/x".into(),
        head_sha: SHA.into(),
        diff_sha256: "def".into(),
        data_class: DataClass::Personal,
    }
}

fn ask(
    policy: &FakePolicy,
    approvals: Vec<ApprovalId>,
    a: &RemoteAction,
) -> pair_core::error::Result<RemoteOutcome> {
    let mut ctx = fake_ctx(Path::new("/ws"));
    ctx.approvals = approvals;
    request_remote_write(
        &RemoteCtx {
            policy,
            ctx: &ctx,
            task: TaskId::new(),
            trace: TraceId::new(),
            cwd: Path::new("/ws/repo"),
        },
        a,
    )
}

#[test]
fn remote_write_requires_approval() {
    let strict = FakePolicy::default();
    for kind in [RemoteKind::Push, RemoteKind::OpenPr] {
        let a = RemoteAction {
            kind,
            ..action("https://github.com/acme/repo.git")
        };
        let out = ask(&strict, vec![], &a).unwrap();
        assert!(
            matches!(out, RemoteOutcome::NeedsApproval { .. }),
            "{kind:?}"
        );
    }
    // even a permissive policy cannot authorize without an approval id
    let lax = FakePolicy {
        allow_remote_writes: true,
        ..FakePolicy::default()
    };
    let a = action("https://github.com/acme/repo.git");
    assert!(matches!(
        ask(&lax, vec![], &a).unwrap(),
        RemoteOutcome::NeedsApproval { .. }
    ));
    assert!(matches!(
        ask(&lax, vec![ApprovalId::new()], &a).unwrap(),
        RemoteOutcome::Authorized { .. }
    ));
    // the permissive fallback hash binds the exact payload (commit, branch, remote)
    let hash = |a: &RemoteAction| match ask(&lax, vec![], a).unwrap() {
        RemoteOutcome::NeedsApproval { payload_hash } => payload_hash,
        o => panic!("{o:?}"),
    };
    let base = hash(&a);
    for changed in [
        RemoteAction {
            head_sha: "f".repeat(40),
            ..a.clone()
        },
        RemoteAction {
            branch: "pair/y".into(),
            ..a.clone()
        },
        RemoteAction {
            remote_url: "https://github.com/acme/other.git".into(),
            ..a.clone()
        },
    ] {
        assert_ne!(base, hash(&changed));
    }
}

#[test]
fn egress_host_is_derived_from_every_remote_spelling() {
    for (url, host) in [
        ("https://github.com/acme/repo.git", "github.com"),
        ("HTTPS://GitHub.com:443/acme/repo", "github.com"),
        ("https://x-access@github.com/acme/repo.git", "github.com"),
        ("ssh://git@github.com:22/acme/repo.git", "github.com"),
        ("git@github.com:acme/repo.git", "github.com"),
        ("github.com:acme/repo.git", "github.com"),
    ] {
        assert_eq!(egress_host(url).unwrap(), host, "{url}");
    }
    for bad in [
        "origin",
        "",
        "http://github.com/a/b",
        "git://github.com/a/b",
        "file:///etc",
        "/srv/repo.git",
        "../repo",
        "https://user:pw@github.com/a/b",
        "https://github.com:notaport/a",
        "https://-bad.example/a",
        "https://exa mple.com/a",
        "https://github.com..evil/a",
    ] {
        assert_eq!(
            egress_host(bad).unwrap_err().code,
            ErrorCode::PolicyDenied,
            "{bad}"
        );
    }
}

#[test]
fn push_to_a_denied_host_is_refused_for_every_url_spelling() {
    let policy = FakePolicy {
        denied_destinations: vec!["evil.example".into()],
        ..FakePolicy::default()
    };
    for url in [
        "https://evil.example/a/b.git",
        "ssh://git@evil.example/a/b.git",
        // scp-style remotes used to skip the egress check altogether
        "git@evil.example:a/b.git",
    ] {
        let err = ask(&policy, vec![], &action(url)).unwrap_err();
        assert_eq!(err.code, ErrorCode::PolicyDenied, "{url}");
    }
    let seen = policy.seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert!(seen
        .iter()
        .all(|r| r.destination.as_deref() == Some("evil.example")));
}

#[test]
fn push_refspec_pins_the_exact_commit_and_rejects_odd_refs() {
    let a = action("https://github.com/acme/repo.git");
    assert_eq!(
        a.push_argv(),
        [
            "git",
            "push",
            "https://github.com/acme/repo.git",
            &format!("{SHA}:refs/heads/pair/x")
        ]
    );
    for bad_branch in ["", "-f", "a..b", "a b", "x.lock", "/a", "a/", "a//b", "a;b"] {
        let bad = RemoteAction {
            branch: bad_branch.into(),
            ..a.clone()
        };
        assert!(
            ask(&FakePolicy::default(), vec![], &bad).is_err(),
            "{bad_branch:?}"
        );
    }
    let short = RemoteAction {
        head_sha: "abc".into(),
        ..a
    };
    assert!(ask(&FakePolicy::default(), vec![], &short).is_err());
}

#[tokio::test]
async fn approval_request_equals_the_request_the_runner_presents() {
    let policy = Arc::new(FakePolicy::default());
    let gate = Gate::unaudited_for_tests(policy.clone(), None);
    let (task, trace) = (TaskId::new(), TraceId::new());
    let cwd = std::env::temp_dir();
    let a = action("git@github.com:acme/repo.git");
    let host = a.host().unwrap();
    // what the approver is shown
    let asked = a.action_request(task, trace, &cwd).unwrap();
    // what executing the push presents to the Gate
    let runner = Runner::new(
        &gate,
        RunnerSetup {
            task,
            trace,
            ctx: fake_ctx(&cwd),
            home: cwd.join("home"),
            timeout: std::time::Duration::from_secs(2),
            passthrough: Vec::new(),
            data_class: DataClass::Personal,
        },
    )
    .with_sandbox(host_sandbox());
    let err = runner
        .run_tool(crate::tools::GIT_PUSH, &a.push_argv(), &cwd, Some(&host))
        .await
        .unwrap_err();
    assert_eq!(err.code, ErrorCode::ApprovalRequired);
    let seen = policy.seen.lock().unwrap();
    assert_eq!(
        serde_json::to_value(&asked).unwrap(),
        serde_json::to_value(&seen[0]).unwrap(),
        "approval is bound to a different request than the one executed"
    );
}
