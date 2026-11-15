-- 0002_kg_index.sql — extraction bookkeeping.
--
-- A memory is indexed once. The marker is a timestamp, not a boolean, so the age of the
-- extraction is visible and a re-run can be scoped by date.

BEGIN;

ALTER TABLE memories ADD COLUMN IF NOT EXISTS kg_indexed_at timestamptz;

CREATE INDEX IF NOT EXISTS idx_memories_kg_pending
    ON memories (agent_id, created_at) WHERE kg_indexed_at IS NULL;

COMMIT;
