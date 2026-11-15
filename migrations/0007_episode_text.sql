-- 0007_episode_text.sql — episodes are text, because the existing thread ids are text.
--
-- `thread_id` looks like `solitary-d7d4f4cc` or `reverie-vigil-b60fd1270ea7`, not a UUID. The
-- spine must match reality, so `episode_id` becomes text and is backfilled from `thread_id`.
-- The view is rebuilt around the type change.

BEGIN;

DROP VIEW IF EXISTS memory_map;

ALTER TABLE memories ALTER COLUMN episode_id TYPE text USING episode_id::text;
ALTER TABLE self_reflections ALTER COLUMN episode_id TYPE text USING episode_id::text;
ALTER TABLE causal_lessons ALTER COLUMN episode_id TYPE text USING episode_id::text;
ALTER TABLE mind_events ALTER COLUMN episode_id TYPE text USING episode_id::text;

UPDATE mind_events
SET episode_id = thread_id
WHERE episode_id IS NULL AND thread_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_mind_episode_text ON mind_events (episode_id) WHERE episode_id IS NOT NULL;

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
