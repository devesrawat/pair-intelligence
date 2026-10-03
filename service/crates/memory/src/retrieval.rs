//! Retrieval (spec section 7): hard filters first (project, source visibility, validity,
//! deletion), then full-text and structured signals merged with reciprocal rank fusion.
//! Superseded decisions are returned beside their replacement, which is marked current.
use crate::{error::db_err, normalize::normalize_search_text, store::PgMemory};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use pair_core::{
    error::Result,
    ids::{MemoryId, SourceId},
    traits::Memory,
    types::{EvidenceItem, EvidenceRef, EvidenceStatus, MemoryCandidate, RetrievalQuery},
};
use serde::Serialize;
use sqlx::{PgConnection, Row};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

pub const DEFAULT_LIMIT: usize = 8;
pub const MAX_LIMIT: usize = 32;
/// Reciprocal rank fusion constant.
const RRF_K: f64 = 60.0;
/// Fraction of (non-stop-word) query terms a memory must match to be returned, counted over the
/// union of all its chunks (memory text and evidence spans). Keeps single-word overlaps from
/// producing unsupported answers.
const MIN_TERM_COVERAGE: f64 = 0.5;
const MAX_QUERY_TERMS: usize = 24;
const MAX_CANDIDATES: i64 = 200;
const MAX_CHAIN_DEPTH: usize = 16;
/// Superseded-decision history is shown below its replacement at reduced weight.
const HISTORY_SCORE_FACTOR: f64 = 0.5;

/// One retrieved memory with lifecycle context. `item` is the contract type and carries status,
/// supersession, conflict and inferred flags.
#[derive(Debug, Clone, Serialize)]
pub struct RetrievedMemory {
    pub item: EvidenceItem,
    pub kind: String,
    /// Valid as of the query time. False means this is superseded history.
    pub current: bool,
    pub valid_from: DateTime<Utc>,
    pub valid_to: Option<DateTime<Utc>>,
    pub supersedes: Option<MemoryId>,
}

#[derive(Debug, Clone)]
struct Row0 {
    id: Uuid,
    kind: String,
    content: String,
    status: String,
    inferred: bool,
    importance: i16,
    observed_at: DateTime<Utc>,
    valid_from: DateTime<Utc>,
    valid_to: Option<DateTime<Utc>>,
    supersedes: Option<Uuid>,
    rank: f64,
    matched: i64,
}

impl Row0 {
    fn valid_at(&self, t: DateTime<Utc>) -> bool {
        self.valid_from <= t && self.valid_to.is_none_or(|end| end > t)
    }
    fn superseded_before(&self, t: DateTime<Utc>) -> bool {
        self.status == "superseded" && self.valid_to.is_some_and(|end| end <= t)
    }
}

const MEMORY_COLUMNS: &str =
    "m.id, m.kind, m.content, m.status, m.confidence, m.importance, m.observed_at, m.valid_from, m.valid_to, m.supersedes_id";
/// Hard filters shared by every retrieval query. `$project` is bound by position by callers.
const VISIBLE_FILTER: &str = "m.invalidated_reason IS NULL AND (m.project IS NULL OR m.project = $PROJECT) \
    AND EXISTS (SELECT 1 FROM memory_evidence e JOIN sources s ON s.id = e.source_id \
                WHERE e.memory_id = m.id AND s.deletion_state = 'active' AND s.visibility = 'visible')";

fn row0(r: &sqlx::postgres::PgRow, rank: f64, matched: i64) -> Result<Row0> {
    Ok(Row0 {
        id: r.try_get("id").map_err(db_err)?,
        kind: r.try_get("kind").map_err(db_err)?,
        content: r.try_get("content").map_err(db_err)?,
        status: r.try_get("status").map_err(db_err)?,
        inferred: r.try_get::<String, _>("confidence").map_err(db_err)? == "inferred",
        importance: r.try_get("importance").map_err(db_err)?,
        observed_at: r.try_get("observed_at").map_err(db_err)?,
        valid_from: r.try_get("valid_from").map_err(db_err)?,
        valid_to: r.try_get("valid_to").map_err(db_err)?,
        supersedes: r.try_get("supersedes_id").map_err(db_err)?,
        rank,
        matched,
    })
}

pub(crate) fn query_terms(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    normalize_search_text(text)
        .split(' ')
        .filter(|t| !t.is_empty() && seen.insert(t.to_string()))
        .take(MAX_QUERY_TERMS)
        .map(str::to_string)
        .collect()
}

fn rrf(rank: usize) -> f64 {
    1.0 / (RRF_K + rank as f64 + 1.0)
}

