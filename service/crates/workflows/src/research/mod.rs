//! workflows::research — source-grounded research (spec section 10, Task 13).
//!
//! Stages: scope -> queries -> source discovery -> capture source versions -> dedupe ->
//! extract evidence -> compare claims -> synthesize -> citation validation -> save report.
mod capture;
mod extract;
mod fetch;
mod model;
mod pipeline;
mod report;
mod store;
mod support;
mod synth;
mod types;

pub use fetch::{normalize_url, Candidate, FetchOutcome, SourceFetcher};
pub use pipeline::{run_research, ResearchDeps, ResearchOutput, ResearchRun};
pub use report::render_markdown;
pub use store::EvidenceStore;
pub use support::{check_support, locate_span, JudgeVerdict, Support, SupportJudge};
pub use types::{
    Claim, Conflict, RawClaim, RejectReason, RejectedStatement, Report, ResearchScope, Source, Statement,
};

#[cfg(test)]
mod testdb;
#[cfg(test)]
mod tests;
