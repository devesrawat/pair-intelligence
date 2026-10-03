mod common;

use common::*;
use ops::export::export_all;
use sha2::{Digest, Sha256};
use std::os::unix::fs::{symlink, PermissionsExt};
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

fn mode(path: &Path) -> u32 {
    std::fs::symlink_metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

#[tokio::test]
async fn export_files_are_0600_and_dir_0700() {
    let db = TestDb::create().await;
    seed(&db).await;
    let out = temp_dir("modes").join("nested").join("export");

    let manifest = export_all(&db.pool, &out, true).await.expect("export");

    assert_eq!(mode(&out), 0o700, "export directory");
    assert_eq!(
        mode(out.parent().expect("parent")),
        0o700,
        "created parents too"
    );
    for f in manifest
        .files
        .iter()
        .map(|f| f.name.as_str())
        .chain(["manifest.json"])
    {
        assert_eq!(mode(&out.join(f)), 0o600, "{f}");
    }
    db.drop_db().await;
}

#[tokio::test]
async fn export_refuses_symlink_target() {
    let db = TestDb::create().await;
    seed(&db).await;
    let victim_dir = temp_dir("victim");
    let victim = victim_dir.join("bashrc");
    std::fs::write(&victim, "export PATH=safe").expect("victim");

    // --out itself is a symlink to a real (empty) directory.
    let empty_target = temp_dir("empty_target");
    let link = temp_dir("link_parent").join("out");
    symlink(&empty_target, &link).expect("symlink out");
    export_all(&db.pool, &link, false)
        .await
        .expect_err("a symlinked --out must be refused");
    assert_eq!(std::fs::read_dir(&empty_target).expect("ls").count(), 0);

    // A file inside --out is a symlink to somewhere else (manifest.json -> victim).
    let out = temp_dir("planted");
    symlink(&victim, out.join("manifest.json")).expect("symlink manifest");
    export_all(&db.pool, &out, false)
        .await
        .expect_err("a planted symlink must be refused, never followed");

    assert_eq!(
        read(&victim_dir, "bashrc"),
        "export PATH=safe",
        "victim untouched"
    );
    assert_eq!(std::fs::read_dir(&victim_dir).expect("ls").count(), 1);
    db.drop_db().await;
}

#[tokio::test]
async fn export_refuses_non_empty_out_dir() {
    let db = TestDb::create().await;
    seed(&db).await;
    let out = temp_dir("busy");
    std::fs::write(out.join("notes.txt"), "mine").expect("unrelated file");

    export_all(&db.pool, &out, false)
        .await
        .expect_err("a directory with foreign content is refused");

    assert_eq!(read(&out, "notes.txt"), "mine");
    assert!(!out.join("manifest.json").exists(), "nothing was written");
    db.drop_db().await;
}

#[tokio::test]
async fn stale_config_jsonl_not_left_behind() {
    let db = TestDb::create().await;
    seed(&db).await;
    let out = temp_dir("stale");

    export_all(&db.pool, &out, true).await.expect("with config");
    assert!(out.join("config.jsonl").exists());
    // Re-exporting into the previous export's own directory replaces it entirely.
    let manifest = export_all(&db.pool, &out, false)
        .await
        .expect("without config");

    assert!(
        !out.join("config.jsonl").exists(),
        "config.jsonl from the earlier run must not survive an export without config"
    );
    let mut on_disk: Vec<String> = std::fs::read_dir(&out)
        .expect("ls")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    let mut expected: Vec<String> = manifest.files.iter().map(|f| f.name.clone()).collect();
    expected.push("manifest.json".to_owned());
    on_disk.sort();
    expected.sort();
    assert_eq!(on_disk, expected, "only manifest files remain");
    db.drop_db().await;
}

#[tokio::test]
async fn export_markdown_groups_evidence_under_the_right_memory() {
    let db = TestDb::create().await;
    let src = insert_source(&db.pool, 10, false, "file:///a").await;
    let first = insert_memory(&db.pool, "fact", "first fact", src, "span of first", 10).await;
    let second = insert_memory(&db.pool, "fact", "second fact", src, "span of second", 10).await;
    sqlx::query("INSERT INTO memory_evidence (memory_id, source_id, span, extraction_version) VALUES ($1, $2, 'extra span of first', 'v1')")
        .bind(first)
        .bind(src)
        .execute(&db.pool)
        .await
        .expect("second evidence row");
    let out = temp_dir("group");

    export_all(&db.pool, &out, false).await.expect("export");

    let index = read(&out, "index.md");
    let section = |id: uuid::Uuid| {
        let start = index
            .find(&format!("### memory {id}"))
            .expect("memory section");
        let rest = &index[start + 1..];
        let end = rest.find("### memory ").unwrap_or(rest.len());
        rest[..end].to_owned()
    };
    let (a, b) = (section(first), section(second));
    assert!(
        a.contains("span of first") && a.contains("extra span of first"),
        "{a}"
    );
    assert!(!a.contains("span of second"), "{a}");
    assert!(
        b.contains("span of second") && !b.contains("span of first"),
        "{b}"
    );
    db.drop_db().await;
}
