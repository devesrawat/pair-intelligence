//! `pair-ops purge|export`. Reads DATABASE_URL (required: there is deliberately no default, so a
//! typo can never point a purge at the live database).
use ops::cli::{self, Command, USAGE};
use ops::{export, retention, OpsError};
use sqlx::postgres::PgPoolOptions;
use std::process::ExitCode;

const DATABASE_URL_ENV: &str = "DATABASE_URL";
const MAX_CONNECTIONS: u32 = 2;
const EXIT_USAGE: u8 = 2;

async fn run(command: Command) -> Result<String, OpsError> {
    if command == Command::Help {
        return Ok(USAGE.to_owned());
    }
    let url = std::env::var(DATABASE_URL_ENV)
        .map_err(|_| OpsError::InvalidArgument(format!("{DATABASE_URL_ENV} is not set")))?;
    let pool = PgPoolOptions::new()
        .max_connections(MAX_CONNECTIONS)
        .connect(&url)
        .await?;
    let text = match command {
        Command::Purge(policy) => {
            let report = retention::purge(&pool, chrono::Utc::now(), &policy).await?;
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
                out.push_str(&format!("  {s}: skipped (not in this schema)\n"));
            }
            out
        }
        Command::Export {
            out_dir,
            include_config,
        } => {
            let manifest = export::export_all(&pool, &out_dir, include_config).await?;
            let mut out = format!("export written to {}\n", out_dir.display());
            for f in &manifest.files {
                out.push_str(&format!(
                    "  {} rows={} sha256={}\n",
                    f.name, f.rows, f.sha256
                ));
            }
            out
        }
        Command::Help => USAGE.to_owned(),
    };
    pool.close().await;
    Ok(text)
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
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("pair-ops: {e}");
            ExitCode::FAILURE
        }
    }
}
