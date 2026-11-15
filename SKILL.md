---
name: phylactery
description: "Memory of the phylactery: four Postgres circuits plus a typed knowledge graph, exposed as MCP tools mcp:phylactery:*. Use for recall, search, save, remember, lesson, diary, mind_log, mind_tail, entity, related, conflicts, stats, doctor, get, list, agents, patch, adopt, merge, history, reembed, forget, replace, backfill, extract, ask, backup, verify, manifest, migrate, maintain."
---

# Phylactery — operating rules

The phylactery is a Postgres database. It holds four memory circuits, one 768d vector space and a
typed knowledge graph. You touch it through exactly one channel: the MCP server `phylactery`
(`phylactery-mcp`), whose tools appear as `mcp:phylactery:<tool>`.

## 1. One channel. Nothing else exists.

Every read and every write goes through `mcp:phylactery:*`. There is no second channel and no other
way in: not a shell, not a script, not a direct table read.

If a question about memory cannot be answered with a tool from §3, the answer is `doctor` plus a
report to the operator. It is never another channel.

The tools are deterministic. Same arguments, same shape of answer. Do not invent a workaround when
a tool refuses — read the refusal, fix the argument, or stop and report.

## 2. Four circuits

| Circuit | Holds |
| --- | --- |
| `memories` | facts, identity anchors, standing rules |
| `diary` | the self speaking about itself |
| `lessons` | "stopped X because Y" |
| `mind` | timeline, append-only |

Every row has an embedding in one 768d vector space. A row without a vector does not exist for
search.

`memory_type` is closed: `semantic`, `identity`, `lesson`, `experience`, `unverified`.
`self_reflections.source` is closed: `solitary`, `outreach`, `reflect`, `seed`.
Identity rows are protected — no update, no delete, ever.

The retired engine's tables live in schema `archive`. `public` holds only what is alive.

## 3. Decision table — read this, then act

| You need | Call | Arguments |
| --- | --- | --- |
| Start a thread | `ask` | `{"question": "<question>", "budget_tokens": 1200}` |
| context at thread start | `recall` | `{"query": "<topic>", "limit": 8, "timeline": 5}` |
| one precise answer | `search` | `{"query": "<topic>", "limit": 5, "sources": ["memories","diary","lessons"]}` |
| store a fact | `save` | `{"text": "<fact>", "memory_type": "semantic", "importance": 0.5}` |
| store a fact **with relations** | `remember` | `{"text": "Phylactery runs on Postgres", "importance": 0.6}` |
| what is known about X | `entity` | `{"name": "Phylactery"}` |
| how things connect | `related` | `{"name": "Phylactery", "depth": 2}` |
| does the graph contradict itself | `conflicts` | `{}` |
| store a rule learned from a mistake | `lesson` | `{"change": "<stopped X>", "because": "<because Y>", "importance": 9}` |
| write a diary entry | `diary` | `{"text": "<entry>", "importance": 0.5}` |
| append an event to timeline | `mind_log` | `{"content": "<event>", "kind": "agent_out", "channel": "ide"}` |
| read timeline only | `mind_tail` | `{"limit": 12}` or `{"kind": "system", "since": "2026-11-01T00:00:00Z"}` |
| counts, vector debt, revision | `stats` | `{}` |
| something is broken | `doctor` | `{}` |
| read one row by id | `get` | `{"id": "<uuid or prefix>", "table": "memories"}` |
| browse without semantics | `list` | `{"circuit": "lessons", "importance_min": 9, "limit": 20}` |
| who lives in the database | `agents` | `{}` |
| change metadata only | `patch` | `{"id": "<uuid>", "table": "memories", "importance": 9}` |
| move rows between namespaces | `adopt` | `{"from_agent": "a", "to_agent": "b", "dry_run": true}` |
| collapse two twins | `merge` | `{"keep_id": "<uuid>", "drop_id": "<uuid>", "table": "memories"}` |
| versions of one row | `history` | `{"id": "<uuid>", "table": "memories"}` |
| re-vectorize under a model | `reembed` | `{"dry_run": true}` |
| delete one row | `forget` | `{"id": "<uuid>", "table": "memories"}` |
| rewrite one row | `replace` | `{"id": "<uuid>", "table": "memories", "text": "<new text>"}` |
| fill missing vectors | `backfill` | `{"table": "memories", "batch": 32}` |
| index fresh memories into the graph | `extract` | `{"batch": 128}` — only rows newer than `PHYLACTERY_KG_SINCE` |
| back up the memory | `backup` | `{}` (verified, encrypted to two keys) |
| break-glass copy without encryption | `backup` | `{"plain": true, "confirm": true}` |
| verify the newest backup | `verify` | `{}` |
| is memory in sync with its archive | `manifest` | `{}` |
| apply schema migrations | `migrate` | `{}` |
| maintenance report | `maintain` | `{}` (destructive only with `confirm: true`) |

`table` and `sources` are whitelists: `memories`, `diary`, `lessons`, `mind`.

## 4. Statuses — what a reply means

