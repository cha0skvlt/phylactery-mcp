//! Mutations: forget, replace, backfill.
//!
//! Three rules are enforced here, not negotiated at call sites:
//! * `mind_events` is append-only;
//! * `memory_type = identity`, diary seed rows and lessons with importance >= 9 are protected;
//! * a replacement re-embeds the text and re-runs the guard, because a new text is new content.

use chrono::Local;
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::circuit::Circuit;
use crate::db::{Db, DbError};
use crate::embed::{EmbedError, Embedder, vector_literal};
use crate::guard;

/// Importance at or above this value is a standing order: not deletable, not rewritable.
pub const STANDING_ORDER: f32 = 9.0;

#[derive(Debug, Serialize)]
pub struct MutateOutcome {
    pub status: &'static str,
    pub id: String,
}

#[derive(Debug, Serialize)]
pub struct BackfillReport {
    pub table: &'static str,
    pub scanned: usize,
    pub embedded: usize,
    pub failed: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum MutateError {
    #[error("injection guard blocked the text (pattern {0})")]
    Guard(&'static str),
    #[error("no row {0} in {1}")]
    NotFound(String, &'static str),
    #[error("{0} is not a UUID")]
    BadId(String),
    #[error("row {0} is protected in {1}")]
    Protected(String, &'static str),
    #[error("mind_events is append-only: correct a mistake with a new event")]
    AppendOnly,
    #[error("{0}")]
    Unsupported(&'static str),
    #[error("nothing to write")]
    Empty,
    #[error("{0}")]
    ConfirmRequired(&'static str),
    #[error("id {0} exists in several circuits; pass table")]
    Ambiguous(String),
    #[error(transparent)]
    Embed(#[from] EmbedError),
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Sql(#[from] tokio_postgres::Error),
}

impl MutateError {
    pub fn code(&self) -> &'static str {
        match self {
            MutateError::Guard(_) => "injection_blocked",
            MutateError::NotFound(_, _) => "not_found",
            MutateError::BadId(_) => "invalid_id",
            MutateError::Protected(_, _) => "protected",
            MutateError::AppendOnly => "append_only",
            MutateError::Unsupported(_) => "unsupported_table",
            MutateError::Empty => "invalid_text",
            MutateError::ConfirmRequired(_) => "confirm_required",
            MutateError::Ambiguous(_) => "ambiguous_id",
            MutateError::Embed(_) => "embed_unavailable",
            MutateError::Db(_) | MutateError::Sql(_) => "db_unavailable",
        }
    }
}

/// Delete one row. Returns `protected` rather than deleting an anchor.
pub async fn forget(
    db: &Db,
    agent_id: &str,
    id: &str,
    table: &str,
) -> Result<MutateOutcome, MutateError> {
    let circuit = Circuit::parse(table).ok_or(MutateError::Unsupported(
        "table must be memories, diary, lessons or mind",
    ))?;
    if circuit == Circuit::Mind {
        return Err(MutateError::AppendOnly);
    }
    let row_id = Uuid::parse_str(id).map_err(|_| MutateError::BadId(id.to_string()))?;
    let mut client = db.client().await?;
    let tx = client.transaction().await?;

    let protected = match circuit {
        Circuit::Memories => {
            let row = tx
                .query_opt(
                    "SELECT memory_type FROM memories
                     WHERE id = $1::uuid AND agent_id = $2
                     FOR UPDATE",
                    &[&row_id, &agent_id],
                )
                .await?;
            let Some(row) = row else {
                return Err(MutateError::NotFound(id.to_string(), circuit.label()));
            };
            let kind: Option<String> = row.get("memory_type");
            kind.as_deref() == Some("identity")
        }
        Circuit::Diary => {
            let row = tx
                .query_opt(
                    "SELECT source FROM self_reflections
                     WHERE id = $1::uuid AND agent_id = $2
                     FOR UPDATE",
                    &[&row_id, &agent_id],
                )
                .await?;
            let Some(row) = row else {
                return Err(MutateError::NotFound(id.to_string(), circuit.label()));
            };
            let source: String = row.get("source");
            source == "seed"
        }
        Circuit::Lessons => {
            let row = tx
                .query_opt(
                    "SELECT importance FROM causal_lessons
                     WHERE id = $1::uuid AND agent_id = $2
                     FOR UPDATE",
                    &[&row_id, &agent_id],
                )
                .await?;
            let Some(row) = row else {
                return Err(MutateError::NotFound(id.to_string(), circuit.label()));
            };
            let importance: f32 = row.get("importance");
            importance >= STANDING_ORDER
        }
        Circuit::Mind => unreachable!("handled above"),
    };

    if protected {
        return Err(MutateError::Protected(id.to_string(), circuit.label()));
    }

    let sql = format!(
        "DELETE FROM {} WHERE id = $1::uuid AND agent_id = $2",
        circuit.table()
    );
    let deleted = tx.execute(&sql, &[&row_id, &agent_id]).await?;
    if deleted == 0 {
        return Err(MutateError::NotFound(id.to_string(), circuit.label()));
    }
    tx.commit().await?;
    tracing::info!(id, table = circuit.label(), "mem.forget");
    Ok(MutateOutcome {
        status: "forgotten",
        id: id.to_string(),
    })
}

/// Rewrite the text of one row and re-embed it. Importance and provenance stay untouched.
pub async fn replace(
    db: &Db,
    embedder: &Embedder,
    agent_id: &str,
    id: &str,
    table: &str,
    text: &str,
) -> Result<MutateOutcome, MutateError> {
    let circuit = Circuit::parse(table).ok_or(MutateError::Unsupported(
        "table must be memories, diary, lessons or mind",
    ))?;
    match circuit {
        Circuit::Mind => return Err(MutateError::AppendOnly),
        Circuit::Lessons => {
            return Err(MutateError::Unsupported(
                "lessons are rewritten through change/because, not replace",
            ));
        }
        Circuit::Memories | Circuit::Diary => {}
    }
    if let Some(pattern) = guard::check(text) {
        return Err(MutateError::Guard(pattern));
    }
    let body = guard::strip_date_prefix(text).trim();
    if body.is_empty() {
        return Err(MutateError::Empty);
    }
    let row_id = Uuid::parse_str(id).map_err(|_| MutateError::BadId(id.to_string()))?;
    let stored = guard::with_date_prefix(body, Local::now());
    let vector = embedder.embed_one(body).await?;
    let literal = vector_literal(&vector);

    let mut client = db.client().await?;
    let tx = client.transaction().await?;

    let protected = if circuit == Circuit::Memories {
        let row = tx
            .query_opt(
                "SELECT memory_type FROM memories
                 WHERE id = $1::uuid AND agent_id = $2
                 FOR UPDATE",
                &[&row_id, &agent_id],
            )
            .await?;
        let Some(row) = row else {
            return Err(MutateError::NotFound(id.to_string(), circuit.label()));
        };
        let kind: Option<String> = row.get("memory_type");
        kind.as_deref() == Some("identity")
    } else {
        let row = tx
            .query_opt(
                "SELECT source FROM self_reflections
                 WHERE id = $1::uuid AND agent_id = $2
                 FOR UPDATE",
                &[&row_id, &agent_id],
            )
            .await?;
        let Some(row) = row else {
            return Err(MutateError::NotFound(id.to_string(), circuit.label()));
        };
        let source: String = row.get("source");
        source == "seed"
    };

    if protected {
        return Err(MutateError::Protected(id.to_string(), circuit.label()));
    }

    let sql = format!(
        "UPDATE {} SET content = $3, embedding = $4::text::vector
         WHERE id = $1::uuid AND agent_id = $2",
        circuit.table()
    );
    let updated = tx
        .execute(&sql, &[&row_id, &agent_id, &stored, &literal])
        .await?;
    if updated == 0 {
        return Err(MutateError::NotFound(id.to_string(), circuit.label()));
    }
    tx.commit().await?;
    tracing::info!(id, table = circuit.label(), "mem.replace");
    Ok(MutateOutcome {
        status: "replaced",
        id: id.to_string(),
    })
}

/// Fill missing embeddings for one circuit, in batches. One bad row does not stop the batch.
pub async fn backfill(
    db: &Db,
    embedder: &Embedder,
    agent_id: &str,
    table: &str,
    batch: i64,
) -> Result<BackfillReport, MutateError> {
    let circuit = Circuit::parse(table).ok_or(MutateError::Unsupported(
        "table must be memories, diary, lessons or mind",
    ))?;
    let batch = batch.clamp(1, 256);
    let client = db.client().await?;

    let sql = format!(
        "SELECT id::text AS id, {} AS content FROM {}
         WHERE agent_id = $1 AND embedding IS NULL{}
         ORDER BY created_at
         LIMIT $2",
        if circuit == Circuit::Lessons {
            "change_text || ' ' || COALESCE(because_text, '')"
        } else {
            "content"
        },
        circuit.table(),
        if circuit == Circuit::Memories {
            " AND memory_type <> 'identity'"
        } else {
            ""
        }
    );
    let rows = client.query(&sql, &[&agent_id, &batch]).await?;

    let mut report = BackfillReport {
        table: circuit.table(),
        scanned: rows.len(),
        embedded: 0,
        failed: Vec::new(),
    };
    let update = format!(
        "UPDATE {} SET embedding = $2::text::vector WHERE id = $1::uuid",
        circuit.table()
    );
    for row in rows {
        let id: String = row.get("id");
        let content: String = row.get("content");
        let Ok(row_id) = Uuid::parse_str(&id) else {
            report.failed.push(format!("{id}: not a UUID"));
            continue;
        };
        let body = guard::strip_date_prefix(&content);
        match embedder.embed_one(body).await {
            Ok(vector) => {
                if let Err(error) = client
                    .execute(&update, &[&row_id, &vector_literal(&vector)])
                    .await
                {
                    report.failed.push(format!("{id}: {error}"));
                } else {
                    report.embedded += 1;
                }
            }
            Err(error) => report.failed.push(format!("{id}: {error}")),
        }
    }
    tracing::info!(
        table = circuit.label(),
        scanned = report.scanned,
        embedded = report.embedded,
        failed = report.failed.len(),
        "mem.backfill"
    );
    Ok(report)
}

/// What `forget_any` was asked to do.
#[derive(Debug)]
pub struct ForgetRequest<'a> {
    /// The server's own namespace: who is acting, and where the audit event lands.
    pub caller: &'a str,
    /// The namespace the row lives in. Differs from `caller` only by explicit request.
    pub target: &'a str,
    /// Full UUID.
    pub id: &'a str,
    /// One circuit, or `None` to look in all of them.
    pub table: Option<&'a str>,
    pub dry_run: bool,
    pub confirm: bool,
    /// Lifts the protections (identity, seed, standing order, mind append-only).
    pub force: bool,
}

#[derive(Debug, Serialize)]
pub struct ForgetReport {
    pub status: &'static str,
    pub id: String,
    pub circuit: &'static str,
    pub agent_id: String,
    /// Why the row is normally protected, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protection_lifted: Option<&'static str>,
    pub needs_confirm: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit_id: Option<String>,
}

// Fixed statements: the only caller-dependent parts are bind parameters.
const LOCK_MEMORIES: &str = "SELECT memory_type::text AS reg, NULL::float8 AS imp FROM memories
     WHERE id = $1::uuid AND agent_id = $2 FOR UPDATE";
const LOCK_DIARY: &str = "SELECT source::text AS reg, NULL::float8 AS imp FROM self_reflections
     WHERE id = $1::uuid AND agent_id = $2 FOR UPDATE";
const LOCK_LESSONS: &str = "SELECT NULL::text AS reg, importance::float8 AS imp FROM causal_lessons
     WHERE id = $1::uuid AND agent_id = $2 FOR UPDATE";
const LOCK_MIND: &str = "SELECT kind::text AS reg, NULL::float8 AS imp FROM mind_events
     WHERE id = $1::uuid AND agent_id = $2 FOR UPDATE";

/// Why a row is protected, if it is. The same rules `forget` has always applied.
fn protection(
    circuit: Circuit,
    reg: Option<&str>,
    importance: Option<f64>,
) -> Option<&'static str> {
    match circuit {
        Circuit::Memories if reg == Some("identity") => Some("identity"),
        Circuit::Diary if reg == Some("seed") => Some("seed"),
        Circuit::Lessons if importance.unwrap_or(0.0) >= f64::from(STANDING_ORDER) => {
            Some("standing_order")
        }
        Circuit::Mind => Some("append_only"),
        _ => None,
    }
}

/// `detail` of the `phylactery_runs` row that records a forced or cross-namespace delete.
/// Exactly seven keys. `lifted` names the protection that was actually overridden (null when
/// none was); `forced` records that `force` was set on the call.
pub fn audit_detail(
    request: &ForgetRequest<'_>,
    circuit: Circuit,
    row_id: Uuid,
    lifted: Option<&'static str>,
) -> Value {
    json!({
        "circuit": circuit.label(),
        "row_id": row_id.hyphenated().to_string(),
        "agent_id": request.target,
        "caller": request.caller,
        "lifted": lifted,
        "forced": request.force,
        "foreign": request.target != request.caller,
    })
}

/// General forget: any circuit, optional namespace, optional lifting of protections.
///
/// Without `force` it behaves exactly like [`forget`]: anchors, seeds, standing orders and
/// mind events are refused. With `force` + `confirm` it deletes them too, in one transaction
/// that also writes an audit row into `phylactery_runs` (never `mind_events`: an audit line is
/// not a memory and must not carry, or lack, a vector). A foreign namespace needs
/// `confirm` even without `force`. Graph edges citing a deleted memory go with it (the
/// `knowledge_graph.evidence_id` foreign key cascades).
pub async fn forget_any(db: &Db, request: &ForgetRequest<'_>) -> Result<ForgetReport, MutateError> {
    let row_id =
        Uuid::parse_str(request.id).map_err(|_| MutateError::BadId(request.id.to_string()))?;
    let circuits: Vec<Circuit> = match request.table {
        Some(name) => {
            let circuit = Circuit::parse(name).ok_or(MutateError::Unsupported(
                "table must be memories, diary, lessons or mind",
            ))?;
            if circuit == Circuit::Mind && !request.force {
                return Err(MutateError::AppendOnly);
            }
            vec![circuit]
        }
        None => Circuit::ALL
            .into_iter()
            .filter(|circuit| request.force || *circuit != Circuit::Mind)
            .collect(),
    };

    let mut client = db.client().await?;
    let tx = client.transaction().await?;

    let mut found: Vec<(Circuit, Option<&'static str>)> = Vec::new();
    for circuit in circuits {
        let sql = match circuit {
            Circuit::Memories => LOCK_MEMORIES,
            Circuit::Diary => LOCK_DIARY,
            Circuit::Lessons => LOCK_LESSONS,
            Circuit::Mind => LOCK_MIND,
        };
        if let Some(row) = tx.query_opt(sql, &[&row_id, &request.target]).await? {
            let reg: Option<String> = row.get("reg");
            let imp: Option<f64> = row.get("imp");
            found.push((circuit, protection(circuit, reg.as_deref(), imp)));
        }
    }
    let (circuit, reason) = match found.as_slice() {
        [] => {
            return Err(MutateError::NotFound(
                request.id.to_string(),
                request
                    .table
                    .and_then(Circuit::parse)
                    .map_or("any", Circuit::label),
            ));
        }
        [one] => *one,
        _ => return Err(MutateError::Ambiguous(request.id.to_string())),
    };

    if reason.is_some() && !request.force {
        return Err(if circuit == Circuit::Mind {
            MutateError::AppendOnly
        } else {
            MutateError::Protected(request.id.to_string(), circuit.label())
        });
    }
    let foreign = request.target != request.caller;
    let needs_confirm = foreign || reason.is_some();

    let mut report = ForgetReport {
        status: "dry_run",
        id: request.id.to_string(),
        circuit: circuit.label(),
        agent_id: request.target.to_string(),
        protection_lifted: reason,
        needs_confirm,
        audit_id: None,
    };
    if request.dry_run {
        return Ok(report);
    }
    if needs_confirm && !request.confirm {
        return Err(MutateError::ConfirmRequired(if reason.is_some() {
            "force on a protected row requires confirm=true"
        } else {
            "a foreign namespace requires confirm=true"
        }));
    }

    // The storage trigger guards anchors; the operator switch is scoped to this transaction.
    if reason == Some("identity") {
        tx.execute(
            "SELECT set_config('phylactery.allow_anchor', 'on', true)",
            &[],
        )
        .await?;
    }
    let sql = format!(
        "DELETE FROM {} WHERE id = $1::uuid AND agent_id = $2",
        circuit.table()
    );
    let deleted = tx.execute(&sql, &[&row_id, &request.target]).await?;
    if deleted == 0 {
        return Err(MutateError::NotFound(
            request.id.to_string(),
            circuit.label(),
        ));
    }

    if foreign || reason.is_some() {
        // An operation, not a memory: `phylactery_runs` carries no vector by design, so the
        // audit row can never become vector debt. It commits with the DELETE or not at all.
        let detail = audit_detail(request, circuit, row_id, reason);
        let audit = tx
            .query_one(
                "INSERT INTO phylactery_runs (id, tool, started_at, finished_at, status, detail)
                 VALUES ($1, 'forget', now(), now(), 'ok', $2)
                 RETURNING id::text AS id",
                &[&Uuid::new_v4(), &detail],
            )
            .await?;
        report.audit_id = Some(audit.get("id"));
    }
    tx.commit().await?;
    tracing::info!(
        id = request.id,
        table = circuit.label(),
        agent = request.target,
        forced = reason.is_some(),
        "mem.forget_any"
    );
    report.status = "forgotten";
    Ok(report)
}

pub fn mutate_json(outcome: &MutateOutcome) -> serde_json::Value {
    json!({ "status": outcome.status, "id": outcome.id })
}

pub fn backfill_json(report: &BackfillReport) -> serde_json::Value {
    json!({
        "table": report.table,
        "scanned": report.scanned,
        "embedded": report.embedded,
        "failed": report.failed,
    })
}

#[allow(dead_code)]
fn _assert_send_sync() {
    fn assert_send<T: Send>() {}
    assert_send::<MutateOutcome>();
}

#[cfg(test)]
mod forget_any_tests {
    use super::*;

    #[test]
    fn protection_rules_match_the_old_forget() {
        assert_eq!(
            protection(Circuit::Memories, Some("identity"), None),
            Some("identity")
        );
        assert_eq!(protection(Circuit::Memories, Some("semantic"), None), None);
        assert_eq!(protection(Circuit::Diary, Some("seed"), None), Some("seed"));
        assert_eq!(protection(Circuit::Diary, Some("reflect"), None), None);
        assert_eq!(
            protection(Circuit::Lessons, None, Some(9.0)),
            Some("standing_order")
        );
        assert_eq!(protection(Circuit::Lessons, None, Some(8.9)), None);
        assert_eq!(
            protection(Circuit::Mind, Some("agent_out"), None),
            Some("append_only")
        );
    }

    fn request<'a>(caller: &'a str, target: &'a str, force: bool) -> ForgetRequest<'a> {
        ForgetRequest {
            caller,
            target,
            id: "00000000-0000-0000-0000-000000000001",
            table: None,
            dry_run: false,
            confirm: true,
            force,
        }
    }

    #[test]
    fn audit_detail_has_exactly_the_seven_keys() {
        let id = Uuid::parse_str("D7D4F4CC-1234-4ABC-8DEF-0123456789AB").unwrap();
        let detail = audit_detail(&request("me", "other", false), Circuit::Memories, id, None);
        let object = detail.as_object().expect("object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "agent_id", "caller", "circuit", "forced", "foreign", "lifted", "row_id"
            ]
        );
        assert_eq!(detail["circuit"], "memories");
        assert_eq!(detail["row_id"], "d7d4f4cc-1234-4abc-8def-0123456789ab");
        assert_eq!(detail["agent_id"], "other");
        assert_eq!(detail["caller"], "me");
        assert!(detail["lifted"].is_null());
        assert_eq!(detail["forced"], false);
        assert_eq!(detail["foreign"], true);
    }

    #[test]
    fn protected_row_reports_lifted_and_forced() {
        let id = Uuid::nil();
        let detail = audit_detail(
            &request("me", "me", true),
            Circuit::Memories,
            id,
            Some("identity"),
        );
        assert_eq!(detail["lifted"], "identity");
        assert_eq!(detail["forced"], true);
        assert_eq!(detail["foreign"], false);
    }

    #[test]
    fn new_codes_match_the_contract() {
        assert_eq!(MutateError::ConfirmRequired("x").code(), "confirm_required");
        assert_eq!(MutateError::Ambiguous("x".into()).code(), "ambiguous_id");
    }
}
