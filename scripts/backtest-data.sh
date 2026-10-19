#!/usr/bin/env bash
# Carry the backtest DB across self-improvement-loop runs via a GitHub
# Release asset, so history accumulates past bitFlyer's 31-day execution
# retention window.
#
# Why a release asset: the daily loop runs as a cloud routine that gets a
# fresh git checkout, so nothing on local disk survives between runs. The DB
# must not live in the repo (it is generated data, and it grows), and Actions
# cache evicts after 7 days. A release asset is out of git history, needs no
# credentials beyond the routine's existing GITHUB_TOKEN, and has a 2GB
# per-file ceiling.
#
# The asset lives on a dedicated NON-SEMVER tag so the auto-updater cannot
# see it: self_update's `update()` calls `get_latest_releases()`, which lists
# every release and then filters with
# `bump_is_greater(current, &r.version).unwrap_or(false)`. "backtest-data"
# fails to parse as a semver, so it is dropped. The release is also marked
# prerelease so it never shows as "Latest" on the releases page. Do not
# rename this tag to anything semver-shaped.
#
# This script is now a thin wrapper around `shirube backtest-data`: the
# actual GitHub-release and SQLite work happens in the binary (`src/cli.rs`),
# via `reqwest` and `rusqlite`, which it already links. It used to shell out
# to the `gh` and `sqlite3` CLIs directly, but neither is installed in the
# self-improvement loop's cloud-routine container, so the whole data
# bootstrap could not run there — `pull` hard-exited on "gh CLI not found on
# PATH" before a single backtest could run. Moving the logic into the binary
# means this script (and the loop) depends on nothing but `shirube` itself.
#
# Usage:
#   scripts/backtest-data.sh pull [--db ./run.db]
#     Download and decompress the stored DB. Prints a shell-eval-able
#     `BACKFILL_DAYS=<n>` line: how many days the caller should pass to
#     `shirube backfill-executions` to close the gap without leaving a hole.
#     Prints BACKFILL_DAYS=31 when no stored DB exists yet (first run), and
#     also when GITHUB_TOKEN/GH_TOKEN is unset — see `run_backtest_data_pull`
#     in `src/cli.rs` for the exact fallback rules.
#
#   scripts/backtest-data.sh push [--db ./run.db]
#     VACUUM, gzip, and upload the DB, replacing the stored copy. Requires
#     GITHUB_TOKEN or GH_TOKEN to be set (unlike `pull`, this fails hard
#     without one — a skipped push would silently lose accumulated history).

set -euo pipefail

# A relative --db must resolve against the caller's cwd, not the repo root,
# or a caller running from another directory would silently read and write
# the repo's own run.db. Remember where the caller was before doing anything
# that might change directory.
ORIG_PWD="$PWD"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

DB="./run.db"

CMD="${1:-}"; shift || true
while [[ $# -gt 0 ]]; do
    case "$1" in
        --db) DB="$2"; shift 2 ;;
        *) echo "unknown flag: $1" >&2; exit 1 ;;
    esac
done

case "$DB" in /*) ;; *) DB="$ORIG_PWD/${DB#./}" ;; esac

case "$CMD" in
    pull|push) ;;
    *)
        grep '^#' "$0" | sed 's/^# \{0,1\}//'
        exit 1
        ;;
esac

BIN="$REPO_DIR/target/release/shirube"
if [[ ! -x "$BIN" ]]; then
    echo "shirube release binary not found, building..." >&2
    (cd "$REPO_DIR" && cargo build --release) >&2
fi

exec "$BIN" backtest-data "$CMD" --db "$DB"
