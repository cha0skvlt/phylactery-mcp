//! Operations: backup, verification, manifest, migrations, maintenance.
//!
//! Every function here runs only when an agent calls the matching tool. The server has no
//! schedule, no background worker and no self-started work: it acts when asked, and reports what
//! it did into `phylactery_runs`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::db::Db;

pub const SCRATCH_DB: &str = "phylactery_verify";

#[derive(Debug, thiserror::Error)]
pub enum OpsError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("database: {0}")]
    Db(#[from] crate::db::DbError),
    #[error("sql: {0}")]
    Sql(#[from] tokio_postgres::Error),
    #[error("pg_dump failed: {0}")]
    Dump(String),
    #[error("restore failed: {0}")]
    Restore(String),
    #[error("encryption failed: {0}")]
    Encrypt(String),
    #[error(
        "verification needs an admin DSN: set PHYLACTERY_ADMIN_DSN or agent.databases.phylactery.admin_dsn"
    )]
    NoAdminDsn,
    #[error("no backup found")]
    NoBackup,
    #[error("a plaintext backup requires confirm=true")]
    PlainNeedsConfirm,
    #[error("backup {0} not found")]
    NotFound(String),
}

impl OpsError {
    pub fn code(&self) -> &'static str {
        match self {
            OpsError::Io(_) => "io_error",
            OpsError::Db(_) | OpsError::Sql(_) => "db_unavailable",
            OpsError::Dump(_) => "dump_failed",
            OpsError::Restore(_) => "restore_failed",
            OpsError::Encrypt(_) => "encrypt_failed",
            OpsError::NoAdminDsn => "no_admin_dsn",
            OpsError::NoBackup => "not_found",
            OpsError::PlainNeedsConfirm => "confirm_required",
            OpsError::NotFound(_) => "not_found",
        }
    }
}

/// `phylactery-YYYY-MM-DD-HHMM.sql.gz`
pub fn backup_name(now: DateTime<Utc>) -> String {
    format!("phylactery-{}.sql.gz", now.format("%Y-%m-%d-%H%M"))
}

/// Files to delete under the 7-daily / 12-monthly policy. The newest file is never returned.
pub fn retention_prune(files: &[PathBuf], now: DateTime<Utc>) -> Vec<PathBuf> {
    let mut sorted: Vec<PathBuf> = files.to_vec();
    sorted.sort();
    if sorted.len() <= 7 {
        return Vec::new();
    }
    let mut keep: Vec<PathBuf> = sorted[sorted.len().saturating_sub(7)..].to_vec();
    let mut seen_months: Vec<String> = Vec::new();
    for file in sorted.iter().rev() {
        let name = file
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let month = name.get(11..18).unwrap_or("").to_string();
        if month.is_empty() || seen_months.contains(&month) {
            continue;
        }
        if seen_months.len() >= 12 {
            break;
        }
        seen_months.push(month);
        if !keep.contains(file) {
            keep.push(file.clone());
        }
    }
    let _ = now;
    sorted
        .into_iter()
        .filter(|file| !keep.contains(file))
        .collect()
}

fn sha256_file(path: &Path) -> Result<String, OpsError> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

struct DsnParts {
    host: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    dbname: Option<String>,
    password: Option<String>,
}

fn dsn_parts(dsn: &str) -> Result<DsnParts, OpsError> {
    let config: tokio_postgres::Config = dsn
        .parse()
        .map_err(|error| OpsError::Dump(format!("bad dsn: {error}")))?;
    Ok(DsnParts {
        host: config.get_hosts().first().map(|host| match host {
            tokio_postgres::config::Host::Tcp(name) => name.clone(),
            #[cfg(unix)]
            tokio_postgres::config::Host::Unix(path) => path.display().to_string(),
        }),
        port: config.get_ports().first().copied(),
        user: config.get_user().map(str::to_string),
        dbname: config.get_dbname().map(str::to_string),
        password: config
            .get_password()
            .map(|bytes| String::from_utf8_lossy(bytes).to_string()),
    })
}

