//! Integration tests for the knowledge graph: extraction, resolution, temporal supersede,
//! traversal and conflicts. Disposable database only (`PHYLACTERY_TEST_DSN`).

use std::path::PathBuf;

use phylactery_mcp::config::{Config, DEFAULT_AGENT_ID, DEFAULT_EMBED_MODEL, DEFAULT_OLLAMA_URL};
use phylactery_mcp::db::Db;
use phylactery_mcp::embed::Embedder;
use phylactery_mcp::kg;
use uuid::Uuid;

fn context() -> Option<(Db, Embedder, String)> {
    let dsn = std::env::var("PHYLACTERY_TEST_DSN").ok()?;
    let config = Config {
        dsn,
        dsn_source: "test".to_string(),
        ollama_url: std::env::var("PHYLACTERY_OLLAMA_URL")
            .unwrap_or_else(|_| DEFAULT_OLLAMA_URL.to_string()),
        ollama_source: "test".to_string(),
        embed_api: Default::default(),
        embed_url: "http://127.0.0.1:11434".to_string(),
        embed_source: "test".to_string(),
        embed_api_key: None,
        embed_model: DEFAULT_EMBED_MODEL.to_string(),
        agent_id: DEFAULT_AGENT_ID.to_string(),
        pool_max: 4,
        statement_timeout_ms: 10_000,
        registry_path: PathBuf::from("/nonexistent/settings.json"),
        backup_dir: PathBuf::from("/tmp/phylactery-backups"),
        migrations_dir: PathBuf::from("/tmp/phylactery-migrations"),
        admin_dsn: None,
        backup_keys: vec!["phylactery-auto".to_string()],
        kg_since: chrono::DateTime::parse_from_rfc3339("2026-11-14T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
    };
    let db = Db::new(&config).expect("pool");
    let embed = Embedder::new(&config).expect("embedder");
    let agent = format!("mcp_kg_{}", Uuid::new_v4().simple());
    Some((db, embed, agent))
}

async fn cleanup(db: &Db, agent: &str) {
    if let Ok(client) = db.client().await {
        for sql in [
            "DELETE FROM knowledge_graph WHERE agent_id = $1",
            "DELETE FROM entities WHERE agent_id = $1",
            "DELETE FROM memories WHERE agent_id = $1",
        ] {
            let _ = client.execute(sql, &[&agent]).await;
        }
    }
}

#[tokio::test]
async fn remember_writes_memory_and_graph_together() {
    let Some((db, embed, agent)) = context() else {
        eprintln!("skipped: PHYLACTERY_TEST_DSN is not set");
        return;
    };

    let report = kg::remember(
        &db,
        &embed,
        &agent,
        "Phylactery runs on Postgres",
        "semantic",
        0.7,
        "test",
    )
    .await
    .expect("remember");

    assert_eq!(report.triples.len(), 1, "expected one triple: {report:?}");
    assert_eq!(report.triples[0].predicate, "runs_on");
    assert!(report.entities.contains(&"Phylactery".to_string()));

    let profile = kg::entity_profile(&db, &agent, "Phylactery", 20)
        .await
        .expect("profile");
    let edges = profile["edges_out"].as_array().expect("edges");
    assert!(
        edges
            .iter()
            .any(|edge| edge["predicate"] == "runs_on" && profile["edges_out"].is_array()),
        "edge missing: {profile}"
    );

    let paths = kg::related(&db, &agent, "Phylactery", 2, &[], 20)
        .await
        .expect("related");
    assert!(
        !paths["edges"].as_array().expect("edges").is_empty(),
        "traversal returned nothing: {paths}"
    );

    cleanup(&db, &agent).await;
}

#[tokio::test]
async fn a_correction_closes_the_old_edge_instead_of_duplicating_it() {
    let Some((db, embed, agent)) = context() else {
        eprintln!("skipped: PHYLACTERY_TEST_DSN is not set");
        return;
    };

    kg::remember(
        &db,
        &embed,
        &agent,
        "Phylactery uses port 5432",
        "semantic",
        0.6,
        "test",
    )
    .await
    .expect("first");
    kg::remember(
        &db,
        &embed,
        &agent,
        "Phylactery uses port 5433",
        "semantic",
        0.6,
        "test",
    )
    .await
    .expect("second");

    let client = db.client().await.expect("client");
    let row = client
        .query_one(
            "SELECT count(*) FILTER (WHERE valid_to IS NULL) AS live,
                    count(*) FILTER (WHERE valid_to IS NOT NULL) AS closed
             FROM knowledge_graph
             WHERE agent_id = $1 AND predicate = 'uses'",
            &[&agent],
        )
        .await
        .expect("count");
    let live: i64 = row.get("live");
    let closed: i64 = row.get("closed");
    assert_eq!(live, 1, "exactly one live edge expected");
    assert_eq!(closed, 1, "the superseded edge must stay as history");

    let conflicts = kg::conflicts(&db, &agent, 10).await.expect("conflicts");
    assert_eq!(conflicts["count"], 0, "supersede must not leave a conflict");

    cleanup(&db, &agent).await;
}

#[tokio::test]
async fn unknown_entity_is_created_with_a_kind() {
    let Some((db, embed, agent)) = context() else {
        eprintln!("skipped: PHYLACTERY_TEST_DSN is not set");
        return;
    };

    kg::remember(
        &db,
        &embed,
        &agent,
        "Keter is a project",
        "semantic",
        0.5,
        "test",
    )
    .await
    .expect("remember");

    let client = db.client().await.expect("client");
    let row = client
        .query_one(
            "SELECT kind FROM entities WHERE agent_id = $1 AND norm = 'keter'",
            &[&agent],
        )
        .await
        .expect("entity row");
    let kind: String = row.get("kind");
    assert_eq!(kind, "concept");

    let object = client
        .query_one(
            "SELECT kind FROM entities WHERE agent_id = $1 AND norm = 'project'",
            &[&agent],
        )
        .await
        .expect("object row");
    let object_kind: String = object.get("kind");
    assert_eq!(object_kind, "project", "kind hint from `is a project`");

    cleanup(&db, &agent).await;
}

#[tokio::test]
async fn traversal_respects_depth_and_cycles() {
    let Some((db, embed, agent)) = context() else {
        eprintln!("skipped: PHYLACTERY_TEST_DSN is not set");
        return;
    };

    for sentence in ["Alpha uses Beta", "Beta uses Gamma", "Gamma uses Alpha"] {
        kg::remember(&db, &embed, &agent, sentence, "semantic", 0.5, "test")
            .await
            .expect("remember");
    }

    let one = kg::related(&db, &agent, "Alpha", 1, &[], 50)
        .await
        .expect("depth 1");
    assert_eq!(
        one["edges"].as_array().expect("edges").len(),
        2,
        "traversal is undirected: Alpha→Beta and Gamma→Alpha"
    );

    let three = kg::related(&db, &agent, "Alpha", 3, &[], 50)
        .await
        .expect("depth 3");
    let edges = three["edges"].as_array().expect("edges");
    assert!(edges.len() >= 2, "expected a path, got {three}");
    assert!(edges.len() <= 50, "edge cap must hold");

    cleanup(&db, &agent).await;
}
