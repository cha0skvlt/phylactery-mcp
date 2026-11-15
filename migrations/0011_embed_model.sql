-- Embedding provenance: which model produced each vector, and at what dimension.
BEGIN;

ALTER TABLE memories         ADD COLUMN IF NOT EXISTS embed_model text;
ALTER TABLE memories         ADD COLUMN IF NOT EXISTS embed_dim   int;
ALTER TABLE self_reflections ADD COLUMN IF NOT EXISTS embed_model text;
ALTER TABLE self_reflections ADD COLUMN IF NOT EXISTS embed_dim   int;
ALTER TABLE causal_lessons   ADD COLUMN IF NOT EXISTS embed_model text;
ALTER TABLE causal_lessons   ADD COLUMN IF NOT EXISTS embed_dim   int;
ALTER TABLE mind_events      ADD COLUMN IF NOT EXISTS embed_model text;
ALTER TABLE mind_events      ADD COLUMN IF NOT EXISTS embed_dim   int;

-- The model that produced the vectors already in the database. This is a fact, not a guess:
-- every vector was written by the configured embedder before this migration existed.
-- Identity rows are anchors: the storage trigger refuses any UPDATE on them, so they keep a
-- NULL embed_model and are reported, never rewritten. That is the same limit `backfill` has.
UPDATE memories         SET embed_model = 'nomic-embed-text-v2-moe', embed_dim = 768
    WHERE embedding IS NOT NULL AND embed_model IS NULL AND memory_type <> 'identity';
UPDATE self_reflections SET embed_model = 'nomic-embed-text-v2-moe', embed_dim = 768
    WHERE embedding IS NOT NULL AND embed_model IS NULL;
UPDATE causal_lessons   SET embed_model = 'nomic-embed-text-v2-moe', embed_dim = 768
    WHERE embedding IS NOT NULL AND embed_model IS NULL;
UPDATE mind_events      SET embed_model = 'nomic-embed-text-v2-moe', embed_dim = 768
    WHERE embedding IS NOT NULL AND embed_model IS NULL;

COMMIT;
