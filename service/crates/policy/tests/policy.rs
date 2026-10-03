mod common;

use common::{fixture, request};
use pair_core::traits::Policy;
use pair_core::types::{DataClass, Decision};

fn reason(d: &Decision) -> &str {
    match d {
        Decision::Deny { reason } => reason,
        _ => "",
    }
}

#[test]
fn external_write_requires_approval() {
    let f = fixture();
    for tool in [
        "message.send",
        "git.push",
        "pr.create",
        "calendar.modify_event",
    ] {
        let out = f.engine.authorize(&request(tool), &f.ctx);
        assert!(
            matches!(out.decision, Decision::NeedsApproval { .. }),
            "{tool}: {:?}",
            out.decision
        );
    }
    for tool in [
        "pr.merge",
        "deploy.run",
        "fs.delete_recursive",
        "payment.send",
    ] {
        let out = f.engine.authorize(&request(tool), &f.ctx);
        assert!(
            matches!(out.decision, Decision::NeedsApproval { .. }),
            "{tool}: {:?}",
            out.decision
        );
    }
}

#[test]
fn external_write_payload_hash_is_deterministic_and_payload_bound() {
    let f = fixture();
    let mut a = request("message.send");
    a.args = vec!["hello".into()];
    let mut same = a.clone();
    same.trace = pair_core::ids::TraceId::new();
    let mut changed = a.clone();
    changed.args = vec!["hello!".into()];
    let hash = |r| match f.engine.authorize(r, &f.ctx).decision {
        Decision::NeedsApproval { payload_hash } => payload_hash,
        other => panic!("expected approval, got {other:?}"),
    };
    assert_eq!(hash(&a), hash(&same));
    assert_ne!(hash(&a), hash(&changed));
    assert_eq!(hash(&a).len(), 64);
}

#[test]
fn read_and_local_edit_in_workspace_are_allowed() {
    let f = fixture();
    std::fs::write(f.workspace.join("a.txt"), "x").expect("write");
    let mut read = request("fs.read");
    read.paths = vec!["a.txt".into()];
    assert_eq!(f.engine.authorize(&read, &f.ctx).decision, Decision::Allow);
    let mut edit = request("fs.write");
    edit.paths = vec!["new/dir/b.txt".into()];
    assert_eq!(f.engine.authorize(&edit, &f.ctx).decision, Decision::Allow);
    let commit = request("git.commit");
    assert_eq!(
        f.engine.authorize(&commit, &f.ctx).decision,
        Decision::Allow
    );
}

#[test]
fn unknown_tool_is_denied() {
    let f = fixture();
    let out = f.engine.authorize(&request("totally.safe.tool"), &f.ctx);
    assert!(matches!(out.decision, Decision::Deny { .. }));
}

#[test]
fn stale_policy_version_is_denied() {
    let mut f = fixture();
    f.ctx.policy_version = "old".into();
    let out = f.engine.authorize(&request("fs.read"), &f.ctx);
    assert!(matches!(out.decision, Decision::Deny { .. }));
}

#[cfg(unix)]
#[test]
fn symlink_escape_denied() {
    let f = fixture();
    let outside = f.workspace.parent().expect("parent").join("outside");
    std::fs::create_dir_all(&outside).expect("mkdir");
    std::fs::write(outside.join("data.txt"), "x").expect("write");
    std::os::unix::fs::symlink(&outside, f.workspace.join("link")).expect("symlink");
    std::os::unix::fs::symlink(
        f.workspace.parent().expect("parent").join("nowhere"),
        f.workspace.join("dangling"),
    )
    .expect("symlink");

    for path in [
        "link/data.txt",
        "link/new.txt",
        "dangling/x",
        "../outside/data.txt",
        "nonexistent/../../outside/x",
    ] {
        let mut r = request("fs.write");
        r.paths = vec![path.into()];
        let out = f.engine.authorize(&r, &f.ctx);
        assert!(
            matches!(out.decision, Decision::Deny { .. }),
            "{path}: {:?}",
            out.decision
        );
    }

    let mut via_arg = request("shell.exec");
    via_arg.executable = Some("cat".into());
    via_arg.args = vec!["link/data.txt".into()];
    assert!(matches!(
        f.engine.authorize(&via_arg, &f.ctx).decision,
        Decision::Deny { .. }
    ));
}

#[test]
fn unapproved_egress_denied() {
    let f = fixture();
    let cases = [
        "https://evil.example.com/x",
        "evil.example.com:443",
        "https://api.github.com.evil.com/",
        "https://evil.com\\@api.github.com/",
        "",
    ];
    for dest in cases {
        let mut r = request("fs.read");
        r.destination = Some(dest.into());
        let out = f.engine.authorize(&r, &f.ctx);
        assert!(
            matches!(out.decision, Decision::Deny { .. }),
            "{dest}: {:?}",
            out.decision
        );
    }
    let mut url_arg = request("shell.exec");
    url_arg.executable = Some("git".into());
    url_arg.args = vec!["clone".into(), "https://evil.example.com/r.git".into()];
    assert!(matches!(
        f.engine.authorize(&url_arg, &f.ctx).decision,
        Decision::Deny { .. }
    ));

    let mut ok = request("fs.read");
    ok.destination = Some("https://user@api.github.com:443/repos".into());
    assert_eq!(f.engine.authorize(&ok, &f.ctx).decision, Decision::Allow);

    let mut wrong_class = request("fs.read");
    wrong_class.destination = Some("pypi.org".into());
    wrong_class.data_class = DataClass::Employer;
    assert!(matches!(
        f.engine.authorize(&wrong_class, &f.ctx).decision,
        Decision::Deny { .. }
    ));
}

