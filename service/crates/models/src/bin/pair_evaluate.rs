//! `scripts/evaluate baseline|jev`. See evals/README.md. Prints modeled/draft-label numbers only.
use pair_budget::{BudgetConfig, PgBudget, PriceBook};
use pair_core::types::DataClass;
use pair_models::classification::eval::{
    load_router, render, require_budget_for_real_endpoint, run_classifier_assisted,
    run_fixed_baseline, run_rules, Dataset, Split, StrategyReport,
};
use pair_models::classification::jev::{ApiKey, JevClassifier, JevSettings, API_KEY_ENV};
use pair_models::classification::questions::QuestionSet;
use sqlx::postgres::PgPoolOptions;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

const DATABASE_URL_ENV: &str = "DATABASE_URL";
const BUDGET_CONFIG: &str = "config/budget.yaml";
const DB_CONNECTIONS: u32 = 2;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

async fn run(mode: &str) -> Result<String, String> {
    let root = root();
    let dataset =
        Dataset::load(&root.join("evals/datasets/routing.jsonl")).map_err(|e| e.to_string())?;
    let router = load_router(&root.join("config/models.yaml")).map_err(|e| e.to_string())?;
    let mut reports: Vec<StrategyReport> = Vec::new();
    for (name, split) in [("dev", Split::Dev), ("held_out", Split::HeldOut)] {
        let cases = dataset.split(split);
        reports.push(run_fixed_baseline(&cases, &router, name));
        reports.push(run_rules(&cases, &router, name));
    }
    let mut out = render(&reports);
    if mode == "jev" {
        match live_prerequisites(&root, &router).await {
            Ok((key, budget)) => {
                out.push('\n');
                out.push_str(&run_live(&root, &router, &dataset, key, &budget).await?);
            }
            // Offline strategies above are still reported; nothing is spent without a key and a budget.
            Err(reason) => out.push_str(&format!(
                "\n[skipped] classifier-assisted (Jev) strategy: {reason}\n"
            )),
        }
    } else {
        out.push_str(&format!(
            "\n[skipped] classifier-assisted (Jev) strategy: needs the owner's {API_KEY_ENV} (run `scripts/evaluate jev`) and,\n\
             to mean anything, real downstream workflow runs on owner-reviewed labels. No Jev accuracy is claimed.\n"
        ));
    }
    Ok(out)
}

/// The vendor key and a budget to meter every classifier call. Missing either means no live run.
async fn live_prerequisites(
    root: &std::path::Path,
    router: &pair_models::router::ConfigRouter,
) -> Result<(ApiKey, PgBudget), String> {
    let key = ApiKey::from_env()
        .map_err(|_| format!("jev strategy needs the owner's {API_KEY_ENV}; not set"))?;
    let cfg = router.config();
    let database_url = std::env::var(DATABASE_URL_ENV).ok();
    require_budget_for_real_endpoint(&cfg.classifier.endpoint, database_url.is_some())
        .map_err(|e| e.to_string())?;
    let url = database_url
        .ok_or_else(|| format!("{DATABASE_URL_ENV} is required to meter classifier calls"))?;
    let budget_yaml = std::fs::read_to_string(root.join(BUDGET_CONFIG))
        .map_err(|e| format!("read {BUDGET_CONFIG}: {e}"))?;
    let budget_cfg = BudgetConfig::from_yaml_strict(&budget_yaml).map_err(|e| e.to_string())?;
    let pool = PgPoolOptions::new()
        .max_connections(DB_CONNECTIONS)
        .connect(&url)
        .await
        .map_err(|e| format!("connect to budget database: {e}"))?;
    let prices = PriceBook::new(Some(cfg.classifier.price_version.clone()), []);
    Ok((key, PgBudget::new(pool, budget_cfg, prices)))
}

async fn run_live(
    root: &std::path::Path,
    router: &pair_models::router::ConfigRouter,
    dataset: &Dataset,
    key: ApiKey,
    budget: &PgBudget,
) -> Result<String, String> {
    let cfg = router.config();
    let questions = QuestionSet::from_path(&root.join(&cfg.classifier.questions_path))
        .map_err(|e| e.to_string())?;
    let settings = JevSettings {
        endpoint: cfg.classifier.endpoint.clone(),
        model: cfg.classifier.model.clone(),
        deadline: Duration::from_millis(cfg.classifier.deadline_ms),
    };
    let jev = JevClassifier::new(settings, key, questions.clone()).map_err(|e| e.to_string())?;
    let mut live = Vec::new();
    for (name, split) in [("dev", Split::Dev), ("held_out", Split::HeldOut)] {
        // Only public cases are sent to the vendor.
        let cases: Vec<_> = dataset
            .split(split)
            .into_iter()
            .filter(|c| c.data_class == DataClass::Public)
            .collect();
        let mut r = run_classifier_assisted(&cases, &jev, router, budget, &questions, name).await;
        r.notes.push("live Jev; public-data cases only; every call reserved and reconciled in the budget ledger; downstream acceptance NOT measured (needs real workflow runs)".into());
        live.push(r);
    }
    Ok(render(&live))
}

#[tokio::main]
async fn main() -> ExitCode {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "baseline".into());
    if mode != "baseline" && mode != "jev" {
        eprintln!("usage: scripts/evaluate [baseline|jev]");
        return ExitCode::from(2);
    }
    match run(&mode).await {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("evaluate failed: {e}");
            ExitCode::FAILURE
        }
    }
}
