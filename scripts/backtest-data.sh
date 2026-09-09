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
# Dependencies: the script needs a way to talk to the GitHub release API and
# a way to query SQLite, and it accepts either option for each — see
# "portability helpers" below. The cloud routine's image has neither the `gh`
# CLI nor the `sqlite3` CLI, so both fallbacks are load-bearing, not luxuries.
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

# ---------------------------------------------------------------------------
# Portability helpers
#
# NOTE: this script originally shelled out to `gh` and `sqlite3` directly.
# Neither is installed in the cloud routine's container, so the daily loop
# could not restore or persist the DB at all — the accumulation this whole
# mechanism exists for silently stopped at whatever the last human-run push
# left behind. Each dependency therefore has a fallback that is present in
# that image: `gh` -> curl against the REST API with $GITHUB_TOKEN, and
# `sqlite3` -> python3's stdlib sqlite3 module. `gh` is still preferred when
# available so an interactive user keeps their own credentials.
# ---------------------------------------------------------------------------

have() { command -v "$1" >/dev/null 2>&1; }

PY="$(command -v python3 || command -v python || true)"
TOKEN="${GITHUB_TOKEN:-${GH_TOKEN:-}}"
GH_API="${GITHUB_API_URL:-https://api.github.com}"
GH_UPLOADS="${GITHUB_UPLOADS_URL:-https://uploads.github.com}"

if ! have gh && { [[ -z "$TOKEN" ]] || ! have curl || [[ -z "$PY" ]]; }; then
    echo "need either the gh CLI, or curl + python3 + \$GITHUB_TOKEN" >&2
    exit 1
fi
if ! have sqlite3 && [[ -z "$PY" ]]; then
    echo "need either the sqlite3 CLI or python3" >&2
    exit 1
fi

# Run one or more ';'-separated SQL statements against $DB and print the
# result rows, tab-separated — the subset of `sqlite3 <db> <sql>` this
# script actually uses.
db_sql() {
    if have sqlite3; then
        sqlite3 "$DB" "$1"
    else
        "$PY" - "$DB" "$1" <<'PY'
import sqlite3, sys

# isolation_level=None keeps the driver from opening an implicit
# transaction, which VACUUM and PRAGMA wal_checkpoint cannot run inside.
con = sqlite3.connect(sys.argv[1], isolation_level=None)
for stmt in (s for s in sys.argv[2].split(";") if s.strip()):
    for row in con.execute(stmt):
        print("\t".join("" if v is None else str(v) for v in row))
con.close()
PY
    fi
}

# owner/repo of the checkout, for the REST API path (gh infers this itself).
repo_slug() {
    local url
    url="$(git -C "$REPO_DIR" remote get-url origin)"
    url="${url%.git}"
    case "$url" in
        *:*//*|http*) printf '%s\n' "$url" | sed -E 's#^[a-z]+://[^/]+/##' ;;
        *:*)          printf '%s\n' "${url##*:}" ;;
        *)            printf '%s\n' "$url" ;;
    esac
}

api() { # api <METHOD> <url> [extra curl args...]
    local method="$1" url="$2"; shift 2
    curl -sS --fail-with-body -X "$method" \
        -H "Authorization: Bearer $TOKEN" \
        -H "Accept: application/vnd.github+json" \
        -H "X-GitHub-Api-Version: 2022-11-28" \
        "$@" "$url"
}