#[cfg(unix)]
#[test]
fn host_credential_paths_denied() {
    let f = fixture();
    let ssh_key = f.home.join(".ssh/id_rsa").to_string_lossy().into_owned();
    let tilde = "~/.aws/credentials".to_owned();
    for path in [
        ssh_key,
        tilde,
        "/var/run/docker.sock".to_owned(),
        "/run/docker.sock".to_owned(),
    ] {
        let mut r = request("fs.read");
        r.paths = vec![path.clone()];
        let out = f.engine.authorize(&r, &f.ctx);
        assert!(
            reason(&out.decision).contains("host credential"),
            "{path}: {:?}",
            out.decision
        );
    }
    std::os::unix::fs::symlink(f.home.join(".ssh"), f.workspace.join("sneaky")).expect("symlink");
    let mut via_link = request("fs.read");
    via_link.paths = vec!["sneaky/id_rsa".into()];
    let out = f.engine.authorize(&via_link, &f.ctx);
    assert!(
        reason(&out.decision).contains("host credential"),
        "{:?}",
        out.decision
    );

    let mut via_arg = request("shell.exec");
    via_arg.executable = Some("cat".into());
    via_arg.args = vec!["--file=/var/run/docker.sock".into()];
    let out = f.engine.authorize(&via_arg, &f.ctx);
    assert!(
        reason(&out.decision).contains("host credential"),
        "{:?}",
        out.decision
    );
}

#[test]
fn executable_not_on_allowlist_is_denied() {
    let f = fixture();
    for exe in ["docker", "/usr/bin/curl", "../evil", "git "] {
        let mut r = request("shell.exec");
        r.executable = Some(exe.into());
        let out = f.engine.authorize(&r, &f.ctx);
        assert!(
            matches!(out.decision, Decision::Deny { .. }),
            "{exe}: {:?}",
            out.decision
        );
    }
}

#[test]
fn malformed_config_fails_closed() {
    let home = std::path::Path::new("/tmp");
    assert!(pair_policy::PolicyEngine::from_config_str("not json", home).is_err());
    assert!(pair_policy::PolicyEngine::from_config_str(
        "{\"version\":\"1\",\"tools\":{\"x\":\"safe\"}}",
        home
    )
    .is_err());
}

fn exec_with(f: &common::Fixture, args: &[&str]) -> Decision {
    let mut r = request("shell.exec");
    r.executable = Some("cat".into());
    r.args = args.iter().map(ToString::to_string).collect();
    f.engine.authorize(&r, &f.ctx).decision
}

#[test]
fn attached_flag_path_is_checked() {
    let f = fixture();
    for arg in [
        "-o/etc/x",
        "--output=/etc/x",
        "-o../escape",
        "-o..",
        "-o~/.ssh/id_rsa",
        "-O=/etc/x",
    ] {
        assert!(
            matches!(exec_with(&f, &[arg]), Decision::Deny { .. }),
            "{arg} must be denied"
        );
    }
    std::fs::write(f.workspace.join("a.txt"), "x").expect("write");
    // `-osub/new.txt` is intentionally no longer allowed (see below): it is indistinguishable
    // from the cluster `-o -s -u -b` followed by `/new.txt`, so the `/new.txt` suffix is checked.
    assert!(matches!(
        exec_with(&f, &["-osub/new.txt"]),
        Decision::Deny { .. }
    ));
    for arg in ["-oa.txt", "--output=a.txt", "-n", "-5"] {
        assert_eq!(
            exec_with(&f, &[arg]),
            Decision::Allow,
            "{arg} is in-workspace"
        );
    }
}

#[test]
fn attached_flag_url_is_checked() {
    let f = fixture();
    for arg in [
        "-ohttps://evil.example.com/x",
        "--url=https://evil.example.com/x",
        "-Ipath/x=https://evil.example.com/x",
    ] {
        assert!(
            matches!(exec_with(&f, &[arg]), Decision::Deny { .. }),
            "{arg} must be denied"
        );
    }
    let ok = "-ohttps://api.github.com/repos";
    assert_eq!(exec_with(&f, &[ok]), Decision::Allow);
}

const MINIMAL_POLICY: &str = r#"
# comment at top
version: "t1"   # trailing comment
tools:
  fs.read: read   # read-only
executables_allow: [git]
denied_paths: ["~/.ssh"]
egress: []
"#;

#[test]
fn yaml_comment_is_accepted() {
    let home = std::path::Path::new("/tmp");
    assert!(pair_policy::PolicyEngine::from_config_str(MINIMAL_POLICY, home).is_ok());
}

#[test]
fn malformed_yaml_fails_closed() {
    let home = std::path::Path::new("/tmp");
    for bad in [
        "version: [unclosed",
        "version: 1\n  tools: bad indent: :",
        "",
        "version: t\ntools: {}\nexecutables_allow: []\ndenied_paths: []\negress: []\n",
        "version: t\nsurprise: 1\n",
    ] {
        assert!(
            pair_policy::PolicyEngine::from_config_str(bad, home).is_err(),
            "{bad:?}"
        );
    }
}

#[test]
fn model_supplied_safe_label_is_ignored() {
    let f = fixture();
    let mut r = request("message.send");
    r.args = vec!["safe=true".into()];
    r.executable = None;
    assert!(matches!(
        f.engine.authorize(&r, &f.ctx).decision,
        Decision::NeedsApproval { .. }
    ));
    let mut t = request("safe");
    t.args = vec![];
    assert!(matches!(
        f.engine.authorize(&t, &f.ctx).decision,
        Decision::Deny { .. }
    ));
}
