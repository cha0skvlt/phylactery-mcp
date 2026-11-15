-- 0001_kg.sql — knowledge graph, additive migration.
--
-- Idempotent: safe to run twice. Nothing is dropped, nothing is rewritten in place except
-- backfills of the new columns. Legacy triples are quarantined, not deleted.

BEGIN;

-- entities: resolution key, mandatory kind, merge pointer
ALTER TABLE entities ADD COLUMN IF NOT EXISTS norm text;
ALTER TABLE entities ADD COLUMN IF NOT EXISTS merged_into uuid REFERENCES entities(id) ON DELETE SET NULL;

UPDATE entities
SET norm = regexp_replace(lower(btrim(canonical)), '[^[:alnum:][:space:]]', '', 'g')
WHERE norm IS NULL;

UPDATE entities
SET norm = regexp_replace(norm, '[[:space:]]+', ' ', 'g')
WHERE norm IS NOT NULL;

UPDATE entities SET kind = 'concept' WHERE kind IS NULL;
ALTER TABLE entities ALTER COLUMN kind SET DEFAULT 'concept';
ALTER TABLE entities ALTER COLUMN kind SET NOT NULL;

-- one live entity per (agent, norm); merged rows keep their history but leave the index
WITH ranked AS (
    SELECT id,
           row_number() OVER (PARTITION BY agent_id, norm ORDER BY created_at NULLS LAST, id) AS rn,
           first_value(id) OVER (PARTITION BY agent_id, norm ORDER BY created_at NULLS LAST, id) AS keeper
    FROM entities
    WHERE merged_into IS NULL
)
UPDATE entities e
SET merged_into = ranked.keeper
FROM ranked
WHERE e.id = ranked.id AND ranked.rn > 1;

CREATE UNIQUE INDEX IF NOT EXISTS uq_entities_agent_norm
    ON entities (agent_id, norm) WHERE merged_into IS NULL;

-- knowledge_graph: evidence, confidence, provenance, temporal validity
ALTER TABLE knowledge_graph ADD COLUMN IF NOT EXISTS object_norm text;
ALTER TABLE knowledge_graph ADD COLUMN IF NOT EXISTS evidence_id uuid REFERENCES memories(id) ON DELETE CASCADE;
ALTER TABLE knowledge_graph ADD COLUMN IF NOT EXISTS confidence real NOT NULL DEFAULT 0.9;
ALTER TABLE knowledge_graph ADD COLUMN IF NOT EXISTS extractor text NOT NULL DEFAULT 'rule';
ALTER TABLE knowledge_graph ADD COLUMN IF NOT EXISTS valid_from timestamptz NOT NULL DEFAULT now();
ALTER TABLE knowledge_graph ADD COLUMN IF NOT EXISTS valid_to timestamptz;

UPDATE knowledge_graph
SET object_norm = regexp_replace(lower(btrim(object)), '[^[:alnum:][:space:]]', '', 'g')
WHERE object_norm IS NULL;

UPDATE knowledge_graph
SET object_norm = regexp_replace(object_norm, '[[:space:]]+', ' ', 'g')
WHERE object_norm IS NOT NULL;

-- quarantine: everything that predates this migration came from the retired extractor
UPDATE knowledge_graph
SET extractor = 'legacy', confidence = 0.2
WHERE extractor = 'rule' AND evidence_id IS NULL;

CREATE INDEX IF NOT EXISTS idx_kg_live_subject
    ON knowledge_graph (agent_id, subject_id, predicate) WHERE valid_to IS NULL;
CREATE INDEX IF NOT EXISTS idx_kg_evidence ON knowledge_graph (evidence_id);
CREATE INDEX IF NOT EXISTS idx_kg_object_norm ON knowledge_graph (agent_id, object_norm);

-- triple identity is agent-scoped: two agents may hold the same sentence
DROP INDEX IF EXISTS uq_knowledge_graph_triple;
CREATE UNIQUE INDEX IF NOT EXISTS uq_kg_agent_triple
    ON knowledge_graph (agent_id, subject, predicate, object);

COMMIT;
