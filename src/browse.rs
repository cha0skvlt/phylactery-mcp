//! Browse: `agents` (census of namespaces) and `list` (non-semantic browse).
//!
//! Both are reads. Neither embeds, neither accepts SQL, a table or a column name: the only
//! caller-dependent parts of a statement are bind parameters and one of three fixed orderings.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_postgres::types::ToSql;
use uuid::Uuid;

use crate::circuit::Circuit;
use crate::db::Db;
use crate::surgical::{SurgicalError, stamp};

const DEFAULT_LIMIT: usize = 20;
const MAX_LIMIT: usize = 200;
/// `list` returns a preview; `get` returns the whole text.
const TEXT_PREVIEW_CHARS: usize = 300;

// ---------------------------------------------------------------------------------- agents

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct AgentsParams {
    /// Also report the server's own namespace when it holds no rows yet.
    #[serde(default)]
    pub include_empty: bool,
}

const AGENTS_SQL: &str = "\
WITH u AS (
    SELECT 'memories' AS c, agent_id::text AS a, created_at, (embedding IS NULL) AS nv
        FROM memories
    UNION ALL
    SELECT 'diary', agent_id::text, created_at, (embedding IS NULL) FROM self_reflections
    UNION ALL
    SELECT 'lessons', agent_id::text, created_at, (embedding IS NULL) FROM causal_lessons
    UNION ALL
    SELECT 'mind', agent_id::text, created_at, (embedding IS NULL) FROM mind_events
)
SELECT a,
       count(*) FILTER (WHERE c = 'memories') AS memories,
       count(*) FILTER (WHERE c = 'diary') AS diary,
       count(*) FILTER (WHERE c = 'lessons') AS lessons,
       count(*) FILTER (WHERE c = 'mind') AS mind,
       count(*) FILTER (WHERE nv) AS without_vector,
       min(created_at) AS first_at,
       max(created_at) AS last_at
FROM u
GROUP BY a
ORDER BY a";

/// One grouped query over the four circuits: who lives here, and how much.
pub async fn agents(db: &Db, own: &str, params: &AgentsParams) -> Result<Value, SurgicalError> {
    let client = db.client().await?;
    let rows = client.query(AGENTS_SQL, &[]).await?;
    let mut agents: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "agent_id": row.get::<_, Option<String>>("a"),
                "memories": row.get::<_, i64>("memories"),
                "diary": row.get::<_, i64>("diary"),
                "lessons": row.get::<_, i64>("lessons"),
                "mind": row.get::<_, i64>("mind"),
                "without_vector": row.get::<_, i64>("without_vector"),
                "first_at": row.get::<_, Option<DateTime<Utc>>>("first_at").map(stamp),
                "last_at": row.get::<_, Option<DateTime<Utc>>>("last_at").map(stamp),
            })
        })
        .collect();
    let own_present = agents.iter().any(|agent| agent["agent_id"] == own);
    if params.include_empty && !own_present {
        agents.push(json!({
            "agent_id": own, "memories": 0, "diary": 0, "lessons": 0, "mind": 0,
            "without_vector": 0, "first_at": null, "last_at": null,
        }));
    }
    Ok(json!({ "agents": agents, "total": agents.len(), "self_agent": own }))
}

