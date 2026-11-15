-- Immutable before-images for surgical edits and deletions.
BEGIN;

CREATE TABLE IF NOT EXISTS memory_revisions (
    id          bigserial PRIMARY KEY,
    circuit     text NOT NULL,
    row_id      uuid NOT NULL,
    agent_id    text NOT NULL,
    content     text,
    importance  double precision,
    metadata    jsonb,
    op          text NOT NULL CHECK (op IN ('replace', 'patch', 'merge', 'forget')),
    changed_at  timestamptz NOT NULL DEFAULT now(),
    changed_by  text NOT NULL
);

CREATE INDEX IF NOT EXISTS memory_revisions_row
    ON memory_revisions (circuit, row_id, changed_at DESC);

-- The memory role must be able to read and write its own history; the read-only role only reads.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'phylactery_rw') THEN
        GRANT SELECT, INSERT, UPDATE, DELETE ON memory_revisions TO phylactery_rw;
        GRANT USAGE, SELECT ON SEQUENCE memory_revisions_id_seq TO phylactery_rw;
    END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'phylactery_ro') THEN
        GRANT SELECT ON memory_revisions TO phylactery_ro;
    END IF;
END $$;

COMMIT;
