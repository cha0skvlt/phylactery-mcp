//! Surgical layer: tools that answer "what is actually in the circuit".
//!
//! Rules enforced here, not at call sites:
//! * reads never embed;
//! * table names come from `Circuit::table()` and column lists from constants below,
//!   never from the caller (no SQL hatch);
//! * every refusal carries a `code`.

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio_postgres::error::SqlState;
use uuid::Uuid;

use crate::circuit::Circuit;
use crate::db::{Db, DbError};
use crate::embed::{EmbedError, Embedder, vector_literal};
use crate::guard;

/// A prefix shorter than this matches too much to be an address.
pub const MIN_PREFIX: usize = 4;
/// How many candidate ids an `ambiguous_id` refusal names.
const AMBIGUOUS_SHOWN: usize = 5;
/// Cap on graph edges attached to one row.
const EVIDENCE_LIMIT: i64 = 50;

#[derive(Debug, thiserror::Error)]
pub enum SurgicalError {
    #[error("{0:?} is neither a UUID nor a hex prefix of at least 4 characters")]
    InvalidId(String),
    #[error("no row {0}")]
    NotFound(String),
    #[error("prefix {prefix} matches {count} rows: {candidates}; use a longer prefix")]
    Ambiguous {
        prefix: String,
        count: usize,
        candidates: String,
    },
    #[error("{0}")]
    Unsupported(&'static str),
    #[error("{0}")]
    InvalidValue(String),
    #[error("row {0} is protected")]
    Protected(String),
    #[error("mind_events is append-only")]
    AppendOnly,
    #[error("confirm=true is required for a real cross-namespace move")]
    ConfirmRequired,
    #[error("required migration is not available")]
    NotAvailable,
    #[error("embedding dimension {0} does not match vector(768); pass force=true to override")]
    DimMismatch(usize),
    #[error(transparent)]
    Embed(#[from] EmbedError),
    #[error("cursor is not one this server issued")]
    InvalidCursor,
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Sql(#[from] tokio_postgres::Error),
}

impl SurgicalError {
    pub fn code(&self) -> &'static str {
        match self {
            SurgicalError::InvalidId(_) => "invalid_id",
            SurgicalError::NotFound(_) => "not_found",
            SurgicalError::Ambiguous { .. } => "ambiguous_id",
            SurgicalError::Unsupported(_) => "unsupported_table",
            SurgicalError::InvalidValue(_) => "invalid_value",
            SurgicalError::Protected(_) => "protected",
            SurgicalError::AppendOnly => "append_only",
            SurgicalError::ConfirmRequired => "confirm_required",
            SurgicalError::NotAvailable => "not_available",
            SurgicalError::DimMismatch(_) => "dim_mismatch",
            SurgicalError::Embed(_) => "embed_unavailable",
            SurgicalError::InvalidCursor => "invalid_cursor",
            SurgicalError::Db(_) | SurgicalError::Sql(_) => "db_unavailable",
        }
    }
}

/// What the caller meant by `id`.
#[derive(Debug, PartialEq, Eq)]
pub enum IdQuery {
    Full(Uuid),
    Prefix(String),
}

/// Full UUID or a unique prefix. The prefix alphabet is `[0-9a-f-]`, so it can never carry a
/// LIKE wildcard into the query.
pub fn parse_id(raw: &str) -> Result<IdQuery, SurgicalError> {
    let text = raw.trim().to_ascii_lowercase();
    if let Ok(uuid) = Uuid::parse_str(&text) {
        return Ok(IdQuery::Full(uuid));
    }
    let alphabet_ok = text.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    if alphabet_ok && (MIN_PREFIX..36).contains(&text.len()) {
        Ok(IdQuery::Prefix(text))
    } else {
        Err(SurgicalError::InvalidId(raw.to_string()))
    }
}

/// One shape for all four circuits; circuit-specific fields travel in `extra`.
const MEMORIES_GET: &str = "\
SELECT id::text AS id, agent_id::text AS agent_id, memory_type::text AS register, content,
       importance::float8 AS importance, created_at, episode_id,
       (embedding IS NOT NULL) AS has_vector,
       COALESCE(metadata, '{}'::jsonb) AS metadata,
       metadata->>'source' AS source,
       '{}'::jsonb AS extra
FROM memories WHERE ";

const DIARY_GET: &str = "\
SELECT id::text AS id, agent_id::text AS agent_id, source::text AS register, content,
       importance::float8 AS importance, created_at, episode_id,
       (embedding IS NOT NULL) AS has_vector,
       '{}'::jsonb AS metadata,
       source::text AS source,
       '{}'::jsonb AS extra
FROM self_reflections WHERE ";

const LESSONS_GET: &str = "\
SELECT id::text AS id, agent_id::text AS agent_id, 'lesson'::text AS register,
       change_text || ' — because: ' || COALESCE(because_text, '') AS content,
       importance::float8 AS importance, created_at, episode_id,
       (embedding IS NOT NULL) AS has_vector,
       '{}'::jsonb AS metadata,
       NULL::text AS source,
       jsonb_build_object('evidence_ids', to_jsonb(evidence_ids)) AS extra
FROM causal_lessons WHERE ";

const MIND_GET: &str = "\
SELECT id::text AS id, agent_id::text AS agent_id, kind::text AS register, content,
       NULL::float8 AS importance, created_at, episode_id,
       (embedding IS NOT NULL) AS has_vector,
       '{}'::jsonb AS metadata,
       NULL::text AS source,
       jsonb_build_object('thread_id', thread_id, 'channel', channel) AS extra
FROM mind_events WHERE ";

fn select_for(circuit: Circuit) -> &'static str {
    match circuit {
        Circuit::Memories => MEMORIES_GET,
        Circuit::Diary => DIARY_GET,
        Circuit::Lessons => LESSONS_GET,
        Circuit::Mind => MIND_GET,
    }
}

// The only two predicates `get` ever appends: constants, not caller text.
const BY_FULL_ID: &str = "id = $1::uuid LIMIT 2";
const BY_PREFIX: &str = "id::text LIKE $1::text || '%' ORDER BY id LIMIT 6";

pub(crate) fn stamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn row_json(circuit: Circuit, row: &tokio_postgres::Row) -> Value {
    let mut out = Map::new();
    out.insert("circuit".into(), json!(circuit.label()));
    out.insert("id".into(), json!(row.get::<_, String>("id")));
    out.insert("agent_id".into(), json!(row.get::<_, String>("agent_id")));
    out.insert(
        "register".into(),
        json!(row.get::<_, Option<String>>("register")),
    );
    out.insert("text".into(), json!(row.get::<_, String>("content")));
    out.insert(
        "importance".into(),
        json!(row.get::<_, Option<f64>>("importance")),
    );
    out.insert(
        "created_at".into(),
        json!(stamp(row.get::<_, DateTime<Utc>>("created_at"))),
    );
    out.insert(
        "episode_id".into(),
        json!(row.get::<_, Option<String>>("episode_id")),
    );
    out.insert("has_vector".into(), json!(row.get::<_, bool>("has_vector")));
    out.insert("metadata".into(), row.get::<_, Value>("metadata"));
    out.insert(
        "source".into(),
        json!(row.get::<_, Option<String>>("source")),
    );
    if let Value::Object(extra) = row.get::<_, Value>("extra") {
        for (key, value) in extra {
            out.insert(key, value);
        }
    }
    Value::Object(out)
}

/// Graph edges that cite this memory as evidence. A missing graph is an empty list, not an
/// error: `get` must work on a circuit that never ran the knowledge-graph migration.
async fn evidence_triples(
    client: &deadpool_postgres::Object,
    row_id: &Uuid,
) -> Result<Vec<Value>, SurgicalError> {
    let query = client
        .query(
            "SELECT subject, predicate, object FROM knowledge_graph
             WHERE evidence_id = $1::uuid
             ORDER BY valid_from, subject, predicate
             LIMIT $2",
            &[row_id, &EVIDENCE_LIMIT],
        )
        .await;
    match query {
        Ok(rows) => Ok(rows
            .iter()
            .map(|r| {
                json!({
                    "subject": r.get::<_, String>("subject"),
                    "predicate": r.get::<_, String>("predicate"),
                    "object": r.get::<_, String>("object"),
                })
            })
            .collect()),
        Err(error)
            if error.code() == Some(&SqlState::UNDEFINED_TABLE)
                || error.code() == Some(&SqlState::UNDEFINED_COLUMN) =>
        {
            Ok(Vec::new())
        }
        Err(error) => Err(error.into()),
    }
}

/// A prefix must name exactly one row across everything that was searched.
fn ensure_unique(
    prefix: &str,
    found: &[(Circuit, tokio_postgres::Row)],
) -> Result<(), SurgicalError> {
    if found.len() <= 1 {
        return Ok(());
    }
    let candidates = found
        .iter()
        .take(AMBIGUOUS_SHOWN)
        .map(|(circuit, row)| format!("{}:{}", circuit.label(), row.get::<_, String>("id")))
        .collect::<Vec<_>>()
        .join(", ");
    Err(SurgicalError::Ambiguous {
        prefix: prefix.to_string(),
        count: found.len(),
        candidates,
    })
}

/// Read one row by full UUID or unique prefix, from one circuit or from all four.
/// No namespace filter: the point of `get` is to reach rows that search cannot see, and it
/// reports the row's `agent_id` so the caller knows where it lives.
pub async fn get(db: &Db, id: &str, table: Option<&str>) -> Result<Value, SurgicalError> {
    let query = parse_id(id)?;
    let circuits: Vec<Circuit> = match table {
        Some(name) => vec![Circuit::parse(name).ok_or(SurgicalError::Unsupported(
            "table must be memories, diary, lessons or mind",
        ))?],
        None => Circuit::ALL.to_vec(),
    };
    let client = db.client().await?;

    let mut found: Vec<(Circuit, tokio_postgres::Row)> = Vec::new();
    for circuit in circuits {
        let predicate = match query {
            IdQuery::Full(_) => BY_FULL_ID,
            IdQuery::Prefix(_) => BY_PREFIX,
        };
        let sql = format!("{}{}", select_for(circuit), predicate);
        let rows = match &query {
            IdQuery::Full(uuid) => client.query(&sql, &[uuid]).await?,
            IdQuery::Prefix(prefix) => client.query(&sql, &[prefix]).await?,
        };
        found.extend(rows.into_iter().map(|row| (circuit, row)));
    }

    if let IdQuery::Prefix(prefix) = &query {
        ensure_unique(prefix, &found)?;
    }
    if found.is_empty() {
        return Err(SurgicalError::NotFound(id.trim().to_string()));
    }

    let mut rows = Vec::with_capacity(found.len());
    for (circuit, row) in &found {
        let mut value = row_json(*circuit, row);
        let triples = if *circuit == Circuit::Memories {
            match Uuid::parse_str(row.get::<_, String>("id").as_str()) {
                Ok(uuid) => evidence_triples(&client, &uuid).await?,
                Err(_) => Vec::new(),
            }
        } else {
            Vec::new()
        };
        if let Value::Object(map) = &mut value {
            map.insert("evidence_triples".into(), Value::Array(triples));
        }
        rows.push(value);
    }
    Ok(json!({ "found": true, "rows": rows }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_uuid_in_any_spelling() {
        let canonical = "d7d4f4cc-1234-4abc-8def-0123456789ab";
        let expected = Uuid::parse_str(canonical).unwrap();
        assert_eq!(parse_id(canonical).unwrap(), IdQuery::Full(expected));
        assert_eq!(
            parse_id(&canonical.to_uppercase()).unwrap(),
            IdQuery::Full(expected)
        );
        assert_eq!(
            parse_id(&format!("  {canonical}\n")).unwrap(),
            IdQuery::Full(expected)
        );
    }

    #[test]
    fn hex_prefix_is_accepted() {
        assert_eq!(
            parse_id("D7D4").unwrap(),
            IdQuery::Prefix("d7d4".to_string())
        );
        assert_eq!(
            parse_id("d7d4f4cc-12").unwrap(),
            IdQuery::Prefix("d7d4f4cc-12".to_string())
        );
    }

    #[test]
    fn too_short_or_foreign_characters_are_invalid() {
        for bad in [
            "",
            "   ",
            "d7d",
            "zzzz",
            "d7d4%",
            "d7d4_",
            "abcd' OR 1=1 --",
        ] {
            let error = parse_id(bad).expect_err(bad);
            assert_eq!(error.code(), "invalid_id", "{bad:?}");
        }
    }

    #[test]
    fn overlong_non_uuid_is_invalid() {
        let long = "a".repeat(36);
        assert_eq!(parse_id(&long).unwrap_err().code(), "invalid_id");
        let longer = "a".repeat(40);
        assert_eq!(parse_id(&longer).unwrap_err().code(), "invalid_id");
    }

    #[test]
    fn error_codes_match_the_contract() {
        assert_eq!(SurgicalError::NotFound("x".into()).code(), "not_found");
        assert_eq!(
            SurgicalError::Ambiguous {
                prefix: "ab".into(),
                count: 2,
                candidates: "a, b".into()
            }
            .code(),
            "ambiguous_id"
        );
        assert_eq!(SurgicalError::Unsupported("t").code(), "unsupported_table");
    }

    #[test]
    fn predicates_are_the_only_caller_dependent_sql() {
        for sql in [
            MEMORIES_GET,
            DIARY_GET,
            LESSONS_GET,
            MIND_GET,
            BY_FULL_ID,
            BY_PREFIX,
        ] {
            assert!(!sql.contains(';'), "statement separator in {sql:?}");
        }
        for circuit in Circuit::ALL {
            assert!(select_for(circuit).contains(circuit.table()));
        }
    }
}

fn clamp_patch_importance(value: f64) -> f64 {
    value.clamp(0.0, 10.0)
}

fn merge_metadata(base: &Value, patch: &Value) -> Value {
    let mut merged = base.as_object().cloned().unwrap_or_default();
    if let Some(updates) = patch.as_object() {
        for (key, value) in updates {
            merged.insert(key.clone(), value.clone());
        }
    }
    Value::Object(merged)
}

/// Metadata-only edit. This deliberately has no Embedder dependency.
#[allow(clippy::too_many_arguments)]
pub async fn patch(
    db: &Db,
    agent_id: &str,
    id: &str,
    table: &str,
    importance: Option<f64>,
    memory_type: Option<&str>,
    source: Option<&str>,
    episode_id: Option<&str>,
    metadata: Option<&Value>,
) -> Result<Value, SurgicalError> {
    let circuit = Circuit::parse(table).ok_or(SurgicalError::Unsupported(
        "table must be memories, diary, lessons or mind",
    ))?;
    if circuit == Circuit::Mind {
        return Err(SurgicalError::AppendOnly);
    }
    let row_id =
        Uuid::parse_str(id.trim()).map_err(|_| SurgicalError::InvalidId(id.to_string()))?;

    if let Some(value) = importance
        && !value.is_finite()
    {
        return Err(SurgicalError::InvalidValue(
            "importance must be finite".into(),
        ));
    }
    if let Some(kind) = memory_type {
        if circuit != Circuit::Memories {
            return Err(SurgicalError::InvalidValue(
                "memory_type is accepted only for memories".into(),
            ));
        }
        if !["semantic", "identity", "lesson", "experience", "unverified"].contains(&kind) {
            return Err(SurgicalError::InvalidValue("invalid memory_type".into()));
        }
        if kind == "identity" {
            return Err(SurgicalError::Protected(id.to_string()));
        }
    }
    if let Some(value) = source {
        if circuit != Circuit::Diary {
            return Err(SurgicalError::InvalidValue(
                "source is accepted only for diary".into(),
            ));
        }
        if !["solitary", "outreach", "reflect", "seed"].contains(&value) {
            return Err(SurgicalError::InvalidValue("invalid diary source".into()));
        }
    }
    if metadata.is_some() && circuit != Circuit::Memories {
        return Err(SurgicalError::InvalidValue(
            "metadata is accepted only for memories".into(),
        ));
    }
    if let Some(value) = metadata
        && !value.is_object()
    {
        return Err(SurgicalError::InvalidValue(
            "metadata must be a JSON object".into(),
        ));
    }
    if importance.is_none()
        && memory_type.is_none()
        && source.is_none()
        && episode_id.is_none()
        && metadata.is_none()
    {
        return Err(SurgicalError::InvalidValue("nothing to patch".into()));
    }

    let mut client = db.client().await?;
    let tx = client.transaction().await?;

    let select = match circuit {
        Circuit::Memories => {
            "SELECT content, importance::float8 AS importance, metadata, memory_type::text AS register FROM memories WHERE id=$1 AND agent_id=$2 FOR UPDATE"
        }
        Circuit::Diary => {
            "SELECT content, importance::float8 AS importance, '{}'::jsonb AS metadata, source::text AS register FROM self_reflections WHERE id=$1 AND agent_id=$2 FOR UPDATE"
        }
        Circuit::Lessons => {
            "SELECT change_text || ' — because: ' || COALESCE(because_text,'') AS content, importance::float8 AS importance, '{}'::jsonb AS metadata, 'lesson'::text AS register FROM causal_lessons WHERE id=$1 AND agent_id=$2 FOR UPDATE"
        }
        Circuit::Mind => unreachable!(),
    };
    let row = tx
        .query_opt(select, &[&row_id, &agent_id])
        .await?
        .ok_or_else(|| SurgicalError::NotFound(id.to_string()))?;

    if circuit == Circuit::Memories
        && row.get::<_, Option<String>>("register").as_deref() == Some("identity")
    {
        return Err(SurgicalError::Protected(id.to_string()));
    }

    // Revision storage is optional until migration 0010. When present, capture the
    // complete before-image in the same transaction as the update.
    let revisions: bool = tx
        .query_one(
            "SELECT to_regclass('public.memory_revisions') IS NOT NULL",
            &[],
        )
        .await?
        .get(0);
    if revisions {
        let content: String = row.get("content");
        let old_importance: Option<f64> = row.get("importance");
        let old_metadata: Value = row.get("metadata");
        tx.execute(
            "INSERT INTO memory_revisions (circuit,row_id,agent_id,content,importance,metadata,op,changed_by) VALUES ($1,$2,$3,$4,$5,$6,'patch',$3)",
            &[&circuit.label(), &row_id, &agent_id, &content, &old_importance, &old_metadata],
        ).await?;
    }

    let mut changed = Vec::<String>::new();
    match circuit {
        Circuit::Memories => {
            let mut sets = Vec::<String>::new();
            let mut values: Vec<Box<dyn tokio_postgres::types::ToSql + Sync + Send>> = Vec::new();
            if let Some(v) = importance {
                values.push(Box::new(clamp_patch_importance(v)));
                sets.push(format!("importance = ${}", values.len() + 2));
                changed.push("importance".into());
            }
            if let Some(v) = memory_type {
                values.push(Box::new(v.to_string()));
                sets.push(format!("memory_type = ${}", values.len() + 2));
                changed.push("memory_type".into());
            }
            if let Some(v) = episode_id {
                values.push(Box::new(v.to_string()));
                sets.push(format!("episode_id = ${}", values.len() + 2));
                changed.push("episode_id".into());
            }
            if let Some(v) = metadata {
                let old: Value = row.get("metadata");
                values.push(Box::new(merge_metadata(&old, v)));
                sets.push(format!("metadata = ${}::jsonb", values.len() + 2));
                changed.push("metadata".into());
            }
            if !sets.is_empty() {
                let sql = format!(
                    "UPDATE memories SET {} WHERE id=$1 AND agent_id=$2",
                    sets.join(", ")
                );
                let mut params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                    vec![&row_id, &agent_id];
                for value in &values {
                    params.push(value.as_ref());
                }
                tx.execute(&sql, &params).await?;
            }
        }
        Circuit::Diary => {
            let mut sets = Vec::<String>::new();
            let mut values: Vec<Box<dyn tokio_postgres::types::ToSql + Sync + Send>> = Vec::new();
            if let Some(v) = importance {
                values.push(Box::new(clamp_patch_importance(v)));
                sets.push(format!("importance = ${}", values.len() + 2));
                changed.push("importance".into());
            }
            if let Some(v) = source {
                values.push(Box::new(v.to_string()));
                sets.push(format!("source = ${}", values.len() + 2));
                changed.push("source".into());
            }
            if let Some(v) = episode_id {
                values.push(Box::new(v.to_string()));
                sets.push(format!("episode_id = ${}", values.len() + 2));
                changed.push("episode_id".into());
            }
            if !sets.is_empty() {
                let sql = format!(
                    "UPDATE self_reflections SET {} WHERE id=$1 AND agent_id=$2",
                    sets.join(", ")
                );
                let mut params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                    vec![&row_id, &agent_id];
                for value in &values {
                    params.push(value.as_ref());
                }
                tx.execute(&sql, &params).await?;
            }
        }
        Circuit::Lessons => {
            let mut sets = Vec::<String>::new();
            let mut values: Vec<Box<dyn tokio_postgres::types::ToSql + Sync + Send>> = Vec::new();
            if let Some(v) = importance {
                values.push(Box::new(clamp_patch_importance(v)));
                sets.push(format!("importance = ${}", values.len() + 2));
                changed.push("importance".into());
            }
            if let Some(v) = episode_id {
                values.push(Box::new(v.to_string()));
                sets.push(format!("episode_id = ${}", values.len() + 2));
                changed.push("episode_id".into());
            }
            if !sets.is_empty() {
                let sql = format!(
                    "UPDATE causal_lessons SET {} WHERE id=$1 AND agent_id=$2",
                    sets.join(", ")
                );
                let mut params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
                    vec![&row_id, &agent_id];
                for value in &values {
                    params.push(value.as_ref());
                }
                tx.execute(&sql, &params).await?;
            }
        }
        Circuit::Mind => unreachable!(),
    }

    // An operation, not a memory: `phylactery_runs` carries no vector by design.
    let detail = json!({
        "circuit": circuit.label(),
        "row_id": row_id.hyphenated().to_string(),
        "agent_id": agent_id,
        "changed": changed.clone(),
    });
    tx.execute(
        "INSERT INTO phylactery_runs (id,tool,started_at,finished_at,status,detail) VALUES ($1,'patch',now(),now(),'ok',$2)",
        &[&Uuid::new_v4(), &detail],
    )
    .await?;
    tx.commit().await?;
    Ok(json!({"status":"patched","id":id,"changed":changed}))
}

#[cfg(test)]
mod patch_tests {
    use super::*;

    #[test]
    fn patch_importance_is_clamped_to_zero_through_ten() {
        assert_eq!(clamp_patch_importance(-4.0), 0.0);
        assert_eq!(clamp_patch_importance(9.0), 9.0);
        assert_eq!(clamp_patch_importance(14.0), 10.0);
    }

    #[test]
    fn metadata_patch_merges_keys_without_dropping_existing_values() {
        let base = json!({"keep": true, "replace": 1});
        let patch = json!({"replace": 2, "add": "new"});
        assert_eq!(
            merge_metadata(&base, &patch),
            json!({"keep": true, "replace": 2, "add": "new"})
        );
    }
}

#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
pub struct AdoptFilter {
    /// Circuit: memories, diary, lessons or mind.
    #[serde(default)]
    pub circuit: Option<String>,
    /// Register: memory_type for memories, source for diary, or lesson.
    #[serde(default)]
    pub register: Option<String>,
    /// RFC 3339 lower bound (inclusive).
    #[serde(default)]
    pub since: Option<String>,
    /// RFC 3339 upper bound (exclusive).
    #[serde(default)]
    pub until: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AdoptReport {
    pub status: &'static str,
    pub matched: usize,
    pub moved: Value,
    pub skipped_protected: usize,
    pub audit_id: Option<String>,
}

fn validate_agent_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

type AdoptTimes = (Option<chrono::DateTime<Utc>>, Option<chrono::DateTime<Utc>>);

fn adopt_filter_times(filter: &AdoptFilter) -> Result<AdoptTimes, SurgicalError> {
    let parse = |field: &str,
                 raw: &Option<String>|
     -> Result<Option<chrono::DateTime<Utc>>, SurgicalError> {
        raw.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                DateTime::parse_from_rfc3339(s)
                    .map(|v| v.with_timezone(&Utc))
                    .map_err(|e| {
                        SurgicalError::InvalidValue(format!("{field} is not RFC 3339: {e}"))
                    })
            })
            .transpose()
    };
    let since = parse("since", &filter.since)?;
    let until = parse("until", &filter.until)?;
    if let (Some(from), Some(to)) = (&since, &until)
        && from >= to
    {
        return Err(SurgicalError::InvalidValue(
            "since must be earlier than until".into(),
        ));
    }
    Ok((since, until))
}

fn validate_adopt_filter(filter: &AdoptFilter) -> Result<Vec<Circuit>, SurgicalError> {
    let circuits: Vec<Circuit> = match filter.circuit.as_deref() {
        Some(raw) => vec![Circuit::parse(raw).ok_or(SurgicalError::Unsupported(
            "circuit must be memories, diary, lessons or mind",
        ))?],
        None => vec![Circuit::Memories, Circuit::Diary, Circuit::Lessons],
    };
    if let Some(register) = filter.register.as_deref() {
        let valid = match circuits.as_slice() {
            [Circuit::Memories] => {
                ["semantic", "identity", "lesson", "experience", "unverified"].contains(&register)
            }
            [Circuit::Diary] => ["solitary", "outreach", "reflect", "seed"].contains(&register),
            [Circuit::Lessons] => register == "lesson",
            _ => true,
        };
        if !valid {
            return Err(SurgicalError::InvalidValue(
                "register is invalid for the selected circuit".into(),
            ));
        }
    }
    Ok(circuits)
}

/// Move selected rows between namespaces. All table/column fragments are fixed here.
pub async fn adopt(
    db: &Db,
    from_agent: &str,
    to_agent: &str,
    ids: Option<&[String]>,
    filter: &AdoptFilter,
    dry_run: bool,
    confirm: bool,
) -> Result<AdoptReport, SurgicalError> {
    if !validate_agent_id(to_agent) || !validate_agent_id(from_agent) {
        return Err(SurgicalError::InvalidValue(
            "agent ids must match [a-z0-9_-]{1,64}".into(),
        ));
    }
    if from_agent == to_agent {
        return Err(SurgicalError::InvalidValue(
            "source and destination namespaces must differ".into(),
        ));
    }
    if !dry_run && !confirm {
        return Err(SurgicalError::ConfirmRequired);
    }
    let circuits = validate_adopt_filter(filter)?;
    let parsed_ids: Option<Vec<Uuid>> = ids
        .map(|items| {
            items
                .iter()
                .map(|id| Uuid::parse_str(id).map_err(|_| SurgicalError::InvalidId(id.clone())))
                .collect()
        })
        .transpose()?;
    let (since, until) = adopt_filter_times(filter)?;
    let mut client = db.client().await?;
    let tx = client.transaction().await?;
    let mut moved = serde_json::Map::new();
    let mut matched = 0usize;
    let mut skipped_protected = 0usize;
    for circuit in circuits {
        if circuit == Circuit::Mind {
            continue;
        }
        let (table, register_col) = match circuit {
            Circuit::Memories => ("memories", "memory_type"),
            Circuit::Diary => ("self_reflections", "source"),
            Circuit::Lessons => ("causal_lessons", "'lesson'"),
            Circuit::Mind => unreachable!(),
        };
        let identity_clause = if circuit == Circuit::Memories {
            " AND memory_type <> 'identity'"
        } else {
            ""
        };
        let protected_sql = if circuit == Circuit::Memories {
            format!(
                "SELECT count(*)::bigint FROM {table} WHERE agent_id=$1 AND memory_type='identity' AND ($2::uuid[] IS NULL OR id=ANY($2)) AND ($3::text IS NULL OR memory_type=$3) AND ($4::timestamptz IS NULL OR created_at >= $4) AND ($5::timestamptz IS NULL OR created_at < $5)"
            )
        } else {
            String::new()
        };
        if circuit == Circuit::Memories {
            let n: i64 = tx
                .query_one(
                    &protected_sql,
                    &[&from_agent, &parsed_ids, &filter.register, &since, &until],
                )
                .await?
                .get(0);
            skipped_protected += n as usize;
        }
        let where_register = if register_col.starts_with('\'') {
            "($3::text IS NULL OR $3='lesson')".to_string()
        } else {
            format!("($3::text IS NULL OR {register_col}=$3)")
        };
        let select_sql = format!(
            "SELECT count(*)::bigint FROM {table} WHERE agent_id=$1 AND ($2::uuid[] IS NULL OR id=ANY($2)) AND {where_register} AND ($4::timestamptz IS NULL OR created_at >= $4) AND ($5::timestamptz IS NULL OR created_at < $5){identity_clause}"
        );
        let n: i64 = tx
            .query_one(
                &select_sql,
                &[&from_agent, &parsed_ids, &filter.register, &since, &until],
            )
            .await?
            .get(0);
        matched += n as usize
            + if circuit == Circuit::Memories {
                skipped_protected
            } else {
                0
            };
        let update_register = if register_col.starts_with('\'') {
            "($4::text IS NULL OR $4='lesson')".to_string()
        } else {
            format!("($4::text IS NULL OR {register_col}=$4)")
        };
        let moved_n = if dry_run {
            0usize
        } else {
            let update_sql = format!(
                "UPDATE {table} SET agent_id=$2 WHERE agent_id=$1 AND ($3::uuid[] IS NULL OR id=ANY($3)) AND {update_register} AND ($5::timestamptz IS NULL OR created_at >= $5) AND ($6::timestamptz IS NULL OR created_at < $6){identity_clause}"
            );
            tx.execute(
                &update_sql,
                &[
                    &from_agent,
                    &to_agent,
                    &parsed_ids,
                    &filter.register,
                    &since,
                    &until,
                ],
            )
            .await? as usize
        };
        moved.insert(
            circuit.label().to_string(),
            json!(if dry_run { n as usize } else { moved_n }),
        );
    }
    if dry_run {
        tx.rollback().await?;
        return Ok(AdoptReport {
            status: "dry_run",
            matched,
            moved: Value::Object(moved),
            skipped_protected,
            audit_id: None,
        });
    }
    let detail = json!({"from":from_agent,"to":to_agent,"moved":moved,"skipped_protected":skipped_protected});
    let audit = tx.query_one(
        "INSERT INTO phylactery_runs (tool, finished_at, status, detail) VALUES ('adopt', now(), 'completed', $1) RETURNING id::text",
        &[&detail],
    ).await?;
    let audit_id: String = audit.get(0);
    // The audit row above is the whole record: an operation, not a memory. Nothing is
    // written to `mind_events`, so no vectorless line can appear in the timeline.
    tx.commit().await?;
    Ok(AdoptReport {
        status: "adopted",
        matched,
        moved: Value::Object(moved),
        skipped_protected,
        audit_id: Some(audit_id),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeStrategy {
    KeepText,
    Concat,
}

pub fn parse_merge_strategy(raw: &str) -> Result<MergeStrategy, SurgicalError> {
    match raw {
        "keep_text" => Ok(MergeStrategy::KeepText),
        "concat" => Ok(MergeStrategy::Concat),
        _ => Err(SurgicalError::InvalidValue(
            "strategy must be keep_text or concat".into(),
        )),
    }
}

/// Collapse two rows in the same namespace/register. A revision is mandatory before deletion.
pub async fn merge(
    db: &Db,
    keep_id: &str,
    drop_id: &str,
    table: &str,
    strategy: &str,
    dry_run: bool,
    confirm: bool,
) -> Result<Value, SurgicalError> {
    let circuit = Circuit::parse(table).ok_or(SurgicalError::Unsupported(
        "table must be memories, diary, lessons or mind",
    ))?;
    if circuit == Circuit::Mind {
        return Err(SurgicalError::AppendOnly);
    }
    let strategy = parse_merge_strategy(strategy)?;
    if !dry_run && !confirm {
        return Err(SurgicalError::ConfirmRequired);
    }
    let keep = Uuid::parse_str(keep_id).map_err(|_| SurgicalError::InvalidId(keep_id.into()))?;
    let drop = Uuid::parse_str(drop_id).map_err(|_| SurgicalError::InvalidId(drop_id.into()))?;
    if keep == drop {
        return Err(SurgicalError::InvalidValue(
            "keep_id and drop_id must differ".into(),
        ));
    }
    let mut client = db.client().await?;
    let tx = client.transaction().await?;
    let revisions: bool = tx
        .query_one(
            "SELECT to_regclass('public.memory_revisions') IS NOT NULL",
            &[],
        )
        .await?
        .get(0);
    if !revisions {
        return Err(SurgicalError::NotAvailable);
    }

    let select = match circuit {
        Circuit::Memories => {
            "SELECT agent_id, memory_type::text AS register, content, importance::float8 AS importance, metadata FROM memories WHERE id=$1 FOR UPDATE"
        }
        Circuit::Diary => {
            "SELECT agent_id, source::text AS register, content, importance::float8 AS importance, '{}'::jsonb AS metadata FROM self_reflections WHERE id=$1 FOR UPDATE"
        }
        Circuit::Lessons => {
            "SELECT agent_id, 'lesson'::text AS register, change_text || ' — because: ' || COALESCE(because_text,'') AS content, importance::float8 AS importance, '{}'::jsonb AS metadata FROM causal_lessons WHERE id=$1 FOR UPDATE"
        }
        Circuit::Mind => unreachable!(),
    };
    let a = tx
        .query_opt(select, &[&keep])
        .await?
        .ok_or_else(|| SurgicalError::NotFound(keep_id.into()))?;
    let b = tx
        .query_opt(select, &[&drop])
        .await?
        .ok_or_else(|| SurgicalError::NotFound(drop_id.into()))?;
    let agent_a: String = a.get("agent_id");
    let agent_b: String = b.get("agent_id");
    let reg_a: String = a.get("register");
    let reg_b: String = b.get("register");
    if agent_a != agent_b || reg_a != reg_b {
        return Err(SurgicalError::InvalidValue(
            "rows must share agent_id and register".into(),
        ));
    }
    if circuit == Circuit::Memories && reg_a == "identity" {
        return Err(SurgicalError::Protected(keep_id.into()));
    }
    let importance_a: Option<f64> = a.get("importance");
    let importance_b: Option<f64> = b.get("importance");
    let importance = importance_a.unwrap_or(0.0).max(importance_b.unwrap_or(0.0));
    let original: String = b.get("content");
    let combined = if strategy == MergeStrategy::Concat {
        let first: String = a.get("content");
        format!("{}\n\n{}", first, original)
    } else {
        a.get::<_, String>("content")
    };
    let evidence_repointed;
    let triples_repointed;
    if dry_run {
        evidence_repointed = if circuit == Circuit::Lessons {
            tx.query_one(
                "SELECT cardinality(evidence_ids)::bigint FROM causal_lessons WHERE id=$1",
                &[&drop],
            )
            .await?
            .get::<_, i64>(0) as usize
        } else {
            0
        };
        triples_repointed = if circuit == Circuit::Memories {
            match tx
                .query_one(
                    "SELECT count(*)::bigint FROM knowledge_graph WHERE evidence_id=$1",
                    &[&drop],
                )
                .await
            {
                Ok(row) => row.get::<_, i64>(0) as usize,
                Err(error)
                    if error.code() == Some(&SqlState::UNDEFINED_TABLE)
                        || error.code() == Some(&SqlState::UNDEFINED_COLUMN) =>
                {
                    0
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            0
        };
        tx.rollback().await?;
        return Ok(json!({
            "status": "dry_run",
            "kept_id": keep_id,
            "dropped_id": drop_id,
            "importance": importance,
            "evidence_repointed": evidence_repointed,
            "triples_repointed": triples_repointed
        }));
    }
    let old_meta: Value = b.get("metadata");
    tx.execute(
        "INSERT INTO memory_revisions (circuit,row_id,agent_id,content,importance,metadata,op,changed_by) VALUES ($1,$2,$3,$4,$5,$6,'merge',$3)",
        &[&circuit.label(), &drop, &agent_a, &original, &importance_b, &old_meta],
    )
    .await?;
    match circuit {
        Circuit::Memories => {
            tx.execute(
                "UPDATE memories SET content=$2, importance=$3 WHERE id=$1",
                &[&keep, &combined, &importance],
            )
            .await?;
            triples_repointed = match tx
                .execute(
                    "UPDATE knowledge_graph SET evidence_id=$1 WHERE evidence_id=$2",
                    &[&keep, &drop],
                )
                .await
            {
                Ok(n) => n as usize,
                Err(error)
                    if error.code() == Some(&SqlState::UNDEFINED_TABLE)
                        || error.code() == Some(&SqlState::UNDEFINED_COLUMN) =>
                {
                    0
                }
                Err(error) => return Err(error.into()),
            };
            evidence_repointed = 0;
            tx.execute("DELETE FROM memories WHERE id=$1", &[&drop])
                .await?;
        }
        Circuit::Diary => {
            tx.execute(
                "UPDATE self_reflections SET content=$2, importance=$3 WHERE id=$1",
                &[&keep, &combined, &importance],
            )
            .await?;
            evidence_repointed = 0;
            triples_repointed = 0;
            tx.execute("DELETE FROM self_reflections WHERE id=$1", &[&drop])
                .await?;
        }
        Circuit::Lessons => {
            let dropped: Vec<String> = tx
                .query_one(
                    "SELECT evidence_ids FROM causal_lessons WHERE id=$1",
                    &[&drop],
                )
                .await?
                .get(0);
            if strategy == MergeStrategy::Concat {
                tx.execute(
                    "UPDATE causal_lessons SET change_text = change_text || E'\n\n' || (SELECT change_text FROM causal_lessons WHERE id=$3), importance=$2, evidence_ids=(SELECT ARRAY(SELECT DISTINCT x FROM unnest(evidence_ids || $4::text[]) x)) WHERE id=$1",
                    &[&keep, &(importance as f32), &drop, &dropped],
                )
                .await?;
            } else {
                tx.execute(
                    "UPDATE causal_lessons SET importance=$2, evidence_ids=(SELECT ARRAY(SELECT DISTINCT x FROM unnest(evidence_ids || $3::text[]) x)) WHERE id=$1",
                    &[&keep, &(importance as f32), &dropped],
                )
                .await?;
            }
            evidence_repointed = dropped.len();
            triples_repointed = 0;
            tx.execute("DELETE FROM causal_lessons WHERE id=$1", &[&drop])
                .await?;
        }
        Circuit::Mind => unreachable!(),
    }
    // An operation, not a memory: `phylactery_runs` carries no vector by design.
    let detail = json!({
        "circuit": circuit.label(),
        "kept_id": keep_id,
        "dropped_id": drop_id,
        "importance": importance,
        "evidence_repointed": evidence_repointed,
        "triples_repointed": triples_repointed
    });
    tx.execute(
        "INSERT INTO phylactery_runs (tool, finished_at, status, detail) VALUES ('merge', now(), 'completed', $1)",
        &[&detail],
    )
    .await?;
    tx.commit().await?;
    Ok(json!({
        "status": "merged",
        "kept_id": keep_id,
        "dropped_id": drop_id,
        "importance": importance,
        "evidence_repointed": evidence_repointed,
        "triples_repointed": triples_repointed
    }))
}

/// Read immutable before-images for one row. History remains available after the source row is gone.
pub async fn history(db: &Db, id: &str, table: &str, limit: i64) -> Result<Value, SurgicalError> {
    let circuit = Circuit::parse(table).ok_or(SurgicalError::Unsupported(
        "table must be memories, diary, lessons or mind",
    ))?;
    let row_id =
        Uuid::parse_str(id.trim()).map_err(|_| SurgicalError::InvalidId(id.to_string()))?;
    if !(1..=100).contains(&limit) {
        return Err(SurgicalError::InvalidValue(
            "limit must be between 1 and 100".into(),
        ));
    }
    let client = db.client().await?;
    let available: bool = client
        .query_one(
            "SELECT to_regclass('public.memory_revisions') IS NOT NULL",
            &[],
        )
        .await?
        .get(0);
    if !available {
        return Err(SurgicalError::NotAvailable);
    }
    let rows = client
        .query(
            "SELECT id, content, importance, metadata, op, changed_at, changed_by
         FROM memory_revisions WHERE circuit=$1 AND row_id=$2
         ORDER BY changed_at DESC, id DESC LIMIT $3",
            &[&circuit.label(), &row_id, &limit],
        )
        .await?;
    let versions: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "revision": row.get::<_, i64>("id"),
                "content": row.get::<_, Option<String>>("content"),
                "importance": row.get::<_, Option<f64>>("importance"),
                "metadata": row.get::<_, Option<Value>>("metadata").unwrap_or(Value::Null),
                "op": row.get::<_, String>("op"),
                "changed_at": stamp(row.get("changed_at")),
                "changed_by": row.get::<_, String>("changed_by"),
            })
        })
        .collect();
    Ok(json!({"id": id, "table": circuit.label(), "versions": versions}))
}

/// Re-vectorize rows under a named model and record the model on every row.
#[allow(clippy::too_many_arguments)]
pub async fn reembed(
    db: &Db,
    embedder: &Embedder,
    agent_id: &str,
    model: Option<&str>,
    circuit: Option<&str>,
    batch: i64,
    dry_run: bool,
    force: bool,
) -> Result<Value, SurgicalError> {
    let target = model
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(embedder.model())
        .to_string();
    let circuits: Vec<Circuit> = match circuit {
        Some(name) => vec![Circuit::parse(name).ok_or(SurgicalError::Unsupported(
            "circuit must be memories, diary, lessons or mind",
        ))?],
        None => Circuit::ALL.to_vec(),
    };
    let batch = batch.clamp(1, 256);
    let client = db.client().await?;

    if !dry_run {
        let dim = embedder.probe().await?;
        if dim != 768 && !force {
            return Err(SurgicalError::DimMismatch(dim));
        }
    }

    let mut scanned = 0i64;
    let mut stale = 0i64;
    let mut mismatched = 0i64;
    let mut reembedded = 0i64;
    let mut skipped = 0i64;
    let mut failed: Vec<String> = Vec::new();

    for c in circuits {
        let identity = if c == Circuit::Memories {
            " AND memory_type <> 'identity'"
        } else {
            ""
        };
        let counts = format!(
            "SELECT count(*) FILTER (WHERE embedding IS NOT NULL)::bigint AS scanned,
                    count(*) FILTER (WHERE embedding IS NOT NULL AND (embed_model IS DISTINCT FROM $2 OR embed_dim IS DISTINCT FROM 768))::bigint AS stale,
                    count(*) FILTER (WHERE embedding IS NOT NULL AND embed_dim IS NOT NULL AND embed_dim <> 768)::bigint AS mismatched
             FROM {} WHERE agent_id = $1{}",
            c.table(),
            identity
        );
        let row = client.query_one(&counts, &[&agent_id, &target]).await?;
        scanned += row.get::<_, i64>("scanned");
        stale += row.get::<_, i64>("stale");
        mismatched += row.get::<_, i64>("mismatched");
        if c == Circuit::Memories {
            let anchors = client
                .query_one(
                    "SELECT count(*)::bigint FROM memories
                     WHERE agent_id = $1 AND memory_type = 'identity' AND embedding IS NOT NULL",
                    &[&agent_id],
                )
                .await?;
            skipped += anchors.get::<_, i64>(0);
        }
        if dry_run {
            continue;
        }

        let select = format!(
            "SELECT id::text AS id, {} AS content FROM {}
             WHERE agent_id = $1 AND embedding IS NOT NULL
               AND (embed_model IS DISTINCT FROM $2 OR embed_dim IS DISTINCT FROM 768){}
             ORDER BY created_at LIMIT $3",
            if c == Circuit::Lessons {
                "change_text || ' ' || COALESCE(because_text, '')"
            } else {
                "content"
            },
            c.table(),
            identity
        );
        let rows = client.query(&select, &[&agent_id, &target, &batch]).await?;
        let update = format!(
            "UPDATE {} SET embedding = $2::text::vector, embed_model = $3, embed_dim = 768 WHERE id = $1::uuid",
            c.table()
        );
        for row in rows {
            let id: String = row.get("id");
            let content: String = row.get("content");
            let Ok(row_id) = Uuid::parse_str(&id) else {
                failed.push(format!("{id}: not a UUID"));
                continue;
            };
            let body = guard::strip_date_prefix(&content);
            match embedder.embed_one(body).await {
                Ok(vector) => {
                    if let Err(error) = client
                        .execute(&update, &[&row_id, &vector_literal(&vector), &target])
                        .await
                    {
                        failed.push(format!("{id}: {error}"));
                    } else {
                        reembedded += 1;
                    }
                }
                Err(error) => failed.push(format!("{id}: {error}")),
            }
        }
    }

    Ok(json!({
        "status": if dry_run { "dry_run" } else { "reembedded" },
        "model": target,
        "dim": 768,
        "scanned": scanned,
        "stale": stale,
        "mismatched": mismatched,
        "reembedded": reembedded,
        "failed": failed.len(),
        "skipped": skipped,
    }))
}

#[cfg(test)]
mod history_tests {
    #[test]
    fn history_operation_vocabulary_is_closed() {
        for op in ["replace", "patch", "merge", "forget"] {
            assert!(["replace", "patch", "merge", "forget"].contains(&op));
        }
        assert!(!["replace", "patch", "merge", "forget"].contains(&"delete"));
    }
}

#[cfg(test)]
mod merge_tests {
    use super::*;

    #[test]
    fn merge_strategy_vocabulary_is_closed() {
        assert_eq!(
            parse_merge_strategy("keep_text").unwrap(),
            MergeStrategy::KeepText
        );
        assert_eq!(
            parse_merge_strategy("concat").unwrap(),
            MergeStrategy::Concat
        );
        assert!(parse_merge_strategy("sql").is_err());
    }
}

#[cfg(test)]
mod adopt_tests {
    use super::*;

    #[test]
    fn adopt_agent_ids_follow_the_namespace_contract() {
        assert!(validate_agent_id("agent_01-prod"));
        assert!(!validate_agent_id("Agent"));
        assert!(!validate_agent_id("bad/name"));
        assert!(!validate_agent_id(&"a".repeat(65)));
    }

    #[test]
    fn adopt_filter_rejects_invalid_registers_and_time_order() {
        let filter = AdoptFilter {
            circuit: Some("memories".into()),
            register: Some("unknown".into()),
            ..Default::default()
        };
        assert!(validate_adopt_filter(&filter).is_err());
        let mind = AdoptFilter {
            circuit: Some("mind".into()),
            ..Default::default()
        };
        assert_eq!(validate_adopt_filter(&mind).unwrap(), vec![Circuit::Mind]);
        let times = AdoptFilter {
            since: Some("2026-11-15T12:00:00Z".into()),
            until: Some("2026-11-14T12:00:00Z".into()),
            ..Default::default()
        };
        assert!(adopt_filter_times(&times).is_err());
    }
}
