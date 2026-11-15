//! Knowledge graph: entities, typed relations, temporal validity, evidence.
//!
//! The graph is an index of relations over the four memory circuits. Every triple carries the
//! `memories.id` it came from, so a claim can be traced to its evidence. Extraction is
//! deterministic first: a closed predicate vocabulary and a fixed pattern table. A sentence that
//! matches no pattern produces no triple — precision beats recall.

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::db::{Db, DbError};
use crate::embed::Embedder;
use crate::write::{self, WriteError};

/// Closed predicate vocabulary. Free-form predicates are not written.
pub const PREDICATES: [&str; 20] = [
    "is_a",
    "part_of",
    "located_in",
    "owns",
    "uses",
    "depends_on",
    "runs_on",
    "created_by",
    "alias_of",
    "prefers",
    "opposes",
    "has_property",
    "has_value",
    "works_for",
    "member_of",
    "supersedes",
    "blocks",
    "requires",
    "produces",
    "measures",
];

/// Closed entity kinds. An untyped entity is not written.
pub const KINDS: [&str; 10] = [
    "agent", "person", "project", "host", "service", "repo", "document", "concept", "place",
    "artifact",
];

/// Keyword table: the text between two mentions decides the predicate. English and Russian,
/// because memory is written in both.
const PATTERNS: &[(&str, &str)] = &[
    (" is part of ", "part_of"),
    (" is a part of ", "part_of"),
    (" является частью ", "part_of"),
    (" входит в ", "part_of"),
    (" is an ", "is_a"),
    (" is a ", "is_a"),
    (" — это ", "is_a"),
    (" является ", "is_a"),
    (" lives in ", "located_in"),
    (" is located in ", "located_in"),
    (" находится в ", "located_in"),
    (" живёт в ", "located_in"),
    (" owns ", "owns"),
    (" владеет ", "owns"),
    (" uses ", "uses"),
    (" использует ", "uses"),
    (" depends on ", "depends_on"),
    (" зависит от ", "depends_on"),
    (" runs on ", "runs_on"),
    (" работает на ", "runs_on"),
    (" created ", "created_by"),
    (" создал ", "created_by"),
    (" создала ", "created_by"),
    (" prefers ", "prefers"),
    (" предпочитает ", "prefers"),
    (" opposes ", "opposes"),
    (" противостоит ", "opposes"),
    (" works for ", "works_for"),
    (" is a member of ", "member_of"),
    (" supersedes ", "supersedes"),
    (" заменяет ", "supersedes"),
    (" blocks ", "blocks"),
    (" блокирует ", "blocks"),
    (" requires ", "requires"),
    (" требует ", "requires"),
    (" produces ", "produces"),
    (" производит ", "produces"),
    (" measures ", "measures"),
    (" измеряет ", "measures"),
];

