//! Races between proposals/acceptances and source deletion.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{candidate, source, verified, TestDb};
use pair_core::{error::ErrorCode, traits::Memory, types::TrustClass};
use std::time::Duration;

const BLOCK_PROBE: Duration = Duration::from_millis(400);

const RACE_ROUNDS: usize = 8;

#[tokio::test]
async fn concurrent_contradictory_accepts_do_not_both_become_current() {
    let db = TestDb::new().await;
    let mem = db.memory();

    for round in 0..RACE_ROUNDS {
        let topic = format!("Theme {round}");
        let dark = source(&mem, &format!("dark-{round}"), TrustClass::Owner).await;
        let light = source(&mem, &format!("light-{round}"), TrustClass::Owner).await;
        let (m1, m2) = (mem.clone(), mem.clone());
        let (d, l) = (
            verified(candidate(
                "preference",
                &format!("{topic}: dark"),
                None,
                dark.id,
                "dark please",
            )),
            verified(candidate(
                "preference",
                &format!("{topic}: light"),
                None,
                light.id,
                "light please",
            )),
        );
        let (a, b) = tokio::join!(
            tokio::spawn(async move { m1.propose_with_outcome(d).await.unwrap() }),
            tokio::spawn(async move { m2.propose_with_outcome(l).await.unwrap() }),
        );
        let (a, b) = (a.unwrap(), b.unwrap());

        let current: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM memories WHERE status = 'accepted' AND topic_key = $1",
        )
        .bind(topic.to_lowercase())
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(
            current <= 1,
            "round {round}: both contradictory memories are current"
        );
        // The loser is held for review, linked to what it contradicts.
        let losers: Vec<_> = [&a, &b]
            .into_iter()
            .filter(|p| p.auto_accepted.is_none())
            .collect();
        assert!(
            !losers.is_empty(),
            "round {round}: nothing was held for review"
        );
        assert!(losers
            .iter()
            .all(|p| p.needs_review && p.review_reasons.iter().any(|r| r == "contradiction")));
    }
}

#[tokio::test]
async fn delete_source_takes_the_register_source_lock() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let src = source(&mem, "vault", TrustClass::Owner).await;

    // Hold the identity lock register_source takes; delete_source must queue behind it.
    let mut holder = db.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind("note:vault")
        .execute(&mut *holder)
        .await
        .unwrap();
    let deleter = {
        let mem = mem.clone();
        tokio::spawn(async move { mem.delete_source(src.id, "owner").await })
    };
    tokio::time::sleep(BLOCK_PROBE).await;
    assert!(
        !deleter.is_finished(),
        "delete_source ignored the identity lock"
    );
    holder.commit().await.unwrap();
    deleter.await.unwrap().unwrap();
}

#[tokio::test]
async fn accept_waits_for_in_flight_source_deletion() {
    let db = TestDb::new().await;
    let mem = db.memory();
    let src = source(&mem, "vault", TrustClass::Owner).await;
    let cid = mem
        .propose(candidate(
            "fact",
            "The vault code is 1234",
            None,
            src.id,
            "code 1234",
        ))
        .await
        .unwrap();

    // A deletion that has marked the source but not yet committed.
    let mut deleting = db.pool.begin().await.unwrap();
    sqlx::query("UPDATE sources SET deletion_state = 'deleted', deleted_at = now() WHERE id = $1")
        .bind(src.id.0)
        .execute(&mut *deleting)
        .await
        .unwrap();
    let accepting = {
        let mem = mem.clone();
        tokio::spawn(async move { mem.accept(cid, "owner").await })
    };
    tokio::time::sleep(BLOCK_PROBE).await;
    assert!(
        !accepting.is_finished(),
        "accept read the source without locking it"
    );
    deleting.commit().await.unwrap();
    let err = accepting.await.unwrap().unwrap_err();
    assert_eq!(err.code, ErrorCode::SourceDeleted);
    let memories: i64 = sqlx::query_scalar("SELECT count(*) FROM memories")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(memories, 0);
}

#[tokio::test]
async fn concurrent_similar_preferences_with_different_topics_do_not_both_become_current() {
    let db = TestDb::new().await;
    let mem = db.memory();

    for round in 0..RACE_ROUNDS {
        let project = format!("p{round}");
        let dark = source(&mem, &format!("dark-{round}"), TrustClass::Owner).await;
        let light = source(&mem, &format!("light-{round}"), TrustClass::Owner).await;
        // Extractor-chosen topics differ, so the per-topic lock alone cannot order these.
        let mut d = verified(candidate(
            "preference",
            "I prefer dark mode",
            Some(&project),
            dark.id,
            "dark please",
        ));
        d.topic = Some("ui-a".into());
        let mut l = verified(candidate(
            "preference",
            "I prefer light mode",
            Some(&project),
            light.id,
            "light please",
        ));
        l.topic = Some("ui-b".into());
        let (m1, m2) = (mem.clone(), mem.clone());
        let (a, b) = tokio::join!(
            tokio::spawn(async move { m1.propose_with_outcome(d).await.unwrap() }),
            tokio::spawn(async move { m2.propose_with_outcome(l).await.unwrap() }),
        );
        let (a, b) = (a.unwrap(), b.unwrap());

        let current: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM memories WHERE status = 'accepted' AND project = $1",
        )
        .bind(&project)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(
            current <= 1,
            "round {round}: similar contradictory preferences are both current"
        );
        assert!(
            [&a, &b].into_iter().any(|p| p.auto_accepted.is_none()),
            "round {round}: nothing was held for review"
        );
    }
}
