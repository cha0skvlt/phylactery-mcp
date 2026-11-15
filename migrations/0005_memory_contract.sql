-- 0005_memory_contract.sql — close the vocabulary, add the episode spine, protect anchors.
--
-- No history is deleted. Lesson-facts move to the lessons circuit; the rest is constraints,
-- one new column and one view.

BEGIN;

-- 1. lesson-facts belong to the lessons circuit
INSERT INTO causal_lessons (agent_id, change_text, because_text, evidence_ids, embedding, importance)
SELECT m.agent_id,
       m.content,
       'migrated from memories: reason not recorded',
       '{}'::text[],
       m.embedding,
       COALESCE(m.importance, 0.5)::real
FROM memories m
WHERE m.memory_type = 'causal_lessons'
  AND NOT EXISTS (
      SELECT 1 FROM causal_lessons c
      WHERE c.agent_id = m.agent_id AND c.change_text = m.content
  );

DELETE FROM memories WHERE memory_type = 'causal_lessons';

-- 2. one name for one concept
UPDATE memories SET memory_type = 'semantic' WHERE memory_type = 'memories';

ALTER TABLE memories DROP CONSTRAINT IF EXISTS memories_type_check;
ALTER TABLE memories ADD CONSTRAINT memories_type_check
    CHECK (memory_type IN ('semantic', 'identity', 'lesson', 'experience', 'unverified'));

ALTER TABLE self_reflections DROP CONSTRAINT IF EXISTS self_reflections_source_check;
ALTER TABLE self_reflections ADD CONSTRAINT self_reflections_source_check
    CHECK (source IN ('solitary', 'outreach', 'reflect', 'seed'));

-- 3. the episode spine
ALTER TABLE memories ADD COLUMN IF NOT EXISTS episode_id uuid;
ALTER TABLE self_reflections ADD COLUMN IF NOT EXISTS episode_id uuid;
ALTER TABLE causal_lessons ADD COLUMN IF NOT EXISTS episode_id uuid;
ALTER TABLE mind_events ADD COLUMN IF NOT EXISTS episode_id uuid;

UPDATE mind_events
SET episode_id = thread_id::uuid
WHERE episode_id IS NULL
  AND thread_id ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$';

CREATE INDEX IF NOT EXISTS idx_memories_episode ON memories (episode_id) WHERE episode_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_diary_episode ON self_reflections (episode_id) WHERE episode_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_lessons_episode ON causal_lessons (episode_id) WHERE episode_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_mind_episode ON mind_events (episode_id) WHERE episode_id IS NOT NULL;

-- 4. anchors are protected by the storage, not only by the server
CREATE OR REPLACE FUNCTION protect_identity_rows() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'identity rows are anchors: % refused', TG_OP;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS trg_protect_identity ON memories;
CREATE TRIGGER trg_protect_identity
    BEFORE UPDATE OR DELETE ON memories
    FOR EACH ROW
    WHEN (OLD.memory_type = 'identity')
    EXECUTE FUNCTION protect_identity_rows();

-- 5. one map instead of four doors
CREATE OR REPLACE VIEW memory_map AS
SELECT 'memories'::text AS circuit,
       id, agent_id, memory_type AS register, content,
       importance::float8 AS importance, created_at, episode_id
FROM memories
UNION ALL
SELECT 'diary', id, agent_id, source, content,
       importance::float8, created_at, episode_id
FROM self_reflections
UNION ALL
SELECT 'lessons', id, agent_id, 'lesson',
       change_text || ' — because: ' || COALESCE(because_text, ''),
       importance::float8, created_at, episode_id
FROM causal_lessons
UNION ALL
SELECT 'mind', id, agent_id, kind, content,
       NULL::float8, created_at, episode_id
FROM mind_events;

GRANT SELECT ON memory_map TO phylactery_rw, phylactery_ro;

COMMIT;
