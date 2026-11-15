//! The write path: guard, embed, dedup, insert — one transaction, one advisory lock.
//!
//! Invariants enforced here:
//! * refused text never reaches the database;
//! * a row without a vector is never inserted;
//! * concurrent writers of the same text produce exactly one row;
//! * a numeric disagreement is a correction, not a duplicate.

use chrono::Local;
use serde::Serialize;
use serde_json::json;

use crate::db::{Db, DbError};
use crate::dedup;
use crate::embed::{EmbedError, Embedder, vector_literal};
use crate::guard;
use crate::math;

#[derive(Debug, Serialize)]
pub struct WriteOutcome {
    pub status: &'static str,
    pub id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("injection guard blocked the text (pattern {0})")]
    Guard(&'static str),
    #[error("nothing to write: {0}")]
    Empty(&'static str),
    #[error(transparent)]
    Embed(#[from] EmbedError),
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Sql(#[from] tokio_postgres::Error),
}

impl WriteError {
    pub fn code(&self) -> &'static str {
        match self {
            WriteError::Guard(_) => "injection_blocked",
            WriteError::Empty(_) => "invalid_text",
            WriteError::Embed(_) => "embed_unavailable",
            WriteError::Db(_) | WriteError::Sql(_) => "db_unavailable",
        }
    }
}

const NEIGHBOURS_SQL: &str = "\
SELECT id::text AS id, content,
       (1 - (embedding <=> $4::text::vector))::float8 AS similarity
FROM memories
WHERE agent_id = $1 AND memory_type = $2 AND embedding IS NOT NULL
ORDER BY embedding <=> $4::text::vector
LIMIT $3";

/// Write a fact into `memories`.
pub async fn save(
    db: &Db,
    embedder: &Embedder,
    agent_id: &str,
    text: &str,
    memory_type: &str,
    importance: f64,
    source: &str,
) -> Result<WriteOutcome, WriteError> {
    if let Some(pattern) = guard::check(text) {
        return Err(WriteError::Guard(pattern));
    }
    let body = guard::strip_date_prefix(text).trim().to_string();
    if body.is_empty() {
        return Err(WriteError::Empty("empty text"));
    }
    let memory_type = math::normalize_memory_type(memory_type);
    let importance = math::sanitize_importance(importance);
    let stored = guard::with_date_prefix(&body, Local::now());
    let vector = embedder.embed_one(&body).await?;
    let literal = vector_literal(&vector);

    let mut client = db.client().await?;
    let tx = client.transaction().await?;
    let lock_key = format!("mem_save:{agent_id}:{memory_type}");
    tx.execute("SELECT pg_advisory_xact_lock(hashtext($1))", &[&lock_key])
        .await?;

    let neighbours = tx
        .query(
            NEIGHBOURS_SQL,
            &[&agent_id, &memory_type, &math::DEDUP_NEIGHBORS, &literal],
        )
        .await?;

    for row in &neighbours {
        let similarity: f64 = row.get("similarity");
        let neighbour_id: String = row.get("id");
        let neighbour_text: String = row.get("content");
        let neighbour_body = guard::strip_date_prefix(&neighbour_text);
        let conflict = dedup::numeric_conflict(&body, neighbour_body);
        let lexical = dedup::overlap(&body, neighbour_body);
        if dedup::is_twin(similarity, lexical, conflict) {
            tx.commit().await?;
            tracing::info!(id = %neighbour_id, similarity, "mem.dedup");
            return Ok(WriteOutcome {
                status: "dedup",
                id: neighbour_id,
            });
        }
    }

    let row = tx
        .query_one(
            "INSERT INTO memories (agent_id, memory_type, content, embedding, importance, metadata)
             VALUES ($1, $2, $3, $4::text::vector, $5, jsonb_build_object('source', $6::text))
             RETURNING id::text AS id",
            &[
                &agent_id,
                &memory_type,
                &stored,
                &literal,
                &importance,
                &source,
            ],
        )
        .await?;
    let id: String = row.get("id");
    tx.commit().await?;
    tracing::info!(id = %id, memory_type, importance, "mem.save");
    Ok(WriteOutcome {
        status: "saved",
        id,
    })
}

/// Write a causal lesson: stopped doing X because Y.
pub async fn lesson(
    db: &Db,
    embedder: &Embedder,
    agent_id: &str,
    change: &str,
    because: &str,
    evidence_ids: &[String],
    importance: f64,
) -> Result<WriteOutcome, WriteError> {
    let combined = format!("{change}\n{because}");
    if let Some(pattern) = guard::check(&combined) {
        return Err(WriteError::Guard(pattern));
    }
    let change = change.trim();
    let because = because.trim();
    let importance = math::sanitize_importance(importance);
    if change.is_empty() || because.is_empty() {
        return Err(WriteError::Empty("lesson needs both change and because"));
    }
    let vector = embedder.embed_one(&combined).await?;
    let literal = vector_literal(&vector);

    let mut client = db.client().await?;
    let tx = client.transaction().await?;
    let lock_key = format!("lesson_save:{agent_id}");
    tx.execute("SELECT pg_advisory_xact_lock(hashtext($1))", &[&lock_key])
        .await?;

    let existing = tx
        .query_opt(
            "SELECT id::text AS id FROM causal_lessons
             WHERE agent_id = $1 AND change_text = $2 AND because_text = $3
             LIMIT 1",
            &[&agent_id, &change, &because],
        )
        .await?;

    if let Some(row) = existing {
        let id: String = row.get("id");
        tx.execute(
            "UPDATE causal_lessons SET importance = $2 WHERE id = $1::text::uuid",
            &[&id, &(importance as f32)],
        )
        .await?;
        tx.commit().await?;
        tracing::info!(id = %id, importance, "lesson.dedup");
        return Ok(WriteOutcome {
            status: "dedup",
            id,
        });
    }

    let row = tx
        .query_one(
            "INSERT INTO causal_lessons
                 (agent_id, change_text, because_text, evidence_ids, embedding, importance)
             VALUES ($1, $2, $3, $4, $5::text::vector, $6)
             RETURNING id::text AS id",
            &[
                &agent_id,
                &change,
                &because,
                &evidence_ids,
                &literal,
                &(importance as f32),
            ],
        )
        .await?;
    let id: String = row.get("id");
    tx.commit().await?;
    tracing::info!(id = %id, importance, "lesson.save");
    Ok(WriteOutcome {
        status: "saved",
        id,
    })
}

/// Write a diary entry behind the echo gate.
pub async fn diary(
    db: &Db,
    embedder: &Embedder,
    agent_id: &str,
    text: &str,
    importance: f64,
    source: &str,
) -> Result<WriteOutcome, WriteError> {
    if let Some(pattern) = guard::check(text) {
        return Err(WriteError::Guard(pattern));
    }
    let body = guard::strip_date_prefix(text).trim().to_string();
    if body.is_empty() {
        return Err(WriteError::Empty("empty text"));
    }
    let importance = math::sanitize_importance(importance);
    let stored = guard::with_date_prefix(&body, Local::now());
    let vector = embedder.embed_one(&body).await?;
    let literal = vector_literal(&vector);

    let mut client = db.client().await?;
    let tx = client.transaction().await?;
    let lock_key = format!("diary_save:{agent_id}");
    tx.execute("SELECT pg_advisory_xact_lock(hashtext($1))", &[&lock_key])
        .await?;

    let echo = tx
        .query(
            "SELECT id::text AS id, (1 - (embedding <=> $2::text::vector))::float8 AS similarity
             FROM self_reflections
             WHERE agent_id = $1 AND embedding IS NOT NULL
             ORDER BY created_at DESC, embedding <=> $2::text::vector
             LIMIT $3",
            &[&agent_id, &literal, &math::DIARY_ECHO_ROWS],
        )
        .await?;

    for row in &echo {
        let similarity: f64 = row.get("similarity");
        if similarity > math::DIARY_ECHO_THRESHOLD {
            let id: String = row.get("id");
            tx.commit().await?;
            tracing::info!(id = %id, similarity, "diary.echo");
            return Ok(WriteOutcome { status: "echo", id });
        }
    }

    let row = tx
        .query_one(
            "INSERT INTO self_reflections (agent_id, content, embedding, importance, source)
             VALUES ($1, $2, $3::text::vector, $4, $5)
             RETURNING id::text AS id",
            &[&agent_id, &stored, &literal, &(importance as f32), &source],
        )
        .await?;
    let id: String = row.get("id");
    tx.commit().await?;
    tracing::info!(id = %id, "diary.save");
    Ok(WriteOutcome {
        status: "saved",
        id,
    })
}

/// Append an event to the timeline. `mind_events` is append-only.
pub async fn mind_log(
    db: &Db,
    embedder: &Embedder,
    agent_id: &str,
    content: &str,
    kind: &str,
    channel: &str,
    thread_id: Option<&str>,
) -> Result<WriteOutcome, WriteError> {
    if let Some(pattern) = guard::check(content) {
        return Err(WriteError::Guard(pattern));
    }
    let body = content.trim();
    if body.is_empty() {
        return Err(WriteError::Empty("empty content"));
    }
    let vector = embedder.embed_one(body).await?;
    let literal = vector_literal(&vector);

    let client = db.client().await?;
    let row = client
        .query_one(
            "INSERT INTO mind_events (agent_id, kind, channel, thread_id, content, embedding)
             VALUES ($1, $2, $3, $4, $5, $6::text::vector)
             RETURNING id::text AS id",
            &[&agent_id, &kind, &channel, &thread_id, &body, &literal],
        )
        .await?;
    let id: String = row.get("id");
    tracing::info!(id = %id, kind, channel, "mind.log");
    Ok(WriteOutcome {
        status: "saved",
        id,
    })
}

/// Small helper for callers that want the outcome as JSON (matches the tool surface).
pub fn outcome_json(outcome: &WriteOutcome) -> serde_json::Value {
    json!({ "status": outcome.status, "id": outcome.id })
}