fn dump_to_gzip(dsn: &str, target: &Path) -> Result<(), OpsError> {
    let parts = dsn_parts(dsn)?;
    let mut dump = Command::new("pg_dump");
    dump.arg("--no-owner").arg("--no-privileges");
    if let Some(host) = &parts.host {
        dump.arg("-h").arg(host);
    }
    if let Some(port) = parts.port {
        dump.arg("-p").arg(port.to_string());
    }
    if let Some(user) = &parts.user {
        dump.arg("-U").arg(user);
    }
    if let Some(dbname) = &parts.dbname {
        dump.arg("-d").arg(dbname);
    }
    if let Some(password) = &parts.password {
        dump.env("PGPASSWORD", password);
    }
    dump.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = dump.spawn()?;
    let stdout = child.stdout.take().expect("dump stdout");
    let file = std::fs::File::create(target)?;
    let mut gzip = Command::new("gzip")
        .arg("-9")
        .stdin(Stdio::from(stdout))
        .stdout(Stdio::from(file))
        .spawn()?;

    let dump_status = child.wait()?;
    let gzip_status = gzip.wait()?;
    if !dump_status.success() {
        let mut message = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut message);
        }
        return Err(OpsError::Dump(message.trim().to_string()));
    }
    if !gzip_status.success() {
        return Err(OpsError::Dump("gzip failed".to_string()));
    }
    Ok(())
}

fn decrypt_to_temp(archive: &Path) -> Result<PathBuf, OpsError> {
    let temp =
        std::env::temp_dir().join(format!("phylactery-verify-{}.sql.gz", std::process::id()));
    let status = Command::new("gpg")
        .args(["--batch", "--yes", "--output"])
        .arg(&temp)
        .arg("--decrypt")
        .arg(archive)
        .status()?;
    if !status.success() {
        return Err(OpsError::Restore(format!(
            "cannot decrypt {}: gpg exited with {status}",
            archive.display()
        )));
    }
    Ok(temp)
}

fn restore_into_scratch(admin_dsn: &str, archive: &Path) -> Result<(), OpsError> {
    let encrypted = archive
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext == "gpg")
        .unwrap_or(false);
    let source = if encrypted {
        decrypt_to_temp(archive)?
    } else {
        archive.to_path_buf()
    };
    let result = restore_plain(admin_dsn, &source);
    if encrypted {
        let _ = std::fs::remove_file(&source);
    }
    result
}

fn restore_plain(admin_dsn: &str, archive: &Path) -> Result<(), OpsError> {
    let parts = dsn_parts(admin_dsn)?;
    let mut psql = Command::new("psql");
    psql.arg("-q").arg("-v").arg("ON_ERROR_STOP=1");
    if let Some(host) = &parts.host {
        psql.arg("-h").arg(host);
    }
    if let Some(port) = parts.port {
        psql.arg("-p").arg(port.to_string());
    }
    if let Some(user) = &parts.user {
        psql.arg("-U").arg(user);
    }
    psql.arg("-d").arg(SCRATCH_DB);
    if let Some(password) = &parts.password {
        psql.env("PGPASSWORD", password);
    }
    psql.stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());

    let mut gunzip = Command::new("gunzip")
        .arg("-c")
        .arg(archive)
        .stdout(Stdio::piped())
        .spawn()?;
    let stream = gunzip.stdout.take().expect("gunzip stdout");
    psql.stdin(Stdio::from(stream));
    let mut child = psql.spawn()?;
    let status = child.wait()?;
    let _ = gunzip.wait()?;
    if !status.success() {
        let mut message = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            let _ = stderr.read_to_string(&mut message);
        }
        return Err(OpsError::Restore(message.trim().to_string()));
    }

    // A restore that produced no tables is a failed restore, not an empty database.
    let check = Command::new("psql")
        .args(["-tA", "-d", SCRATCH_DB, "-c"])
        .arg("SELECT count(*) FROM information_schema.tables WHERE table_schema = 'public'")
        .output()?;
    let tables: i64 = String::from_utf8_lossy(&check.stdout)
        .trim()
        .parse()
        .unwrap_or(0);
    if tables == 0 {
        return Err(OpsError::Restore("restore produced no tables".to_string()));
    }
    Ok(())
}

