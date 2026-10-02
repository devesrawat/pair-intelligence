//! scope -> queries -> discovery -> capture -> dedupe -> extract -> compare -> synthesize
//! -> citation validation -> save report.
use super::{
    capture::{capture_sources, Gate},
    extract::{compare_claims, extract_claims, validate_claims},
    fetch::SourceFetcher,
    model::ResearchLlm,
    report::render_markdown,
    store::EvidenceStore,
    support::SupportJudge,
    synth::synthesize,
    types::{Report, ResearchScope},
};
use chrono::Utc;
use pair_core::{
    error::{ErrorCode, PairError, Result},
    ids::{TaskId, TraceId},
    money::Micros,
    traits::{Budget, Policy, Provider},
};

pub struct ResearchDeps<'a> {
    pub provider: &'a dyn Provider,
    pub policy: &'a dyn Policy,
    pub budget: &'a dyn Budget,
    pub fetcher: &'a dyn SourceFetcher,
    pub store: &'a EvidenceStore,
    /// Optional semantic veto on top of the deterministic support check.
    pub judge: Option<&'a dyn SupportJudge>,
}

pub struct ResearchRun {
    pub task: TaskId,
    pub trace: TraceId,
    pub model_id: String,
    pub max_call_cost: Micros,
}

pub struct ResearchOutput {
    pub report: Report,
    pub markdown: String,
}

fn validate_scope(scope: &ResearchScope) -> Result<()> {
    if scope.question.trim().is_empty() || scope.max_sources == 0 {
        return Err(PairError::new(
            ErrorCode::InvalidInput,
            "research scope needs a question and max_sources > 0",
        ));
    }
    Ok(())
}

pub async fn run_research(
    deps: &ResearchDeps<'_>,
    run: &ResearchRun,
    scope: &ResearchScope,
) -> Result<ResearchOutput> {
    validate_scope(scope)?;
    let run_id = deps.store.create_run(scope).await?;
    match execute(deps, run, scope, run_id).await {
        Ok(out) => {
            deps.store.finish_run(run_id, true).await?;
            Ok(out)
        }
        Err(e) => {
            if let Err(fe) = deps.store.finish_run(run_id, false).await {
                tracing::error!(error = %fe.message, "could not mark research run failed");
            }
            Err(e)
        }
    }
}

async fn execute(
    deps: &ResearchDeps<'_>,
    run: &ResearchRun,
    scope: &ResearchScope,
    run_id: uuid::Uuid,
) -> Result<ResearchOutput> {
    let gate = Gate {
        policy: deps.policy,
        task: run.task,
        trace: run.trace,
    };
    let llm = ResearchLlm {
        provider: deps.provider,
        budget: deps.budget,
        task: run.task,
        trace: run.trace,
        model_id: run.model_id.clone(),
        max_cost: run.max_call_cost,
    };

    let sources = capture_sources(&gate, deps.fetcher, scope).await?;
    for s in sources
        .iter()
        .filter(|s| s.duplicate_of.is_none())
        .chain(sources.iter().filter(|s| s.duplicate_of.is_some()))
    {
        deps.store.save_source(run_id, s).await?;
    }
    tracing::info!(run = %run_id, sources = sources.len(), "sources captured");

    let raws = extract_claims(&llm, &sources).await?;
    let claims = validate_claims(raws, &sources, deps.judge).await?;
    let conflicts = compare_claims(&claims);
    let (statements, rejected_statements) =
        synthesize(&llm, &scope.question, &claims, &conflicts).await?;
    for c in &claims {
        deps.store.save_claim(run_id, c, &sources).await?;
    }

    let limitations = limitations(deps, &sources, &claims, &conflicts, &rejected_statements);
    let report = Report {
        run_id,
        question: scope.question.clone(),
        generated_at: Utc::now(),
        sources,
        claims,
        conflicts,
        statements,
        rejected_statements,
        limitations,
    };
    let markdown = render_markdown(&report);
    deps.store.save_report(run_id, &markdown).await?;
    Ok(ResearchOutput { report, markdown })
}

fn limitations(
    deps: &ResearchDeps<'_>,
    sources: &[super::types::Source],
    claims: &[super::types::Claim],
    conflicts: &[super::types::Conflict],
    rejected_statements: &[super::types::RejectedStatement],
) -> Vec<String> {
    let mut out = Vec::new();
    let unavailable = sources.iter().filter(|s| !s.available).count();
    if unavailable > 0 {
        out.push(format!(
            "{unavailable} source(s) were inaccessible; evidence from them is missing."
        ));
    }
    let rejected = claims.iter().filter(|c| !c.is_valid()).count();
    if rejected > 0 {
        out.push(format!(
            "{rejected} extracted claim(s) failed citation validation and were excluded."
        ));
    }
    if !rejected_statements.is_empty() {
        out.push(format!(
            "{} synthesized statement(s) failed citation validation and were excluded.",
            rejected_statements.len()
        ));
    }
    if !conflicts.is_empty() {
        out.push(format!(
            "{} topic(s) have conflicting evidence; no resolution is asserted.",
            conflicts.len()
        ));
    }
    if sources
        .iter()
        .any(|s| s.published_at.is_none() && s.available)
    {
        out.push("Some sources carry no publication date.".into());
    }
    if deps.judge.is_none() {
        out.push("Support was checked lexically against captured text only (no semantic judge); every claim still needs a manual audit.".into());
    }
    out
}
