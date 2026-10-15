#!/usr/bin/env bash
# Acceptance tests for `scripts/csid-sync`.
#
# `csid-prune` deletes only what the bucket confirms (tests/prune.sh). That is
# safe only if everything that is no longer being written does reach the
# bucket. These tests pin the upload half of that contract against a synthetic
# spool and a stub rclone, so they need no bucket and no node:
#
#   1. A finished session ships and its marker records the key.
#   2. A never-closed session (`"status": "capturing"`, its process dead) ships
#      once it has been quiet for CSID_SYNC_STALE_MINUTES, and its sidecar is
#      left exactly as it was. Until 2026-10-15 it never shipped at all.
#   3. A directory changed within the quiet window is not shipped.
#   4. The live session named in the status file is not shipped, root or segment.
#   5. A shipped directory whose bucket copy does not match is copied again.
#   6. A shipped directory whose bucket copy matches is not copied again.
#   7. A shipped directory with no payload left costs no bucket call.
#   8. A malformed quiet-window knob is rejected before anything runs.
#
# Usage: tests/sync.sh   (bash 3.2+, jq)
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
SYNC="${HERE}/../scripts/csid-sync"
[[ -x "$SYNC" ]] || { echo "not executable: $SYNC" >&2; exit 1; }
command -v jq >/dev/null || { echo "jq is required" >&2; exit 1; }

fails=0
pass() { printf '  ok   %s\n' "$1"; }
fail() { printf '  FAIL %s\n' "$1" >&2; fails=$(( fails + 1 )); }

touch_ago() {
    local path="$1" mins="$2" stamp
    if stamp="$(date -u -d "@$(( $(date -u +%s) - mins * 60 ))" +%Y%m%d%H%M.%S 2>/dev/null)"; then
        touch -t "$stamp" "$path"
    else
        stamp="$(date -u -r "$(( $(date -u +%s) - mins * 60 ))" +%Y%m%d%H%M.%S)"
        touch -t "$stamp" "$path"
    fi
}