fn encrypt_file(archive: &Path, recipients: &[String]) -> Result<PathBuf, OpsError> {
    let target = PathBuf::from(format!("{}.gpg", archive.display()));
    let mut command = Command::new("gpg");
    command.args(["--batch", "--yes", "--trust-model", "always"]);
    for recipient in recipients {
        command.arg("-r").arg(recipient);
    }
    let status = command
        .arg("--output")
        .arg(&target)
        .arg("--encrypt")
        .arg(archive)
        .status()?;
    if !status.success() {
        return Err(OpsError::Encrypt(format!(
            "gpg exited with {status}; are the keys {recipients:?} present?"
        )));
    }
    Ok(target)
}

async fn circuit_counts(db: &Db, agent_id: &str) -> Result<Value, OpsError> {
    let client = db.client().await?;
    let mut counts = serde_json::Map::new();
    for table in [
        "memories",
        "self_reflections",
        "causal_lessons",
        "mind_events",
    ] {
        let sql = format!("SELECT count(*) AS n FROM {table} WHERE agent_id = $1");
        let row = client.query_one(&sql, &[&agent_id]).await?;
        counts.insert(table.to_string(), json!(row.get::<_, i64>("n")));
    }
    Ok(Value::Object(counts))
}

async fn schema_revision(db: &Db) -> Option<String> {
    let client = db.client().await.ok()?;
    let row = client
        .query_opt("SELECT version_num FROM alembic_version LIMIT 1", &[])
        .await
        .ok()
        .flatten()?;
    Some(row.get::<_, String>("version_num"))
}

async fn digest(db: &Db, agent_id: &str) -> Result<String, OpsError> {
    let client = db.client().await?;
    let rows = client
        .query(
            "SELECT id::text AS id FROM memories WHERE agent_id = $1
             ORDER BY created_at DESC LIMIT 50",
            &[&agent_id],
        )
        .await?;
    let mut ids: Vec<String> = rows.iter().map(|row| row.get("id")).collect();
    ids.sort();
    let mut hasher = Sha256::new();
    for id in ids {
        hasher.update(id.as_bytes());
    }
    Ok(format!("{:x}", hasher.finalize()))
}

async fn record_run(db: &Db, tool: &str, status: &str, detail: &Value) -> Result<(), OpsError> {
    let client = db.client().await?;
    client
        .execute(
            "INSERT INTO phylactery_runs (tool, finished_at, status, detail)
             VALUES ($1, now(), $2, $3)",
            &[&tool, &status, detail],
        )
        .await?;
    Ok(())
}

