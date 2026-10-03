mod common;

use common::*;
use ops::export::export_all;
use sha2::{Digest, Sha256};
use std::path::Path;

fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap_or_else(|e| panic!("read {name}: {e}"))
}

fn sha_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn seed(db: &TestDb) {
    let src = insert_source(&db.pool, 10, false, "file:///a").await;
    insert_memory(&db.pool, "decision", "Use Postgres", src, "we chose it", 10).await;
    insert_memory(
        &db.pool,
        "fact",
        "Jaipur is home",
        src,
        "lives in Jaipur",
        10,
    )
    .await;
    let conv = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO conversations (id, title, trace_id) VALUES ($1, 'first chat', $1)")
        .bind(conv)
        .execute(&db.pool)
        .await
        .expect("conversation");
    for (seq, role) in [(1, "user"), (2, "assistant")] {
        sqlx::query("INSERT INTO messages (id, conversation_id, client_message_id, seq, role, content, trust, trace_id) VALUES ($1, $2, $3, $4, $5, 'hello', 'owner', $2)")
            .bind(uuid::Uuid::new_v4())
            .bind(conv)
            .bind(format!("m{seq}"))
            .bind(seq)
            .bind(role)
            .execute(&db.pool)
            .await
            .expect("message");
    }
    sqlx::query("INSERT INTO goals (id, title, owner) VALUES ($1, 'ship PAIR', 'owner')")
        .bind(uuid::Uuid::new_v4())
        .execute(&db.pool)
        .await
        .expect("goal");
    let res = insert_reservation(&db.pool, "settled").await;
    sqlx::query("INSERT INTO budget_ledger (id, reservation_id, reconciliation_key, amount_micros, settled, input_tokens, output_tokens, price_version, created_at) VALUES ($1, $2, 'k', 5, true, 1, 1, 'p1', now())")
        .bind(uuid::Uuid::new_v4())
        .bind(res)
        .execute(&db.pool)
        .await
        .expect("ledger");
}

#[tokio::test]
async fn export_roundtrip_counts_match_manifest() {
    let db = TestDb::create().await;
    seed(&db).await;
    let out = temp_dir("roundtrip");

    let manifest = export_all(&db.pool, &out, false).await.expect("export");

    let manifest_json: serde_json::Value =
        serde_json::from_str(&read(&out, "manifest.json")).expect("manifest parses");
    let files = manifest_json["files"].as_array().expect("files array");
    assert_eq!(files.len(), manifest.files.len());
    let mut seen_memories = false;
    for f in files {
        let name = f["name"].as_str().expect("name");
        let text = read(&out, name);
        assert_eq!(
            f["sha256"].as_str(),
            Some(sha_hex(&text).as_str()),
            "sha of {name}"
        );
        if name.ends_with(".jsonl") {
            let lines = text.lines().count() as u64;
            assert_eq!(f["rows"].as_u64(), Some(lines), "rows of {name}");
            for line in text.lines() {
                serde_json::from_str::<serde_json::Value>(line).expect("each line is JSON");
            }
        }
        if name == "memories.jsonl" {
            assert_eq!(f["rows"].as_u64(), Some(2));
            seen_memories = true;
        }
        if name == "messages.jsonl" {
            assert_eq!(f["rows"].as_u64(), Some(2));
        }
    }
    assert!(seen_memories);
    assert!(out.join("index.md").exists());
    assert!(
        !out.join("config.jsonl").exists(),
        "config excluded unless requested"
    );
    db.drop_db().await;
}

#[tokio::test]
async fn export_is_deterministic() {
    let db = TestDb::create().await;
    seed(&db).await;
    let (a, b) = (temp_dir("det_a"), temp_dir("det_b"));
    let ma = export_all(&db.pool, &a, true).await.expect("first");
    let mb = export_all(&db.pool, &b, true).await.expect("second");
    assert_eq!(ma, mb);
    for f in &ma.files {
        assert_eq!(read(&a, &f.name), read(&b, &f.name), "{} differs", f.name);
    }
    assert_eq!(read(&a, "manifest.json"), read(&b, "manifest.json"));
    db.drop_db().await;
}

