//! `ask` — one call, one answer.
//!
//! The agent should not chain ten tools to learn one thing. This module runs the whole retrieval
//! plan inside the server: vector ANN over the four circuits, entity detection, graph expansion,
//! conflicts, timeline, ranking, deduplication and a token budget. The result is a finished answer
//! pack with citations, and an honest `truncated` flag when the budget cut something.

use std::collections::BTreeSet;

use serde_json::{Value, json};
use uuid::Uuid;

use crate::circuit::{self, Circuit, Hit};
use crate::db::Db;
use crate::embed::{EmbedError, Embedder};
use crate::math;

pub const DEFAULT_BUDGET_TOKENS: usize = 1200;
pub const MAX_BUDGET_TOKENS: usize = 4000;
const FACT_LIMIT: usize = 20;
const ENTITY_LIMIT: usize = 5;
const RELATION_LIMIT: i64 = 60;
const TIMELINE_LIMIT: i64 = 10;
const CONFLICT_LIMIT: i64 = 5;

#[derive(Debug, thiserror::Error)]
pub enum AskError {
    #[error(transparent)]
    Db(#[from] crate::db::DbError),
    #[error(transparent)]
    Embed(#[from] EmbedError),
    #[error(transparent)]
    Sql(#[from] tokio_postgres::Error),
}

impl AskError {
    pub fn code(&self) -> &'static str {
        match self {
            AskError::Db(_) | AskError::Sql(_) => "db_unavailable",
            AskError::Embed(_) => "embed_unavailable",
        }
    }
}

/// Rough token estimate: four characters per token. Deliberately conservative.
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

#[derive(Debug, Clone)]
struct MatchedEntity {
    id: Uuid,
    name: String,
    kind: String,
    aliases: Vec<String>,
}

async fn detect_entities(
    db: &Db,
    agent_id: &str,
    question: &str,
) -> Result<Vec<Value>, crate::db::DbError> {
    let client = db.client().await?;
    let rows = client
        .query(
            "SELECT id, canonical, kind, aliases FROM entities
             WHERE agent_id = $1 AND merged_into IS NULL
             ORDER BY length(canonical) DESC LIMIT 2000",
            &[&agent_id],
        )
        .await?;
    let lowered = question.to_lowercase();
    let mut matched: Vec<MatchedEntity> = Vec::new();
    for row in rows {
        let canonical: String = row.get("canonical");
        let kind: String = row.get("kind");
        let aliases: Value = row.get("aliases");
        let aliases: Vec<String> = aliases
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let hit = std::iter::once(&canonical)
            .chain(aliases.iter())
            .any(|name| {
                let needle = name.to_lowercase();
                needle.len() >= 3 && lowered.contains(&needle)
            });
        if hit {
            matched.push(MatchedEntity {
                id: row.get("id"),
                name: canonical,
                kind,
                aliases,
            });
            if matched.len() >= ENTITY_LIMIT {
                break;
            }
        }
    }
    Ok(matched
        .into_iter()
        .map(|entity| {
            json!({
                "id": entity.id.to_string(),
                "name": entity.name,
                "kind": entity.kind,
                "aliases": entity.aliases,
            })
        })
        .collect())
}

async fn expand(
    db: &Db,
    agent_id: &str,
    entity_ids: &[Uuid],
    depth: i64,
    include_legacy: bool,
) -> Result<Vec<Value>, AskError> {
    if entity_ids.is_empty() {
        return Ok(Vec::new());
    }
    let client = db.client().await?;
    let mut frontier: Vec<Uuid> = entity_ids.to_vec();
    let mut visited: BTreeSet<Uuid> = entity_ids.iter().copied().collect();
    let mut seen_edges: BTreeSet<(String, String, String)> = BTreeSet::new();
    let mut edges: Vec<Value> = Vec::new();

    for level in 1..=depth {
        if frontier.is_empty() || edges.len() as i64 >= RELATION_LIMIT {
            break;
        }
        let rows = client
            .query(
                "SELECT subject_id, subject, predicate, object_id, object, confidence,
                        extractor, evidence_id::text AS evidence
                 FROM knowledge_graph
                 WHERE agent_id = $1 AND valid_to IS NULL
                   AND (subject_id = ANY($2) OR object_id = ANY($2))
                   AND ($3 OR extractor <> 'legacy')
                 ORDER BY confidence DESC
                 LIMIT $4",
                &[&agent_id, &frontier, &include_legacy, &RELATION_LIMIT],
            )
            .await?;
        let mut next: Vec<Uuid> = Vec::new();
        for row in rows {
            let subject_id: Uuid = row.get("subject_id");
            let object_id: Uuid = row.get("object_id");
            let subject: String = row.get("subject");
            let predicate: String = row.get("predicate");
            let object: String = row.get("object");
            if !seen_edges.insert((subject.clone(), predicate.clone(), object.clone())) {
                continue;
            }
            edges.push(json!({
                "depth": level,
                "subject": subject,
                "predicate": predicate,
                "object": object,
                "confidence": row.get::<_, f32>("confidence"),
                "evidence_id": row.get::<_, Option<String>>("evidence"),
            }));
            for candidate in [subject_id, object_id] {
                if visited.insert(candidate) {
                    next.push(candidate);
                }
            }
            if edges.len() as i64 >= RELATION_LIMIT {
                break;
            }
        }
        frontier = next;
    }
    Ok(edges)
}

fn hit_json(hit: &Hit) -> Value {
    json!({
        "id": hit.id,
        "text": hit.content,
        "source": hit.source,
        "score": hit.score,
        "similarity": hit.similarity,
        "importance": hit.importance,
        "created_at": hit.created_at,
    })
}

/// The whole retrieval plan, one call.
pub async fn ask(
    db: &Db,
    embedder: &Embedder,
    agent_id: &str,
    question: &str,
    budget_tokens: usize,
    include_legacy: bool,
) -> Result<Value, AskError> {
    let budget = budget_tokens.clamp(100, MAX_BUDGET_TOKENS);

    let hits = circuit::search(
        db,
        embedder,
        agent_id,
        question,
        &Circuit::ALL,
        FACT_LIMIT,
        math::SIMILARITY_FLOOR,
    )
    .await
    .map_err(|error| match error {
        circuit::SearchError::Db(error) => AskError::Db(error),
        circuit::SearchError::Embed(error) => AskError::Embed(error),
    })?;

    let entities = detect_entities(db, agent_id, question).await?;
    let entity_ids: Vec<Uuid> = entities
        .iter()
        .filter_map(|entity| entity["id"].as_str())
        .filter_map(|id| Uuid::parse_str(id).ok())
        .collect();
    let relations = expand(db, agent_id, &entity_ids, 2, include_legacy).await?;
    let timeline = circuit::timeline_filtered(db, agent_id, TIMELINE_LIMIT, None, None).await?;
    let conflicts = {
        let client = db.client().await?;
        let rows = client
            .query(
                "SELECT subject, predicate, array_agg(DISTINCT object) AS objects
                 FROM knowledge_graph
                 WHERE agent_id = $1 AND valid_from IS NOT NULL AND valid_to IS NULL
                   AND ($3 OR extractor <> 'legacy')
                 GROUP BY subject, predicate
                 HAVING count(DISTINCT object_norm) > 1
                 ORDER BY subject
                 LIMIT $2",
                &[&agent_id, &CONFLICT_LIMIT, &include_legacy],
            )
            .await?;
        rows.iter()
            .map(|row| {
                let objects: Vec<String> = row.get("objects");
                json!({
                    "subject": row.get::<_, String>("subject"),
                    "predicate": row.get::<_, String>("predicate"),
                    "objects": objects,
                })
            })
            .collect::<Vec<Value>>()
    };

    // Assemble under the budget: facts first, then relations, then timeline.
    let mut used = estimate_tokens(question);
    let mut facts: Vec<Value> = Vec::new();
    let mut citations: Vec<String> = Vec::new();
    let mut truncated = false;

    for hit in &hits {
        let cost = estimate_tokens(&hit.content) + 8;
        if used + cost > budget {
            truncated = true;
            break;
        }
        used += cost;
        if !citations.contains(&hit.id) {
            citations.push(hit.id.clone());
        }
        facts.push(hit_json(hit));
    }

    let mut kept_relations: Vec<Value> = Vec::new();
    for relation in relations {
        let cost = 12;
        if used + cost > budget {
            truncated = true;
            break;
        }
        used += cost;
        if let Some(evidence) = relation["evidence_id"].as_str()
            && !citations.contains(&evidence.to_string())
        {
            citations.push(evidence.to_string());
        }
        kept_relations.push(relation);
    }

    let mut kept_timeline: Vec<Value> = Vec::new();
    for row in timeline {
        let cost = estimate_tokens(&row.content) + 6;
        if used + cost > budget {
            truncated = true;
            break;
        }
        used += cost;
        kept_timeline.push(json!({
            "at": row.created_at,
            "kind": row.kind,
            "channel": row.channel,
            "text": row.content,
        }));
    }

    Ok(json!({
        "question": question,
        "facts": facts,
        "relations": kept_relations,
        "entities": entities,
        "timeline": kept_timeline,
        "conflicts": conflicts,
        "citations": citations,
        "truncated": truncated,
        "budget_tokens": budget,
        "used_tokens": used,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_is_conservative() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens("привет"), 2);
    }
}