impl PgMemory {
    pub async fn retrieve_detailed(&self, q: RetrievalQuery) -> Result<Vec<RetrievedMemory>> {
        let limit = if q.limit == 0 {
            DEFAULT_LIMIT
        } else {
            q.limit.min(MAX_LIMIT)
        };
        let terms = query_terms(&q.text);
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let mut conn = self.pool.acquire().await.map_err(db_err)?;
        let hits = fulltext_hits(&mut conn, &terms, q.project.as_deref()).await?;
        if hits.is_empty() {
            return Ok(Vec::new());
        }

        // Partition: valid now, or superseded history whose replacement we pull in.
        let mut active: HashMap<Uuid, Row0> = HashMap::new();
        let mut history: Vec<(Row0, Uuid)> = Vec::new();
        for hit in hits {
            if hit.valid_at(q.as_of) {
                active.insert(hit.id, hit);
            } else if hit.superseded_before(q.as_of) {
                if let Some(head) =
                    current_successor(&mut conn, hit.id, q.as_of, q.project.as_deref()).await?
                {
                    let mut inherited = head;
                    inherited.rank = hit.rank;
                    inherited.matched = hit.matched;
                    active.entry(inherited.id).or_insert(inherited.clone());
                    history.push((hit, inherited.id));
                }
            }
        }
        let ranked = fuse(active.values().cloned().collect());
        let emitted = assemble(
            &mut conn,
            ranked,
            &history,
            &active,
            q.as_of,
            q.project.as_deref(),
            limit,
        )
        .await?;
        decorate(&mut conn, emitted, q.as_of, q.project.as_deref()).await
    }
}

async fn fulltext_hits(
    conn: &mut PgConnection,
    terms: &[String],
    project: Option<&str>,
) -> Result<Vec<Row0>> {
    let required_terms: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM unnest($1::text[]) t WHERE plainto_tsquery('english', t)::text <> ''",
    )
    .bind(terms)
    .fetch_one(&mut *conn)
    .await
    .map_err(db_err)?;
    if required_terms == 0 {
        return Ok(Vec::new());
    }
    let min_matched = ((required_terms as f64) * MIN_TERM_COVERAGE)
        .ceil()
        .max(1.0) as i64;
    // The term-coverage gate runs in SQL, before the candidate limit: otherwise strongly ranked
    // single-term matches could fill the window and push out the memories that do cover the query.
    let sql = format!(
        "SELECT * FROM ( \
             SELECT {MEMORY_COLUMNS}, max(ts_rank_cd(c.search_vector, to_tsquery('english', $2)))::float8 AS rank, \
                    (SELECT count(*) FROM unnest($1::text[]) t WHERE EXISTS ( \
                        SELECT 1 FROM memory_chunks c2 WHERE c2.memory_id = m.id \
                          AND c2.search_vector @@ plainto_tsquery('english', t))) AS matched \
             FROM memory_chunks c JOIN memories m ON m.id = c.memory_id \
             WHERE c.search_vector @@ to_tsquery('english', $2) AND {filter} \
             GROUP BY m.id) covered \
         WHERE covered.matched >= $4 ORDER BY covered.rank DESC, covered.id LIMIT {MAX_CANDIDATES}",
        filter = VISIBLE_FILTER.replace("$PROJECT", "$3"),
    );
    let rows = sqlx::query(&sql)
        .bind(terms)
        .bind(terms.join(" | "))
        .bind(project)
        .bind(min_matched)
        .fetch_all(&mut *conn)
        .await
        .map_err(db_err)?;
    rows.iter()
        .map(|r| {
            let matched: i64 = r.try_get("matched").map_err(db_err)?;
            row0(r, r.try_get("rank").map_err(db_err)?, matched)
        })
        .collect()
}

/// Follow `supersedes_id` forward until a memory valid at `as_of` that is still visible.
async fn current_successor(
    conn: &mut PgConnection,
    from: Uuid,
    as_of: DateTime<Utc>,
    project: Option<&str>,
) -> Result<Option<Row0>> {
    let mut cursor = from;
    for _ in 0..MAX_CHAIN_DEPTH {
        let next: Option<Uuid> =
            sqlx::query_scalar("SELECT id FROM memories WHERE supersedes_id = $1")
                .bind(cursor)
                .fetch_optional(&mut *conn)
                .await
                .map_err(db_err)?;
        let Some(next) = next else { return Ok(None) };
        let found = load_visible(conn, &[next], project).await?;
        match found.into_iter().next() {
            Some(row) if row.valid_at(as_of) => return Ok(Some(row)),
            Some(row) if row.superseded_before(as_of) => cursor = row.id,
            _ => return Ok(None),
        }
    }
    Ok(None)
}

