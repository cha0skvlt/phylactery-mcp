//! phylactery-mcp — MCP stdio server over one vector circuit.
//!
//! Four tables, one 768d space: `memories`, `self_reflections`, `causal_lessons`, `mind_events`.
//! This binary exposes the circuit to any agent as `mcp:phylactery:<tool>` in CHA0S AOE, and to any
//! MCP client over stdio.
//!
//! Run: `phylactery-mcp` (no arguments, no working directory dependency).
//! Configuration: `~/.config/cha0s/settings.json` (`agent.databases`, `language_models.ollama`)
//! with `PHYLACTERY_*` environment overrides.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::Parameters;
use rmcp::model::{CallToolResult, Content, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::json;

use phylactery_mcp::circuit::{self, Circuit};
use phylactery_mcp::config::Config;
use phylactery_mcp::db::Db;
use phylactery_mcp::embed::Embedder;
use phylactery_mcp::{ask, backup, browse, kg, math, mutate, ops, surgical, write};

const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RECALL_LIMIT: usize = 50;
const MAX_SEARCH_LIMIT: usize = 50;

struct App {
    config: Config,
    db: Db,
    embed: Embedder,
}

impl App {
    fn new() -> anyhow::Result<Self> {
        let config = Config::load()?;
        let db = Db::new(&config)?;
        let embed = Embedder::new(&config)?;
        Ok(Self { config, db, embed })
    }
}

#[derive(Clone)]
struct PhylacteryServer {
    app: Arc<App>,
    tool_router: ToolRouter<Self>,
}

/// An execution failure the model must see: MCP wants it inside the result (`isError`),
/// where the text survives, not as a JSON-RPC error a client may swallow.
fn failure(code: &str, message: impl Into<String>) -> CallToolResult {
    let message = message.into();
    let body = serde_json::to_string_pretty(&json!({ "code": code, "message": message }))
        .unwrap_or_else(|_| message.clone());
    CallToolResult::error(vec![Content::text(body)])
}

fn success_json(value: &serde_json::Value) -> Result<CallToolResult, McpError> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|error| McpError::internal_error(error.to_string(), None))?;
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

fn bounded(limit: usize, ceiling: usize) -> usize {
    limit.clamp(1, ceiling)
}

