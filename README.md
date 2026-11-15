# phylactery-mcp


An MCP server that adds vectors and a knowledge graph to the Postgres database CHA0S AOE already
reaches through its `database` tool: four memory circuits, one 768d space, a typed graph, and the
operations that keep the whole thing alive.

Private. Not published. The database it serves is `phylactery`; the server is a client, not a copy.

## What it does

- **Memory** — `recall`, `search`, `save`, `lesson`, `diary`, `mind_log`, `mind_tail`, `forget`,
  `replace`, `backfill`.
- **Graph** — `remember`, `entity`, `related`, `conflicts`, `extract`.
- **Answer** — `ask`: one call returns facts, relations, entities, timeline, conflicts and
  citations, cut to a token budget.
- **Operations** — `backup`, `verify`, `manifest`, `migrate`, `maintain`, `doctor`, `stats`.

Thirty-one tools. Every one is an explicit act: the server has no schedule, no background
worker, and never starts work on its own.

## Requirements

- Rust (edition 2024)
- PostgreSQL with `pgvector`, a database carrying the four circuits
- An embedding endpoint: Ollama with `nomic-embed-text-v2-moe` (768d) at `http://127.0.0.1:11434`,
  called as `POST /api/embed`. The server computes no vectors itself — without a reachable
  endpoint, reads work and writes refuse
- `pg_dump`, `psql`, `gzip`, `gpg` on the host, for the operations tools

## Configuration

The server reads the AOE registry, not a `.env`:

```
~/.config/cha0s/settings.json
  agent.databases.phylactery.dsn
  language_models.ollama.api_url
```

Environment overrides:

| Variable | Purpose |
| --- | --- |
| `PHYLACTERY_DSN` | database DSN (wins over the registry) |
| `PHYLACTERY_ADMIN_DSN` | role used for dump, restore and migrations |
| `PHYLACTERY_OLLAMA_URL` | embedding endpoint |
| `PHYLACTERY_EMBED_MODEL` | embedding model, default `nomic-embed-text-v2-moe` |
| `PHYLACTERY_AGENT_ID` | namespace, default `agent` |
| `PHYLACTERY_BACKUP_DIR` | where archives are written |
| `PHYLACTERY_MIGRATIONS_DIR` | where `*.sql` migrations live |
| `PHYLACTERY_KG_SINCE` | graph covers memory written after this timestamp |
| `PHYLACTERY_POOL_MAX`, `PHYLACTERY_STATEMENT_TIMEOUT_MS`, `PHYLACTERY_LOG` | pool, timeout, logging |

## Wiring into CHA0S AOE

```json
{
  "context_servers": {
    "phylactery": {
      "command": "/home/user/phylactery-mcp/target/release/phylactery-mcp",
      "args": [],
      "env": { "PHYLACTERY_LOG": "info" },
      "timeout": 120
    }
  }
}
```

Tools appear as `mcp:phylactery:<tool>`. No IDE changes are required.

## Build and test

```sh
cargo build --release
make check        # fmt + clippy -D warnings + hermetic tests
make test-live    # full pyramid against a disposable database
make smoke        # end-to-end against the release binary
```

The suite is 91 tests: unit, hermetic smoke, MCP contract, write race, mutations, graph, ask,
golden extraction set, live smoke, and one full live surface covering every tool.

## Layout

```
src/            config, db, embed, guard, dedup, math, circuit, write, mutate,
                kg, ask, backup, ops, main
migrations/     additive, idempotent SQL (0001–0009)
tests/          unit-level smoke, contract, integration, golden set
scripts/        test database preparation, skill sync
SKILL.md        operating rules for the agent (canonical copy; sync with scripts/sync-skill.sh)
ARCHITECTURE.md target architecture and the reasons behind it
```

## Security posture

- The server runs as `phylactery_rw`: four circuits, the graph, the operations tables. Not a
  superuser, no DDL, no `agent_identity`.
- Arbitrary SQL is not exposed. Table names come from a whitelist.
- Identity rows are protected by a database trigger, not only by application code.
- Foreign text that pretends to be an instruction is refused before it reaches the database.
- Backups are encrypted to two recipients (a server key without a passphrase and an operator key
  with one), so losing a single key is survivable. A verified unencrypted break-glass archive is
  available only through an explicit `confirm` flag; the trade-offs are documented in
  `ARCHITECTURE.md` §19.

## Documentation

`ARCHITECTURE.md` is the source of truth: data model, ranking mathematics, write invariants,
graph design, answer pack, operations and known limits.
