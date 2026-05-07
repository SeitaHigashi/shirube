#!/usr/bin/env bash
# Post-commit hook: bump Cargo.toml version to match git commit count.
# Runs after every Bash tool call via Claude Code PostToolUse hook.
# Exits silently (0) when no update is needed to avoid noise.
set -euo pipefail

REPO_DIR="/home/seita/trader2"

# Only run inside the trader2 repo
cd "$REPO_DIR" 2>/dev/null || exit 0

# Only proceed if this is a git repo with at least one commit
git rev-parse HEAD > /dev/null 2>&1 || exit 0

# Skip if the last commit is already a version bump (loop prevention)
LAST_MSG=$(git log -1 --pretty=%s 2>/dev/null || echo "")
if [[ "$LAST_MSG" == "chore: bump version"* ]]; then
  exit 0
fi

# Compute expected version from total commit count
COUNT=$(git rev-list --count HEAD)
VERSION="0.1.${COUNT}"

# Read current version from Cargo.toml
CURRENT=$(grep '^version = ' Cargo.toml | head -1 | sed 's/version = "\(.*\)"/\1/')

# Skip if already up to date
if [[ "$CURRENT" == "$VERSION" ]]; then
  exit 0
fi

# Update Cargo.toml and commit
sed -i "s/^version = \".*\"/version = \"${VERSION}\"/" Cargo.toml
git add Cargo.toml
git commit -m "chore: bump version to ${VERSION}"