/// Create a backup: dump, checksum, optional verification in a scratch database, encryption.
pub async fn backup(
    db: &Db,
    config: &Config,
    agent_id: &str,
    verify: bool,
    encrypt: bool,
    plain: bool,
    confirm: bool,
) -> Result<Value, OpsError> {
    if plain && !confirm {
        return Err(OpsError::PlainNeedsConfirm);
    }
    std::fs::create_dir_all(&config.backup_dir)?;
    let now = Utc::now();
    let name = backup_name(now);
    let archive_path = config.backup_dir.join(&name);

    // A full logical dump needs read access to every table, including the ones the memory role
    // is not granted. The admin DSN is the operator's key to the whole database; without it the
    // dump would silently miss objects, so it is required rather than approximated.
    let dsn = config.admin_dsn.clone().ok_or(OpsError::NoAdminDsn)?;
    let target = archive_path.clone();
    tokio::task::spawn_blocking(move || dump_to_gzip(&dsn, &target))
        .await
        .map_err(|error| OpsError::Dump(error.to_string()))??;

    let checksum = sha256_file(&archive_path)?;
    let counts = circuit_counts(db, agent_id).await?;
    let revision = schema_revision(db).await;

    let mut verified = false;
    let mut verify_note = "skipped".to_string();
    if verify {
        match config.admin_dsn.clone() {
            None => verify_note = "skipped: no admin dsn".to_string(),
            Some(admin) => {
                let archive = archive_path.clone();
                let admin_clone = admin.clone();
                let prepared = tokio::task::spawn_blocking(move || {
                    prepare_scratch(&admin_clone)
                        .and_then(|_| restore_into_scratch(&admin_clone, &archive))
                })
                .await
                .map_err(|error| OpsError::Restore(error.to_string()))?;
                match prepared {
                    Ok(()) => {
                        verified = true;
                        verify_note = "restored into phylactery_verify".to_string();
                    }
                    Err(error) => {
                        verify_note = format!("failed: {error}");
                    }
                }
            }
        }
    }

    let mut final_path = archive_path.clone();
    let mut key_fingerprint = None;
    if encrypt && !plain {
        let archive = archive_path.clone();
        let recipients = config.backup_keys.clone();
        let encrypted = tokio::task::spawn_blocking(move || encrypt_file(&archive, &recipients))
            .await
            .map_err(|error| OpsError::Encrypt(error.to_string()))??;
        std::fs::remove_file(&archive_path)?;
        final_path = encrypted;
        key_fingerprint = Some(config.backup_keys.join(","));
    } else if plain {
        // Break-glass copy: readable without any key. It is written only on explicit confirm and
        // is meant to be moved to offline media and deleted from the machine.
        let break_glass = config
            .backup_dir
            .join(format!("{}-PLAIN.sql.gz", now.format("%Y-%m-%d-%H%M")));
        std::fs::rename(&archive_path, &break_glass)?;
        final_path = break_glass;
    }

    let bytes = std::fs::metadata(&final_path)?.len() as i64;
    let client = db.client().await?;
    let row = client
        .query_one(
            "INSERT INTO phylactery_backups
                 (taken_at, path, bytes, sha256, schema_revision, counts, verified, verified_at, key_fingerprint, agent_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, CASE WHEN $7 THEN now() ELSE NULL END, $8, $9)
             RETURNING id::text AS id",
            &[
                &now,
                &final_path.display().to_string(),
                &bytes,
                &checksum,
                &revision,
                &counts,
                &verified,
                &key_fingerprint,
                &agent_id,
            ],
        )
        .await?;
    let id: String = row.get("id");

    let report = json!({
        "id": id,
        "path": final_path.display().to_string(),
        "bytes": bytes,
        "sha256": checksum,
        "counts": counts,
        "schema_revision": revision,
        "verified": verified,
        "verify_note": verify_note,
        "encrypted": encrypt,
        "key": key_fingerprint,
    });
    record_run(db, "backup", "ok", &report).await?;
    Ok(report)
}

fn psql_exec(admin_dsn: &str, sql: &str) -> Result<(), OpsError> {
    let parts = dsn_parts(admin_dsn)?;
    let mut psql = Command::new("psql");
    psql.arg("-q").arg("-v").arg("ON_ERROR_STOP=1");
    if let Some(host) = &parts.host {
        psql.arg("-h").arg(host);
    }
    if let Some(port) = parts.port {
        psql.arg("-p").arg(port.to_string());
    }
    if let Some(user) = &parts.user {
        psql.arg("-U").arg(user);
    }
    psql.arg("-d").arg("postgres");
    if let Some(password) = &parts.password {
        psql.env("PGPASSWORD", password);
    }
    psql.arg("-c").arg(sql);
    let status = psql.stdout(Stdio::null()).stderr(Stdio::piped()).status()?;
    if !status.success() {
        return Err(OpsError::Restore(format!("psql failed: {sql}")));
    }
    Ok(())
}

