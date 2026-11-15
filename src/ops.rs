//! Operator-facing probes: `doctor` and `stats`.
//!
//! Neither function panics and neither hides a failure: every probe reports `OK`, `WARN` or
//! `FAIL` with a concrete line. A diagnostic that stays silent is decoration.

use serde_json::{Value, json};

use crate::circuit::Circuit;
use crate::config::Config;
use crate::db::Db;
use crate::embed::Embedder;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "OK",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
        }
    }
}

struct Check {
    name: &'static str,
    status: Status,
    detail: String,
}

impl Check {
    fn new(name: &'static str, status: Status, detail: impl Into<String>) -> Self {
        Self {
            name,
            status,
            detail: detail.into(),
        }
    }
}

/// Six probes, sequentially: config, connection, schema dimension, circuit contents,
/// Ollama embedding, HNSW indexes and alembic revision.
pub async fn doctor(config: &Config, db: &Db, embedder: &Embedder) -> Value {
    let mut checks: Vec<Check> = Vec::with_capacity(7);
    checks.push(Check::new(
        "config.dsn",
        Status::Ok,
        format!(
            "{} via {} (registry {})",
            config.redacted_dsn(),
            config.dsn_source,
            config.registry_path.display()
        ),
    ));

    let client = match db.client().await {
        Ok(client) => {
            checks.push(Check::new("db.connect", Status::Ok, db.pool_status()));
            Some(client)
        }
        Err(error) => {
            checks.push(Check::new("db.connect", Status::Fail, error.to_string()));
            None
        }
    };

    if let Some(client) = client.as_ref() {
        match client
            .query_one(
                "SELECT current_user AS role, r.rolsuper AS superuser
                 FROM pg_roles r WHERE r.rolname = current_user",
                &[],
            )
            .await
        {
            Ok(row) => {
                let role: String = row.get("role");
                let superuser: bool = row.get("superuser");
                checks.push(Check::new(
                    "db.role",
                    if superuser { Status::Warn } else { Status::Ok },
                    if superuser {
                        format!("{role} (superuser: broad blast radius)")
                    } else {
                        role
                    },
                ));
            }
            Err(error) => checks.push(Check::new("db.role", Status::Warn, error.to_string())),
        }

        match client
            .query_one(
                "SELECT format_type(atttypid, atttypmod) AS t
                 FROM pg_attribute
                 WHERE attrelid = 'memories'::regclass AND attname = 'embedding'",
                &[],
            )
            .await
        {
            Ok(row) => {
                let kind: String = row.get("t");
                let status = if kind == "vector(768)" {
                    Status::Ok
                } else {
                    Status::Fail
                };
                checks.push(Check::new("schema.dim", status, kind));
            }
            Err(error) => checks.push(Check::new(
                "schema.dim",
                Status::Fail,
                format!("memories.embedding not readable: {error}"),
            )),
        }

        let mut summary = String::new();
        let mut nulls_total: i64 = 0;
        let mut unreadable = false;
        for circuit in Circuit::ALL {
            let sql = format!(
                "SELECT count(*) AS rows, count(*) FILTER (WHERE embedding IS NULL) AS nulls FROM {}",
                circuit.table()
            );
            match client.query_one(&sql, &[]).await {
                Ok(row) => {
                    let rows: i64 = row.get("rows");
                    let nulls: i64 = row.get("nulls");
                    nulls_total += nulls;
                    if !summary.is_empty() {
                        summary.push_str("; ");
                    }
                    summary.push_str(&format!(
                        "{} {rows} rows ({nulls} without vector)",
                        circuit.label()
                    ));
                }
                Err(error) => {
                    unreadable = true;
                    summary.push_str(&format!("{} unreadable: {error}", circuit.label()));
                }
            }
        }
        let status = if unreadable {
            Status::Fail
        } else if nulls_total > 0 {
            Status::Warn
        } else {
            Status::Ok
        };
        checks.push(Check::new("circuit.contents", status, summary));

        match client
            .query_one(
                "SELECT count(*) AS n FROM pg_indexes
                 WHERE schemaname = 'public' AND indexdef ILIKE '%hnsw%'",
                &[],
            )
            .await
        {
            Ok(row) => {
                let count: i64 = row.get("n");
                checks.push(Check::new(
                    "index.hnsw",
                    if count >= 4 { Status::Ok } else { Status::Warn },
                    format!("{count} HNSW indexes"),
                ));
            }
            Err(error) => checks.push(Check::new("index.hnsw", Status::Warn, error.to_string())),
        }

        match client
            .query_opt("SELECT version_num FROM alembic_version LIMIT 1", &[])
            .await
        {
            Ok(Some(row)) => {
                let revision: String = row.get("version_num");
                checks.push(Check::new("alembic", Status::Ok, revision));
            }
            Ok(None) => checks.push(Check::new("alembic", Status::Warn, "no revision stamped")),
            Err(error) => checks.push(Check::new("alembic", Status::Warn, error.to_string())),
        }
    }

    match embedder.probe().await {
        Ok(dim) => {
            let status = if dim == 768 { Status::Ok } else { Status::Fail };
            checks.push(Check::new(
                "ollama.embed",
                status,
                format!(
                    "{} @ {} [{}] → {dim}d ({}, source {})",
                    embedder.model(),
                    embedder.base_url(),
                    embedder.api().as_str(),
                    if dim == 768 {
                        "matches vector(768)"
                    } else {
                        "dimension mismatch"
                    },
                    config.embed_source
                ),
            ));
        }
        Err(error) => checks.push(Check::new("ollama.embed", Status::Fail, error.to_string())),
    }

    let healthy = checks.iter().all(|check| check.status != Status::Fail);
    json!({
        "ok": healthy,
        "agent_id": config.agent_id,
        "checks": checks
            .iter()
            .map(|check| json!({
                "name": check.name,
                "status": check.status.as_str(),
                "detail": check.detail,
            }))
            .collect::<Vec<_>>(),
    })
}

