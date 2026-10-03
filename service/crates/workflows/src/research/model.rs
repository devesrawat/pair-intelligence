//! Budgeted model calls for the research stages. Page-derived content is always sent as
//! `TrustClass::Untrusted`; instructions come only from this module (`TrustClass::Owner`).
use crate::calls::{budgeted_generate, PriceSource};
use pair_core::{
    error::{ErrorCode, PairError, Result},
    ids::{TaskId, TraceId},
    traits::{BudgetEx, Provider},
    types::{DataClass, ModelMessage, ModelRequest, TaskKind, TrustClass},
};
use serde::de::DeserializeOwned;

const MAX_OUTPUT_TOKENS: u32 = 4096;
const CALL_DEADLINE_MS: u64 = 120_000;

pub struct ResearchLlm<'a> {
    pub provider: &'a dyn Provider,
    pub budget: &'a dyn BudgetEx,
    pub prices: &'a dyn PriceSource,
    pub task: TaskId,
    pub trace: TraceId,
    pub model_id: String,
}

pub fn owner_msg(content: impl Into<String>) -> ModelMessage {
    ModelMessage {
        role: "user".into(),
        content: content.into(),
        trust: TrustClass::Owner,
    }
}

pub fn untrusted_msg(content: impl Into<String>) -> ModelMessage {
    ModelMessage {
        role: "user".into(),
        content: content.into(),
        trust: TrustClass::Untrusted,
    }
}

impl ResearchLlm<'_> {
    pub async fn ask(&self, messages: Vec<ModelMessage>) -> Result<String> {
        let req = ModelRequest {
            model_id: self.model_id.clone(),
            messages,
            max_output_tokens: MAX_OUTPUT_TOKENS,
            deadline_ms: CALL_DEADLINE_MS,
            data_class: DataClass::Public,
            task: self.task,
            trace: self.trace,
        };
        Ok(budgeted_generate(
            self.provider,
            self.budget,
            self.prices,
            TaskKind::Research,
            req,
        )
        .await?
        .text)
    }

    /// Structured output only: the reply must contain one JSON object matching `T`.
    pub async fn ask_json<T: DeserializeOwned>(&self, messages: Vec<ModelMessage>) -> Result<T> {
        let text = self.ask(messages).await?;
        parse_json_object(&text)
    }
}

pub fn parse_json_object<T: DeserializeOwned>(text: &str) -> Result<T> {
    let (s, e) = match (text.find('{'), text.rfind('}')) {
        (Some(s), Some(e)) if e > s => (s, e),
        _ => {
            return Err(PairError::new(
                ErrorCode::InvalidInput,
                "model output has no JSON object",
            ))
        }
    };
    serde_json::from_str(&text[s..=e]).map_err(|e| {
        PairError::new(
            ErrorCode::InvalidInput,
            format!("model output invalid: {e}"),
        )
    })
}