fn prepare_scratch(admin_dsn: &str) -> Result<(), OpsError> {
    // DROP/CREATE DATABASE cannot share a transaction block, so they are separate calls.
    psql_exec(
        admin_dsn,
        &format!("DROP DATABASE IF EXISTS {SCRATCH_DB} WITH (FORCE)"),
    )?;
    psql_exec(admin_dsn, &format!("CREATE DATABASE {SCRATCH_DB}"))
}

/// Restore the newest (or named) backup into the scratch database and compare counts.
pub async fn verify(db: &Db, config: &Config, path: Option<String>) -> Result<Value, OpsError> {
    let admin = config.admin_dsn.clone().ok_or(OpsError::NoAdminDsn)?;
    let client = db.client().await?;
    let row =
        match path {
            Some(path) => client
                .query_opt(
                    "SELECT id::text AS id, path, counts, sha256, agent_id FROM phylactery_backups
                 WHERE path = $1 ORDER BY taken_at DESC LIMIT 1",
                    &[&path],
                )
                .await?,
            None => client
                .query_opt(
                    "SELECT id::text AS id, path, counts, sha256, agent_id FROM phylactery_backups
                     ORDER BY taken_at DESC LIMIT 1",
                    &[],
                )
                .await?,
        }
        .ok_or(OpsError::NoBackup)?;

    let id: String = row.get("id");
    let archive: String = row.get("path");
    let expected: Value = row.get("counts");
    let agent_id: String = row
        .get::<_, Option<String>>("agent_id")
        .unwrap_or_else(|| config.agent_id.clone());
    let archive_path = PathBuf::from(&archive);
    if !archive_path.exists() {
        return Err(OpsError::NotFound(archive));
    }

    let admin_clone = admin.clone();
    let archive_clone = archive_path.clone();
    tokio::task::spawn_blocking(move || {
        prepare_scratch(&admin_clone)
            .and_then(|_| restore_into_scratch(&admin_clone, &archive_clone))
    })
    .await
    .map_err(|error| OpsError::Restore(error.to_string()))??;

    let scratch_dsn = scratch_dsn(&admin);
    let (scratch_config, connection) = tokio_postgres::connect(&scratch_dsn, tokio_postgres::NoTls)
        .await
        .map_err(|error| OpsError::Restore(error.to_string()))?;
    tokio::spawn(async move {
        let _ = connection.await;
    });

    let mut actual = serde_json::Map::new();
    let mut drift = Vec::new();
    for table in [
        "memories",
        "self_reflections",
        "causal_lessons",
        "mind_events",
    ] {
        let sql = format!("SELECT count(*) AS n FROM {table} WHERE agent_id = $1");
        let count: i64 = scratch_config
            .query_one(&sql, &[&agent_id])
            .await
            .map(|row| row.get("n"))
            .unwrap_or(-1);
        actual.insert(table.to_string(), json!(count));
        let expected_count = expected[table].as_i64().unwrap_or(-1);
        if expected_count != count {
            drift.push(json!({
                "table": table,
                "expected": expected_count,
                "actual": count,
            }));
        }
    }

    client
        .execute(
            "UPDATE phylactery_backups SET verified = $2, verified_at = now() WHERE id = $1::text::uuid",
            &[&id, &drift.is_empty()],
        )
        .await?;

    let report = json!({
        "id": id,
        "path": archive,
        "verified": drift.is_empty(),
        "expected": expected,
        "actual": actual,
        "drift": drift,
    });
    record_run(
        db,
        "verify",
        if drift.is_empty() { "ok" } else { "drift" },
        &report,
    )
    .await?;
    Ok(report)
}