// ------------------------------------------------------------------------------------ list

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ListParams {
    /// Circuit: memories, diary, lessons or mind. Omitted: all four.
    #[serde(default)]
    pub circuit: Option<String>,
    /// Namespace. Default: the server's own. "*" reads every namespace (reads only).
    #[serde(default)]
    pub agent_id: Option<String>,
    /// memory_type for memories, source for diary, kind for mind, "lesson" for lessons.
    #[serde(default)]
    pub register: Option<String>,
    /// Only rows with importance >= this (mind has no importance and is excluded).
    #[serde(default)]
    pub importance_min: Option<f64>,
    /// RFC 3339; rows created at or after.
    #[serde(default)]
    pub since: Option<String>,
    /// RFC 3339; rows created strictly before.
    #[serde(default)]
    pub until: Option<String>,
    /// true: only rows with a vector; false: only rows without one.
    #[serde(default)]
    pub has_vector: Option<bool>,
    #[serde(default)]
    pub episode_id: Option<String>,
    /// mind_events.thread_id; other circuits never match.
    #[serde(default)]
    pub thread_id: Option<String>,
    /// created_at_desc (default), created_at_asc or importance_desc.
    #[serde(default)]
    pub order: Option<String>,
    /// Rows per page, 1..=200, default 20.
    #[serde(default)]
    pub limit: Option<usize>,
    /// `next_cursor` of the previous page; pass it back unchanged with the same filters.
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Order {
    CreatedDesc,
    CreatedAsc,
    ImportanceDesc,
}

impl Order {
    fn parse(raw: &str) -> Result<Order, SurgicalError> {
        match raw.trim() {
            "created_at_desc" => Ok(Order::CreatedDesc),
            "created_at_asc" => Ok(Order::CreatedAsc),
            "importance_desc" => Ok(Order::ImportanceDesc),
            other => Err(SurgicalError::InvalidValue(format!(
                "order must be created_at_desc, created_at_asc or importance_desc, got {other:?}"
            ))),
        }
    }

    /// Keyset predicate. `$10`/`$11` are the cursor's time and id, `$13` its sort key.
    fn after_cursor(self) -> &'static str {
        match self {
            Order::CreatedDesc => {
                "($10::timestamptz IS NULL OR (m.created_at, m.id) < ($10::timestamptz, $11::uuid))"
            }
            Order::CreatedAsc => {
                "($10::timestamptz IS NULL OR (m.created_at, m.id) > ($10::timestamptz, $11::uuid))"
            }
            Order::ImportanceDesc => {
                "($10::timestamptz IS NULL OR (COALESCE(m.importance, -1::float8), m.created_at, m.id) \
                 < ($13::float8, $10::timestamptz, $11::uuid))"
            }
        }
    }

    fn order_by(self) -> &'static str {
        match self {
            Order::CreatedDesc => "ORDER BY m.created_at DESC, m.id DESC",
            Order::CreatedAsc => "ORDER BY m.created_at ASC, m.id ASC",
            Order::ImportanceDesc => {
                "ORDER BY COALESCE(m.importance, -1::float8) DESC, m.created_at DESC, m.id DESC"
            }
        }
    }
}

/// Position of the last row of a page. Opaque to callers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cursor {
    ts: DateTime<Utc>,
    id: Uuid,
    key: f64,
}

impl Cursor {
    pub fn encode(&self) -> String {
        format!(
            "v1.{}.{}.{:016x}",
            self.ts.timestamp_micros(),
            self.id,
            self.key.to_bits()
        )
    }

    pub fn decode(raw: &str) -> Result<Cursor, SurgicalError> {
        let parts: Vec<&str> = raw.trim().split('.').collect();
        let [version, micros, id, key] = parts.as_slice() else {
            return Err(SurgicalError::InvalidCursor);
        };
        if *version != "v1" {
            return Err(SurgicalError::InvalidCursor);
        }
        let ts = micros
            .parse::<i64>()
            .ok()
            .and_then(DateTime::from_timestamp_micros)
            .ok_or(SurgicalError::InvalidCursor)?;
        let id = Uuid::parse_str(id).map_err(|_| SurgicalError::InvalidCursor)?;
        let key = u64::from_str_radix(key, 16)
            .map(f64::from_bits)
            .ok()
            .filter(|key| key.is_finite())
            .ok_or(SurgicalError::InvalidCursor)?;
        Ok(Cursor { ts, id, key })
    }
}

/// `ListParams` after every check; nothing here can still fail validation.
#[derive(Debug)]
pub struct Validated {
    circuit: Option<&'static str>,
    agent: Option<String>,
    register: Option<String>,
    importance_min: Option<f64>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    has_vector: Option<bool>,
    episode: Option<String>,
    thread: Option<String>,
    order: Order,
    limit: usize,
    cursor: Option<Cursor>,
}

