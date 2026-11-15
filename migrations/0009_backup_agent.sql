-- 0009_backup_agent.sql — a backup remembers whose memory it counted.
--
-- `backup` records counts for one agent, while `verify` used to compare them with whole-database
-- counts. The row now carries the agent id, and verification counts the same namespace.

BEGIN;

ALTER TABLE phylactery_backups ADD COLUMN IF NOT EXISTS agent_id text;

UPDATE phylactery_backups SET agent_id = 'agent' WHERE agent_id IS NULL;

COMMIT;
