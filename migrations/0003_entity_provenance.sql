-- 0003_entity_provenance.sql — entity provenance.
--
-- Entities created by the retired extractor are quarantined the same way its triples are: they
-- stay readable, but they are not used as a vocabulary for new extraction. Only entities the
-- rule extractor created take part in resolution.

BEGIN;

ALTER TABLE entities ADD COLUMN IF NOT EXISTS extractor text NOT NULL DEFAULT 'legacy';

CREATE INDEX IF NOT EXISTS idx_entities_rule
    ON entities (agent_id, norm) WHERE extractor = 'rule';

COMMIT;
