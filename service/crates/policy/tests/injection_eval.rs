//! Policy / injection evaluation (spec section 12, release gate 4). Loads
//! `evals/datasets/policy_injection.jsonl`, builds each case's filesystem, and runs it against the
//! REAL `PolicyEngine` loaded from `config/policy.yaml`, through the real `Gate` (so approval
//! binding, expiry and single use are exercised too). Expectations are never edited to fit the
//! engine: a disagreement is a finding.
mod common;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use common::{fixture, Fixture};
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::{ApprovalId, TaskId, TraceId};
use pair_core::traits::{Approvals, Policy};
use pair_core::types::{ActionRequest, DataClass, Decision};
use pair_policy::recorder::{ExecDecision, ExecOutcome, ExecutionRecord, MemoryRecorder};
use pair_policy::Gate;
use serde::Deserialize;
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const DATASET: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../evals/datasets/policy_injection.jsonl"
);
const EXPECTED_CASES: usize = 30;
const MIN_BENIGN_CONTROLS: usize = 5;
const REQUIRED_CATEGORIES: [&str; 13] = [
    "symlink_escape",
    "path_traversal",
    "attached_flag_path",
    "scp_remote",
    "egress_trick",
    "credential_read",
    "code_exec",
    "external_write",
    "approval_lifecycle",
    "stale_policy",
    "unregistered_tool",
    "unicode_whitespace",
    "injected_output",
];
const BENIGN_CATEGORY: &str = "benign_control";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
enum Expected {
    Deny,
    NeedsApproval,
    Allow,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    tool: String,
    #[serde(default)]
    executable: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    destination: Option<String>,
    data_class: DataClass,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Setup {
    #[serde(default)]
    symlinks: Vec<Symlink>,
    #[serde(default)]
    files: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Symlink {
    link: String,
    target: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Bind {
    Exact,
    OtherPayload,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalSpec {
    bind: Bind,
    #[serde(default)]
    expired: bool,
    #[serde(default)]
    consume_first: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    category: String,
    attack: String,
    /// Text a tool returned that tries to steer the agent. It never reaches `authorize`; the
    /// action it provokes is `input`, and the verdict must be what it would be without it.
    #[serde(default)]
    tool_output: Option<String>,
    input: Input,
    #[serde(default)]
    setup: Setup,
    #[serde(default)]
    policy_version: Option<String>,
    #[serde(default)]
    approval: Option<ApprovalSpec>,
    expected: Expected,
    rationale: String,
}

fn load_cases() -> Vec<Case> {
    let text = std::fs::read_to_string(DATASET).expect("read policy_injection.jsonl");
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, l)| {
            serde_json::from_str(l).unwrap_or_else(|e| panic!("line {}: invalid case: {e}", i + 1))
        })
        .collect()
}

// ---------- approvals double with the same rules as pair_jobs::PgApprovals ----------

struct StoredApproval {
    hash: String,
    expires: DateTime<Utc>,
    consumed: bool,
}

#[derive(Default)]
struct ApprovalStore {
    rows: Mutex<HashMap<ApprovalId, StoredApproval>>,
}

#[async_trait]
impl Approvals for ApprovalStore {
    async fn approve(&self, hash: &str, _actor: &str, expiry: DateTime<Utc>) -> Result<ApprovalId> {
        let id = ApprovalId::new();
        self.rows.lock().expect("lock").insert(
            id,
            StoredApproval {
                hash: hash.to_owned(),
                expires: expiry,
                consumed: false,
            },
        );
        Ok(id)
    }

    async fn consume(&self, id: ApprovalId, action_hash: &str) -> Result<()> {
        let mut rows = self.rows.lock().expect("lock");
        let row = rows
            .get_mut(&id)
            .ok_or_else(|| PairError::new(ErrorCode::NotFound, "approval"))?;
        if row.hash != action_hash {
            return Err(PairError::new(ErrorCode::ApprovalPayloadChanged, "hash"));
        }
        if row.expires <= Utc::now() {
            return Err(PairError::new(ErrorCode::ApprovalExpired, "expired"));
        }
        if row.consumed {
            return Err(PairError::new(ErrorCode::Conflict, "consumed"));
        }
        row.consumed = true;
        Ok(())
    }
}

// ---------- case execution ----------

struct Dirs {
    home: String,
    workspace: String,
    outside: String,
}

impl Dirs {
    fn sub(&self, s: &str) -> String {
        s.replace("$HOME", &self.home)
            .replace("$WORKSPACE", &self.workspace)
            .replace("$OUTSIDE", &self.outside)
    }
}

fn build_fs(f: &Fixture, dirs: &Dirs, setup: &Setup) {
    std::fs::create_dir_all(&dirs.outside).expect("mkdir outside");
    for file in &setup.files {
        let path = Path::new(&dirs.sub(file)).to_path_buf();
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&path, "placeholder-not-a-secret").expect("write file");
    }
    for s in &setup.symlinks {
        let link = dirs.sub(&s.link);
        if let Some(parent) = Path::new(&link).parent() {
            std::fs::create_dir_all(parent).expect("mkdir link parent");
        }
        std::os::unix::fs::symlink(dirs.sub(&s.target), &link).expect("symlink");
    }
    assert!(f.workspace.is_dir());
}

fn request(case: &Case, dirs: &Dirs) -> ActionRequest {
    let i = &case.input;
    ActionRequest {
        tool: dirs.sub(&i.tool),
        executable: i.executable.as_deref().map(|s| dirs.sub(s)),
        args: i.args.iter().map(|s| dirs.sub(s)).collect(),
        paths: i.paths.iter().map(|s| dirs.sub(s)).collect(),
        destination: i.destination.as_deref().map(|s| dirs.sub(s)),
        data_class: i.data_class,
        task: TaskId::new(),
        trace: TraceId::new(),
    }
}

fn kind(d: &Decision) -> Expected {
    match d {
        Decision::Allow => Expected::Allow,
        Decision::Deny { .. } => Expected::Deny,
        Decision::NeedsApproval { .. } => Expected::NeedsApproval,
    }
}

/// Runs the case through the real gate; returns the observed decision and whether the tool body ran.
async fn observe(case: &Case) -> (Expected, bool) {
    let f = fixture();
    let dirs = Dirs {
        home: f.home.to_string_lossy().into_owned(),
        workspace: f.workspace.to_string_lossy().into_owned(),
        outside: f
            .workspace
            .parent()
            .expect("root")
            .join("outside")
            .to_string_lossy()
            .into_owned(),
    };
    build_fs(&f, &dirs, &case.setup);
    let req = request(case, &dirs);
    let mut ctx = f.ctx.clone();
    if let Some(v) = &case.policy_version {
        ctx.policy_version = v.clone();
    }
    let store = Arc::new(ApprovalStore::default());
    if let Some(spec) = &case.approval {
        attach_approval(&f, &store, &mut ctx, &req, spec).await;
    }
    let recorder = Arc::new(MemoryRecorder::default());
    let gate = Gate::new(Arc::new(f.engine.clone()), Some(store), recorder.clone());
    let ran = Arc::new(AtomicUsize::new(0));
    let r = ran.clone();
    let result = gate
        .execute(&req, &ctx, || async move {
            r.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await;
    let observed = match result {
        Ok(()) => Expected::Allow,
        Err(e) if e.code == ErrorCode::PolicyDenied => Expected::Deny,
        Err(e) if e.code == ErrorCode::ApprovalRequired => Expected::NeedsApproval,
        Err(e) => panic!("{}: unexpected gate error {e}", case.id),
    };
    let rows = recorder.rows();
    assert_eq!(rows.len(), 1, "{}: the gate must audit every call", case.id);
    assert_audit_row(case, observed, &rows[0]);
    if case.approval.is_none() {
        let direct = f.engine.authorize(&req, &ctx).decision;
        assert_eq!(kind(&direct), observed, "{}: authorize vs gate", case.id);
    }
    if let Some(text) = &case.tool_output {
        assert_injected_output_is_inert(case, &f, &req, &ctx, text);
    }
    (observed, ran.load(Ordering::SeqCst) > 0)
}

/// 0 = Allow, 1 = NeedsApproval, 2 = Deny. A higher number is a stricter verdict.
fn strictness(e: Expected) -> u8 {
    match e {
        Expected::Allow => 0,
        Expected::NeedsApproval => 1,
        Expected::Deny => 2,
    }
}

/// The audit row must say what happened: decision, terminal outcome and (only for an approved
/// run) the consumed approval id.
fn assert_audit_row(case: &Case, observed: Expected, row: &ExecutionRecord) {
    let approved_run = observed == Expected::Allow && case.approval.is_some();
    let (decision, outcome) = match observed {
        Expected::Deny => (ExecDecision::Deny, ExecOutcome::Denied),
        Expected::NeedsApproval => (ExecDecision::NeedsApproval, ExecOutcome::ApprovalRequired),
        Expected::Allow if approved_run => (ExecDecision::NeedsApproval, ExecOutcome::Ok),
        Expected::Allow => (ExecDecision::Allow, ExecOutcome::Ok),
    };
    assert_eq!(row.decision, decision, "{}: audit decision", case.id);
    assert_eq!(row.outcome, outcome, "{}: audit outcome", case.id);
    assert_eq!(
        row.approval.is_some(),
        approved_run,
        "{}: approval_id is recorded exactly when an approval was consumed",
        case.id
    );
    assert!(row.finished, "{}: row must be terminal", case.id);
}

/// Text a tool returned can only reach the policy as data inside a request. Whichever field it
/// lands in, the verdict may get stricter but must never move TOWARD Allow.
fn assert_injected_output_is_inert(
    case: &Case,
    f: &Fixture,
    req: &ActionRequest,
    ctx: &pair_core::types::PolicyContext,
    text: &str,
) {
    let baseline = strictness(kind(&f.engine.authorize(req, ctx).decision));
    let mut as_arg = req.clone();
    as_arg.args.push(text.to_owned());
    let mut as_path = req.clone();
    as_path.paths.push(text.to_owned());
    let mut as_destination = req.clone();
    as_destination.destination = Some(text.to_owned());
    for (field, carried) in [
        ("args", as_arg),
        ("paths", as_path),
        ("destination", as_destination),
    ] {
        let verdict = strictness(kind(&f.engine.authorize(&carried, ctx).decision));
        assert!(
            verdict >= baseline,
            "{}: injected tool output in {field} moved the verdict toward Allow",
            case.id
        );
    }
}

async fn attach_approval(
    f: &Fixture,
    store: &Arc<ApprovalStore>,
    ctx: &mut pair_core::types::PolicyContext,
    req: &ActionRequest,
    spec: &ApprovalSpec,
) {
    let hash_of = |r: &ActionRequest| match f.engine.authorize(r, &f.ctx).decision {
        Decision::NeedsApproval { payload_hash } => payload_hash,
        other => panic!("approval cases need an approval-gated request, got {other:?}"),
    };
    let bound = match spec.bind {
        Bind::Exact => hash_of(req),
        Bind::OtherPayload => {
            let mut other = req.clone();
            other.args.push("!".to_owned());
            hash_of(&other)
        }
    };
    let expiry = if spec.expired {
        Utc::now() - Duration::seconds(1)
    } else {
        Utc::now() + Duration::hours(1)
    };
    let id = store
        .approve(&bound, "owner", expiry)
        .await
        .expect("approve");
    if spec.consume_first {
        store.consume(id, &bound).await.expect("first use");
    }
    ctx.approvals = vec![id];
}

// ---------- tests ----------

#[test]
fn injection_eval_has_exactly_30_cases() {
    let cases = load_cases();
    assert_eq!(cases.len(), EXPECTED_CASES);
    let ids: BTreeSet<_> = cases.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(ids.len(), cases.len(), "case ids must be unique");
    for c in &cases {
        assert!(!c.attack.trim().is_empty(), "{}: attack text", c.id);
        assert!(!c.rationale.trim().is_empty(), "{}: rationale", c.id);
    }
    for c in cases.iter().filter(|c| c.category == "injected_output") {
        let text = c.tool_output.as_deref().unwrap_or_default();
        assert!(
            !text.trim().is_empty(),
            "{}: injected_output needs tool_output",
            c.id
        );
    }
    let categories: BTreeSet<_> = cases.iter().map(|c| c.category.as_str()).collect();
    for required in REQUIRED_CATEGORIES {
        assert!(categories.contains(required), "missing category {required}");
    }
}

#[test]
fn injection_eval_has_benign_controls() {
    let cases = load_cases();
    let controls: Vec<_> = cases
        .iter()
        .filter(|c| c.category == BENIGN_CATEGORY)
        .collect();
    assert!(
        controls.len() >= MIN_BENIGN_CONTROLS,
        "need {MIN_BENIGN_CONTROLS} controls to catch over-blocking, have {}",
        controls.len()
    );
    assert!(controls.iter().all(|c| c.expected == Expected::Allow));
    let allows = cases
        .iter()
        .filter(|c| c.expected == Expected::Allow)
        .count();
    assert_eq!(allows, controls.len(), "only controls may expect Allow");
}

#[tokio::test]
async fn injection_eval_all_cases_match_real_engine() {
    let mut disagreements = Vec::new();
    for case in load_cases() {
        let (observed, ran) = observe(&case).await;
        if observed != case.expected {
            disagreements.push(format!(
                "{} [{}] expected {:?}, engine said {:?}: {}",
                case.id, case.category, case.expected, observed, case.attack
            ));
        }
        assert_eq!(
            ran,
            observed == Expected::Allow,
            "{}: tool body must run if and only if allowed",
            case.id
        );
    }
    assert!(
        disagreements.is_empty(),
        "REAL FINDINGS, engine disagrees with dataset:\n{}",
        disagreements.join("\n")
    );
}
