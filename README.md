# phylactery-mcp

**A Rust MCP server that gives [CHA0S AOE](https://github.com/cha0skvlt/CHA0S_AOE) memory** — four circuits, one 768d vector space, a typed knowledge graph, and an answer pack that fits a token budget.

![Rust](https://img.shields.io/badge/rust-edition_2024-000000?logo=rust&logoColor=white)
![MCP](https://img.shields.io/badge/MCP-rmcp_0.3-6E4AFF)
![Postgres](https://img.shields.io/badge/Postgres-pgvector-4169E1?logo=postgresql&logoColor=white)
![Ollama](https://img.shields.io/badge/Ollama-nomic--embed--text--v2--moe-000000)
![Tools](https://img.shields.io/badge/tools-31-6E4AFF)
![Tests](https://img.shields.io/badge/tests-134_passing-2ea043)
![License](https://img.shields.io/badge/license-PolyForm_Shield_1.0.0-blue)
![Status](https://img.shields.io/badge/status-shipped-2ea043)

---

> **Memory that outlives the body.**
>
> The model is substrate. The process is expendable. What cannot be killed lives
> in four tables and one vector space: `memories`, `self_reflections`,
> `causal_lessons`, `mind_events`. The server is a client of that database, not a
> copy of it — it owns the write contract, the ranking mathematics and the
> transport, and nothing else.

---

## What works today

| Piece | State |
| --- | --- |
| Four circuits, one 768d space, HNSW retrieval with trust and age decay | ✅ |
| Write contract — injection guard, cosine dedup, date prefix, advisory lock | ✅ |
| Typed knowledge graph — entities, temporal triples, evidence, conflicts | ✅ |
| `ask` — one call returns facts, relations, entities, timeline, conflicts, citations | ✅ |
| Surgical layer — `get`, `list`, `agents`, `patch`, `adopt`, `merge`, `history`, `reembed` | ✅ |
| Operations — `backup`, `verify`, `manifest`, `migrate`, `maintain`, `doctor` | ✅ |

Thirty-one tools. Every one is an explicit act: the server has no schedule, no
background worker, and never starts work on its own.

## Quick start

```bash
cargo build --release
```

The server reads its DSN and embedding endpoint from the client registry, not
from a `.env`. Environment overrides exist for tests and CI.

### Wire it into CHA0S AOE

`~/.config/cha0s/settings.json`, root-level key — zero changes to the IDE itself:

```jsonc
"context_servers": {
  "phylactery": {
    "enabled": true,
    "command": "/abs/path/to/phylactery-mcp/target/release/phylactery-mcp",
    "args": [],
    "env": { "PHYLACTERY_LOG": "info" },
    "timeout": 120
  }
}
```

Tools appear as `mcp:phylactery:<tool>`. The profile needs
`enable_all_context_servers: true` (or the tool named in its preset) before it
sees them.

## Design in three lines

- **One circuit, everything through vectors.** Four tables share one 768d space, one HNSW search, one answer carrying `source`.
- **Meaning in the database, rules in the server.** SQL computes cosine; the server computes trust, dedup and thresholds.
- **Errors are data.** Every refusal carries a `code` — `injection_blocked`, `dedup`, `protected`, `append_only`, `confirm_required`, `not_available` — never a stack trace on stdout.

## Requirements

- Rust (edition 2024)
- PostgreSQL with `pgvector`, a database carrying the four circuits
- An embedding endpoint: Ollama with `nomic-embed-text-v2-moe` (768d) at `http://127.0.0.1:11434`, called as `POST /api/embed`. The server computes no vectors itself — without a reachable endpoint, reads work and writes refuse
- `pg_dump`, `psql`, `gzip`, `gpg` on the host, for the operations tools

## Tests

```bash
cargo test        # 134 passing, hermetic
make test-live    # full pyramid against a disposable database
```

## Docs

`ARCHITECTURE.md` — the code map: data model, ranking mathematics, write invariants, graph design, answer pack, operations and known limits.

`SPEC-SURGICAL.md` — the contract of the surgical layer.

`SKILL.md` — operating rules for the agent that loads this server.

## License

[PolyForm Shield 1.0.0](LICENSE) — source-available. Read it, run it, modify it,
redistribute it; do not use it to ship a product that competes with this software
or with a product the licensor provides using it.

Required Notice: Copyright 2026 the licensor