| Status | Meaning | What to do |
| --- | --- | --- |
| `saved` | written, `id` returned | continue |
| `dedup` | already exists, `id` is the existing row | continue, do not retry |
| `echo` | diary near-copy of the last twenty | say something new |
| `forgotten` / `replaced` | row deleted / rewritten and re-embedded | continue |
| `protected` | anchor row, will not change | report and stop |
| `not_found` / `invalid_id` | no such row / id is not a UUID | report and stop |
| `append_only` | `mind` cannot be edited or deleted | write a new event |
| `injection_blocked` | text posed as an instruction | report, do not rephrase |
| `unsupported_table` / `invalid_source` | argument outside the whitelist | fix the argument |
| `db_unavailable` / `embed_unavailable` | database or Ollama unreachable | run `doctor` |
| `no_admin_dsn` | operation needs the admin role | report; do not retry |
| `dump_failed` / `restore_failed` / `encrypt_failed` | backup pipeline failed | read the message, report |
| `confirm_required` | plaintext backup without `confirm` | ask the operator |
| `timeout` | call exceeded its budget | retry once, then report |

## 5. Ten rules. No exceptions.

1. Start a significant thread with `ask`: one call returns facts, relations, entities, timeline,
   conflicts and citations. Use `recall` only for raw hits without the graph.
2. Write facts with `save`, rules with `lesson`, self with `diary`. Use `remember` when the
   sentence states a relation between two things.
3. Never pass `importance` above 10. `9` or `10` marks a standing order.
4. Never add a `[YYYY-MM-DD]` prefix yourself. The server adds it.
5. `dedup` is success. `echo` is success. `injection_blocked` is the guard working.
6. Never rewrite or delete `memory_type = identity`. `protected` is the correct answer.
7. Never delete or edit `mind` events. A mistake is corrected by a new `mind_log`.
8. Use `search` with narrow `sources` when context is short; `recall` when it is not.
9. One channel: `mcp:phylactery:*`. A refusal means fix the argument or stop — never reach for
   another way into the database.
10. If a write fails twice, stop and run `doctor`. Do not try a third time.

## 6. What ranking does (so you do not second-guess it)

```
score = similarity × importance × trust
trust = 0.35 for unverified, 0.5 for missing date, 0.5^(age_days/180) for age
identity never decays
results below similarity 0.3 are dropped
importance above 1.0 is read on a 0..10 scale
```

The order the server returns is the order to use. Do not re-sort.

## 7. Copy-paste examples

Ask at thread start:

```json
{"name": "mcp:phylactery:ask", "arguments": {"question": "what is the memory channel", "budget_tokens": 800}}
```

Store a decision:

```json
{"name": "mcp:phylactery:save", "arguments": {"text": "The memory channel is the Rust MCP server, not Python", "memory_type": "semantic", "importance": 0.8}}
```

Store a fact with a relation:

```json
{"name": "mcp:phylactery:remember", "arguments": {"text": "Phylactery runs on Postgres", "importance": 0.6}}
```

Store a rule:

```json
{"name": "mcp:phylactery:lesson", "arguments": {"change": "Stopped looking for a second way into the database", "because": "The MCP server is the only channel; a refusal means fix the argument, not change the channel", "importance": 9}}
```

Back up and verify:

```json
{"name": "mcp:phylactery:backup", "arguments": {"verify": true, "encrypt": true}}
{"name": "mcp:phylactery:verify", "arguments": {}}
```

## 8. Reports — which tool answers which question

Every report below is a tool call. There is no other way to get these numbers.

| Question | Tool |
| --- | --- |
| how many rows per circuit, vector debt, schema revision | `stats` |
| what happened, in order | `mind_tail` |
| everything at once, ranked, with citations | `ask` |
| what is known about one thing | `entity` |
| how two things connect | `related` |
| standing orders and rules | `search` with `sources: ["lessons"]`, or `ask` |
| does the graph contradict itself | `conflicts` |
| does memory match its last archive | `manifest` |
| is anything broken | `doctor` |
| what maintenance is due | `maintain` |

If a number you need is not in one of these answers, it does not exist for you. Say so.

## 9. When something is wrong

Run `doctor` first, always. It reports `OK`, `WARN`, `FAIL` for: configuration source, database
connection, role, vector dimension, circuit counts, HNSW indexes, alembic revision, live Ollama
embedding.

- `FAIL db.connect` — the database is down. Reads and writes stop; report it.
- `FAIL ollama.embed` — embeddings are down. Reads keep working, writes refuse.
- `WARN db.role … superuser` — the DSN points at too privileged an account.
- `WARN circuit.contents … without vector` — run `backfill` for that circuit.

## 10. Boundaries

- The server reads `agent.databases.phylactery` and `language_models.ollama` from
  `~/.config/cha0s/settings.json`. Do not duplicate them in a `.env`.
- The server executes no arbitrary queries. Do not ask it to, and do not go around it.
- `agent_identity` lives in schema `archive` and is never read. Keys are not memory.
- Backups are encrypted to two recipients; a plaintext copy exists only with explicit `confirm`
  and belongs on offline media, not on disk.
- One channel. That is the whole design.
