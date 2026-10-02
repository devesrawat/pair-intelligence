use crate::config::RunClass;
use crate::error::db_err;
use crate::state::parse_state;
use chrono::{DateTime, Utc};
use pair_core::error::{ErrorCode, PairError, Result};
use pair_core::ids::{ApprovalId, RunId};
use pair_core::types::RunState;
use sqlx::{postgres::PgRow, Row};
use uuid::Uuid;

pub(crate) const RUN_COLS: &str = "id, kind, input, run_class, state, next_step, tool_calls, \
     deadline_at, approval_id, pending_action_hash";

#[derive(Debug, Clone)]
pub struct RunRecord {
    pub id: RunId,
    pub kind: String,
    pub input: serde_json::Value,
    pub class: RunClass,
    pub state: RunState,
    pub next_step: u32,
    pub tool_calls: u32,
    pub deadline_at: DateTime<Utc>,
    pub approval_id: Option<ApprovalId>,
    pub pending_action_hash: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StepRecord {
    pub idx: u32,
    pub name: String,
    pub output: serde_json::Value,
}

fn to_u32(v: i32) -> Result<u32> {
    u32::try_from(v).map_err(|_| PairError::new(ErrorCode::Internal, "negative counter"))
}

pub(crate) fn map_run(row: &PgRow) -> Result<RunRecord> {
    let class: String = row.try_get("run_class").map_err(db_err)?;
    let state: String = row.try_get("state").map_err(db_err)?;
    let approval: Option<Uuid> = row.try_get("approval_id").map_err(db_err)?;
    Ok(RunRecord {
        id: RunId(row.try_get("id").map_err(db_err)?),
        kind: row.try_get("kind").map_err(db_err)?,
        input: row.try_get("input").map_err(db_err)?,
        class: RunClass::parse(&class)
            .ok_or_else(|| PairError::new(ErrorCode::Internal, format!("unknown run class {class}")))?,
        state: parse_state(&state)?,
        next_step: to_u32(row.try_get("next_step").map_err(db_err)?)?,
        tool_calls: to_u32(row.try_get("tool_calls").map_err(db_err)?)?,
        deadline_at: row.try_get("deadline_at").map_err(db_err)?,
        approval_id: approval.map(ApprovalId),
        pending_action_hash: row.try_get("pending_action_hash").map_err(db_err)?,
    })
}

pub(crate) fn map_step(row: &PgRow) -> Result<StepRecord> {
    Ok(StepRecord {
        idx: to_u32(row.try_get("idx").map_err(db_err)?)?,
        name: row.try_get("name").map_err(db_err)?,
        output: row.try_get("output").map_err(db_err)?,
    })
}
