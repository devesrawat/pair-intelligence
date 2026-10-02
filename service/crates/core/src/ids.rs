use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! typed_id {
    ($($name:ident),+ $(,)?) => {$(
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);
        impl $name {
            pub fn new() -> Self { Self(Uuid::now_v7()) }
        }
        impl Default for $name { fn default() -> Self { Self::new() } }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { self.0.fmt(f) }
        }
    )+};
}

typed_id!(
    TaskId,
    ReservationId,
    LedgerEntryId,
    CandidateId,
    MemoryId,
    SourceId,
    RunId,
    ApprovalId,
    TraceId,
    ModelCallId,
    ToolExecutionId,
    ConversationId,
    IdempotencyKey,
);
