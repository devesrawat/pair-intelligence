//! `scripts/evaluate baseline|jev`. See evals/README.md. Prints modeled/draft-label numbers only.
use pair_core::types::DataClass;
use pair_models::classification::eval::{
    load_router, render, run_classifier_assisted, run_fixed_baseline, run_rules, Dataset, Split,
    StrategyReport,
};
use pair_models::classification::jev::{ApiKey, JevClassifier, JevSettings, API_KEY_ENV};
use pair_models::classification::questions::QuestionSet;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

async fn run(mode: &str) -> Result<String, String> {
    let root = root();
    let dataset =
        Dataset::load(&root.join("evals/datasets/routing.jsonl")).map_err(|e| e.to_string())?;
    let router = load_router(&root.join("config/routing.yaml")).map_err(|e| e.to_string())?;
    let mut reports: Vec<StrategyReport> = Vec::new();
    for (name, split) in [("dev", Split::Dev), ("held_out", Split::HeldOut)] {
        let cases = dataset.split(split);
        reports.push(run_fixed_baseline(&cases, &router, name));
        reports.push(run_rules(&cases, &router, name));
    }
    let mut out = render(&reports);
    if mode == "jev" {
        let key = ApiKey::from_env()
            .map_err(|_| format!("jev strategy needs the owner's {API_KEY_ENV}; not set"))?;
        let cfg = router.config();
        let questions = QuestionSet::from_path(&root.join(&cfg.classifier.questions_path))
            .map_err(|e| e.to_string())?;
        let settings = JevSettings {
            endpoint: cfg.classifier.endpoint.clone(),
            model: cfg.classifier.model.clone(),
            deadline: Duration::from_millis(cfg.classifier.deadline_ms),
        };
        let jev = JevClassifier::new(settings, key, questions).map_err(|e| e.to_string())?;
        let mut live = Vec::new();
        for (name, split) in [("dev", Split::Dev), ("held_out", Split::HeldOut)] {
            // Only public cases are sent to the vendor.
            let cases: Vec<_> = dataset
                .split(split)
                .into_iter()
                .filter(|c| c.data_class == DataClass::Public)
                .collect();
            let mut r = run_classifier_assisted(
                &cases,
                &jev,
                &router,
                cfg.classifier.input_price_micros_per_mtok,
                name,
            )
            .await;
            r.notes.push("live Jev; public-data cases only; downstream acceptance NOT measured (needs real workflow runs)".into());
            live.push(r);
        }
        out.push('\n');
        out.push_str(&render(&live));
    } else {
        out.push_str(&format!(
            "\n[skipped] classifier-assisted (Jev) strategy: needs the owner's {API_KEY_ENV} (run `scripts/evaluate jev`) and,\n\
             to mean anything, real downstream workflow runs on owner-reviewed labels. No Jev accuracy is claimed.\n"
        ));
    }
    Ok(out)
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
