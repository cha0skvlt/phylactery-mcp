//! The four circuits: one vector search shape, four tables.
//!
//! Cosine is computed by pgvector inside Postgres. Everything else — floor, importance,
//! trust, ordering — happens here, where it can be tested without a database.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::db::{Db, DbError};
use crate::embed::{EmbedError, Embedder, vector_literal};
use crate::math;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Circuit {
    Memories,
    Diary,
    Lessons,
    Mind,
}

impl Circuit {
    pub const ALL: [Circuit; 4] = [
        Circuit::Memories,
        Circuit::Diary,
        Circuit::Lessons,
        Circuit::Mind,
    ];

    pub fn parse(name: &str) -> Option<Circuit> {
        match name.trim().to_ascii_lowercase().as_str() {
            "memories" | "memory" => Some(Circuit::Memories),
            "diary" | "self_reflections" | "reflections" => Some(Circuit::Diary),
            "lessons" | "causal_lessons" => Some(Circuit::Lessons),
            "mind" | "mind_events" => Some(Circuit::Mind),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Circuit::Memories => "memories",
            Circuit::Diary => "diary",
            Circuit::Lessons => "lessons",
            Circuit::Mind => "mind",
        }
    }

    pub fn table(self) -> &'static str {
        match self {
            Circuit::Memories => "memories",
            Circuit::Diary => "self_reflections",
            Circuit::Lessons => "causal_lessons",
            Circuit::Mind => "mind_events",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Hit {
    pub source: &'static str,
    pub id: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub importance: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(skip_serializing_if = "is_zero")]
    pub similarity: f64,
    pub score: f64,
    pub created_at: DateTime<Utc>,
}

fn is_zero(value: &f64) -> bool {
    *value == 0.0
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Embed(#[from] EmbedError),
}

impl SearchError {
    pub fn code(&self) -> &'static str {
        match self {
            SearchError::Db(_) => "db_unavailable",
            SearchError::Embed(_) => "embed_unavailable",
        }
    }
}

const MEMORIES_SQL: &str = "\
SELECT id::text AS id, memory_type, content, importance::float8 AS importance, created_at,
       (1 - (embedding <=> $1::text::vector))::float8 AS similarity
FROM memories
WHERE agent_id = $2 AND embedding IS NOT NULL
ORDER BY embedding <=> $1::text::vector
LIMIT $3";

const DIARY_SQL: &str = "\
SELECT id::text AS id, content, importance::float8 AS importance, created_at,
       (1 - (embedding <=> $1::text::vector))::float8 AS similarity
FROM self_reflections
WHERE agent_id = $2 AND embedding IS NOT NULL
ORDER BY embedding <=> $1::text::vector
LIMIT $3";

const LESSONS_SQL: &str = "\
SELECT id::text AS id,
       change_text || ' — because: ' || COALESCE(because_text, '') AS content,
       importance::float8 AS importance, created_at,
       (1 - (embedding <=> $1::text::vector))::float8 AS similarity
FROM causal_lessons
WHERE agent_id = $2 AND embedding IS NOT NULL
ORDER BY embedding <=> $1::text::vector
LIMIT $3";

const MIND_SQL: &str = "\
SELECT id::text AS id, content, kind, channel, created_at,
       (1 - (embedding <=> $1::text::vector))::float8 AS similarity
FROM mind_events
WHERE agent_id = $2 AND embedding IS NOT NULL
ORDER BY embedding <=> $1::text::vector
LIMIT $3";

/// ANN pass over one circuit. Cosine only; ranking is applied by the caller.
async fn ann(
    client: &deadpool_postgres::Object,
    circuit: Circuit,
    agent_id: &str,
    literal: &str,
    candidates: i64,
    now: DateTime<Utc>,
) -> Result<Vec<Hit>, DbError> {
    let rows = match circuit {
        Circuit::Memories => {
            client
                .query(MEMORIES_SQL, &[&literal, &agent_id, &candidates])
                .await?
        }
        Circuit::Diary => {
            client
                .query(DIARY_SQL, &[&literal, &agent_id, &candidates])
                .await?
        }
        Circuit::Lessons => {
            client
                .query(LESSONS_SQL, &[&literal, &agent_id, &candidates])
                .await?
        }
        Circuit::Mind => {
            client
                .query(MIND_SQL, &[&literal, &agent_id, &candidates])
                .await?
        }
    };

    let mut hits = Vec::with_capacity(rows.len());
    for row in rows {
        let id: String = row.get("id");
        let content: String = row.get("content");
        let created_at: DateTime<Utc> = row.get("created_at");
        let similarity: f64 = row.get("similarity");
        let memory_type: Option<String> = match circuit {
            Circuit::Memories => row.get("memory_type"),
            Circuit::Diary => Some("diary".to_string()),
            Circuit::Lessons => Some("lesson".to_string()),
            Circuit::Mind => None,
        };
        let importance: Option<f64> = match circuit {
            Circuit::Memories | Circuit::Diary | Circuit::Lessons => row.get("importance"),
            Circuit::Mind => None,
        };
        let kind: Option<String> = match circuit {
            Circuit::Mind => row.get("kind"),
            _ => None,
        };
        let channel: Option<String> = match circuit {
            Circuit::Mind => row.get("channel"),
            _ => None,
        };
        let trust_type = memory_type
            .clone()
            .unwrap_or_else(|| circuit.label().to_string());
        let score =
            math::retrieval_score(similarity, importance, &trust_type, Some(created_at), now);
        hits.push(Hit {
            source: circuit.label(),
            id,
            content,
            importance,
            memory_type,
            kind,
            channel,
            similarity,
            score,
            created_at,
        });
    }
    Ok(hits)
}

/// Semantic search across the selected circuits.
pub async fn search(
    db: &Db,
    embedder: &Embedder,
    agent_id: &str,
    query: &str,
    circuits: &[Circuit],
    limit: usize,
    min_similarity: f64,
) -> Result<Vec<Hit>, SearchError> {
    let vector = embedder.embed_one(query).await?;
    let literal = vector_literal(&vector);
    let floor = min_similarity.clamp(0.0, 1.0);
    let now = Utc::now();
    let client = db.client().await?;

    let mut hits = Vec::new();
    for circuit in circuits {
        hits.extend(
            ann(
                &client,
                *circuit,
                agent_id,
                &literal,
                math::ANN_CANDIDATES,
                now,
            )
            .await?,
        );
    }
    hits.retain(|hit| hit.similarity >= floor);
    hits.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.truncate(limit.max(1));
    Ok(hits)
}

/// Fresh chronology from `mind_events`, newest first, optionally filtered by kind and time.
pub async fn timeline_filtered(
    db: &Db,
    agent_id: &str,
    limit: i64,
    kind: Option<&str>,
    since: Option<DateTime<Utc>>,
) -> Result<Vec<Hit>, DbError> {
    let client = db.client().await?;
    let rows = client
        .query(
            "SELECT id::text AS id, content, kind, channel, created_at
             FROM mind_events
             WHERE agent_id = $1
               AND ($2::text IS NULL OR kind = $2::text)
               AND ($3::timestamptz IS NULL OR created_at > $3::timestamptz)
               AND lower(btrim(content)) NOT IN (
                   'hi','hey','hello','ok','okay','test','да','нет','ага','угу','привет','привет!'
               )
             ORDER BY created_at DESC
             LIMIT $4",
            &[&agent_id, &kind, &since, &limit],
        )
        .await?;
    let now = Utc::now();
    let mut hits = Vec::with_capacity(rows.len());
    for row in rows {
        let created_at: DateTime<Utc> = row.get("created_at");
        hits.push(Hit {
            source: "mind",
            id: row.get("id"),
            content: row.get("content"),
            importance: None,
            memory_type: None,
            kind: row.get("kind"),
            channel: row.get("channel"),
            similarity: 0.0,
            score: math::distrust("mind", Some(created_at), now),
            created_at,
        });
    }
    Ok(hits)
}

/// Unfiltered timeline, kept for callers that only need "what happened lately".
pub async fn timeline(db: &Db, agent_id: &str, limit: i64) -> Result<Vec<Hit>, DbError> {
    timeline_filtered(db, agent_id, limit, None, None).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn circuit_names_round_trip() {
        for circuit in Circuit::ALL {
            assert_eq!(Circuit::parse(circuit.label()), Some(circuit));
            assert_eq!(Circuit::parse(circuit.table()), Some(circuit));
        }
    }

    #[test]
    fn unknown_circuit_is_rejected() {
        assert_eq!(Circuit::parse("agent_identity"), None);
        assert_eq!(Circuit::parse("pg_catalog"), None);
    }
}