/// Resolution key: lowercase, punctuation stripped, whitespace collapsed.
pub fn normalize(text: &str) -> String {
    let lowered = text.to_lowercase();
    let mut cleaned = String::with_capacity(lowered.len());
    for ch in lowered.chars() {
        if ch.is_alphanumeric() || ch.is_whitespace() || ch == '.' {
            cleaned.push(ch);
        } else if ch == '-' || ch == '_' {
            cleaned.push(' ');
        }
    }
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Stable identity of a triple: md5 over agent, subject, predicate and normalized object.
pub fn triple_hash(
    agent_id: &str,
    subject_id: &Uuid,
    predicate: &str,
    object_norm: &str,
) -> String {
    let payload = format!("{agent_id}|{subject_id}|{predicate}|{object_norm}");
    format!("{:x}", md5::compute(payload.as_bytes()))
}

const STOPWORDS: &[&str] = &[
    "this", "that", "these", "those", "he", "she", "they", "we", "i", "you", "it", "there", "here",
    "what", "who", "when", "where", "why", "how", "and", "but", "or", "if", "then", "so", "every",
    "each", "some", "any", "my", "your", "the", "a", "an", "это", "этот", "эта", "эти", "для",
    "него", "неё", "они", "он", "она", "оно", "мы", "я", "ты", "вы",
];

const NEGATIONS: [&str; 8] = ["не", "нет", "ни", "not", "no", "never", "никогда", "ничего"];

fn strip_article(text: &str) -> String {
    let trimmed = text.trim();
    let lowered = trimmed.to_lowercase();
    for prefix in ["the ", "a ", "an "] {
        if lowered.starts_with(prefix) {
            return trimmed[prefix.len()..].trim().to_string();
        }
    }
    trimmed.to_string()
}

fn has_negation(text: &str) -> bool {
    normalize(text)
        .split_whitespace()
        .any(|word| NEGATIONS.contains(&word))
}

/// Final gate: a triple is written only if it survives every structural check.
pub fn valid_triple(triple: &RawTriple) -> bool {
    let subject = normalize(&triple.subject);
    let object = normalize(&triple.object);
    if subject.is_empty() || object.is_empty() || subject == object {
        return false;
    }
    if !PREDICATES.contains(&triple.predicate.as_str()) {
        return false;
    }
    if has_negation(&triple.subject) || has_negation(&triple.object) {
        return false;
    }
    if triple.subject.split_whitespace().count() > 4 || triple.object.split_whitespace().count() > 4
    {
        return false;
    }
    if triple.subject.contains(':') || triple.object.contains(':') {
        return false;
    }
    if triple.subject.contains(',') || triple.object.contains(',') {
        return false;
    }
    let first = subject.split_whitespace().next().unwrap_or("");
    !STOPWORDS.contains(&first)
}

fn plausible_subject(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.len() > 40 {
        return false;
    }
    if trimmed.contains('(') || trimmed.contains(')') || trimmed.contains('—') {
        return false;
    }
    let words: Vec<&str> = trimmed.split_whitespace().collect();
    if words.is_empty() || words.len() > 4 {
        return false;
    }
    let lowered = trimmed.to_lowercase();
    let has_article = ["the ", "a ", "an "]
        .iter()
        .any(|prefix| lowered.starts_with(prefix));
    let core = strip_article(trimmed);
    let core_words: Vec<&str> = core.split_whitespace().collect();
    if core_words.is_empty() {
        return false;
    }
    let first = core_words[0].to_lowercase();
    if STOPWORDS.contains(&first.as_str()) {
        return false;
    }
    if has_article {
        return true;
    }
    if core.contains('.') && !core.contains(' ') {
        return true;
    }
    core.chars().next().is_some_and(|c| c.is_uppercase())
}

fn plausible_object(text: &str) -> bool {
    let trimmed = trimmed_object(text);
    if trimmed.is_empty() || trimmed.len() > 40 {
        return false;
    }
    if trimmed.contains('(')
        || trimmed.contains(')')
        || trimmed.contains('—')
        || trimmed.contains(':')
        || trimmed.contains('\'')
        || trimmed.contains('"')
    {
        return false;
    }
    let words: Vec<&str> = trimmed.split_whitespace().collect();
    if words.is_empty() || words.len() > 3 {
        return false;
    }
    let first = words[0].to_lowercase();
    if STOPWORDS.contains(&first.as_str()) {
        return false;
    }
    const PREPOSITIONS: &[&str] = &[
        "in", "on", "at", "of", "to", "from", "by", "with", "for", "into", "в", "на", "с", "из",
        "от", "до", "по", "при", "для", "под", "над", "за", "к",
    ];
    if PREPOSITIONS.contains(&first.as_str()) {
        return false;
    }
    let last = words[words.len() - 1].to_lowercase();
    !matches!(
        last.as_str(),
        "and" | "or" | "the" | "a" | "an" | "of" | "to"
    )
}

fn trimmed_object(text: &str) -> String {
    static SPLIT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let split =
        SPLIT.get_or_init(|| regex::Regex::new(r"[.,;](?:\s+|$)").expect("object splitter"));
    let trimmed = text
        .trim()
        .trim_start_matches(|ch: char| !ch.is_alphanumeric());
    split
        .split(trimmed)
        .next()
        .unwrap_or_default()
        .trim()
        .to_string()
}

#[derive(Debug, Clone)]
pub struct KnownEntity {
    pub canonical: String,
    pub norm: String,
    pub kind: String,
    pub aliases: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RawTriple {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_kind: Option<String>,
}

fn sentences(text: &str) -> Vec<&str> {
    static SPLIT: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let split =
        SPLIT.get_or_init(|| regex::Regex::new(r"[.!?;](?:\s+|$)|\n").expect("sentence splitter"));
    split
        .split(text)
        .map(str::trim)
        .filter(|part| part.len() > 3)
        .collect()
}

fn kind_of(word: &str) -> Option<String> {
    let candidate = normalize(word);
    KINDS
        .iter()
        .find(|kind| normalize(kind) == candidate)
        .map(|kind| (*kind).to_string())
}

/// Mentions of known entities in a sentence, longest match first, non-overlapping.
fn mentions(sentence: &str, known: &[KnownEntity]) -> Vec<(usize, usize, String)> {
    let lowered = sentence.to_lowercase();
    let mut found: Vec<(usize, usize, String)> = Vec::new();
    let mut candidates: Vec<(String, String)> = Vec::new();
    for entity in known {
        candidates.push((entity.canonical.clone(), entity.canonical.clone()));
        for alias in &entity.aliases {
            candidates.push((alias.clone(), entity.canonical.clone()));
        }
    }
    candidates.sort_by_key(|item| std::cmp::Reverse(item.0.len()));
    for (needle, canonical) in candidates {
        let needle = needle.to_lowercase();
        if needle.len() < 2 {
            continue;
        }
        let mut from = 0;
        while let Some(position) = lowered[from..].find(&needle) {
            let start = from + position;
            let end = start + needle.len();
            let overlaps = found.iter().any(|(existing_start, existing_end, _)| {
                start < *existing_end && end > *existing_start
            });
            if !overlaps {
                found.push((start, end, canonical.clone()));
            }
            from = end;
        }
    }
    found.sort_by_key(|(start, _, _)| *start);
    found
}

/// Deterministic extraction: known entities plus the fixed pattern table.
pub fn extract(text: &str, known: &[KnownEntity]) -> Vec<RawTriple> {
    let mut triples: Vec<RawTriple> = Vec::new();
    let mut seen: BTreeSet<(String, String, String)> = BTreeSet::new();

    for sentence in sentences(text) {
        let found = mentions(sentence, known);
        if found.len() >= 2 {
            for pair in found.windows(2) {
                let (_, left_end, left_name) = &pair[0];
                let (right_start, _, right_name) = &pair[1];
                if right_start <= left_end {
                    continue;
                }
                let between = sentence[*left_end..*right_start].to_lowercase();
                let between = format!(" {} ", between.trim());
                if let Some((_, predicate)) = PATTERNS
                    .iter()
                    .find(|(pattern, _)| between.contains(pattern))
                {
                    let key = (left_name.clone(), predicate.to_string(), right_name.clone());
                    if seen.insert(key) {
                        triples.push(RawTriple {
                            subject: left_name.clone(),
                            predicate: predicate.to_string(),
                            object: right_name.clone(),
                            subject_kind: None,
                            object_kind: None,
                        });
                    }
                }
            }
            continue;
        }

        if found.len() == 1 {
            let (_, end, name) = &found[0];
            let tail_lower = sentence[*end..].to_lowercase();
            if let Some((position, (pattern, predicate))) = PATTERNS
                .iter()
                .filter_map(|pattern| {
                    tail_lower
                        .find(pattern.0)
                        .map(|position| (position, pattern))
                })
                .min_by_key(|(position, _)| *position)
            {
                let object_start = *end + position + pattern.len();
                let object = strip_article(&trimmed_object(&sentence[object_start..]));
                if plausible_object(&object) {
                    let key = (name.clone(), predicate.to_string(), object.clone());
                    if seen.insert(key) {
                        triples.push(RawTriple {
                            subject: name.clone(),
                            predicate: predicate.to_string(),
                            object: object.clone(),
                            subject_kind: None,
                            object_kind: kind_of(&object),
                        });
                    }
                }
            }
            continue;
        }

        // No known mention: a capitalized subject with a pattern still yields a typed triple.
        let trimmed = sentence.trim();
        let mut chars = trimmed.chars();
        let starts_upper = chars.next().map(|ch| ch.is_uppercase()).unwrap_or(false);
        let first_token = trimmed.split_whitespace().next().unwrap_or("");
        let looks_like_host = first_token.contains('.') && !first_token.ends_with('.');
        if !starts_upper && !looks_like_host {
            continue;
        }
        let lowered = trimmed.to_lowercase();
        if let Some((position, (pattern, predicate))) = PATTERNS
            .iter()
            .filter_map(|pattern| lowered.find(pattern.0).map(|position| (position, pattern)))
            .min_by_key(|(position, _)| *position)
        {
            let raw_subject = trimmed[..position].trim().to_string();
            let object_start = position + pattern.len();
            let object = strip_article(&trimmed_object(&trimmed[object_start..]));
            if plausible_subject(&raw_subject) && plausible_object(&object) {
                let subject = strip_article(&raw_subject);
                let key = (subject.clone(), predicate.to_string(), object.clone());
                if seen.insert(key) {
                    triples.push(RawTriple {
                        subject,
                        predicate: predicate.to_string(),
                        object: object.clone(),
                        subject_kind: Some("concept".to_string()),
                        object_kind: kind_of(&object),
                    });
                }
            }
        }
    }
    triples.retain(valid_triple);
    triples
}

#[derive(Debug, Serialize)]
pub struct IndexReport {
    pub memory_id: String,
    pub entities: Vec<String>,
    pub triples: Vec<RawTriple>,
    pub conflicts: Vec<Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum KgError {
    #[error(transparent)]
    Write(#[from] WriteError),
    #[error(transparent)]
    Db(#[from] DbError),
    #[error(transparent)]
    Sql(#[from] tokio_postgres::Error),
    #[error(transparent)]
    Embed(#[from] crate::embed::EmbedError),
    #[error("entity `{0}` not found")]
    EntityNotFound(String),
}

impl KgError {
    pub fn code(&self) -> &'static str {
        match self {
            KgError::Write(error) => error.code(),
            KgError::Db(_) | KgError::Sql(_) => "db_unavailable",
            KgError::Embed(_) => "embed_unavailable",
            KgError::EntityNotFound(_) => "not_found",
        }
    }
}

async fn known_entities(
    client: &deadpool_postgres::Object,
    agent_id: &str,
) -> Result<Vec<KnownEntity>, tokio_postgres::Error> {
    let rows = client
        .query(
            "SELECT canonical, kind, aliases FROM entities
             WHERE agent_id = $1 AND merged_into IS NULL AND extractor = 'rule'
             ORDER BY length(canonical) DESC
             LIMIT 2000",
            &[&agent_id],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|row| {
            let canonical: String = row.get("canonical");
            let kind: String = row.get("kind");
            let aliases: Value = row.get("aliases");
            let aliases = aliases
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            KnownEntity {
                norm: normalize(&canonical),
                canonical,
                kind,
                aliases,
            }
        })
        .collect())
}

async fn resolve_or_create(
    client: &deadpool_postgres::Object,
    embedder: &Embedder,
    agent_id: &str,
    name: &str,
    kind: Option<&str>,
) -> Result<Uuid, KgError> {
    let norm = normalize(name);
    if let Some(row) = client
        .query_opt(
            "SELECT id FROM entities WHERE agent_id = $1 AND norm = $2 AND merged_into IS NULL",
            &[&agent_id, &norm],
        )
        .await?
    {
        return Ok(row.get("id"));
    }
    if let Some(row) = client
        .query_opt(
            "SELECT id FROM entities
             WHERE agent_id = $1 AND merged_into IS NULL AND aliases ? $2
             LIMIT 1",
            &[&agent_id, &name],
        )
        .await?
    {
        return Ok(row.get("id"));
    }
    let kind = kind.unwrap_or("concept");
    let vector = embedder.embed_one(name).await?;
    let literal = crate::embed::vector_literal(&vector);
    let row = client
        .query_one(
            "INSERT INTO entities (agent_id, canonical, norm, kind, aliases, embedding, extractor)
             VALUES ($1, $2, $3, $4, '[]'::jsonb, $5::text::vector, 'rule')
             ON CONFLICT (agent_id, norm) WHERE merged_into IS NULL
             DO UPDATE SET canonical = entities.canonical
             RETURNING id",
            &[&agent_id, &name, &norm, &kind, &literal],
        )
        .await?;
    Ok(row.get("id"))
}

struct TripleWrite<'a> {
    agent_id: &'a str,
    subject_id: Uuid,
    subject: &'a str,
    predicate: &'a str,
    object_id: Uuid,
    object: &'a str,
    evidence_id: Uuid,
    confidence: f32,
    extractor: &'a str,
}

async fn upsert_triple(
    client: &deadpool_postgres::Object,
    write: &TripleWrite<'_>,
) -> Result<(), tokio_postgres::Error> {
    let agent_id = write.agent_id;
    let subject_id = write.subject_id;
    let predicate = write.predicate;
    let object = write.object;
    let object_norm = normalize(object);
    let hash = triple_hash(agent_id, &subject_id, predicate, &object_norm);

    client
        .execute(
            "UPDATE knowledge_graph SET valid_to = now()
             WHERE agent_id = $1 AND subject_id = $2 AND predicate = $3
               AND valid_to IS NULL AND object_norm <> $4",
            &[&agent_id, &subject_id, &predicate, &object_norm],
        )
        .await?;

    client
        .execute(
            "INSERT INTO knowledge_graph
                 (agent_id, subject, predicate, object, subject_id, object_id, object_norm,
                  evidence_id, confidence, extractor, triple_hash)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
             ON CONFLICT (agent_id, triple_hash) DO UPDATE
                 SET valid_to = NULL,
                     evidence_id = EXCLUDED.evidence_id,
                     confidence = GREATEST(knowledge_graph.confidence, EXCLUDED.confidence)",
            &[
                &agent_id,
                &write.subject,
                &predicate,
                &object,
                &subject_id,
                &write.object_id,
                &object_norm,
                &write.evidence_id,
                &write.confidence,
                &write.extractor,
                &hash,
            ],
        )
        .await?;
    Ok(())
}

/// Write a fact and index it in the graph in one act.
pub async fn remember(
    db: &Db,
    embedder: &Embedder,
    agent_id: &str,
    text: &str,
    memory_type: &str,
    importance: f64,
    source: &str,
) -> Result<IndexReport, KgError> {
    let outcome = write::save(
        db,
        embedder,
        agent_id,
        text,
        memory_type,
        importance,
        source,
    )
    .await?;
    let memory_id =
        Uuid::parse_str(&outcome.id).map_err(|_| KgError::EntityNotFound(outcome.id.clone()))?;
    let client = db.client().await?;
    let known = known_entities(&client, agent_id).await?;
    let triples = extract(text, &known);

    let mut entity_names: Vec<String> = Vec::new();
    for triple in &triples {
        let subject_id = resolve_or_create(
            &client,
            embedder,
            agent_id,
            &triple.subject,
            triple.subject_kind.as_deref(),
        )
        .await?;
        let object_id = resolve_or_create(
            &client,
            embedder,
            agent_id,
            &triple.object,
            triple.object_kind.as_deref(),
        )
        .await?;
        upsert_triple(
            &client,
            &TripleWrite {
                agent_id,
                subject_id,
                subject: &triple.subject,
                predicate: &triple.predicate,
                object_id,
                object: &triple.object,
                evidence_id: memory_id,
                confidence: 0.9,
                extractor: "rule",
            },
        )
        .await?;
        if !entity_names.contains(&triple.subject) {
            entity_names.push(triple.subject.clone());
        }
        if !entity_names.contains(&triple.object) {
            entity_names.push(triple.object.clone());
        }
    }

    client
        .execute(
            "UPDATE memories SET kg_indexed_at = now() WHERE id = $1::text::uuid",
            &[&outcome.id],
        )
        .await?;

    let conflicts = open_conflicts(&client, agent_id, 5).await?;
    Ok(IndexReport {
        memory_id: outcome.id,
        entities: entity_names,
        triples,
        conflicts,
    })
}

/// Index an existing memory row without writing a new one.
pub async fn index_memory(
    db: &Db,
    embedder: &Embedder,
    agent_id: &str,
    memory_id: &str,
) -> Result<IndexReport, KgError> {
    let client = db.client().await?;
    let row = client
        .query_opt(
            "SELECT content FROM memories WHERE id = $1::text::uuid AND agent_id = $2",
            &[&memory_id, &agent_id],
        )
        .await?
        .ok_or_else(|| KgError::EntityNotFound(memory_id.to_string()))?;
    let content: String = row.get("content");
    let known = known_entities(&client, agent_id).await?;
    let triples = extract(&content, &known);
    let evidence =
        Uuid::parse_str(memory_id).map_err(|_| KgError::EntityNotFound(memory_id.to_string()))?;

    let mut entity_names: Vec<String> = Vec::new();
    for triple in &triples {
        let subject_id = resolve_or_create(
            &client,
            embedder,
            agent_id,
            &triple.subject,
            triple.subject_kind.as_deref(),
        )
        .await?;
        let object_id = resolve_or_create(
            &client,
            embedder,
            agent_id,
            &triple.object,
            triple.object_kind.as_deref(),
        )
        .await?;
        upsert_triple(
            &client,
            &TripleWrite {
                agent_id,
                subject_id,
                subject: &triple.subject,
                predicate: &triple.predicate,
                object_id,
                object: &triple.object,
                evidence_id: evidence,
                confidence: 0.9,
                extractor: "rule",
            },
        )
        .await?;
        entity_names.push(triple.subject.clone());
        entity_names.push(triple.object.clone());
    }
    let conflicts = open_conflicts(&client, agent_id, 5).await?;
    Ok(IndexReport {
        memory_id: memory_id.to_string(),
        entities: entity_names,
        triples,
        conflicts,
    })
}

async fn open_conflicts(
    client: &deadpool_postgres::Object,
    agent_id: &str,
    limit: i64,
) -> Result<Vec<Value>, tokio_postgres::Error> {
    let rows = client
        .query(
            "SELECT subject, predicate, array_agg(DISTINCT object) AS objects
             FROM knowledge_graph
             WHERE agent_id = $1 AND valid_to IS NULL
             GROUP BY subject, predicate
             HAVING count(DISTINCT object_norm) > 1
             ORDER BY subject
             LIMIT $2",
            &[&agent_id, &limit],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|row| {
            let objects: Vec<String> = row.get("objects");
            json!({
                "subject": row.get::<_, String>("subject"),
                "predicate": row.get::<_, String>("predicate"),
                "objects": objects,
            })
        })
        .collect())
}

/// Profile of one entity: aliases, edges in and out, facts that mention it.
pub async fn entity_profile(
    db: &Db,
    agent_id: &str,
    name: &str,
    limit: i64,
) -> Result<Value, KgError> {
    let client = db.client().await?;
    let norm = normalize(name);
    let row = client
        .query_opt(
            "SELECT id, canonical, kind, aliases FROM entities
             WHERE agent_id = $1 AND norm = $2 AND merged_into IS NULL",
            &[&agent_id, &norm],
        )
        .await?
        .ok_or_else(|| KgError::EntityNotFound(name.to_string()))?;
    let id: Uuid = row.get("id");
    let canonical: String = row.get("canonical");

    let out_rows = client
        .query(
            "SELECT predicate, object, confidence, evidence_id::text AS evidence
             FROM knowledge_graph
             WHERE agent_id = $1 AND subject_id = $2 AND valid_to IS NULL
             ORDER BY predicate LIMIT $3",
            &[&agent_id, &id, &limit],
        )
        .await?;
    let in_rows = client
        .query(
            "SELECT subject, predicate, confidence, evidence_id::text AS evidence
             FROM knowledge_graph
             WHERE agent_id = $1 AND object_id = $2 AND valid_to IS NULL
             ORDER BY predicate LIMIT $3",
            &[&agent_id, &id, &limit],
        )
        .await?;
    let fact_rows = client
        .query(
            "SELECT id::text AS id, LEFT(content, 200) AS text, importance::float8 AS importance
             FROM memories
             WHERE agent_id = $1 AND content ILIKE '%' || $2 || '%'
             ORDER BY importance DESC NULLS LAST, created_at DESC
             LIMIT $3",
            &[&agent_id, &canonical, &limit],
        )
        .await?;

    let edges_out: Vec<Value> = out_rows
        .iter()
        .map(|row| {
            json!({
                "predicate": row.get::<_, String>("predicate"),
                "object": row.get::<_, String>("object"),
                "confidence": row.get::<_, f32>("confidence"),
                "evidence_id": row.get::<_, Option<String>>("evidence"),
            })
        })
        .collect();
    let edges_in: Vec<Value> = in_rows
        .iter()
        .map(|row| {
            json!({
                "subject": row.get::<_, String>("subject"),
                "predicate": row.get::<_, String>("predicate"),
                "confidence": row.get::<_, f32>("confidence"),
                "evidence_id": row.get::<_, Option<String>>("evidence"),
            })
        })
        .collect();
    let facts: Vec<Value> = fact_rows
        .iter()
        .map(|row| {
            json!({
                "id": row.get::<_, String>("id"),
                "text": row.get::<_, String>("text"),
                "importance": row.get::<_, Option<f64>>("importance"),
            })
        })
        .collect();

    Ok(json!({
        "entity": {
            "id": id.to_string(),
            "name": canonical,
            "kind": row.get::<_, String>("kind"),
            "aliases": row.get::<_, Value>("aliases"),
        },
        "edges_out": edges_out,
        "edges_in": edges_in,
        "facts": facts,
    }))
}

/// Breadth-first traversal, depth capped, cycles impossible.
pub async fn related(
    db: &Db,
    agent_id: &str,
    name: &str,
    depth: i64,
    predicates: &[String],
    limit: i64,
) -> Result<Value, KgError> {
    let client = db.client().await?;
    let norm = normalize(name);
    let row = client
        .query_opt(
            "SELECT id, canonical FROM entities
             WHERE agent_id = $1 AND norm = $2 AND merged_into IS NULL",
            &[&agent_id, &norm],
        )
        .await?
        .ok_or_else(|| KgError::EntityNotFound(name.to_string()))?;
    let root: Uuid = row.get("id");
    let root_name: String = row.get("canonical");

    let depth = depth.clamp(1, 3);
    let limit = limit.clamp(1, 200);
    let mut frontier: Vec<Uuid> = vec![root];
    let mut visited: BTreeSet<Uuid> = BTreeSet::from([root]);
    let mut edges: Vec<Value> = Vec::new();

    for level in 1..=depth {
        if frontier.is_empty() || edges.len() as i64 >= limit {
            break;
        }
        let rows = client
            .query(
                "SELECT subject_id, subject, predicate, object_id, object, confidence
                 FROM knowledge_graph
                 WHERE agent_id = $1 AND valid_to IS NULL
                   AND (subject_id = ANY($2) OR object_id = ANY($2))
                   AND ($3::text[] IS NULL OR predicate = ANY($3))
                 ORDER BY confidence DESC
                 LIMIT $4",
                &[
                    &agent_id,
                    &frontier,
                    &optional_predicates(predicates),
                    &limit,
                ],
            )
            .await?;
        let mut next: Vec<Uuid> = Vec::new();
        for row in rows {
            let subject_id: Uuid = row.get("subject_id");
            let object_id: Uuid = row.get("object_id");
            edges.push(json!({
                "depth": level,
                "subject": row.get::<_, String>("subject"),
                "predicate": row.get::<_, String>("predicate"),
                "object": row.get::<_, String>("object"),
                "confidence": row.get::<_, f32>("confidence"),
            }));
            for candidate in [subject_id, object_id] {
                if visited.insert(candidate) {
                    next.push(candidate);
                }
            }
            if edges.len() as i64 >= limit {
                break;
            }
        }
        frontier = next;
    }

    Ok(json!({
        "root": root_name,
        "depth": depth,
        "edges": edges,
        "truncated": edges.len() as i64 >= limit,
    }))
}

fn optional_predicates(predicates: &[String]) -> Option<Vec<String>> {
    if predicates.is_empty() {
        None
    } else {
        Some(predicates.to_vec())
    }
}

/// Open contradictions: one (subject, predicate) with more than one live object.
pub async fn conflicts(db: &Db, agent_id: &str, limit: i64) -> Result<Value, KgError> {
    let client = db.client().await?;
    let rows = open_conflicts(&client, agent_id, limit.clamp(1, 100)).await?;
    Ok(json!({ "conflicts": rows, "count": rows.len() }))
}

#[derive(Debug, Serialize)]
pub struct ExtractReport {
    pub scanned: usize,
    pub triples: usize,
    pub entities: usize,
    pub conflicts: usize,
    pub errors: Vec<String>,
}

/// Batch extraction over memories that were never indexed. Each row is marked once, so a second
/// run scans only what is new.
pub async fn extract_batch(
    db: &Db,
    embedder: &Embedder,
    agent_id: &str,
    batch: i64,
    since: chrono::DateTime<chrono::Utc>,
) -> Result<ExtractReport, KgError> {
    let batch = batch.clamp(1, 256);
    let client = db.client().await?;
    let rows = client
        .query(
            "SELECT id::text AS id, content FROM memories
             WHERE agent_id = $1 AND kg_indexed_at IS NULL AND created_at >= $3
             ORDER BY created_at
             LIMIT $2",
            &[&agent_id, &batch, &since],
        )
        .await?;
    let known = known_entities(&client, agent_id).await?;

    let mut report = ExtractReport {
        scanned: rows.len(),
        triples: 0,
        entities: 0,
        conflicts: 0,
        errors: Vec::new(),
    };
    let mut touched: BTreeSet<String> = BTreeSet::new();
    let confidence = 0.9_f32;
    let extractor = "rule";

    for row in &rows {
        let id: String = row.get("id");
        let content: String = row.get("content");
        let Ok(evidence) = Uuid::parse_str(&id) else {
            continue;
        };
        let triples = extract(&content, &known);
        for triple in triples {
            let subject_id = resolve_or_create(
                &client,
                embedder,
                agent_id,
                &triple.subject,
                triple.subject_kind.as_deref(),
            )
            .await?;
            let object_id = resolve_or_create(
                &client,
                embedder,
                agent_id,
                &triple.object,
                triple.object_kind.as_deref(),
            )
            .await?;
            upsert_triple(
                &client,
                &TripleWrite {
                    agent_id,
                    subject_id,
                    subject: &triple.subject,
                    predicate: &triple.predicate,
                    object_id,
                    object: &triple.object,
                    evidence_id: evidence,
                    confidence,
                    extractor,
                },
            )
            .await?;
            report.triples += 1;
            touched.insert(triple.subject);
            touched.insert(triple.object);
        }
        client
            .execute(
                "UPDATE memories SET kg_indexed_at = now() WHERE id = $1::text::uuid",
                &[&id],
            )
            .await?;
    }

    report.entities = touched.len();
    report.conflicts = open_conflicts(&client, agent_id, 100).await?.len();
    tracing::info!(
        scanned = report.scanned,
        triples = report.triples,
        "kg.extract"
    );
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known() -> Vec<KnownEntity> {
        vec![
            KnownEntity {
                canonical: "Phylactery".to_string(),
                norm: "phylactery".to_string(),
                kind: "project".to_string(),
                aliases: vec!["the phylactery".to_string()],
            },
            KnownEntity {
                canonical: "Postgres".to_string(),
                norm: "postgres".to_string(),
                kind: "service".to_string(),
                aliases: vec![],
            },
        ]
    }

    #[test]
    fn normalize_strips_punctuation() {
        assert_eq!(
            normalize("  The  Phylactery, v2.0!  "),
            "the phylactery v2.0"
        );
    }

    #[test]
    fn known_pair_yields_a_triple() {
        let triples = extract("The phylactery runs on Postgres", &known());
        assert_eq!(triples.len(), 1);
        assert_eq!(triples[0].predicate, "runs_on");
        assert_eq!(triples[0].subject, "Phylactery");
        assert_eq!(triples[0].object, "Postgres");
    }

    #[test]
    fn no_pattern_no_triple() {
        assert!(extract("The phylactery is beautiful today", &known()).is_empty());
    }

    #[test]
    fn unknown_subject_is_typed_as_concept() {
        let triples = extract("Keter is a project", &known());
        assert_eq!(triples.len(), 1);
        assert_eq!(triples[0].predicate, "is_a");
        assert_eq!(triples[0].subject_kind.as_deref(), Some("concept"));
        assert_eq!(triples[0].object_kind.as_deref(), Some("project"));
    }

    #[test]
    fn russian_patterns_work() {
        let triples = extract("Phylactery использует Postgres", &known());
        assert_eq!(triples.len(), 1);
        assert_eq!(triples[0].predicate, "uses");
    }

    #[test]
    fn hash_is_stable_and_32_hex() {
        let id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let first = triple_hash("agent", &id, "uses", "postgres");
        let second = triple_hash("agent", &id, "uses", "postgres");
        assert_eq!(first, second);
        assert_eq!(first.len(), 32);
        assert_ne!(first, triple_hash("agent", &id, "uses", "sqlite"));
    }
}
