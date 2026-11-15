-- 0004_ops.sql — operations bookkeeping.
--
-- Two tables: what was backed up, and what the server did. Both are written only by explicit
-- tool calls; the server has no schedule of its own.

BEGIN;

CREATE TABLE IF NOT EXISTS phylactery_backups (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    taken_at timestamptz NOT NULL DEFAULT now(),
    path text NOT NULL,
    bytes bigint NOT NULL,
    sha256 text NOT NULL,
    schema_revision text,
    counts jsonb NOT NULL DEFAULT '{}'::jsonb,
    verified boolean NOT NULL DEFAULT false,
    verified_at timestamptz,
    key_fingerprint text
);

CREATE INDEX IF NOT EXISTS idx_backups_taken ON phylactery_backups (taken_at DESC);

CREATE TABLE IF NOT EXISTS phylactery_runs (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    tool text NOT NULL,
    started_at timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz,
    status text NOT NULL DEFAULT 'running',
    detail jsonb NOT NULL DEFAULT '{}'::jsonb
);

CREATE INDEX IF NOT EXISTS idx_runs_started ON phylactery_runs (started_at DESC);

COMMIT;