fn parse_sources(raw: &[String]) -> Result<Vec<Circuit>, String> {
    if raw.is_empty() {
        return Ok(Circuit::ALL.to_vec());
    }
    let mut out = Vec::with_capacity(raw.len());
    for name in raw {
        match Circuit::parse(name) {
            Some(circuit) => {
                if !out.contains(&circuit) {
                    out.push(circuit);
                }
            }
            None => {
                return Err(format!(
                    "unknown source `{name}`; expected memories, diary, lessons or mind"
                ));
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RecallArgs {
    /// Semantic query: what to raise from memory.
    query: String,
    /// Maximum semantic hits across all circuits (capped at 50).
    #[serde(default = "default_recall_limit")]
    limit: usize,
    /// How many newest mind_events to append as a timeline (0 disables).
    #[serde(default = "default_timeline")]
    timeline: usize,
    /// Optional circuit filter: memories, diary, lessons, mind.
    #[serde(default)]
    sources: Vec<String>,
}

fn default_recall_limit() -> usize {
    8
}

fn default_timeline() -> usize {
    5
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SearchArgs {
    /// Semantic query text.
    query: String,
    /// Maximum hits (capped at 50).
    #[serde(default = "default_search_limit")]
    limit: usize,
    /// Drop candidates below this cosine similarity (floor 0.3).
    #[serde(default = "default_similarity_floor")]
    min_similarity: f64,
    /// Optional circuit filter: memories, diary, lessons, mind.
    #[serde(default)]
    sources: Vec<String>,
}

fn default_search_limit() -> usize {
    5
}

fn default_similarity_floor() -> f64 {
    math::SIMILARITY_FLOOR
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SaveArgs {
    /// Fact text to store. A [YYYY-MM-DD] prefix is added when absent.
    text: String,
    /// memory_type: semantic (default), identity, unverified, or a domain type.
    #[serde(default = "default_memory_type")]
    memory_type: String,
    /// Importance on the stored scale (0..1, or 0..10 where 9+ is a standing order).
    #[serde(default = "default_importance")]
    importance: f64,
    /// Provenance: tool, user, web, reflect.
    #[serde(default = "default_source")]
    source: String,
}

fn default_memory_type() -> String {
    "semantic".to_string()
}

fn default_importance() -> f64 {
    0.5
}

fn default_source() -> String {
    "tool".to_string()
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct LessonArgs {
    /// What stopped: the change in behaviour.
    change: String,
    /// Why: the causal reason.
    because: String,
    /// Ids of records that support the lesson.
    #[serde(default)]
    evidence_ids: Vec<String>,
    /// Importance on the stored scale; 9+ is a standing order.
    #[serde(default = "default_importance")]
    importance: f64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct DiaryArgs {
    /// Diary text: the self speaking about itself.
    text: String,
    /// Importance on the stored scale.
    #[serde(default = "default_importance")]
    importance: f64,
    /// Provenance of the entry.
    #[serde(default = "default_reflect_source")]
    source: String,
}

fn default_reflect_source() -> String {
    "reflect".to_string()
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MindLogArgs {
    /// Event content.
    content: String,
    /// Event kind: agent_out, agent_in, tool, system.
    #[serde(default = "default_kind")]
    kind: String,
    /// Channel: ide, cloud, cli.
    #[serde(default = "default_channel")]
    channel: String,
    /// Optional thread id to group events.
    #[serde(default)]
    thread_id: Option<String>,
}

fn default_kind() -> String {
    "agent_out".to_string()
}

fn default_channel() -> String {
    "ide".to_string()
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MindTailArgs {
    /// How many newest events to return (capped at 200).
    #[serde(default = "default_tail_limit")]
    limit: usize,
    /// Optional kind filter: agent_out, agent_in, tool, system.
    #[serde(default)]
    kind: Option<String>,
    /// Optional RFC 3339 timestamp: only events newer than this.
    #[serde(default)]
    since: Option<String>,
}

fn default_tail_limit() -> usize {
    12
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GetArgs {
    /// Row id: a full UUID or a unique hex prefix of at least 4 characters.
    id: String,
    /// Circuit: memories, diary, lessons or mind. Omitted: all four are searched.
    #[serde(default)]
    table: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReembedArgs {
    /// Target embedding model. Default: the configured one.
    #[serde(default)]
    model: Option<String>,
    /// Circuit: memories, diary, lessons or mind. Omitted: all four.
    #[serde(default)]
    circuit: Option<String>,
    /// Rows per call (1..=256).
    #[serde(default = "default_reembed_batch")]
    #[schemars(default = "default_reembed_batch")]
    batch: i64,
    /// Report stale rows without writing.
    #[serde(default = "default_true")]
    #[schemars(default = "default_true")]
    dry_run: bool,
    /// Allow a dimension other than 768.
    #[serde(default)]
    force: bool,
}
fn default_reembed_batch() -> i64 {
    32
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct HistoryArgs {
    /// Full UUID of the row, including rows that have since been deleted.
    id: String,
    /// Circuit: memories, diary, lessons or mind.
    table: String,
    /// Number of newest revisions to return (1..=100).
    #[serde(default = "default_history_limit")]
    #[schemars(default = "default_history_limit")]
    limit: i64,
}
fn default_history_limit() -> i64 {
    20
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PatchArgs {
    /// Full UUID of the row.
    id: String,
    /// Circuit: memories, diary or lessons. mind is append-only.
    table: String,
    #[serde(default)]
    importance: Option<f64>,
    #[serde(default)]
    memory_type: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    episode_id: Option<String>,
    /// Object merged key-by-key into memories.metadata.
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AdoptArgs {
    from_agent: String,
    to_agent: String,
    #[serde(default)]
    ids: Option<Vec<String>>,
    #[serde(default)]
    filter: Option<surgical::AdoptFilter>,
    #[serde(default = "default_true")]
    #[schemars(default = "default_true")]
    dry_run: bool,
    #[serde(default)]
    confirm: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MergeArgs {
    keep_id: String,
    drop_id: String,
    table: String,
    #[serde(default = "default_merge_strategy")]
    #[schemars(default = "default_merge_strategy")]
    strategy: String,
    #[serde(default = "default_true")]
    #[schemars(default = "default_true")]
    dry_run: bool,
    #[serde(default)]
    confirm: bool,
}
fn default_merge_strategy() -> String {
    "keep_text".to_string()
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ForgetArgs {
    /// Row id: a full UUID (a prefix is not accepted for deletion).
    id: String,
    /// Circuit: memories, diary, lessons or mind. Omitted: the id is looked up in the circuits.
    #[serde(default)]
    table: Option<String>,
    /// Namespace of the row. Default: the server's own. A foreign namespace needs confirm=true.
    #[serde(default)]
    agent_id: Option<String>,
    /// Report what would be deleted without deleting it.
    #[serde(default)]
    dry_run: bool,
    /// Required for a foreign namespace and for force.
    #[serde(default)]
    confirm: bool,
    /// Lift the protections (identity, seed, standing order, mind append-only). Needs confirm=true.
    #[serde(default)]
    force: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReplaceArgs {
    /// Row id.
    id: String,
    /// Circuit: memories or diary.
    table: String,
    /// New text; re-embedded, and the injection guard runs again.
    text: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BackfillArgs {
    /// Circuit: memories, diary, lessons or mind.
    table: String,
    /// Rows per call (capped at 256).
    #[serde(default = "default_batch")]
    batch: i64,
}

fn default_batch() -> i64 {
    32
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BackupArgs {
    /// Verify the dump by restoring it into a scratch database before encrypting.
    #[serde(default = "default_true")]
    verify: bool,
    /// Encrypt the archive with the phylactery keys.
    #[serde(default = "default_true")]
    encrypt: bool,
    /// Write a break-glass copy without encryption. Requires confirm=true.
    #[serde(default)]
    plain: bool,
    /// Explicit consent for a plaintext archive.
    #[serde(default)]
    confirm: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct VerifyArgs {
    /// Archive path; the newest backup is used when omitted.
    #[serde(default)]
    path: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct MaintainArgs {
    /// Execute destructive steps (pruning old archives). Without it the tool only reports.
    #[serde(default)]
    confirm: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RememberArgs {
    /// Fact text: written to memories and indexed in the graph in one act.
    text: String,
    /// memory_type: semantic (default), identity, unverified, or a domain type.
    #[serde(default = "default_memory_type")]
    memory_type: String,
    /// Importance on the stored scale.
    #[serde(default = "default_importance")]
    importance: f64,
    /// Provenance: tool, user, web, reflect.
    #[serde(default = "default_source")]
    source: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct EntityArgs {
    /// Entity name or alias.
    name: String,
    /// Maximum edges and facts per direction.
    #[serde(default = "default_entity_limit")]
    limit: i64,
}

fn default_entity_limit() -> i64 {
    40
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct RelatedArgs {
    /// Entity name or alias.
    name: String,
    /// Traversal depth, capped at 3.
    #[serde(default = "default_depth")]
    depth: i64,
    /// Optional predicate filter.
    #[serde(default)]
    predicates: Vec<String>,
    /// Maximum edges returned.
    #[serde(default = "default_related_limit")]
    limit: i64,
}

fn default_depth() -> i64 {
    2
}

fn default_related_limit() -> i64 {
    60
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ConflictsArgs {
    /// Maximum contradictions returned.
    #[serde(default = "default_conflict_limit")]
    limit: i64,
}

fn default_conflict_limit() -> i64 {
    20
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AskArgs {
    /// The question, in natural language.
    question: String,
    /// Token budget for the answer pack (200..4000).
    #[serde(default = "default_budget")]
    budget_tokens: usize,
    /// Include triples quarantined from the retired extractor.
    #[serde(default)]
    include_legacy: bool,
}

fn default_budget() -> usize {
    1200
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ExtractArgs {
    /// Memories per call (capped at 256).
    #[serde(default = "default_batch")]
    batch: i64,
}

#[tool_router]
impl PhylacteryServer {
    fn new(app: Arc<App>) -> Self {
        Self {
            app,
            tool_router: Self::tool_router(),
        }
    }

    /// Raise a slice: semantic hits across the four circuits plus the freshest timeline.
    #[tool(
        description = "Raise a memory slice. Runs one HNSW pass per circuit (memories, diary, lessons, mind), filters by similarity floor, reranks by importance and trust, and appends the newest mind_events as a timeline. Use this at the start of a significant thread: one call reconstructs context."
    )]
    async fn recall(
        &self,
        Parameters(args): Parameters<RecallArgs>,
    ) -> Result<CallToolResult, McpError> {
        let circuits = match parse_sources(&args.sources) {
            Ok(value) => value,
            Err(message) => return Ok(failure("invalid_source", message)),
        };
        let limit = bounded(args.limit, MAX_RECALL_LIMIT);
        let timeline_limit = args.timeline as i64;

        let started = Instant::now();
        let search = circuit::search(
            &self.app.db,
            &self.app.embed,
            &self.app.config.agent_id,
            &args.query,
            &circuits,
            limit,
            math::SIMILARITY_FLOOR,
        );
        let search = tokio::time::timeout(CALL_TIMEOUT, search).await;
        let hits = match search {
            Err(_) => return Ok(failure("timeout", "recall exceeded the call budget")),
            Ok(Err(error)) => return Ok(failure(error.code(), error.to_string())),
            Ok(Ok(hits)) => hits,
        };

        let timeline = if timeline_limit > 0 {
            let deadline = CALL_TIMEOUT;
            match tokio::time::timeout(
                deadline,
                circuit::timeline(&self.app.db, &self.app.config.agent_id, timeline_limit),
            )
            .await
            {
                Ok(Ok(rows)) => rows,
                Ok(Err(error)) => return Ok(failure("db_unavailable", error.to_string())),
                Err(_) => return Ok(failure("timeout", "timeline exceeded the call budget")),
            }
        } else {
            Vec::new()
        };

        tracing::info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            hits = hits.len(),
            timeline = timeline.len(),
            "recall"
        );
        success_json(&json!({
            "query": args.query,
            "hits": hits,
            "timeline": timeline,
        }))
    }

    /// Pointed semantic search: hits only, no timeline.
    #[tool(
        description = "Semantic search across the memory circuits. Returns hits with source, similarity, importance and final score. Narrow with `sources` and `min_similarity` when a broad recall would flood the context."
    )]
    async fn search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let circuits = match parse_sources(&args.sources) {
            Ok(value) => value,
            Err(message) => return Ok(failure("invalid_source", message)),
        };
        let limit = bounded(args.limit, MAX_SEARCH_LIMIT);
        let started = Instant::now();
        let outcome = tokio::time::timeout(
            CALL_TIMEOUT,
            circuit::search(
                &self.app.db,
                &self.app.embed,
                &self.app.config.agent_id,
                &args.query,
                &circuits,
                limit,
                args.min_similarity,
            ),
        )
        .await;
        let hits = match outcome {
            Err(_) => return Ok(failure("timeout", "search exceeded the call budget")),
            Ok(Err(error)) => return Ok(failure(error.code(), error.to_string())),
            Ok(Ok(hits)) => hits,
        };
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            hits = hits.len(),
            "search"
        );
        success_json(&json!({
            "query": args.query,
            "sources": circuits.iter().map(|circuit| circuit.label()).collect::<Vec<_>>(),
            "hits": hits,
        }))
    }

    /// Circuit census.
    #[tool(
        description = "Circuit census: rows, rows without vectors, newest row and alembic revision for memories, self_reflections, causal_lessons and mind_events. Cheap, read-only, no embedding call."
    )]
    async fn stats(&self) -> Result<CallToolResult, McpError> {
        match ops::stats(&self.app.config, &self.app.db).await {
            Ok(value) => success_json(&value),
            Err(message) => Ok(failure("db_unavailable", message)),
        }
    }

    /// Honest diagnostic.
    #[tool(
        description = "Diagnostic: configuration source, database connection and pool, vector dimension of memories.embedding, circuit contents, HNSW index count, alembic revision and a live Ollama embedding probe. Reports OK/WARN/FAIL per check; never hides a failure."
    )]
    async fn doctor(&self) -> Result<CallToolResult, McpError> {
        let report = ops::doctor(&self.app.config, &self.app.db, &self.app.embed).await;
        success_json(&report)
    }

    // ---- Planned surface: honest stubs, tagged with their milestone ----

    #[tool(
        description = "Write a fact into memories with an embedding, injection guard and cosine dedup. Returns status saved or dedup with the row id. A numeric disagreement with an existing neighbour is treated as a correction, not a duplicate."
    )]
    async fn save(
        &self,
        Parameters(args): Parameters<SaveArgs>,
    ) -> Result<CallToolResult, McpError> {
        let started = Instant::now();
        let call = tokio::time::timeout(
            CALL_TIMEOUT,
            write::save(
                &self.app.db,
                &self.app.embed,
                &self.app.config.agent_id,
                &args.text,
                &args.memory_type,
                args.importance,
                &args.source,
            ),
        )
        .await;
        match call {
            Err(_) => Ok(failure("timeout", "save exceeded the call budget")),
            Ok(Err(error)) => Ok(failure(error.code(), error.to_string())),
            Ok(Ok(outcome)) => {
                tracing::info!(
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    status = outcome.status,
                    "tool.save"
                );
                success_json(&write::outcome_json(&outcome))
            }
        }
    }

    #[tool(
        description = "Write a causal lesson: stopped doing X because Y. Deduplicates on (agent_id, change_text, because_text) and returns the existing id with status dedup, updating importance."
    )]
    async fn lesson(
        &self,
        Parameters(args): Parameters<LessonArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = tokio::time::timeout(
            CALL_TIMEOUT,
            write::lesson(
                &self.app.db,
                &self.app.embed,
                &self.app.config.agent_id,
                &args.change,
                &args.because,
                &args.evidence_ids,
                args.importance,
            ),
        )
        .await;
        match call {
            Err(_) => Ok(failure("timeout", "lesson exceeded the call budget")),
            Ok(Err(error)) => Ok(failure(error.code(), error.to_string())),
            Ok(Ok(outcome)) => success_json(&write::outcome_json(&outcome)),
        }
    }

    #[tool(
        description = "Write a diary entry behind the echo gate: a near-copy of the last 20 entries (cosine above 0.92) is refused as echo, so the diary only records what has not been said yet."
    )]
    async fn diary(
        &self,
        Parameters(args): Parameters<DiaryArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = tokio::time::timeout(
            CALL_TIMEOUT,
            write::diary(
                &self.app.db,
                &self.app.embed,
                &self.app.config.agent_id,
                &args.text,
                args.importance,
                &args.source,
            ),
        )
        .await;
        match call {
            Err(_) => Ok(failure("timeout", "diary exceeded the call budget")),
            Ok(Err(error)) => Ok(failure(error.code(), error.to_string())),
            Ok(Ok(outcome)) => success_json(&write::outcome_json(&outcome)),
        }
    }

    #[tool(
        description = "Append an event to the mind_events timeline. Append-only: a mistake is corrected by a new event, never by an edit."
    )]
    async fn mind_log(
        &self,
        Parameters(args): Parameters<MindLogArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = tokio::time::timeout(
            CALL_TIMEOUT,
            write::mind_log(
                &self.app.db,
                &self.app.embed,
                &self.app.config.agent_id,
                &args.content,
                &args.kind,
                &args.channel,
                args.thread_id.as_deref(),
            ),
        )
        .await;
        match call {
            Err(_) => Ok(failure("timeout", "mind_log exceeded the call budget")),
            Ok(Err(error)) => Ok(failure(error.code(), error.to_string())),
            Ok(Ok(outcome)) => success_json(&write::outcome_json(&outcome)),
        }
    }

    #[tool(
        description = "Read the mind_events timeline without a semantic query: newest rows, optional kind filter and RFC 3339 lower bound. Cheaper than recall when only chronology is needed."
    )]
    async fn mind_tail(
        &self,
        Parameters(args): Parameters<MindTailArgs>,
    ) -> Result<CallToolResult, McpError> {
        let since = match args.since.as_deref() {
            None => None,
            Some(raw) => match chrono::DateTime::parse_from_rfc3339(raw) {
                Ok(stamp) => Some(stamp.with_timezone(&chrono::Utc)),
                Err(error) => {
                    return Ok(failure(
                        "invalid_params",
                        format!("since is not RFC 3339: {error}"),
                    ));
                }
            },
        };
        let limit = bounded(args.limit, 200) as i64;
        let call = tokio::time::timeout(
            CALL_TIMEOUT,
            circuit::timeline_filtered(
                &self.app.db,
                &self.app.config.agent_id,
                limit,
                args.kind.as_deref(),
                since,
            ),
        )
        .await;
        match call {
            Err(_) => Ok(failure("timeout", "mind_tail exceeded the call budget")),
            Ok(Err(error)) => Ok(failure("db_unavailable", error.to_string())),
            Ok(Ok(rows)) => success_json(&json!({ "rows": rows })),
        }
    }

    #[tool(
        description = "Read one row by id (full UUID or unique prefix) with agent_id, register, importance, episode_id, has_vector, metadata, source and graph evidence_triples. Looks in all four circuits unless table is given, in any namespace. Never embeds."
    )]
    async fn get(&self, Parameters(args): Parameters<GetArgs>) -> Result<CallToolResult, McpError> {
        let call = tokio::time::timeout(
            CALL_TIMEOUT,
            surgical::get(&self.app.db, &args.id, args.table.as_deref()),
        )
        .await;
        match call {
            Err(_) => Ok(failure("timeout", "get exceeded the call budget")),
            Ok(Err(error)) => Ok(failure(error.code(), error.to_string())),
            Ok(Ok(value)) => success_json(&value),
        }
    }

    #[tool(
        description = "Patch metadata on one row without re-embedding. Supports importance, memories.memory_type, diary.source, episode_id and key-wise memories.metadata merge. identity is protected; mind is append-only."
    )]
    async fn patch(
        &self,
        Parameters(args): Parameters<PatchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = tokio::time::timeout(
            Duration::from_millis(100),
            surgical::patch(
                &self.app.db,
                &self.app.config.agent_id,
                &args.id,
                &args.table,
                args.importance,
                args.memory_type.as_deref(),
                args.source.as_deref(),
                args.episode_id.as_deref(),
                args.metadata.as_ref(),
            ),
        )
        .await;
        match call {
            Err(_) => Ok(failure("timeout", "patch exceeded the call budget")),
            Ok(Err(error)) => Ok(failure(error.code(), error.to_string())),
            Ok(Ok(value)) => success_json(&value),
        }
    }

    #[tool(
        description = "Move selected memories, diary entries and lessons between namespaces. Dry-run by default; real moves require dry_run=false and confirm=true. identity rows are skipped; mind_events are never moved."
    )]
    async fn adopt(
        &self,
        Parameters(args): Parameters<AdoptArgs>,
    ) -> Result<CallToolResult, McpError> {
        let filter = args.filter.unwrap_or_default();
        let call = tokio::time::timeout(
            Duration::from_millis(500),
            surgical::adopt(
                &self.app.db,
                &args.from_agent,
                &args.to_agent,
                args.ids.as_deref(),
                &filter,
                args.dry_run,
                args.confirm,
            ),
        )
        .await;
        match call {
            Err(_) => Ok(failure("timeout", "adopt exceeded the call budget")),
            Ok(Err(error)) => Ok(failure(error.code(), error.to_string())),
            Ok(Ok(report)) => {
                let value = serde_json::to_value(report)
                    .map_err(|error| McpError::internal_error(error.to_string(), None))?;
                success_json(&value)
            }
        }
    }

    #[tool(
        description = "Merge two rows in the same namespace and register. Dry-run by default; real deletion requires confirm=true. identity is protected and mind is append-only. Requires migration 0010 for before-image history."
    )]
    async fn merge(
        &self,
        Parameters(args): Parameters<MergeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = tokio::time::timeout(
            Duration::from_millis(300),
            surgical::merge(
                &self.app.db,
                &args.keep_id,
                &args.drop_id,
                &args.table,
                &args.strategy,
                args.dry_run,
                args.confirm,
            ),
        )
        .await;
        match call {
            Err(_) => Ok(failure("timeout", "merge exceeded the call budget")),
            Ok(Err(error)) => Ok(failure(error.code(), error.to_string())),
            Ok(Ok(value)) => success_json(&value),
        }
    }

    #[tool(
        description = "Read newest before-image revisions for a row, including deleted rows. Requires migration 0010; never embeds."
    )]
    async fn history(
        &self,
        Parameters(args): Parameters<HistoryArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = tokio::time::timeout(
            Duration::from_millis(100),
            surgical::history(&self.app.db, &args.id, &args.table, args.limit),
        )
        .await;
        match call {
            Err(_) => Ok(failure("timeout", "history exceeded the call budget")),
            Ok(Err(error)) => Ok(failure(error.code(), error.to_string())),
            Ok(Ok(value)) => success_json(&value),
        }
    }

    #[tool(
        description = "Re-vectorize rows under a named embedding model and record the model on every row. Dry-run by default: reports scanned, stale and mismatched counts without writing. identity rows are skipped and reported. A dimension other than 768 needs force=true."
    )]
    async fn reembed(
        &self,
        Parameters(args): Parameters<ReembedArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = tokio::time::timeout(
            CALL_TIMEOUT,
            surgical::reembed(
                &self.app.db,
                &self.app.embed,
                &self.app.config.agent_id,
                args.model.as_deref(),
                args.circuit.as_deref(),
                args.batch,
                args.dry_run,
                args.force,
            ),
        )
        .await;
        match call {
            Err(_) => Ok(failure("timeout", "reembed exceeded the call budget")),
            Ok(Err(error)) => Ok(failure(error.code(), error.to_string())),
            Ok(Ok(value)) => success_json(&value),
        }
    }

    #[tool(
        description = "Census of agent_id namespaces across the four circuits: row counts per circuit, rows without a vector, first and last timestamp. Shows namespaces that search and list cannot see. include_empty adds the server's own namespace when it holds nothing. Never embeds."
    )]
    async fn agents(
        &self,
        Parameters(args): Parameters<browse::AgentsParams>,
    ) -> Result<CallToolResult, McpError> {
        let call = tokio::time::timeout(
            CALL_TIMEOUT,
            browse::agents(&self.app.db, &self.app.config.agent_id, &args),
        )
        .await;
        match call {
            Err(_) => Ok(failure("timeout", "agents exceeded the call budget")),
            Ok(Err(error)) => Ok(failure(error.code(), error.to_string())),
            Ok(Ok(value)) => success_json(&value),
        }
    }

    #[tool(
        description = "Non-semantic browse over all four circuits with filters (circuit, register, importance_min, since, until, has_vector, episode_id, thread_id), ordering and a cursor. Own namespace by default; agent_id=\"*\" reads every namespace. Text is a preview: use get for the whole row. Never embeds, never writes."
    )]
    async fn list(
        &self,
        Parameters(args): Parameters<browse::ListParams>,
    ) -> Result<CallToolResult, McpError> {
        let call = tokio::time::timeout(
            CALL_TIMEOUT,
            browse::list(&self.app.db, &self.app.config.agent_id, &args),
        )
        .await;
        match call {
            Err(_) => Ok(failure("timeout", "list exceeded the call budget")),
            Ok(Err(error)) => Ok(failure(error.code(), error.to_string())),
            Ok(Ok(value)) => success_json(&value),
        }
    }

    #[tool(
        description = "Delete one row from any circuit (table optional). By default protected rows are refused: memory_type=identity, diary seed rows, lessons with importance >= 9 (protected) and mind_events (append_only). force=true with confirm=true lifts every protection; agent_id targets another namespace and needs confirm=true. Either one records an audit row in phylactery_runs (audit_id), never in mind_events. dry_run reports without deleting. Graph edges citing a deleted memory are removed with it."
    )]
    async fn forget(
        &self,
        Parameters(args): Parameters<ForgetArgs>,
    ) -> Result<CallToolResult, McpError> {
        let caller = self.app.config.agent_id.as_str();
        let request = mutate::ForgetRequest {
            caller,
            target: args.agent_id.as_deref().map_or(caller, str::trim),
            id: &args.id,
            table: args.table.as_deref(),
            dry_run: args.dry_run,
            confirm: args.confirm,
            force: args.force,
        };
        let call =
            tokio::time::timeout(CALL_TIMEOUT, mutate::forget_any(&self.app.db, &request)).await;
        match call {
            Err(_) => Ok(failure("timeout", "forget exceeded the call budget")),
            Ok(Err(error)) => Ok(failure(error.code(), error.to_string())),
            Ok(Ok(report)) => match serde_json::to_value(&report) {
                Ok(value) => success_json(&value),
                Err(error) => Ok(failure("internal", error.to_string())),
            },
        }
    }

    #[tool(
        description = "Replace the text of a memory or diary row and re-embed it. Importance and provenance stay untouched; protected rows and injection patterns are refused."
    )]
    async fn replace(
        &self,
        Parameters(args): Parameters<ReplaceArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = tokio::time::timeout(
            CALL_TIMEOUT,
            mutate::replace(
                &self.app.db,
                &self.app.embed,
                &self.app.config.agent_id,
                &args.id,
                &args.table,
                &args.text,
            ),
        )
        .await;
        match call {
            Err(_) => Ok(failure("timeout", "replace exceeded the call budget")),
            Ok(Err(error)) => Ok(failure(error.code(), error.to_string())),
            Ok(Ok(outcome)) => success_json(&mutate::mutate_json(&outcome)),
        }
    }

    #[tool(
        description = "Fill missing embeddings for one circuit in batches through Ollama. One failed row does not stop the batch; the report lists scanned, embedded and failed ids."
    )]
    async fn backfill(
        &self,
        Parameters(args): Parameters<BackfillArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = tokio::time::timeout(
            CALL_TIMEOUT,
            mutate::backfill(
                &self.app.db,
                &self.app.embed,
                &self.app.config.agent_id,
                &args.table,
                args.batch,
            ),
        )
        .await;
        match call {
            Err(_) => Ok(failure("timeout", "backfill exceeded the call budget")),
            Ok(Err(error)) => Ok(failure(error.code(), error.to_string())),
            Ok(Ok(report)) => success_json(&mutate::backfill_json(&report)),
        }
    }

    #[tool(
        description = "Write a fact and index it in the knowledge graph in one act: guard, save, rule extraction, entity resolution, temporal triples. Returns the memory id, the entities touched, the triples written and any open conflicts."
    )]
    async fn remember(
        &self,
        Parameters(args): Parameters<RememberArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = tokio::time::timeout(
            CALL_TIMEOUT,
            kg::remember(
                &self.app.db,
                &self.app.embed,
                &self.app.config.agent_id,
                &args.text,
                &args.memory_type,
                args.importance,
                &args.source,
            ),
        )
        .await;
        match call {
            Err(_) => Ok(failure("timeout", "remember exceeded the call budget")),
            Ok(Err(error)) => Ok(failure(error.code(), error.to_string())),
            Ok(Ok(report)) => success_json(&json!(report)),
        }
    }

    #[tool(
        description = "Profile of one entity: aliases, typed edges in and out, and the facts that mention it. One call answers what is known about a thing."
    )]
    async fn entity(
        &self,
        Parameters(args): Parameters<EntityArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = tokio::time::timeout(
            CALL_TIMEOUT,
            kg::entity_profile(
                &self.app.db,
                &self.app.config.agent_id,
                &args.name,
                args.limit.clamp(1, 200),
            ),
        )
        .await;
        match call {
            Err(_) => Ok(failure("timeout", "entity exceeded the call budget")),
            Ok(Err(error)) => Ok(failure(error.code(), error.to_string())),
            Ok(Ok(value)) => success_json(&value),
        }
    }

    #[tool(
        description = "Graph traversal from one entity: paths subject → predicate → object up to depth 3, with a predicate filter and a hard edge cap. Returns paths, not fragments."
    )]
    async fn related(
        &self,
        Parameters(args): Parameters<RelatedArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = tokio::time::timeout(
            CALL_TIMEOUT,
            kg::related(
                &self.app.db,
                &self.app.config.agent_id,
                &args.name,
                args.depth,
                &args.predicates,
                args.limit,
            ),
        )
        .await;
        match call {
            Err(_) => Ok(failure("timeout", "related exceeded the call budget")),
            Ok(Err(error)) => Ok(failure(error.code(), error.to_string())),
            Ok(Ok(value)) => success_json(&value),
        }
    }

    #[tool(
        description = "Open contradictions in the graph: one (subject, predicate) with more than one live object. An empty list is a good answer."
    )]
    async fn conflicts(
        &self,
        Parameters(args): Parameters<ConflictsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = tokio::time::timeout(
            CALL_TIMEOUT,
            kg::conflicts(&self.app.db, &self.app.config.agent_id, args.limit),
        )
        .await;
        match call {
            Err(_) => Ok(failure("timeout", "conflicts exceeded the call budget")),
            Ok(Err(error)) => Ok(failure(error.code(), error.to_string())),
            Ok(Ok(value)) => success_json(&value),
        }
    }

    #[tool(
        description = "One call, one answer: vector search across all four circuits, entity detection, graph expansion, conflicts and fresh timeline, ranked and cut to a token budget. Returns facts, relations, entities, timeline, conflicts and citations, with an honest truncated flag."
    )]
    async fn ask(&self, Parameters(args): Parameters<AskArgs>) -> Result<CallToolResult, McpError> {
        let call = tokio::time::timeout(
            CALL_TIMEOUT,
            ask::ask(
                &self.app.db,
                &self.app.embed,
                &self.app.config.agent_id,
                &args.question,
                args.budget_tokens,
                args.include_legacy,
            ),
        )
        .await;
        match call {
            Err(_) => Ok(failure("timeout", "ask exceeded the call budget")),
            Ok(Err(error)) => Ok(failure(error.code(), error.to_string())),
            Ok(Ok(value)) => success_json(&value),
        }
    }

    #[tool(
        description = "Batch extraction: index memories that were never indexed into the knowledge graph with the rule extractor. Each row is marked once, so repeated calls walk forward through the backlog and stop when scanned is zero."
    )]
    async fn extract(
        &self,
        Parameters(args): Parameters<ExtractArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = tokio::time::timeout(
            std::time::Duration::from_secs(600),
            kg::extract_batch(
                &self.app.db,
                &self.app.embed,
                &self.app.config.agent_id,
                args.batch,
                self.app.config.kg_since,
            ),
        )
        .await;
        match call {
            Err(_) => Ok(failure("timeout", "extract exceeded the call budget")),
            Ok(Err(error)) => Ok(failure(error.code(), error.to_string())),
            Ok(Ok(report)) => success_json(&json!(report)),
        }
    }

    #[tool(
        description = "Create a backup: pg_dump, gzip, sha256, optional verification by restoring into a scratch database, optional encryption with the phylactery key. Records the result in phylactery_backups. Never deletes anything."
    )]
    async fn backup(
        &self,
        Parameters(args): Parameters<BackupArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = tokio::time::timeout(
            std::time::Duration::from_secs(600),
            backup::backup(
                &self.app.db,
                &self.app.config,
                &self.app.config.agent_id,
                args.verify,
                args.encrypt,
                args.plain,
                args.confirm,
            ),
        )
        .await;
        match call {
            Err(_) => Ok(failure("timeout", "backup exceeded the call budget")),
            Ok(Err(error)) => Ok(failure(error.code(), error.to_string())),
            Ok(Ok(report)) => success_json(&report),
        }
    }

    #[tool(
        description = "Restore the newest (or named) backup into the scratch database and compare row counts with what was recorded at backup time. The live database is never touched."
    )]
    async fn verify(
        &self,
        Parameters(args): Parameters<VerifyArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = tokio::time::timeout(
            std::time::Duration::from_secs(600),
            backup::verify(&self.app.db, &self.app.config, args.path),
        )
        .await;
        match call {
            Err(_) => Ok(failure("timeout", "verify exceeded the call budget")),
            Ok(Err(error)) => Ok(failure(error.code(), error.to_string())),
            Ok(Ok(report)) => success_json(&report),
        }
    }

    #[tool(
        description = "Compare the current circuit counts, digest and schema revision with the newest backup. Returns drift; an empty drift list means the memory matches its last archive."
    )]
    async fn manifest(&self) -> Result<CallToolResult, McpError> {
        match backup::manifest(&self.app.db, &self.app.config.agent_id).await {
            Ok(report) => success_json(&report),
            Err(error) => Ok(failure(error.code(), error.to_string())),
        }
    }

    #[tool(
        description = "Apply migrations/*.sql in order, idempotently, and report the schema revision. No manual psql is needed."
    )]
    async fn migrate(&self) -> Result<CallToolResult, McpError> {
        match backup::migrate(&self.app.db, &self.app.config).await {
            Ok(report) => success_json(&report),
            Err(error) => Ok(failure(error.code(), error.to_string())),
        }
    }

    #[tool(
        description = "Maintenance: VACUUM ANALYZE on the circuits and the graph, orphan entities, open conflicts, memories without vectors, and the list of archives that fall outside the retention policy. Destructive pruning happens only with confirm=true."
    )]
    async fn maintain(
        &self,
        Parameters(args): Parameters<MaintainArgs>,
    ) -> Result<CallToolResult, McpError> {
        let call = tokio::time::timeout(
            std::time::Duration::from_secs(300),
            backup::maintain(
                &self.app.db,
                &self.app.config,
                &self.app.config.agent_id,
                args.confirm,
            ),
        )
        .await;
        match call {
            Err(_) => Ok(failure("timeout", "maintain exceeded the call budget")),
            Ok(Err(error)) => Ok(failure(error.code(), error.to_string())),
            Ok(Ok(report)) => success_json(&report),
        }
    }
}

#[tool_handler]
impl ServerHandler for PhylacteryServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "phylactery".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            instructions: Some(
                "Phylactery: one vector circuit over memories, self_reflections, causal_lessons and \
                 mind_events. recall raises a slice; search is the pointed query; doctor names what hurts."
                    .to_string(),
            ),
            ..Default::default()
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .json()
        .init();

    let app = match App::new() {
        Ok(app) => Arc::new(app),
        Err(error) => {
            tracing::error!(%error, "phylactery-mcp cannot start");
            eprintln!("phylactery-mcp: {error:#}");
            std::process::exit(1);
        }
    };
    tracing::info!(
        dsn = %app.config.redacted_dsn(),
        dsn_source = %app.config.dsn_source,
        ollama = %app.config.ollama_url,
        model = %app.config.embed_model,
        agent_id = %app.config.agent_id,
        "phylactery-mcp starting"
    );

    let server = PhylacteryServer::new(app);
    let running = server.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}