# A session directory: payload plus a sidecar with what v2_key reads.
make_dir() {
    local d="$1" status="$2" age="$3"
    mkdir -p "$d"
    printf '%*s' 1024 '' > "$d/capture.raw"
    printf '{"status": "%s", "started_at": "2026-10-14T02:01:45Z", "run_id": "r1", "experiment": "explore-lib-ref-night", "radio": {"band": "5", "channel": 48, "width": "HT20"}}\n' \
        "$status" > "$d/metadata.json"
    touch_ago "$d/capture.raw" "$age"
    touch_ago "$d/metadata.json" "$age"
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# Stub rclone: log every call; `check` succeeds unless CHECK_FAILS=1.
STUB="$TMP/stub"; mkdir -p "$STUB"
cat > "$STUB/rclone" <<'STUBEOF'
#!/usr/bin/env bash
# Log `<verb> <basename of source> <destination>`, skipping flags and the value
# that follows `--exclude`, so flag order cannot move the paths.
verb="${1:-}"; shift || true
pos=(); skip=0
for a in "$@"; do
    if [[ $skip -eq 1 ]]; then skip=0; continue; fi
    if [[ "$a" == --exclude ]]; then skip=1; continue; fi
    [[ "$a" == -* ]] && continue
    pos+=("$a")
done
printf '%s %s %s\n' "$verb" "$(basename "${pos[0]:-}")" "${pos[1]:-}" >> "$RCLONE_LOG"
set -- "$verb"
if [[ "${1:-}" == "check" && "${CHECK_FAILS:-0}" == "1" ]]; then exit 1; fi
exit 0
STUBEOF
chmod +x "$STUB/rclone"

run_sync() {
    local spool="$1" log="$2"
    : > "$log"
    env PATH="$STUB:$PATH" RCLONE_LOG="$log" \
        CSID_SPOOL="$spool" CSID_HOSTNAME=monadXX \
        CSID_S3_BUCKET=monad-knowledge CSID_S3_ENDPOINT=https://example.invalid \
        CSID_S3_ACCESS_KEY=k CSID_S3_SECRET_KEY=s CSID_S3_PREFIX=captures/v2 \
        CSID_STATUS_PATH="${STATUS:-$TMP/no-status.json}" \
        CSID_SYNC_STALE_MINUTES="${STALE:-120}" \
        CHECK_FAILS="${CHECK_FAILS:-0}" \
        bash "$SYNC" >/dev/null 2>&1
}

copied() { grep -q "^copy $1 " "$2"; }

echo "tests 1-4: what ships"
S="$TMP/a"; mkdir -p "$S"
make_dir "$S/done_20261014-020145-seg0001" complete 10
make_dir "$S/dead_20261012-200151" capturing 600
make_dir "$S/fresh_20261015-120000" capturing 5
make_dir "$S/live_20261015-132218" capturing 600
make_dir "$S/live_20261015-132218-seg0001" capturing 600
echo '{"session_id": "live_20261015-132218"}' > "$TMP/status.json"
before="$(cat "$S/dead_20261012-200151/metadata.json")"
STATUS="$TMP/status.json" run_sync "$S" "$TMP/a.log"

if copied done_20261014-020145-seg0001 "$TMP/a.log"; then pass "finished session shipped"; else fail "finished session not shipped"; fi
if grep -q '^key=captures/v2/dt=2026-10-14/band=5/channel=48/width=HT20/role=observer/host=monadXX/run=r1/session=done_20261014-020145/segment=0001/$' \
        "$S/done_20261014-020145-seg0001/.synced" 2>/dev/null; then
    pass "marker records the v2 key"
else
    fail "marker key wrong or missing"
fi
if copied dead_20261012-200151 "$TMP/a.log"; then pass "never-closed quiet session shipped"; else fail "never-closed quiet session not shipped"; fi
if [[ -f "$S/dead_20261012-200151/.synced" ]]; then pass "never-closed session marked"; else fail "never-closed session not marked"; fi
if [[ "$(cat "$S/dead_20261012-200151/metadata.json")" == "$before" ]]; then
    pass "never-closed sidecar left unedited"
else
    fail "never-closed sidecar was edited"
fi
if copied fresh_20261015-120000 "$TMP/a.log"; then fail "recently changed directory shipped"; else pass "recently changed directory waits"; fi
if copied live_20261015-132218 "$TMP/a.log"; then fail "live root shipped"; else pass "live root not shipped"; fi
if copied live_20261015-132218-seg0001 "$TMP/a.log"; then fail "live segment shipped"; else pass "live session's open segment not shipped"; fi

echo "test 5: a bucket that does not match is shipped again"
S="$TMP/b"; mkdir -p "$S"
make_dir "$S/marked_20261014-020145-seg0002" complete 600
printf 'synced_at=2026-10-14T03:00:00+00:00\nkey=captures/v2/x/session=marked_20261014-020145/segment=0002/\n' \
    > "$S/marked_20261014-020145-seg0002/.synced"
CHECK_FAILS=1 run_sync "$S" "$TMP/b.log"
if copied marked_20261014-020145-seg0002 "$TMP/b.log"; then pass "mismatch re-shipped"; else fail "mismatch not re-shipped"; fi
if grep -q '^copy marked_20261014-020145-seg0002 s3:monad-knowledge/captures/v2/x/session=marked_20261014-020145/segment=0002/$' "$TMP/b.log"; then
    pass "re-shipped to the key in the marker"
else
    fail "re-ship went elsewhere ($(tr '\n' ' ' < "$TMP/b.log"))"
fi

echo "test 6: a matching bucket is left alone"
CHECK_FAILS=0 run_sync "$S" "$TMP/c.log"
if copied marked_20261014-020145-seg0002 "$TMP/c.log"; then fail "matching directory copied again"; else pass "matching directory not copied"; fi

echo "test 7: a stripped directory costs no bucket call"
S="$TMP/d"; mkdir -p "$S/stripped_20261001-000000-seg0001"
echo '{"status": "complete", "started_at": "2026-10-01T00:00:00Z"}' > "$S/stripped_20261001-000000-seg0001/metadata.json"
printf 'synced_at=x\nkey=captures/v2/y/\n' > "$S/stripped_20261001-000000-seg0001/.synced"
run_sync "$S" "$TMP/d.log"
if [[ ! -s "$TMP/d.log" ]]; then pass "no payload: no rclone call"; else fail "no payload: rclone called ($(tr '\n' ' ' < "$TMP/d.log"))"; fi

echo "test 8: a malformed quiet window is rejected"
set +e
STALE=soon run_sync "$TMP/d" "$TMP/e.log"
rc=$?
set -e
if [[ $rc -eq 2 ]]; then pass "bad CSID_SYNC_STALE_MINUTES exits 2"; else fail "bad knob should exit 2 (got $rc)"; fi

echo
if [[ $fails -eq 0 ]]; then
    echo "csid-sync: all invariants hold"
else
    echo "csid-sync: $fails assertion(s) failed" >&2
    exit 1
fi
