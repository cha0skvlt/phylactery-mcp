//! Integration tests for `ask`: one call, one answer pack.
//!
//! Disposable database only (`PHYLACTERY_TEST_DSN`).

use std::path::PathBuf;

use phylactery_mcp::ask;
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
    let agent = format!("mcp_ask_{}", Uuid::new_v4().simple());
    Some((db, embed, agent))
}

async fn cleanup(db: &Db, agent: &str) {
    if let Ok(client) = db.client().await {
        for sql in [
            "DELETE FROM knowledge_graph WHERE agent_id = $1",
            "DELETE FROM entities WHERE agent_id = $1",
            "DELETE FROM memories WHERE agent_id = $1",
            "DELETE FROM mind_events WHERE agent_id = $1",
        ] {
            let _ = client.execute(sql, &[&agent]).await;
        }
    }
}

#[tokio::test]
async fn ask_returns_facts_relations_and_citations_in_one_call() {
    let Some((db, embed, agent)) = context() else {
        eprintln!("skipped: PHYLACTERY_TEST_DSN is not set");
        return;
    };

    kg::remember(
        &db,
        &embed,
        &agent,
        "Phylactery runs on Postgres",
        "semantic",
        0.8,
        "test",
    )
    .await
    .expect("remember");

    let answer = ask::ask(
        &db,
        &embed,
        &agent,
        "what does Phylactery run on",
        1200,
        false,
    )
    .await
    .expect("ask");

    assert!(
        !answer["facts"].as_array().expect("facts").is_empty(),
        "no facts: {answer}"
    );
    assert!(
        !answer["relations"]
            .as_array()
            .expect("relations")
            .is_empty(),
        "no relations: {answer}"
    );
    assert!(
        !answer["citations"]
            .as_array()
            .expect("citations")
            .is_empty(),
        "no citations: {answer}"
    );
    assert_eq!(answer["truncated"], false);
    let entities = answer["entities"].as_array().expect("entities");
    assert!(
        entities.iter().any(|entity| entity["name"] == "Phylactery"),
        "entity not detected: {answer}"
    );

    cleanup(&db, &agent).await;
}

#[tokio::test]
async fn ask_respects_the_budget_and_reports_truncation() {
    let Some((db, embed, agent)) = context() else {
        eprintln!("skipped: PHYLACTERY_TEST_DSN is not set");
        return;
    };

    for index in 0..8 {
        kg::remember(
            &db,
            &embed,
            &agent,
            &format!("Phylactery uses module{index} for storage"),
            "semantic",
            0.5,
            "test",
        )
        .await
        .expect("remember");
    }

    let answer = ask::ask(
        &db,
        &embed,
        &agent,
        "Phylactery storage modules",
        100,
        false,
    )
    .await
    .expect("ask");

    let used = answer["used_tokens"].as_u64().expect("used_tokens");
    let budget = answer["budget_tokens"].as_u64().expect("budget_tokens");
    assert!(used <= budget, "budget exceeded: {used} > {budget}");
    assert_eq!(
        answer["truncated"], true,
        "a 100-token budget must truncate: {answer}"
    );

    cleanup(&db, &agent).await;
}
