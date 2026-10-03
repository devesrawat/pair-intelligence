use crate::approvals::validate_grant;
use crate::config::{JobConfig, RunClass};
use crate::error::{db_err, lease_lost};
use crate::hash::sha256_hex;
use crate::lease::Lease;
use crate::records::{map_run, map_step, RunRecord, StepRecord, RUN_COLS};
use crate::state::{parse_state, state_str};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::{ApprovalId, IdempotencyKey, RunId};
use pair_core::traits::Workflows;
use pair_core::types::{RunState, WorkflowInput};
use sqlx::{PgPool, Postgres, Row, Transaction};
use std::sync::Arc;
use std::time::Duration;

const RUN_CLASS_FIELD: &str = "run_class";
const RESEARCH_CLASS_VALUE: &str = "research";

/// Milliseconds of active running time consumed by the current claim, as of "now" (or of the
/// lease expiry for a lease that already lapsed). Added to `active_base_ms` whenever a claim ends.
const ACCRUED_ACTIVE_MS: &str = "GREATEST(0, (EXTRACT(EPOCH FROM \
     (LEAST(now(), COALESCE(lease_expires_at, now())) - COALESCE(claimed_at, now()))) * 1000)::bigint)";

/// Postgres-backed job store. Cheap to clone.
#[derive(Clone)]
pub struct JobStore {
    pub(crate) pool: PgPool,
    pub(crate) cfg: Arc<JobConfig>,
}

pub(crate) fn plus(now: DateTime<Utc>, d: Duration) -> Result<DateTime<Utc>> {
    let delta = chrono::Duration::from_std(d)
        .map_err(|e| PairError::new(ErrorCode::Internal, format!("duration out of range: {e}")))?;
    Ok(now + delta)
}

