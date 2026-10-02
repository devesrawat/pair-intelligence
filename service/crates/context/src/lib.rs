//! pair-context: compiles task state, memory and tool output into a model-ready,
//! budgeted prompt with a manifest of everything included or omitted.
mod compiler;
mod config;
mod counter;
mod hashing;
mod items;
mod wrap;

pub use compiler::PairContextCompiler;
pub use config::{ContextBudgets, ContextConfig};
pub use counter::{ApproxTokenCounter, TokenCounter};
pub use wrap::{wrap_external, DATA_CLOSE, DATA_OPEN};
