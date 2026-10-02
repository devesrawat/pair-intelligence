//! Provider-availability source for `/readyz`. The real registry (crates/models)
//! implements [`ProviderHealth`]; until wired, [`StubProviderHealth`] reports none.

use async_trait::async_trait;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProviderStatus {
    pub provider: String,
    pub available: bool,
    pub detail: String,
}

#[async_trait]
pub trait ProviderHealth: Send + Sync {
    async fn statuses(&self) -> Vec<ProviderStatus>;
}

/// Placeholder: no registry wired yet.
pub struct StubProviderHealth;

#[async_trait]
impl ProviderHealth for StubProviderHealth {
    async fn statuses(&self) -> Vec<ProviderStatus> {
        Vec::new()
    }
}
