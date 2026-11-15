-- 0008_anchor_override.sql — anchors stay protected, but an operator can clean a scratch namespace.
--
-- The trigger refuses UPDATE/DELETE on identity rows. That is the point. It also made test
-- cleanup impossible, so the guard now honours an explicit session switch:
--
--   SET LOCAL phylactery.allow_anchor = 'on';
--
-- The server never sets it. Only an operator (or a test harness) does, deliberately, in one
-- transaction.

BEGIN;

CREATE OR REPLACE FUNCTION protect_identity_rows() RETURNS trigger AS $$
BEGIN
    IF current_setting('phylactery.allow_anchor', true) = 'on' THEN
        RETURN COALESCE(NEW, OLD);
    END IF;
    RAISE EXCEPTION 'identity rows are anchors: % refused', TG_OP;
END;
$$ LANGUAGE plpgsql;

COMMIT;
