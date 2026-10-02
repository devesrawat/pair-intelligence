//! workflows::coding — engineering workflow (spec section 10, Task 12).
//!
//! Stages: issue -> context -> plan -> worktree -> edit -> verify -> review -> draft result.
//! Every command goes through the injected `Policy`; no remote write is ever executed here.
mod classify;
mod config;
mod edits;
mod llm;
mod remote;
mod runner;
mod scope;
mod workflow;
mod worktree;

pub use classify::{classify_failure, FailureClass};
pub use config::RepoConfig;
pub use edits::{apply_edits, parse_edit_set, FileEdit};
pub use llm::budgeted_generate;
pub use remote::{request_remote_write, RemoteAction, RemoteKind, RemoteOutcome};
pub use runner::{CmdReport, Runner};
pub use scope::{is_secret_path, Scope};
pub use workflow::{
    discard_workspace, run_coding_task, CodingDeps, CodingResult, CodingStatus, CodingTask, Stage,
};

#[cfg(test)]
pub(crate) mod testkit;
#[cfg(test)]
mod tests;
