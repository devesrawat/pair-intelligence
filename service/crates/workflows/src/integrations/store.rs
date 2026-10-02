use super::client::Provider;
use crate::daily::store::db_err;
use pair_core::error::{ErrorCode, PairError, Result};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountState {
    Connected,
    /// Provider revoked the token; ingestion halted until the owner reconnects.
    Revoked,
    /// Owner disconnected; no future ingestion.
    Disconnected,
}

impl AccountState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Revoked => "revoked",
            Self::Disconnected => "disconnected",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        match s {
            "connected" => Ok(Self::Connected),
            "revoked" => Ok(Self::Revoked),
            "disconnected" => Ok(Self::Disconnected),
            other => Err(PairError::new(
                ErrorCode::Internal,
                format!("unknown account state {other}"),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntegrationAccount {
    pub id: Uuid,
    pub provider: Provider,
    pub state: AccountState,
    /// Folders/labels or calendar IDs that may be read. Empty by default.
    pub allowlist: Vec<String>,
    pub writes_enabled: bool,
}

/// Create an account with an empty allowlist and writes disabled.
pub async fn create_account(pool: &PgPool, provider: Provider) -> Result<Uuid> {
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO integration_accounts (id, provider) VALUES ($1, $2)")
        .bind(id)
        .bind(provider.as_str())
        .execute(pool)
        .await
        .map_err(db_err)?;
    Ok(id)
}

pub async fn get_account(pool: &PgPool, id: Uuid) -> Result<IntegrationAccount> {
    let row = sqlx::query(
        "SELECT provider, state, allowlist, writes_enabled FROM integration_accounts WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?
    .ok_or_else(|| PairError::new(ErrorCode::NotFound, "integration account not found"))?;
    let provider: String = row.try_get("provider").map_err(db_err)?;
    let state: String = row.try_get("state").map_err(db_err)?;
    let allowlist: serde_json::Value = row.try_get("allowlist").map_err(db_err)?;
    Ok(IntegrationAccount {
        id,
        provider: Provider::parse(&provider).ok_or_else(|| {
            PairError::new(ErrorCode::Internal, format!("unknown provider {provider}"))
        })?,
        state: AccountState::parse(&state)?,
        allowlist: serde_json::from_value(allowlist)
            .map_err(|e| PairError::new(ErrorCode::Internal, format!("corrupt allowlist: {e}")))?,
        writes_enabled: row.try_get("writes_enabled").map_err(db_err)?,
    })
}

pub async fn set_allowlist(pool: &PgPool, id: Uuid, scopes: &[String]) -> Result<()> {
    sqlx::query("UPDATE integration_accounts SET allowlist = $2 WHERE id = $1")
        .bind(id)
        .bind(serde_json::json!(scopes))
        .execute(pool)
        .await
        .map_err(db_err)?;
    Ok(())
}

pub async fn set_state(pool: &PgPool, id: Uuid, state: AccountState) -> Result<()> {
    sqlx::query("UPDATE integration_accounts SET state = $2 WHERE id = $1")
        .bind(id)
        .bind(state.as_str())
        .execute(pool)
        .await
        .map_err(db_err)?;
    Ok(())
}

pub async fn set_writes_enabled(pool: &PgPool, id: Uuid, enabled: bool) -> Result<()> {
    sqlx::query("UPDATE integration_accounts SET writes_enabled = $2 WHERE id = $1")
        .bind(id)
        .bind(enabled)
        .execute(pool)
        .await
        .map_err(db_err)?;
    Ok(())
}

pub async fn get_cursor(pool: &PgPool, id: Uuid, scope: &str) -> Result<Option<String>> {
    let row =
        sqlx::query("SELECT cursor FROM integration_cursors WHERE account_id = $1 AND scope = $2")
            .bind(id)
            .bind(scope)
            .fetch_optional(pool)
            .await
            .map_err(db_err)?;
    row.map(|r| r.try_get("cursor").map_err(db_err)).transpose()
}

/// Number of non-tombstoned sources held for the account.
pub async fn active_source_count(pool: &PgPool, id: Uuid) -> Result<i64> {
    sqlx::query_scalar(
        "SELECT count(*) FROM integration_sources WHERE account_id = $1 AND state = 'active'",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .map_err(db_err)
}
