#!/bin/sh
# Sync the canonical skill into the agent skills directory.
#
# The repository copy is canonical: it is versioned, reviewed and shipped with the server.
# The global copy is what the agent actually loads. Run this after editing SKILL.md here.
set -eu

REPO_DIR="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="${HOME}/.agents/skills/phylactery/SKILL.md"

mkdir -p "$(dirname "$TARGET")"
cp "$REPO_DIR/SKILL.md" "$TARGET"
echo "synced: $REPO_DIR/SKILL.md -> $TARGET"
