# SPEC — surgical layer

Status: **shipped.** All eight tools and both migrations are in the tree and live. This document
records the contract they were built against; `ARCHITECTURE.md` §8.1 is the same text in context.

## 1. What the layer answers

The original tools answered one question: *what resembles my query*. None answered *what is
actually in the circuit*. The surgical layer closes that gap.

| # | Tool | Act | Embeds | Migration |
| --- | --- | --- | --- | --- |
| 24 | `get` | read one row by id, with provenance and evidence | no | — |
| 25 | `list` | non-semantic browse over `memory_map` with filters | no | — |
| 26 | `agents` | census of `agent_id` namespaces | no | — |
| 27 | `patch` | change metadata without touching text | no | — |
| 28 | `adopt` | move rows between namespaces, audited | no | — |
| 29 | `merge` | collapse two twins, repoint evidence | no | 0010 |
| 30 | `history` | versions of one row | no | 0010 |
| 31 | `reembed` | re-vectorize under a named model | yes | 0011 |

## 2. Behaviour that matters

- `get` accepts a full UUID or a hex prefix of at least four characters, searches all four circuits
  unless `table` is given, and reports `agent_id`, `has_vector`, `metadata.source` and graph
  `evidence_triples`. It never embeds.
- `list` reads `memory_map`, filters by circuit, register, importance, time, vector presence,
  episode and thread, orders three ways and pages with an opaque keyset cursor. `agent_id="*"`
  reads every namespace; nothing writes across namespaces without `adopt`.
- `agents` names every namespace with per-circuit counts, vector debt and first/last timestamps.
- `patch` changes `importance`, `memory_type`, `source`, `episode_id` and merges `metadata`
  key-by-key. No re-embedding. `identity` is protected, `mind` is append-only.
- `adopt` moves rows between namespaces. Dry-run by default; a real move needs `dry_run=false`
  and `confirm=true`. `identity` and `mind` never move.
- `merge` collapses two rows of the same namespace and register, repoints `evidence_ids` and graph
  edges, and deletes only after a before-image exists.
- `history` returns newest before-images, including for rows that no longer exist.
- `reembed` reports `scanned`, `stale` and `mismatched` without writing; a real run re-vectorizes
  one batch, records `embed_model` and `embed_dim`, and skips anchors.

## 3. Invariants

1. No tool accepts SQL, a table outside the four-circuit whitelist, or a column name.
2. Every mutating tool defaults to `dry_run=true`; a real write needs `dry_run=false` and, for
   cross-namespace acts, `confirm=true`.
3. `identity` is protected everywhere, including `adopt` and `merge`.
4. `mind_events` is append-only everywhere; no tool writes a vectorless line into it.
5. A before-image is written before any destructive act.
6. Reads never embed; only `reembed` does.
7. Every refusal carries a `code`; `not_available` is used when a migration is missing.

## 4. Migrations

- `0010_memory_revisions.sql` — immutable before-images, index on `(circuit, row_id, changed_at)`,
  grants for the read-write and read-only roles inside the same file.
- `0011_embed_model.sql` — `embed_model` and `embed_dim` on all four circuits, backfilled with the
  model that produced the existing vectors. Anchors keep a NULL model: the storage trigger refuses
  any update on them, and they are reported, never rewritten.

Both are idempotent; a second run is a no-op.

## 5. Acceptance

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` green.
- `tools/list` reports 31 tools.
- `reembed` dry-run on a healthy database reports `stale: 0`.
- `merge` and `history` answer `not_available` only when migration 0010 is absent.
