#!/usr/bin/env bash
# git pre-commit hook: update Cargo.toml version to 0.1.<commit-count+1>
# before each commit so the committed version matches the resulting HEAD count.
#
# Install: ln -sf ../../.claude/hooks/pre-commit-version-bump.sh .git/hooks/pre-commit
set -euo pipefail

REPO_DIR="$(git rev-parse --show-toplevel)"
cd "$REPO_DIR"

# Commits so far + 1 (this commit will become HEAD)
COUNT=$(( $(git rev-list --count HEAD 2>/dev/null || echo 0) + 1 ))
VERSION="0.1.${COUNT}"

# Read current version from Cargo.toml
CURRENT=$(grep '^version = ' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')

# Skip if already correct (e.g. hook ran twice somehow)
if [[ "$CURRENT" == "$VERSION" ]]; then
  exit 0
fi

sed -i "s/^version = \".*\"/version = \"${VERSION}\"/" Cargo.toml
git add Cargo.toml
