//! Audit trail for the execution gate (spec sections 2 and 12: every tool execution, approval
//! and routing decision is traceable). The gate writes one row per `execute` call through an
//! [`ExecutionRecorder`]. Rows never carry raw arguments, only their sha256.
use async_trait::async_trait;
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::{ApprovalId, TaskId, ToolExecutionId, TraceId};
use pair_core::types::{ActionRequest, DataClass};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// What the policy decided for the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecDecision {
    Allow,
    Deny,
    NeedsApproval,
}

impl ExecDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::NeedsApproval => "needs_approval",
        }
    }
}

/// What happened. `Started` is the only non-terminal value: a row stuck there means the
/// process died (or the tool never returned) after the call began.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecOutcome {
    Started,
    Ok,
    Error,
    Denied,
    ApprovalRequired,
}

impl ExecOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Denied => "denied",
            Self::ApprovalRequired => "approval_required",
        }
    }

    pub fn is_terminal(self) -> bool {
        self != Self::Started
    }
}

/// One audit row. There is deliberately no field for raw arguments.
#[derive(Debug, Clone)]
pub struct ExecutionRecord {
    pub id: ToolExecutionId,
    pub task: TaskId,
    pub trace: TraceId,
    pub tool: String,
    pub executable: Option<String>,
    pub args_hash: String,
    pub destination: Option<String>,
    pub data_class: DataClass,
    pub policy_version: String,
    pub decision: ExecDecision,
    pub approval: Option<ApprovalId>,
    pub outcome: ExecOutcome,
    pub error_code: Option<String>,
    /// Set by recorders that track completion (the in-memory one); not an input.
    pub finished: bool,
}

/// sha256 (hex) of the canonical JSON array of arguments.
pub fn args_hash(args: &[String]) -> Result<String> {
    let bytes = serde_json::to_vec(args).map_err(|e| {
        PairError::new(ErrorCode::InvalidInput, format!("unserializable args: {e}"))
    })?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

/// Stable snake_case wire name of an error code.
pub fn error_code_str(code: ErrorCode) -> String {
    serde_json::to_value(code)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "internal".to_owned())
}

impl ExecutionRecord {
    pub fn new(
        req: &ActionRequest,
        policy_version: &str,
        decision: ExecDecision,
        approval: Option<ApprovalId>,
        outcome: ExecOutcome,
    ) -> Result<Self> {
        Ok(Self {
            id: ToolExecutionId::new(),
            task: req.task,
            trace: req.trace,
            tool: req.tool.clone(),
            executable: req.executable.clone(),
            args_hash: args_hash(&req.args)?,
            destination: req.destination.clone(),
            data_class: req.data_class,
            policy_version: policy_version.to_owned(),
            decision,
            approval,
            outcome,
            error_code: None,
            finished: outcome.is_terminal(),
        })
    }

    pub fn with_error(self, code: ErrorCode) -> Self {
        Self {
            error_code: Some(error_code_str(code)),
            ..self
        }
    }
}

/// Sink for gate audit rows. `record` inserts a row (terminal for refusals, `Started` for an
/// execution about to run); `finish` closes a `Started` row. The gate refuses to run a tool if
/// `record` fails, so a tool never runs without an audit row.
#[async_trait]
pub trait ExecutionRecorder: Send + Sync {
    async fn record(&self, row: &ExecutionRecord) -> Result<()>;
    async fn finish(
        &self,
        id: ToolExecutionId,
        outcome: ExecOutcome,
        error_code: Option<&str>,
    ) -> Result<()>;
}

/// Records nothing. The default for a gate built without `with_recorder`.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopRecorder;

#[async_trait]
impl ExecutionRecorder for NoopRecorder {
    async fn record(&self, _row: &ExecutionRecord) -> Result<()> {
        Ok(())
    }
    async fn finish(&self, _: ToolExecutionId, _: ExecOutcome, _: Option<&str>) -> Result<()> {
        Ok(())
    }
}

/// In-memory recorder for tests, with switchable write failure.
#[derive(Debug, Default)]
pub struct MemoryRecorder {
    rows: Mutex<Vec<ExecutionRecord>>,
    fail: AtomicBool,
}

impl MemoryRecorder {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Make every `record` and `finish` call fail (a down audit database).
    pub fn fail_writes(&self, fail: bool) {
        self.fail.store(fail, Ordering::SeqCst);
    }

    pub fn rows(&self) -> Vec<ExecutionRecord> {
        self.rows
            .lock()
            .map(|rows| rows.clone())
            .unwrap_or_default()
    }

    fn check_up(&self) -> Result<()> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(PairError::new(ErrorCode::Internal, "recorder unavailable"));
        }
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Vec<ExecutionRecord>>> {
        self.rows
            .lock()
            .map_err(|_| PairError::new(ErrorCode::Internal, "recorder lock poisoned"))
    }
}

#[async_trait]
impl ExecutionRecorder for MemoryRecorder {
    async fn record(&self, row: &ExecutionRecord) -> Result<()> {
        self.check_up()?;
        self.lock()?.push(row.clone());
        Ok(())
    }

    async fn finish(
        &self,
        id: ToolExecutionId,
        outcome: ExecOutcome,
        error_code: Option<&str>,
    ) -> Result<()> {
        self.check_up()?;
        let mut rows = self.lock()?;
        let row = rows
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or_else(|| PairError::new(ErrorCode::NotFound, format!("execution {id}")))?;
        row.outcome = outcome;
        row.error_code = error_code.map(str::to_owned);
        row.finished = true;
        Ok(())
    }
}