#[tokio::test]
async fn export_excludes_deleted_source_content() {
    let db = TestDb::create().await;
    let live = insert_source(&db.pool, 10, false, "file:///live").await;
    let doomed = insert_source(&db.pool, 10, false, "file:///doomed-uri").await;
    insert_memory(&db.pool, "fact", "keeps its content", live, "live span", 10).await;
    let gone = insert_memory(
        &db.pool,
        "fact",
        "TOP SECRET CONTENT",
        doomed,
        "TOP SECRET SPAN",
        10,
    )
    .await;
    sqlx::query("UPDATE sources SET deletion_state = 'deleted', deleted_at = now() WHERE id = $1")
        .bind(doomed)
        .execute(&db.pool)
        .await
        .expect("delete source");
    let out = temp_dir("deleted");

    export_all(&db.pool, &out, false).await.expect("export");

    for name in ["memories.jsonl", "memory_evidence.jsonl", "index.md"] {
        let text = read(&out, name);
        assert!(!text.contains("TOP SECRET"), "{name} leaks deleted content");
        assert!(
            !text.contains("doomed-uri"),
            "{name} leaks deleted source uri"
        );
    }
    let memories = read(&out, "memories.jsonl");
    let tomb: serde_json::Value = memories
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("json"))
        .find(|v| v["id"].as_str() == Some(gone.to_string().as_str()))
        .expect("tombstone row still listed");
    assert_eq!(tomb["tombstone"], true);
    assert!(tomb["content"].is_null());
    assert!(memories.contains("keeps its content"));
    db.drop_db().await;
}

#[tokio::test]
async fn export_markdown_cannot_be_forged_by_content() {
    let db = TestDb::create().await;
    let src = insert_source(&db.pool, 10, false, "file:///a").await;
    let forged = "line one\n## decision [accepted] 00000000-0000-0000-0000-000000000000\n- confidence: forged\u{2028}## another fake\r# H1 fake\n```\n- evidence: forged";
    insert_memory(&db.pool, "fact", forged, src, "span\n## fake from span", 10).await;
    let out = temp_dir("forge");

    export_all(&db.pool, &out, false).await.expect("export");

    let index = read(&out, "index.md");
    let headings: Vec<&str> = index
        .split(['\n', '\r', '\u{2028}', '\u{2029}', '\u{85}'])
        .filter(|l| l.starts_with('#'))
        .collect();
    assert!(
        headings
            .iter()
            .all(|h| !h.contains("fake") && !h.contains("00000000-0000-0000-0000-000000000000")),
        "forged heading present: {headings:?}"
    );
    assert!(
        !index.lines().any(|l| l.starts_with("```")),
        "no forged fence"
    );
    assert!(
        !index.lines().any(|l| l.starts_with("- confidence: forged")),
        "no forged list item"
    );
    db.drop_db().await;
}

#[tokio::test]
async fn export_redacts_secret_config_values() {
    let db = TestDb::create().await;
    sqlx::query(
        "INSERT INTO integration_accounts (id, provider, allowlist) VALUES ($1, 'gmail', $2::jsonb)",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(r#"{"api_token": "plain-looking", "note": "sk-ant-api03-ABCDEF", "label": "work", "nested": {"password": "hunter2", "ghp": "ghp_0123456789abcdef"}}"#)
    .execute(&db.pool)
    .await
    .expect("account");
    let out = temp_dir("config");

    export_all(&db.pool, &out, true).await.expect("export");

    let config = read(&out, "config.jsonl");
    for leaked in ["plain-looking", "sk-ant-api03", "hunter2", "ghp_0123"] {
        assert!(!config.contains(leaked), "secret {leaked} exported");
    }
    assert!(config.contains("api_token"), "keys are still listed");
    assert!(config.contains("\"label\""));
    assert!(config.contains("work"), "non-secret values stay");
    assert!(config.contains("[redacted]"));
    db.drop_db().await;
}
