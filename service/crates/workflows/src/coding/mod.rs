//! workflows::coding — engineering workflow (spec section 10, Task 12).
//!
//! Stages: issue -> context -> plan -> worktree -> edit -> verify -> review -> draft result.
//! Every command goes through the injected `Policy`; no remote write is ever executed here.
mod classify;
mod config;
mod edits;
mod exec;
mod integrity;
mod remote;
mod runner;
mod sandbox;
mod scope;
mod workflow;
mod worktree;

pub use classify::{classify_failure, FailureClass};
pub use config::RepoConfig;
pub use edits::{apply_edits, parse_edit_set, EditContext, FileEdit};
pub use exec::{CommandExecutor, ExecOutput, ExecSpec, ProcessExecutor};
pub use remote::{
    egress_host, request_remote_write, resolve_push_url, RemoteAction, RemoteCtx, RemoteKind,
    RemoteOutcome,
};
pub use runner::{CmdReport, Runner, RunnerSetup};
pub use sandbox::{ContainerSandbox, HostSandbox, Sandbox, HOST_EXEC_ENV, WORKER_IMAGE_ENV};
pub use scope::{is_secret_path, Scope};
pub use workflow::{
    discard_workspace, run_coding_task, CodingDeps, CodingResult, CodingStatus, CodingTask, Stage,
};

#[cfg(test)]
mod exec_tests;
#[cfg(test)]
mod remote_tests;
#[cfg(test)]
mod sandbox_tests;
#[cfg(test)]
pub(crate) mod testkit;
#[cfg(test)]
mod tests;