async fn load_visible(
    conn: &mut PgConnection,
    ids: &[Uuid],
    project: Option<&str>,
) -> Result<Vec<Row0>> {
    let sql = format!(
        "SELECT {MEMORY_COLUMNS} FROM memories m WHERE m.id = ANY($1) AND {filter} ORDER BY m.id",
        filter = VISIBLE_FILTER.replace("$PROJECT", "$2"),
    );
    let rows = sqlx::query(&sql)
        .bind(ids)
        .bind(project)
        .fetch_all(&mut *conn)
        .await
        .map_err(db_err)?;
    rows.iter().map(|r| row0(r, 0.0, 0)).collect()
}

/// Reciprocal rank fusion of the full-text order and a structured order (importance, recency).
fn fuse(mut rows: Vec<Row0>) -> Vec<(Row0, f64)> {
    rows.sort_by(|a, b| {
        b.rank
            .total_cmp(&a.rank)
            .then(b.matched.cmp(&a.matched))
            .then(a.id.cmp(&b.id))
    });
    let text_rank: HashMap<Uuid, usize> = rows.iter().enumerate().map(|(i, r)| (r.id, i)).collect();
    let mut structured = rows.clone();
    structured.sort_by(|a, b| {
        b.importance
            .cmp(&a.importance)
            .then(b.observed_at.cmp(&a.observed_at))
            .then(a.id.cmp(&b.id))
    });
    let struct_rank: HashMap<Uuid, usize> = structured
        .iter()
        .enumerate()
        .map(|(i, r)| (r.id, i))
        .collect();
    let mut scored: Vec<(Row0, f64)> = rows
        .into_iter()
        .map(|r| {
            let score = rrf(text_rank[&r.id]) + rrf(struct_rank[&r.id]);
            (r, score)
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.id.cmp(&b.0.id)));
    scored
}

struct Emitted {
    row: Row0,
    score: f64,
    current: bool,
}

/// Order output: each current memory, then its matched decision history, then conflict partners.
async fn assemble(
    conn: &mut PgConnection,
    ranked: Vec<(Row0, f64)>,
    history: &[(Row0, Uuid)],
    active: &HashMap<Uuid, Row0>,
    as_of: DateTime<Utc>,
    project: Option<&str>,
    limit: usize,
) -> Result<Vec<Emitted>> {
    let mut out: Vec<Emitted> = Vec::new();
    let mut seen: HashSet<Uuid> = HashSet::new();
    let conflicts =
        conflict_partners(conn, &ranked.iter().map(|(r, _)| r.id).collect::<Vec<_>>()).await?;
    let partner_ids: Vec<Uuid> = conflicts
        .values()
        .flatten()
        .copied()
        .filter(|p| !active.contains_key(p))
        .collect();
    let partner_rows: HashMap<Uuid, Row0> = load_visible(conn, &partner_ids, project)
        .await?
        .into_iter()
        .map(|r| (r.id, r))
        .collect();

    'outer: for (row, score) in ranked {
        let mut group = vec![Emitted {
            row: row.clone(),
            score,
            current: true,
        }];
        let mut past: Vec<&(Row0, Uuid)> = history
            .iter()
            .filter(|(h, head)| *head == row.id && h.kind == "decision")
            .collect();
        past.sort_by(|a, b| b.0.valid_to.cmp(&a.0.valid_to));
        group.extend(past.into_iter().map(|(h, _)| Emitted {
            row: h.clone(),
            score: score * HISTORY_SCORE_FACTOR,
            current: false,
        }));
        for partner in conflicts.get(&row.id).into_iter().flatten() {
            let found = active.get(partner).or_else(|| partner_rows.get(partner));
            if let Some(p) = found.filter(|p| p.valid_at(as_of)) {
                group.push(Emitted {
                    row: p.clone(),
                    score: score * HISTORY_SCORE_FACTOR,
                    current: true,
                });
            }
        }
        for item in group {
            if seen.insert(item.row.id) {
                out.push(item);
                if out.len() >= limit {
                    break 'outer;
                }
            }
        }
    }
    Ok(out)
}

