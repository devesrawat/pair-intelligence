//! In-test fakes of external services (policy engine, model provider, budget ledger,
//! memory store, context compiler). Test-only.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use async_trait::async_trait;
use pair_core::{
    error::Result,
    ids::{CandidateId, LedgerEntryId, MemoryId, ReservationId, TaskId},
    money::Micros,
    traits::{Budget, ContextCompiler, Memory, Policy, Provider},
    types::*,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

pub type Responder = Box<dyn Fn(usize, &ModelRequest) -> Result<String> + Send + Sync>;

/// Provider whose reply is computed by a closure from (call index, request).
pub struct FnProvider {
    responder: Responder,
    calls: AtomicUsize,
    pub seen: Mutex<Vec<ModelRequest>>,
}

impl FnProvider {
    pub fn new(responder: Responder) -> Self {
        Self {
            responder,
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        }
    }
    /// Replies with the given texts in order.
    pub fn scripted(texts: Vec<String>) -> Self {
        Self::new(Box::new(move |i, _| {
            Ok(texts.get(i).cloned().unwrap_or_default())
        }))
    }
    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for FnProvider {
    async fn generate(&self, req: ModelRequest) -> Result<ModelResponse> {
        let i = self.calls.fetch_add(1, Ordering::SeqCst);
        let text = (self.responder)(i, &req)?;
        self.seen.lock().unwrap().push(req);
        Ok(ModelResponse {
            resolved_model: "fake".into(),
            text,
            usage: UsageReport {
                input_tokens: 1,
                output_tokens: 1,
                actual_cost: Some(Micros(1)),
                price_version: "t".into(),
            },
            provider_request_id: None,
            latency_ms: 0,
        })
    }
}

/// Denies listed executables / destinations; sends remote writes to approval.
#[derive(Default)]
pub struct FakePolicy {
    pub denied_exes: Vec<String>,
    pub denied_destinations: Vec<String>,
    pub allow_remote_writes: bool,
    pub seen: Mutex<Vec<ActionRequest>>,
}

impl Policy for FakePolicy {
    fn authorize(&self, req: &ActionRequest, _ctx: &PolicyContext) -> PolicyOutcome {
        self.seen.lock().unwrap().push(req.clone());
        let decision =
            if matches!(req.tool.as_str(), "git_push" | "open_pr") && !self.allow_remote_writes {
                Decision::NeedsApproval {
                    payload_hash: "hash".into(),
                }
            } else if req
                .executable
                .as_ref()
                .is_some_and(|e| self.denied_exes.contains(e))
                || req
                    .destination
                    .as_ref()
                    .is_some_and(|d| self.denied_destinations.contains(d))
            {
                Decision::Deny {
                    reason: "denied by fake policy".into(),
                }
            } else {
                Decision::Allow
            };
        PolicyOutcome {
            decision,
            policy_version: "fake-1".into(),
        }
    }
}

/// Policy that answers every request with the same decision.
pub struct FixedPolicy(pub Decision);

impl Policy for FixedPolicy {
    fn authorize(&self, _req: &ActionRequest, _ctx: &PolicyContext) -> PolicyOutcome {
        PolicyOutcome {
            decision: self.0.clone(),
            policy_version: "fixed-1".into(),
        }
    }
}

pub struct FakeBudget {
    pub reserved: AtomicUsize,
    pub reconciled: AtomicUsize,
}

impl Default for FakeBudget {
    fn default() -> Self {
        Self {
            reserved: AtomicUsize::new(0),
            reconciled: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl Budget for FakeBudget {
    async fn reserve(&self, _task: TaskId, _max: Micros) -> Result<ReservationId> {
        self.reserved.fetch_add(1, Ordering::SeqCst);
        Ok(ReservationId::new())
    }
    async fn reconcile(&self, id: ReservationId, _usage: UsageReport) -> Result<LedgerEntry> {
        self.reconciled.fetch_add(1, Ordering::SeqCst);
        Ok(LedgerEntry {
            id: LedgerEntryId::new(),
            reservation: id,
            amount: Micros(1),
            settled: true,
        })
    }
}

pub struct EmptyMemory;

#[async_trait]
impl Memory for EmptyMemory {
    async fn propose(&self, _c: MemoryCandidate) -> Result<CandidateId> {
        Ok(CandidateId::new())
    }
    async fn accept(&self, _id: CandidateId, _actor: &str) -> Result<MemoryId> {
        Ok(MemoryId::new())
    }
    async fn retrieve(&self, _q: RetrievalQuery) -> Result<Vec<EvidenceItem>> {
        Ok(Vec::new())
    }
}

pub struct PlainCompiler;

impl ContextCompiler for PlainCompiler {
    fn compile(
        &self,
        ctx: &TaskContext,
        _limits: &ModelLimits,
        _memory: &[EvidenceItem],
    ) -> Result<CompiledContext> {
        Ok(CompiledContext {
            messages: vec![ModelMessage {
                role: "user".into(),
                content: ctx.objective.clone(),
                trust: TrustClass::Owner,
            }],
            manifest: Vec::new(),
            total_tokens: 1,
        })
    }
}