# Read one top-level field, or an asset id by asset name, out of a release
# JSON object on stdin. Prints an empty line when absent.
json_field() { "$PY" -c 'import json,sys
try: d = json.load(sys.stdin)
except Exception: d = {}
print(d.get(sys.argv[1], "") or "")' "$1"; }

json_asset_id() { "$PY" -c 'import json,sys
try: d = json.load(sys.stdin)
except Exception: d = {}
print(next((a["id"] for a in d.get("assets", []) if a["name"] == sys.argv[1]), ""))' "$1"; }

# Write the stored asset to stdout. Non-zero exit means "nothing stored yet".
asset_download() {
    if have gh; then
        gh release download "$TAG" --pattern "$ASSET_NAME" --output -
        return
    fi
    local release_json asset_id
    release_json="$(api GET "$GH_API/repos/$(repo_slug)/releases/tags/$TAG")" || return 1
    asset_id="$(printf '%s' "$release_json" | json_asset_id "$ASSET_NAME")"
    [[ -n "$asset_id" ]] || return 1
    # The asset endpoint needs the octet-stream Accept header; without it the
    # API returns the asset's JSON metadata instead of its bytes.
    curl -sSL --fail \
        -H "Authorization: Bearer $TOKEN" \
        -H "Accept: application/octet-stream" \
        "$GH_API/repos/$(repo_slug)/releases/assets/$asset_id"
}

# Replace the stored asset with $1, creating the release if it is missing.
asset_upload() {
    local file="$1"
    if have gh; then
        if ! gh release view "$TAG" >/dev/null 2>&1; then
            gh release create "$TAG" --prerelease \
                --title "Backtest data (generated)" \
                --notes "$RELEASE_NOTES" \
                "$file"
        else
            gh release upload "$TAG" "$file" --clobber
        fi
        return
    fi

    local slug release_json release_id asset_id
    slug="$(repo_slug)"
    release_json="$(api GET "$GH_API/repos/$slug/releases/tags/$TAG" || true)"
    release_id="$(printf '%s' "$release_json" | json_field id)"
    if [[ -z "$release_id" ]]; then
        release_json="$(api POST "$GH_API/repos/$slug/releases" \
            -d "$("$PY" -c 'import json,sys
print(json.dumps({"tag_name": sys.argv[1], "name": "Backtest data (generated)",
                  "body": sys.argv[2], "prerelease": True}))' "$TAG" "$RELEASE_NOTES")")"
        release_id="$(printf '%s' "$release_json" | json_field id)"
        [[ -n "$release_id" ]] || { echo "could not create release '$TAG'" >&2; exit 1; }
    fi
    # There is no --clobber equivalent on the API: an upload whose name
    # collides with an existing asset is rejected, so delete it first.
    asset_id="$(printf '%s' "$release_json" | json_asset_id "$ASSET_NAME")"
    if [[ -n "$asset_id" ]]; then
        api DELETE "$GH_API/repos/$slug/releases/assets/$asset_id" >/dev/null
    fi
    api POST "$GH_UPLOADS/repos/$slug/releases/$release_id/assets?name=$ASSET_NAME" \
        -H "Content-Type: application/gzip" \
        --data-binary "@$file" >/dev/null
}

RELEASE_NOTES="Accumulated bitFlyer BTC_JPY OHLCV bars for the self-improvement loop. Generated data, not a software release — the tag is deliberately non-semver so the auto-updater ignores it. See docs/self-improvement-loop.md."

# Days of history missing from $DB, as an integer suitable for
# `backfill-executions --days`. Clamped to [MIN_DAYS, MAX_DAYS].
recommended_days() {
    local newest
    newest="$(db_sql \
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
        if asset_download 2>/dev/null | gunzip > "$DB.tmp"; then
            mv "$DB.tmp" "$DB"
            # A downloaded DB may carry -wal/-shm from the uploader's process;
            # they are not in the asset, so clear any stale local ones.
            rm -f "$DB-wal" "$DB-shm"
            rows="$(db_sql 'SELECT COUNT(*) FROM tickers;')"
            oldest="$(db_sql 'SELECT MIN(timestamp) FROM tickers;')"
            newest="$(db_sql 'SELECT MAX(timestamp) FROM tickers;')"
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
        db_sql "PRAGMA wal_checkpoint(TRUNCATE); VACUUM;" >/dev/null
        # Stage the compressed copy outside the checkout so a failed run never
        # leaves an 8MB artifact sitting in the working tree.
        TMPDIR_ASSET="$(mktemp -d)"
        trap 'rm -rf "$TMPDIR_ASSET"' EXIT
        ASSET="$TMPDIR_ASSET/$ASSET_NAME"
        gzip -9 -c "$DB" > "$ASSET"
        asset_upload "$ASSET"
        echo "uploaded $(db_sql 'SELECT COUNT(*) FROM tickers;') bars to release '$TAG'" >&2
        ;;
    *)
        grep '^#' "$0" | sed 's/^# \{0,1\}//'
        exit 1
        ;;
esac
