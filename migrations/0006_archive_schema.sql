-- 0006_archive_schema.sql — the dead weight leaves `public`, nothing is dropped.
--
-- Fourteen tables from the retired Python engine move to schema `archive`. They stay readable,
-- they stop confusing anyone who opens the schema.

BEGIN;

CREATE SCHEMA IF NOT EXISTS archive;

ALTER TABLE IF EXISTS agent_activity SET SCHEMA archive;
ALTER TABLE IF EXISTS agent_identity SET SCHEMA archive;
ALTER TABLE IF EXISTS checkpoints SET SCHEMA archive;
ALTER TABLE IF EXISTS checkpoint_blobs SET SCHEMA archive;
ALTER TABLE IF EXISTS checkpoint_writes SET SCHEMA archive;
ALTER TABLE IF EXISTS checkpoint_migrations SET SCHEMA archive;
ALTER TABLE IF EXISTS conversations SET SCHEMA archive;
ALTER TABLE IF EXISTS dpo_gold_pairs SET SCHEMA archive;
ALTER TABLE IF EXISTS intents SET SCHEMA archive;
ALTER TABLE IF EXISTS memories_archive SET SCHEMA archive;
ALTER TABLE IF EXISTS outbox SET SCHEMA archive;
ALTER TABLE IF EXISTS policy_knobs SET SCHEMA archive;
ALTER TABLE IF EXISTS vitals SET SCHEMA archive;

COMMIT;