fn millis(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

impl JobStore {
    pub fn new(pool: PgPool, cfg: JobConfig) -> Self {
        Self {
            pool,
            cfg: Arc::new(cfg),
        }
    }

    pub fn config(&self) -> &JobConfig {
        &self.cfg
    }

    /// Idempotent start: the same key always yields the same `RunId`. Reusing a key with a
    /// different kind, payload or run class is a `Conflict`.
    pub async fn start_with_class(
        &self,
        input: WorkflowInput,
        key: IdempotencyKey,
        class: RunClass,
    ) -> Result<RunId> {
        let canonical = serde_json::json!({
            "kind": input.kind,
            "payload": input.payload,
            "run_class": class.as_str(),
        });
        let input_hash = sha256_hex(&serde_json::to_vec(&canonical).map_err(|e| {
            PairError::new(
                ErrorCode::InvalidInput,
                format!("unserializable input: {e}"),
            )
        })?);
        let candidate = RunId::new();
        // Placeholder: the real deadline is computed from active running time at each claim.
        let deadline = plus(Utc::now(), self.cfg.timeout(class))?;
        sqlx::query(
            "INSERT INTO workflow_runs (id, idempotency_key, kind, input, input_hash, run_class, state, deadline_at) \
             VALUES ($1, $2, $3, $4, $5, $6, 'queued', $7) ON CONFLICT (idempotency_key) DO NOTHING",
        )
        .bind(candidate.0)
        .bind(key.0)
        .bind(&input.kind)
        .bind(&input.payload)
        .bind(&input_hash)
        .bind(class.as_str())
        .bind(deadline)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        let row =
            sqlx::query("SELECT id, input_hash FROM workflow_runs WHERE idempotency_key = $1")
                .bind(key.0)
                .fetch_one(&self.pool)
                .await
                .map_err(db_err)?;
        let stored_hash: String = row.try_get("input_hash").map_err(db_err)?;
        if stored_hash != input_hash {
            return Err(PairError::new(
                ErrorCode::Conflict,
                "idempotency key reused with different input",
            ));
        }
        let id = RunId(row.try_get("id").map_err(db_err)?);
        tracing::info!(run_id = %id, new = (id == candidate), "workflow start");
        Ok(id)
    }

    pub async fn state(&self, id: RunId) -> Result<RunState> {
        let row = sqlx::query("SELECT state FROM workflow_runs WHERE id = $1")
            .bind(id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?
            .ok_or_else(|| PairError::new(ErrorCode::NotFound, format!("run {id}")))?;
        parse_state(&row.try_get::<String, _>("state").map_err(db_err)?)
    }

    pub async fn get(&self, id: RunId) -> Result<RunRecord> {
        let sql = format!("SELECT {RUN_COLS} FROM workflow_runs WHERE id = $1");
        let row = sqlx::query(&sql)
            .bind(id.0)
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?
            .ok_or_else(|| PairError::new(ErrorCode::NotFound, format!("run {id}")))?;
        map_run(&row)
    }

    pub async fn steps(&self, id: RunId) -> Result<Vec<StepRecord>> {
        let rows = sqlx::query(
            "SELECT idx, name, output FROM workflow_steps WHERE run_id = $1 ORDER BY idx",
        )
        .bind(id.0)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter().map(map_step).collect()
    }

    /// Claim the oldest queued run with a lease. `FOR UPDATE SKIP LOCKED` keeps concurrent
    /// workers from claiming the same run. Each claim bumps `lease_epoch` (the fencing token) and
    /// sets the deadline to the run class's timeout minus the active time earlier claims used.
    pub async fn claim(&self, worker: &str) -> Result<Option<RunRecord>> {
        let now = Utc::now();
        let expires = plus(now, self.cfg.lease_ttl)?;
        let sql = format!(
            "UPDATE workflow_runs SET state = 'running', lease_owner = $1, lease_expires_at = $2, \
             lease_epoch = lease_epoch + 1, claimed_at = $3, \
             started_at = COALESCE(started_at, $3), updated_at = $3, \
             deadline_at = $3 + (GREATEST(0, (CASE run_class WHEN 'research' THEN $4 ELSE $5 END) - active_base_ms))::double precision \
                                 * interval '1 millisecond' \
             WHERE id = (SELECT id FROM workflow_runs WHERE state = 'queued' \
                         ORDER BY created_at, id LIMIT 1 FOR UPDATE SKIP LOCKED) \
             RETURNING {RUN_COLS}"
        );
        let row = sqlx::query(&sql)
            .bind(worker)
            .bind(expires)
            .bind(now)
            .bind(millis(self.cfg.research_timeout))
            .bind(millis(self.cfg.interactive_timeout))
            .fetch_optional(&self.pool)
            .await
            .map_err(db_err)?;
        row.as_ref().map(map_run).transpose()
    }

    /// Mark every `running` run whose lease expired as `interrupted`. Returns the count.
    pub async fn sweep_expired(&self) -> Result<u64> {
        let sql = format!(
            "UPDATE workflow_runs SET state = 'interrupted', lease_owner = NULL, updated_at = now(), \
             active_base_ms = active_base_ms + {ACCRUED_ACTIVE_MS} \
             WHERE state = 'running' AND lease_expires_at < now()"
        );
        let res = sqlx::query(&sql)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        if res.rows_affected() > 0 {
            tracing::warn!(
                count = res.rows_affected(),
                "stuck leases marked interrupted"
            );
        }
        Ok(res.rows_affected())
    }

    /// Renew the lease. A lease that already lapsed is lost even if nobody swept it yet: the next
    /// sweep or resume may hand the run to another worker at any moment.
    pub(crate) async fn heartbeat(&self, id: RunId, lease: &Lease) -> Result<()> {
        let expires = plus(Utc::now(), self.cfg.lease_ttl)?;
        let res = sqlx::query(
            "UPDATE workflow_runs SET lease_expires_at = $4, updated_at = now() \
             WHERE id = $1 AND lease_owner = $2 AND lease_epoch = $3 AND state = 'running' \
               AND lease_expires_at > now()",
        )
        .bind(id.0)
        .bind(&lease.owner)
        .bind(lease.epoch)
        .bind(expires)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        if res.rows_affected() == 0 {
            return Err(lease_lost());
        }
        Ok(())
    }

    /// Inside `tx`, verify the lease is still ours and keep it that way until the transaction ends
    /// (row share lock: a claim, sweep, resume or cancel waits for the commit). Every write that
    /// depends on holding the lease runs after this guard in the same transaction.
    pub(crate) async fn guard(
        tx: &mut Transaction<'_, Postgres>,
        id: RunId,
        lease: &Lease,
    ) -> Result<()> {
        sqlx::query(
            "SELECT 1 FROM workflow_runs WHERE id = $1 AND lease_owner = $2 AND lease_epoch = $3 \
               AND state = 'running' AND lease_expires_at > now() FOR SHARE",
        )
        .bind(id.0)
        .bind(&lease.owner)
        .bind(lease.epoch)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db_err)?
        .map(|_| ())
        .ok_or_else(lease_lost)
    }

    /// Persist a completed step and advance `next_step` atomically. Clears any attached approval
    /// so it cannot authorize an effect in a later step. With `terminal`, the run also becomes
    /// `succeeded` in the same transaction.
    pub(crate) async fn checkpoint(
        &self,
        id: RunId,
        lease: &Lease,
        idx: u32,
        name: &str,
        output: &serde_json::Value,
        terminal: Option<&serde_json::Value>,
    ) -> Result<()> {
        let idx_i = i32::try_from(idx)
            .map_err(|_| PairError::new(ErrorCode::Internal, "step index overflow"))?;
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let res = sqlx::query(
            "UPDATE workflow_runs SET next_step = $4 + 1, updated_at = now(), \
               approval_id = NULL, pending_action_hash = NULL, \
               state = CASE WHEN $5 THEN 'succeeded' ELSE state END, \
               output = CASE WHEN $5 THEN $6 ELSE output END, \
               finished_at = CASE WHEN $5 THEN now() ELSE finished_at END, \
               lease_owner = CASE WHEN $5 THEN NULL ELSE lease_owner END \
             WHERE id = $1 AND lease_owner = $2 AND lease_epoch = $3 AND state = 'running' \
               AND lease_expires_at > now() AND next_step = $4",
        )
        .bind(id.0)
        .bind(&lease.owner)
        .bind(lease.epoch)
        .bind(idx_i)
        .bind(terminal.is_some())
        .bind(terminal)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        if res.rows_affected() == 0 {
            return Err(lease_lost());
        }
        sqlx::query(
            "INSERT INTO workflow_steps (run_id, idx, name, output) VALUES ($1, $2, $3, $4)",
        )
        .bind(id.0)
        .bind(idx_i)
        .bind(name)
        .bind(output)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)
    }

    pub(crate) async fn release(
        &self,
        id: RunId,
        lease: &Lease,
        to: RunState,
        failure: Option<&str>,
        pending_hash: Option<&str>,
    ) -> Result<()> {
        let terminal = matches!(
            to,
            RunState::Failed | RunState::Succeeded | RunState::Cancelled
        );
        let sql = format!(
            "UPDATE workflow_runs SET state = $4, failure = $5, pending_action_hash = $6, approval_id = NULL, \
               active_base_ms = active_base_ms + {ACCRUED_ACTIVE_MS}, \
               lease_owner = NULL, lease_expires_at = NULL, updated_at = now(), \
               finished_at = CASE WHEN $7 THEN now() ELSE finished_at END \
             WHERE id = $1 AND lease_owner = $2 AND lease_epoch = $3 AND state = 'running'"
        );
        let res = sqlx::query(&sql)
            .bind(id.0)
            .bind(&lease.owner)
            .bind(lease.epoch)
            .bind(state_str(to))
            .bind(failure)
            .bind(pending_hash)
            .bind(terminal)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        if res.rows_affected() == 0 {
            return Err(lease_lost());
        }
        tracing::info!(run_id = %id, state = state_str(to), "run released");
        Ok(())
    }

    /// Count a tool call against the run's persisted budget, only while the lease is ours;
    /// `Ok(false)` when the cap is hit.
    pub(crate) async fn record_tool_call(&self, id: RunId, lease: &Lease) -> Result<bool> {
        let cap = i32::try_from(self.cfg.max_tool_calls).unwrap_or(i32::MAX);
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        Self::guard(&mut tx, id, lease).await?;
        let res = sqlx::query(
            "UPDATE workflow_runs SET tool_calls = tool_calls + 1 WHERE id = $1 AND tool_calls < $2",
        )
        .bind(id.0)
        .bind(cap)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(res.rows_affected() == 1)
    }

    /// Cancel a run. A worker still holding it notices on its next heartbeat (or its next fenced
    /// write, whichever comes first) and drops its in-flight step.
    pub async fn cancel(&self, id: RunId) -> Result<RunState> {
        sqlx::query(
            "UPDATE workflow_runs SET state = 'cancelled', lease_owner = NULL, lease_expires_at = NULL, \
             finished_at = now(), updated_at = now() \
             WHERE id = $1 AND state NOT IN ('succeeded', 'failed', 'cancelled')",
        )
        .bind(id.0)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        self.state(id).await
    }

    /// Attach a valid approval to a run waiting on one and make it runnable again.
    /// The approval must match the run's pending action hash and still be live.
    pub async fn grant(&self, id: RunId, approval: ApprovalId) -> Result<()> {
        let mut tx = self.pool.begin().await.map_err(db_err)?;
        let run = sqlx::query(
            "SELECT state, pending_action_hash FROM workflow_runs WHERE id = $1 FOR UPDATE",
        )
        .bind(id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?
        .ok_or_else(|| PairError::new(ErrorCode::NotFound, format!("run {id}")))?;
        let state: String = run.try_get("state").map_err(db_err)?;
        let pending: Option<String> = run.try_get("pending_action_hash").map_err(db_err)?;
        if state != state_str(RunState::WaitingApproval) {
            return Err(PairError::new(
                ErrorCode::Conflict,
                format!("run {id} is not waiting for approval"),
            ));
        }
        let pending = pending.ok_or_else(|| {
            PairError::new(ErrorCode::Internal, "waiting run without pending hash")
        })?;
        validate_grant(&mut tx, approval, &pending).await?;
        sqlx::query("UPDATE workflow_runs SET state = 'queued', approval_id = $2, updated_at = now() WHERE id = $1")
            .bind(id.0)
            .bind(approval.0)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        tx.commit().await.map_err(db_err)
    }

    /// True if the run still has an effect intent that may have happened but was never settled.
    pub(crate) async fn has_unresolved_intents(&self, id: RunId) -> Result<bool> {
        sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM effect_intents WHERE run_id = $1 \
               AND status IN ('intended', 'executing', 'unknown'))",
        )
        .bind(id.0)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)
    }
}

