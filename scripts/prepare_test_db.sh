#!/bin/sh
# Prepare a disposable database for the integration tests.
#
#   ./scripts/prepare_test_db.sh
#   PHYLACTERY_TEST_DSN=postgresql://localhost:5432/phylactery_test cargo test --test write_race
#
# The schema is cloned from the live database; rows are never copied.
set -eu

SOURCE_DB="${PHYLACTERY_SOURCE_DB:-phylactery}"
TEST_DB="${PHYLACTERY_TEST_DB:-phylactery_test}"

pg_dump --schema-only --no-owner --no-privileges "$SOURCE_DB" \
  | grep -v '^\\' \
  | psql -q "$TEST_DB"

echo "prepared $TEST_DB from $SOURCE_DB (schema only)"
