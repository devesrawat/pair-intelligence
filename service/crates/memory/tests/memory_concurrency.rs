//! Races between proposals/acceptances and source deletion.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{candidate, source, verified, TestDb};
use pair_core::types::TrustClass;

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