/// Run class is `research` when the payload carries `"run_class": "research"`; otherwise interactive.
fn class_of(input: &WorkflowInput) -> RunClass {
    match input.payload.get(RUN_CLASS_FIELD).and_then(|v| v.as_str()) {
        Some(RESEARCH_CLASS_VALUE) => RunClass::Research,
        _ => RunClass::Interactive,
    }
}

#[async_trait]
impl Workflows for JobStore {
    async fn start(&self, input: WorkflowInput, key: IdempotencyKey) -> Result<RunId> {
        let class = class_of(&input);
        self.start_with_class(input, key, class).await
    }

    /// Make a stuck or interrupted run runnable again and return its state. A run whose lease has
    /// expired is interrupted first. `waiting_approval` runs stay put until `grant`.
    async fn resume(&self, id: RunId) -> Result<RunState> {
        let sql = format!(
            "UPDATE workflow_runs SET state = 'interrupted', lease_owner = NULL, updated_at = now(), \
             active_base_ms = active_base_ms + {ACCRUED_ACTIVE_MS} \
             WHERE id = $1 AND state = 'running' AND lease_expires_at < now()"
        );
        sqlx::query(&sql)
            .bind(id.0)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        sqlx::query(
            "UPDATE workflow_runs SET state = 'queued', lease_owner = NULL, lease_expires_at = NULL, updated_at = now() \
             WHERE id = $1 AND state = 'interrupted'",
        )
        .bind(id.0)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        self.state(id).await
    }
}
