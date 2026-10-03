//! `pair-ops purge|export`. Reads DATABASE_URL (required: there is deliberately no default, so a
//! typo can never point a purge at the live database).
use ops::cli::{self, Command, EXIT_STUCK_FOUND, USAGE};
use ops::{audit, export, retention, OpsError};
use sqlx::postgres::PgPoolOptions;
use std::process::ExitCode;

const DATABASE_URL_ENV: &str = "DATABASE_URL";
const MAX_CONNECTIONS: u32 = 2;
const EXIT_USAGE: u8 = 2;

/// Text for stdout and the process exit code.
struct Outcome {
    text: String,
    exit: ExitCode,
}

impl Outcome {
    fn ok(text: String) -> Self {
        Self {
            text,
            exit: ExitCode::SUCCESS,
        }
    }
}

async fn stuck_report(
    pool: &sqlx::PgPool,
    older_than: chrono::Duration,
) -> Result<Outcome, OpsError> {
    let rows = audit::stuck_executions(pool, older_than).await?;
    let total = audit::stuck_count(pool, older_than).await?;
    let mut text = format!(
        "{total} tool execution(s) still 'started' after {} minute(s)\n",
        older_than.num_minutes()
    );
    for r in &rows {
        text.push_str(&format!(
            "  {} tool={} decision={} age={}s trace={} task={}\n",
            r.id, r.tool, r.decision, r.age_secs, r.trace_id, r.task_id
        ));
    }
    let exit = if total > 0 {
        ExitCode::from(EXIT_STUCK_FOUND)
    } else {
        ExitCode::SUCCESS
    };
    Ok(Outcome { text, exit })
}

fn purge_text(report: &retention::PurgeReport) -> String {
    let mut out = format!(
        "purge {}: {} rows{}\n",
        if report.dry_run { "dry run" } else { "done" },
        report.total(),
        if report.truncated {
            " (batch limit hit; run again)"
        } else {
            ""
        },
    );
    for t in &report.targets {
        out.push_str(&format!("  {}: {}\n", t.target, t.erased));
    }
    for s in &report.skipped {
        out.push_str(&format!(
            "  {s}: skipped (nothing to erase in this schema)\n"
        ));
    }
    out
}

fn export_text(out_dir: &std::path::Path, manifest: &export::Manifest) -> String {
    let mut out = format!("export written to {}\n", out_dir.display());
    for f in &manifest.files {
        out.push_str(&format!(
            "  {} rows={} sha256={}\n",
            f.name, f.rows, f.sha256
        ));
    }
    out
}

async fn execute(pool: &sqlx::PgPool, command: Command) -> Result<Outcome, OpsError> {
    match command {
        Command::Purge(policy) => {
            let report = retention::purge(pool, chrono::Utc::now(), &policy).await?;
            Ok(Outcome::ok(purge_text(&report)))
        }
        Command::Export {
            out_dir,
            include_config,
        } => {
            let manifest = export::export_all(pool, &out_dir, include_config).await?;
            Ok(Outcome::ok(export_text(&out_dir, &manifest)))
        }
        Command::AuditStuck { older_than } => stuck_report(pool, older_than).await,
        Command::Help => Ok(Outcome::ok(USAGE.to_owned())),
    }
}

async fn run(command: Command) -> Result<Outcome, OpsError> {
    if command == Command::Help {
        return Ok(Outcome::ok(USAGE.to_owned()));
    }
    let url = std::env::var(DATABASE_URL_ENV)
        .map_err(|_| OpsError::InvalidArgument(format!("{DATABASE_URL_ENV} is not set")))?;
    let pool = PgPoolOptions::new()
        .max_connections(MAX_CONNECTIONS)
        .connect(&url)
        .await?;
    let outcome = execute(&pool, command).await;
    pool.close().await;
    outcome
}

#[tokio::main]
async fn main() -> ExitCode {
    let command = match cli::parse(std::env::args().skip(1)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pair-ops: {e}\n\n{USAGE}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    match run(command).await {
        Ok(outcome) => {
            print!("{}", outcome.text);
            outcome.exit
        }
        Err(e) => {
            eprintln!("pair-ops: {e}");
            ExitCode::FAILURE
        }
    }
}
