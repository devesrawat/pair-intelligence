use chrono::{DateTime, Utc};
use pair_core::error::{ErrorCode, PairError, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! text_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }
        impl $name {
            pub fn as_str(self) -> &'static str { match self { $(Self::$variant => $text),+ } }
            pub fn parse(s: &str) -> Result<Self> {
                match s {
                    $($text => Ok(Self::$variant),)+
                    other => Err(PairError::new(
                        ErrorCode::Internal,
                        format!("unknown {} value in storage: {other}", stringify!($name)),
                    )),
                }
            }
        }
    };
}

text_enum!(LoopKind { Commitment => "commitment", Inference => "inference" });
text_enum!(LoopStatus { Open => "open", Blocked => "blocked", Done => "done", Dropped => "dropped" });
text_enum!(EventKind {
    Intent => "intent",
    ObservedWork => "observed_work",
    ObservedCompletion => "observed_completion",
    Meeting => "meeting",
    Decision => "decision",
});

#[derive(Debug, Clone)]
pub struct NewLoop {
    pub title: String,
    pub owner: String,
    pub kind: LoopKind,
    pub status: LoopStatus,
    pub delegable: bool,
    pub due_at: Option<DateTime<Utc>>,
    pub source_ref: String,
    pub relationship: Option<String>,
    pub goal_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpenLoop {
    pub id: Uuid,
    pub title: String,
    pub owner: String,
    pub kind: LoopKind,
    pub status: LoopStatus,
    pub delegable: bool,
    pub due_at: Option<DateTime<Utc>>,
    pub source_ref: String,
    pub relationship: Option<String>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct NewEvent {
    pub kind: EventKind,
    pub occurred_at: DateTime<Utc>,
    pub summary: String,
    pub loop_id: Option<Uuid>,
    pub source_ref: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    pub id: Uuid,
    pub kind: EventKind,
    pub occurred_at: DateTime<Utc>,
    pub summary: String,
    pub loop_id: Option<Uuid>,
    pub source_ref: String,
}