fn present(raw: &Option<String>) -> Option<String> {
    raw.as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn parse_time(field: &str, raw: &Option<String>) -> Result<Option<DateTime<Utc>>, SurgicalError> {
    match present(raw) {
        None => Ok(None),
        Some(text) => DateTime::parse_from_rfc3339(&text)
            .map(|stamp| Some(stamp.with_timezone(&Utc)))
            .map_err(|error| {
                SurgicalError::InvalidValue(format!("{field} is not RFC 3339: {error}"))
            }),
    }
}

impl ListParams {
    pub fn validate(&self, own: &str) -> Result<Validated, SurgicalError> {
        let circuit = match present(&self.circuit) {
            None => None,
            Some(name) => Some(
                Circuit::parse(&name)
                    .ok_or(SurgicalError::Unsupported(
                        "circuit must be memories, diary, lessons or mind",
                    ))?
                    .label(),
            ),
        };
        let agent = match present(&self.agent_id).as_deref() {
            None => Some(own.to_string()),
            Some("*") => None,
            Some(name) => Some(name.to_string()),
        };
        if self.importance_min.is_some_and(|min| !min.is_finite()) {
            return Err(SurgicalError::InvalidValue(
                "importance_min must be a finite number".into(),
            ));
        }
        let since = parse_time("since", &self.since)?;
        let until = parse_time("until", &self.until)?;
        if matches!((since, until), (Some(from), Some(to)) if from >= to) {
            return Err(SurgicalError::InvalidValue(
                "since must be earlier than until".into(),
            ));
        }
        let order = match present(&self.order) {
            None => Order::CreatedDesc,
            Some(raw) => Order::parse(&raw)?,
        };
        let cursor = match present(&self.cursor) {
            None => None,
            Some(raw) => Some(Cursor::decode(&raw)?),
        };
        Ok(Validated {
            circuit,
            agent,
            register: present(&self.register),
            importance_min: self.importance_min,
            since,
            until,
            has_vector: self.has_vector,
            episode: present(&self.episode_id),
            thread: present(&self.thread_id),
            order,
            limit: self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT),
            cursor,
        })
    }
}

macro_rules! has_vector {
    () => {
        "(mem.embedding IS NOT NULL OR d.embedding IS NOT NULL \
          OR l.embedding IS NOT NULL OR e.embedding IS NOT NULL)"
    };
}