fn scratch_dsn(admin_dsn: &str) -> String {
    let parts = match dsn_parts(admin_dsn) {
        Ok(parts) => parts,
        Err(_) => return admin_dsn.to_string(),
    };
    let mut dsn = String::from("postgresql://");
    if let Some(user) = &parts.user {
        dsn.push_str(user);
        if let Some(password) = &parts.password {
            dsn.push(':');
            dsn.push_str(password);
        }
        dsn.push('@');
    }
    dsn.push_str(parts.host.as_deref().unwrap_or("localhost"));
    if let Some(port) = parts.port {
        dsn.push(':');
        dsn.push_str(&port.to_string());
    }
    dsn.push('/');
    dsn.push_str(SCRATCH_DB);
    dsn
}

/// Current state versus the newest backup: counts, digest, revision.
pub async fn manifest(db: &Db, agent_id: &str) -> Result<Value, OpsError> {
    let counts = circuit_counts(db, agent_id).await?;
    let revision = schema_revision(db).await;
    let current_digest = digest(db, agent_id).await?;

    let client = db.client().await?;
    let last = client
        .query_opt(
            "SELECT id::text AS id, taken_at, path, counts, schema_revision, verified
             FROM phylactery_backups ORDER BY taken_at DESC LIMIT 1",
            &[],
        )
        .await?;

    let (last_backup, drift) = match last {
        None => (Value::Null, vec![json!({"kind": "no_backup"})]),
        Some(row) => {
            let stored: Value = row.get("counts");
            let mut drift = Vec::new();
            for (table, value) in counts.as_object().into_iter().flatten() {
                let expected = stored[table].as_i64().unwrap_or(-1);
                let actual = value.as_i64().unwrap_or(-1);
                if expected != actual {
                    drift.push(json!({
                        "kind": "count",
                        "table": table,
                        "backup": expected,
                        "current": actual,
                    }));
                }
            }
            (
                json!({
                    "id": row.get::<_, String>("id"),
                    "taken_at": row.get::<_, DateTime<Utc>>("taken_at"),
                    "path": row.get::<_, String>("path"),
                    "verified": row.get::<_, bool>("verified"),
                    "schema_revision": row.get::<_, Option<String>>("schema_revision"),
                }),
                drift,
            )
        }
    };

    let report = json!({
        "counts": counts,
        "digest": current_digest,
        "schema_revision": revision,
        "last_backup": last_backup,
        "drift": drift,
        "in_sync": drift.is_empty(),
    });
    record_run(
        db,
        "manifest",
        if drift.is_empty() { "ok" } else { "drift" },
        &report,
    )
    .await?;
    Ok(report)
}

/// Apply `migrations/*.sql` in order. Idempotent by construction.
///
/// DDL needs the admin role: the memory role is deliberately not allowed to create schemas,
/// triggers or constraints. Without an admin DSN the tool refuses instead of half-applying.
pub async fn migrate(db: &Db, config: &Config) -> Result<Value, OpsError> {
    let admin = config.admin_dsn.clone().ok_or(OpsError::NoAdminDsn)?;
    let mut files: Vec<PathBuf> = std::fs::read_dir(&config.migrations_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("sql"))
        .collect();
    files.sort();

    let (client, connection) = tokio_postgres::connect(&admin, tokio_postgres::NoTls)
        .await
        .map_err(|error| OpsError::Restore(format!("admin connect failed: {error}")))?;
    tokio::spawn(async move {
        let _ = connection.await;
    });

    let mut applied = Vec::new();
    for file in &files {
        let sql = std::fs::read_to_string(file)?;
        client.batch_execute(&sql).await?;
        applied.push(
            file.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_string(),
        );
    }
    let revision = schema_revision(db).await;
    let report = json!({
        "applied": applied,
        "schema_revision": revision,
        "directory": config.migrations_dir.display().to_string(),
    });
    record_run(db, "migrate", "ok", &report).await?;
    Ok(report)
}