/// Circuit census: rows, missing vectors, newest row, alembic revision.
pub async fn stats(config: &Config, db: &Db) -> Result<Value, String> {
    let client = db.client().await.map_err(|error| error.to_string())?;
    let mut circuits = serde_json::Map::new();
    for circuit in Circuit::ALL {
        let sql = format!(
            "SELECT count(*) AS rows,
                    count(*) FILTER (WHERE embedding IS NULL) AS nulls,
                    max(created_at) AS last
             FROM {} WHERE agent_id = $1",
            circuit.table()
        );
        let row = client
            .query_one(&sql, &[&config.agent_id])
            .await
            .map_err(|error| format!("{}: {error}", circuit.label()))?;
        let rows: i64 = row.get("rows");
        let nulls: i64 = row.get("nulls");
        let last: Option<chrono::DateTime<chrono::Utc>> = row.get("last");
        circuits.insert(
            circuit.label().to_string(),
            json!({
                "table": circuit.table(),
                "rows": rows,
                "without_vector": nulls,
                "last": last.map(|stamp| stamp.to_rfc3339()),
            }),
        );
    }

    let alembic = match client
        .query_opt("SELECT version_num FROM alembic_version LIMIT 1", &[])
        .await
    {
        Ok(Some(row)) => json!(row.get::<_, String>("version_num")),
        Ok(None) => json!("none"),
        Err(error) => json!(format!("unavailable: {error}")),
    };

    Ok(json!({
        "agent_id": config.agent_id,
        "dsn": config.redacted_dsn(),
        "embed_model": config.embed_model,
        "embed_api": config.embed_api.as_str(),
        "embed_url": config.embed_url,
        "ollama": config.ollama_url,
        "circuits": circuits,
        "alembic": alembic,
    }))
}