async fn conflict_partners(
    conn: &mut PgConnection,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, Vec<Uuid>>> {
    let rows = sqlx::query("SELECT memory_a, memory_b FROM memory_conflicts WHERE memory_a = ANY($1) OR memory_b = ANY($1)")
        .bind(ids)
        .fetch_all(&mut *conn)
        .await
        .map_err(db_err)?;
    let mut map: HashMap<Uuid, Vec<Uuid>> = HashMap::new();
    for r in &rows {
        let a: Uuid = r.try_get("memory_a").map_err(db_err)?;
        let b: Uuid = r.try_get("memory_b").map_err(db_err)?;
        map.entry(a).or_default().push(b);
        map.entry(b).or_default().push(a);
    }
    Ok(map)
}

/// Attach visible evidence, supersession links and conflict flags.
async fn decorate(
    conn: &mut PgConnection,
    emitted: Vec<Emitted>,
    as_of: DateTime<Utc>,
    project: Option<&str>,
) -> Result<Vec<RetrievedMemory>> {
    let ids: Vec<Uuid> = emitted.iter().map(|e| e.row.id).collect();
    let ev_rows = sqlx::query(
        "SELECT e.memory_id, e.source_id, e.span FROM memory_evidence e JOIN sources s ON s.id = e.source_id \
         WHERE e.memory_id = ANY($1) AND s.deletion_state = 'active' AND s.visibility = 'visible' ORDER BY e.id",
    )
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await
    .map_err(db_err)?;
    let mut evidence: HashMap<Uuid, Vec<EvidenceRef>> = HashMap::new();
    for r in &ev_rows {
        let memory: Uuid = r.try_get("memory_id").map_err(db_err)?;
        evidence.entry(memory).or_default().push(EvidenceRef {
            source: SourceId(r.try_get("source_id").map_err(db_err)?),
            span: r.try_get("span").map_err(db_err)?,
        });
    }
    let succ_rows =
        sqlx::query("SELECT supersedes_id, id FROM memories WHERE supersedes_id = ANY($1)")
            .bind(&ids)
            .fetch_all(&mut *conn)
            .await
            .map_err(db_err)?;
    let mut successors: HashMap<Uuid, Uuid> = HashMap::new();
    for r in &succ_rows {
        successors.insert(
            r.try_get("supersedes_id").map_err(db_err)?,
            r.try_get("id").map_err(db_err)?,
        );
    }
    let conflicts = conflict_partners(conn, &ids).await?;
    // Conflict labels come from every valid, visible partner, not only the ones that survived the
    // output limit: a partner cut by the limit is still a competing current memory.
    let partner_ids: Vec<Uuid> = conflicts
        .values()
        .flatten()
        .copied()
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let valid_partners: HashSet<Uuid> = load_visible(conn, &partner_ids, project)
        .await?
        .into_iter()
        .filter(|p| p.valid_at(as_of))
        .map(|p| p.id)
        .collect();

    Ok(emitted
        .into_iter()
        .map(|e| {
            let id = e.row.id;
            let mut partners: Vec<MemoryId> = conflicts
                .get(&id)
                .into_iter()
                .flatten()
                .filter(|p| valid_partners.contains(p))
                .map(|p| MemoryId(*p))
                .collect();
            partners.sort_by_key(|p| p.0);
            let conflicts_with = if e.current { partners } else { Vec::new() };
            let superseded_by = successors
                .get(&id)
                .copied()
                .map(MemoryId)
                .filter(|_| !e.current);
            let status = if !e.current {
                EvidenceStatus::Superseded
            } else if conflicts_with.is_empty() {
                EvidenceStatus::Current
            } else {
                EvidenceStatus::Conflicting
            };
            RetrievedMemory {
                item: EvidenceItem {
                    memory: MemoryId(id),
                    content: e.row.content.clone(),
                    evidence: evidence.remove(&id).unwrap_or_default(),
                    score: e.score,
                    status,
                    superseded_by,
                    conflicts_with,
                    inferred: e.row.inferred,
                },
                kind: e.row.kind.clone(),
                current: e.current,
                valid_from: e.row.valid_from,
                valid_to: e.row.valid_to,
                supersedes: e.row.supersedes.map(MemoryId),
            }
        })
        .collect())
}

#[async_trait]
impl Memory for PgMemory {
    async fn propose(&self, c: MemoryCandidate) -> Result<pair_core::ids::CandidateId> {
        Ok(self
            .propose_with_outcome(crate::model::CandidateDraft::new(c))
            .await?
            .id)
    }

    async fn accept(&self, id: pair_core::ids::CandidateId, actor: &str) -> Result<MemoryId> {
        self.accept_with(id, actor, crate::store::AcceptMode::default())
            .await
    }

    async fn retrieve(&self, q: RetrievalQuery) -> Result<Vec<EvidenceItem>> {
        Ok(self
            .retrieve_detailed(q)
            .await?
            .into_iter()
            .map(|r| r.item)
            .collect())
    }
}