/// Maintenance: safe actions always, destructive ones only with `confirm`.
pub async fn maintain(
    db: &Db,
    config: &Config,
    agent_id: &str,
    confirm: bool,
) -> Result<Value, OpsError> {
    let client = db.client().await?;
    let mut actions: Vec<Value> = Vec::new();

    for table in [
        "memories",
        "self_reflections",
        "causal_lessons",
        "mind_events",
        "entities",
        "knowledge_graph",
    ] {
        let sql = format!("VACUUM (ANALYZE) {table}");
        client.batch_execute(&sql).await?;
        actions.push(json!({"action": "vacuum_analyze", "table": table}));
    }

    let orphans: i64 = client
        .query_one(
            "SELECT count(*) AS n FROM entities e
             WHERE e.agent_id = $1 AND e.merged_into IS NULL
               AND NOT EXISTS (SELECT 1 FROM knowledge_graph k
                               WHERE k.subject_id = e.id OR k.object_id = e.id)",
            &[&agent_id],
        )
        .await?
        .get("n");
    let conflicts: i64 = client
        .query_one(
            "SELECT count(*) AS n FROM (
                 SELECT subject, predicate FROM knowledge_graph
                 WHERE agent_id = $1 AND valid_to IS NULL
                 GROUP BY subject, predicate
                 HAVING count(DISTINCT object_norm) > 1
             ) AS c",
            &[&agent_id],
        )
        .await?
        .get("n");
    let nulls: i64 = client
        .query_one(
            "SELECT count(*) AS n FROM memories WHERE agent_id = $1 AND embedding IS NULL",
            &[&agent_id],
        )
        .await?
        .get("n");

    let mut files: Vec<PathBuf> = std::fs::read_dir(&config.backup_dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .map(|name| name.starts_with("phylactery-") && name.ends_with(".gpg"))
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    let prune = retention_prune(&files, Utc::now());

    let mut pruned: Vec<String> = Vec::new();
    if confirm {
        for file in &prune {
            if std::fs::remove_file(file).is_ok() {
                pruned.push(file.display().to_string());
            }
        }
    }

    let report = json!({
        "actions": actions,
        "orphan_entities": orphans,
        "conflicts": conflicts,
        "memories_without_vector": nulls,
        "backups_total": files.len(),
        "prune_candidates": prune.iter().map(|path| path.display().to_string()).collect::<Vec<_>>(),
        "pruned": pruned,
        "confirm": confirm,
    });
    record_run(db, "maintain", "ok", &report).await?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_names_sort_chronologically() {
        let early = backup_name(
            DateTime::parse_from_rfc3339("2026-11-13T09:05:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        let late = backup_name(
            DateTime::parse_from_rfc3339("2026-11-13T17:53:00Z")
                .unwrap()
                .with_timezone(&Utc),
        );
        assert_eq!(early, "phylactery-2026-11-13-0905.sql.gz");
        assert!(early < late);
    }

    #[test]
    fn retention_keeps_the_newest_seven() {
        let files: Vec<PathBuf> = (1..=10)
            .map(|day| PathBuf::from(format!("phylactery-2026-11-{day:02}-1200.sql.gz.gpg")))
            .collect();
        let pruned = retention_prune(&files, Utc::now());
        assert_eq!(pruned.len(), 3, "expected three pruned: {pruned:?}");
        for file in &pruned {
            let name = file.file_name().unwrap().to_str().unwrap();
            assert!(
                name.contains("2026-11-01")
                    || name.contains("2026-11-02")
                    || name.contains("2026-11-03")
            );
        }
    }

    #[test]
    fn retention_never_prunes_a_short_history() {
        let files: Vec<PathBuf> = (1..=5)
            .map(|day| PathBuf::from(format!("phylactery-2026-11-{day:02}-1200.sql.gz.gpg")))
            .collect();
        assert!(retention_prune(&files, Utc::now()).is_empty());
    }
}