/// `memory_map` has no vector flag, metadata or thread: join back to the owning tables.
const LIST_SELECT: &str = concat!(
    "SELECT m.circuit, m.id::text AS id, m.agent_id::text AS agent_id,
            m.register::text AS register, m.content, m.importance, m.created_at, m.episode_id, ",
    has_vector!(),
    " AS has_vector,
            COALESCE(mem.metadata, '{}'::jsonb) AS metadata,
            COALESCE(mem.metadata->>'source', d.source::text) AS source,
            e.thread_id::text AS thread_id
     FROM memory_map m
     LEFT JOIN memories mem ON m.circuit = 'memories' AND mem.id = m.id
     LEFT JOIN self_reflections d ON m.circuit = 'diary' AND d.id = m.id
     LEFT JOIN causal_lessons l ON m.circuit = 'lessons' AND l.id = m.id
     LEFT JOIN mind_events e ON m.circuit = 'mind' AND e.id = m.id
     WHERE ($1::text IS NULL OR m.circuit = $1::text)
       AND ($2::text IS NULL OR m.agent_id::text = $2::text)
       AND ($3::text IS NULL OR m.register::text = $3::text)
       AND ($4::float8 IS NULL OR m.importance >= $4::float8)
       AND ($5::timestamptz IS NULL OR m.created_at >= $5::timestamptz)
       AND ($6::timestamptz IS NULL OR m.created_at < $6::timestamptz)
       AND ($7::bool IS NULL OR ",
    has_vector!(),
    " = $7::bool)
       AND ($8::text IS NULL OR m.episode_id = $8::text)
       AND ($9::text IS NULL OR e.thread_id::text = $9::text)
       AND "
);

fn preview(text: &str) -> (String, bool) {
    let mut chars = text.chars();
    let head: String = chars.by_ref().take(TEXT_PREVIEW_CHARS).collect();
    let clipped = chars.next().is_some();
    (head, clipped)
}

/// Non-semantic browse. Never embeds.
pub async fn list(db: &Db, own: &str, params: &ListParams) -> Result<Value, SurgicalError> {
    let valid = params.validate(own)?;
    let sql = [
        LIST_SELECT,
        valid.order.after_cursor(),
        " ",
        valid.order.order_by(),
        " LIMIT $12",
    ]
    .concat();

    let cursor_ts = valid.cursor.map(|cursor| cursor.ts);
    let cursor_id = valid.cursor.map(|cursor| cursor.id);
    let cursor_key = valid.cursor.map(|cursor| cursor.key);
    let fetch = valid.limit as i64 + 1;
    let mut binds: Vec<&(dyn ToSql + Sync)> = vec![
        &valid.circuit,
        &valid.agent,
        &valid.register,
        &valid.importance_min,
        &valid.since,
        &valid.until,
        &valid.has_vector,
        &valid.episode,
        &valid.thread,
        &cursor_ts,
        &cursor_id,
        &fetch,
    ];
    if valid.order == Order::ImportanceDesc {
        binds.push(&cursor_key);
    }

    let client = db.client().await?;
    let mut rows = client.query(&sql, &binds).await?;
    let truncated = rows.len() > valid.limit;
    rows.truncate(valid.limit);

    let mut out = Vec::with_capacity(rows.len());
    let mut last: Option<Cursor> = None;
    for row in &rows {
        let created_at: DateTime<Utc> = row.get("created_at");
        let importance: Option<f64> = row.get("importance");
        let id: String = row.get("id");
        if let Ok(uuid) = Uuid::parse_str(&id) {
            last = Some(Cursor {
                ts: created_at,
                id: uuid,
                key: importance.unwrap_or(-1.0),
            });
        }
        let content: Option<String> = row.get("content");
        let (text, text_truncated) = preview(content.as_deref().unwrap_or_default());
        out.push(json!({
            "circuit": row.get::<_, String>("circuit"),
            "id": id,
            "agent_id": row.get::<_, Option<String>>("agent_id"),
            "register": row.get::<_, Option<String>>("register"),
            "text": text,
            "text_truncated": text_truncated,
            "importance": importance,
            "created_at": stamp(created_at),
            "episode_id": row.get::<_, Option<String>>("episode_id"),
            "has_vector": row.get::<_, bool>("has_vector"),
            "metadata": row.get::<_, Value>("metadata"),
            "source": row.get::<_, Option<String>>("source"),
            "thread_id": row.get::<_, Option<String>>("thread_id"),
        }));
    }
    let next_cursor = if truncated {
        last.map(|cursor| cursor.encode())
    } else {
        None
    };
    Ok(json!({ "rows": out, "next_cursor": next_cursor, "truncated": truncated }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> ListParams {
        ListParams::default()
    }

    #[test]
    fn cursor_round_trips() {
        let cursor = Cursor {
            ts: DateTime::from_timestamp_micros(1_763_186_404_123_456).unwrap(),
            id: Uuid::parse_str("d7d4f4cc-1234-4abc-8def-0123456789ab").unwrap(),
            key: 8.5,
        };
        assert_eq!(Cursor::decode(&cursor.encode()).unwrap(), cursor);
        let negative = Cursor {
            key: -1.0,
            ..cursor
        };
        assert_eq!(Cursor::decode(&negative.encode()).unwrap(), negative);
    }

    #[test]
    fn foreign_cursors_are_refused() {
        let good = "v1.1763186404123456.d7d4f4cc-1234-4abc-8def-0123456789ab.4021000000000000";
        assert!(Cursor::decode(good).is_ok());
        for bad in [
            "",
            "garbage",
            "v2.1.d7d4f4cc-1234-4abc-8def-0123456789ab.0000000000000000",
            "v1.x.d7d4f4cc-1234-4abc-8def-0123456789ab.0000000000000000",
            "v1.1.not-a-uuid.0000000000000000",
            "v1.1.d7d4f4cc-1234-4abc-8def-0123456789ab.zz",
            // NaN bit pattern
            "v1.1.d7d4f4cc-1234-4abc-8def-0123456789ab.7ff8000000000000",
        ] {
            assert_eq!(
                Cursor::decode(bad).unwrap_err().code(),
                "invalid_cursor",
                "{bad:?}"
            );
        }
    }

    #[test]
    fn agent_default_own_star_all_named_as_given() {
        assert_eq!(
            params().validate("me").unwrap().agent.as_deref(),
            Some("me")
        );
        let star = ListParams {
            agent_id: Some(" * ".into()),
            ..params()
        };
        assert_eq!(star.validate("me").unwrap().agent, None);
        let named = ListParams {
            agent_id: Some("other".into()),
            ..params()
        };
        assert_eq!(
            named.validate("me").unwrap().agent.as_deref(),
            Some("other")
        );
    }

    #[test]
    fn limit_is_clamped() {
        let limit = |value| {
            ListParams {
                limit: Some(value),
                ..params()
            }
            .validate("me")
            .unwrap()
            .limit
        };
        assert_eq!(limit(0), 1);
        assert_eq!(limit(50), 50);
        assert_eq!(limit(10_000), MAX_LIMIT);
        assert_eq!(params().validate("me").unwrap().limit, DEFAULT_LIMIT);
    }

    #[test]
    fn bad_filters_carry_a_code() {
        let code = |p: ListParams| p.validate("me").unwrap_err().code();
        assert_eq!(
            code(ListParams {
                circuit: Some("pg_catalog".into()),
                ..params()
            }),
            "unsupported_table"
        );
        assert_eq!(
            code(ListParams {
                order: Some("random".into()),
                ..params()
            }),
            "invalid_value"
        );
        assert_eq!(
            code(ListParams {
                since: Some("yesterday".into()),
                ..params()
            }),
            "invalid_value"
        );
        assert_eq!(
            code(ListParams {
                cursor: Some("nope".into()),
                ..params()
            }),
            "invalid_cursor"
        );
        assert_eq!(
            code(ListParams {
                since: Some("2026-11-15T00:00:00Z".into()),
                until: Some("2026-11-14T00:00:00Z".into()),
                ..params()
            }),
            "invalid_value"
        );
        assert_eq!(
            code(ListParams {
                importance_min: Some(f64::NAN),
                ..params()
            }),
            "invalid_value"
        );
    }

    #[test]
    fn circuit_aliases_resolve_to_labels() {
        let p = ListParams {
            circuit: Some("self_reflections".into()),
            ..params()
        };
        assert_eq!(p.validate("me").unwrap().circuit, Some("diary"));
    }

    #[test]
    fn blank_filters_mean_absent() {
        let p = ListParams {
            register: Some("  ".into()),
            episode_id: Some("".into()),
            ..params()
        };
        let v = p.validate("me").unwrap();
        assert_eq!(v.register, None);
        assert_eq!(v.episode, None);
    }

    #[test]
    fn preview_clips_on_char_boundaries() {
        let long = "ж".repeat(TEXT_PREVIEW_CHARS + 5);
        let (head, clipped) = preview(&long);
        assert!(clipped);
        assert_eq!(head.chars().count(), TEXT_PREVIEW_CHARS);
        assert_eq!(preview("short"), ("short".to_string(), false));
    }

    #[test]
    fn statements_hold_no_caller_text() {
        for sql in [LIST_SELECT, AGENTS_SQL] {
            assert!(!sql.contains(';'));
        }
        for order in [Order::CreatedDesc, Order::CreatedAsc, Order::ImportanceDesc] {
            assert!(order.after_cursor().contains("$10"));
            assert!(!order.order_by().contains('$'));
        }
    }
}
