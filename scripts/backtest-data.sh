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
# Usage:
#   scripts/backtest-data.sh pull [--db ./run.db]
#     Download and decompress the stored DB. Prints a shell-eval-able
#     `BACKFILL_DAYS=<n>` line: how many days the caller should pass to
#     `shirube backfill-executions` to close the gap without leaving a hole.
#     Prints BACKFILL_DAYS=31 when no stored DB exists yet (first run).
#
#   scripts/backtest-data.sh push [--db ./run.db]
#     VACUUM, gzip, and upload the DB, replacing the stored copy.

set -euo pipefail

# `gh release` infers the repo from the working directory, so we have to run
# from inside the checkout. Remember where the caller was first: a relative
# --db must resolve against *their* cwd, not the repo root, or a caller in
# another directory would silently read and write the repo's own run.db.
ORIG_PWD="$PWD"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

TAG="backtest-data"
ASSET_NAME="backtest.db.gz"
DB="./run.db"
# bitFlyer serves at most 31 days of executions; asking for more is wasted
# requests. 2 days is the floor so a same-day re-run still re-fetches the
# partial edge bars the previous run dropped.
MAX_DAYS=31
MIN_DAYS=2

CMD="${1:-}"; shift || true
while [[ $# -gt 0 ]]; do
    case "$1" in
        --db) DB="$2"; shift 2 ;;
        *) echo "unknown flag: $1" >&2; exit 1 ;;
    esac
done

case "$DB" in /*) ;; *) DB="$ORIG_PWD/${DB#./}" ;; esac
cd "$REPO_DIR"

command -v gh >/dev/null || { echo "gh CLI not found on PATH" >&2; exit 1; }

# Days of history missing from $DB, as an integer suitable for
# `backfill-executions --days`. Clamped to [MIN_DAYS, MAX_DAYS].
recommended_days() {
    local newest
    newest="$(sqlite3 "$DB" \
        "SELECT COALESCE(MAX(timestamp), '') FROM tickers WHERE product_code='BTC_JPY';" 2>/dev/null || echo "")"
    if [[ -z "$newest" ]]; then
        echo "$MAX_DAYS"; return
    fi
    local newest_epoch now_epoch gap
    newest_epoch="$(date -u -d "$newest" +%s 2>/dev/null || echo 0)"
    now_epoch="$(date -u +%s)"
    if [[ "$newest_epoch" -eq 0 ]]; then
        echo "$MAX_DAYS"; return
    fi
    # Round the gap up to whole days and add one day of overlap, so the
    # backfill always covers the seam rather than stopping just short of it.
    gap=$(( (now_epoch - newest_epoch + 86399) / 86400 + 1 ))
    (( gap < MIN_DAYS )) && gap=$MIN_DAYS
    (( gap > MAX_DAYS )) && gap=$MAX_DAYS
    echo "$gap"
}

case "$CMD" in
    pull)
        if gh release download "$TAG" --pattern "$ASSET_NAME" --output - 2>/dev/null | gunzip > "$DB.tmp"; then
            mv "$DB.tmp" "$DB"
            # A downloaded DB may carry -wal/-shm from the uploader's process;
            # they are not in the asset, so clear any stale local ones.
            rm -f "$DB-wal" "$DB-shm"
            rows="$(sqlite3 "$DB" 'SELECT COUNT(*) FROM tickers;')"
            oldest="$(sqlite3 "$DB" 'SELECT MIN(timestamp) FROM tickers;')"
            newest="$(sqlite3 "$DB" 'SELECT MAX(timestamp) FROM tickers;')"
            echo "restored $rows bars: $oldest .. $newest" >&2
        else
            rm -f "$DB.tmp"
            echo "no stored DB at tag '$TAG' — first run, full backfill needed" >&2
        fi
        echo "BACKFILL_DAYS=$(recommended_days)"
        ;;
    push)
        [[ -f "$DB" ]] || { echo "no such DB: $DB" >&2; exit 1; }
        # Checkpoint the WAL into the main file, then VACUUM — without this the
        # asset can miss recently written bars and carries free pages.
        # wal_checkpoint prints a result row; keep stdout clean for callers.
        sqlite3 "$DB" "PRAGMA wal_checkpoint(TRUNCATE); VACUUM;" >/dev/null
        # Stage the compressed copy outside the checkout so a failed run never
        # leaves an 8MB artifact sitting in the working tree.
        TMPDIR_ASSET="$(mktemp -d)"
        trap 'rm -rf "$TMPDIR_ASSET"' EXIT
        ASSET="$TMPDIR_ASSET/$ASSET_NAME"
        gzip -9 -c "$DB" > "$ASSET"
        if ! gh release view "$TAG" >/dev/null 2>&1; then
            gh release create "$TAG" --prerelease \
                --title "Backtest data (generated)" \
                --notes "Accumulated bitFlyer BTC_JPY OHLCV bars for the self-improvement loop. Generated data, not a software release — the tag is deliberately non-semver so the auto-updater ignores it. See docs/self-improvement-loop.md." \
                "$ASSET"
        else
            gh release upload "$TAG" "$ASSET" --clobber
        fi
        echo "uploaded $(sqlite3 "$DB" 'SELECT COUNT(*) FROM tickers;') bars to release '$TAG'" >&2
        ;;
    *)
        grep '^#' "$0" | sed 's/^# \{0,1\}//'
        exit 1
        ;;
esac
