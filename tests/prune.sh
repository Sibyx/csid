#!/usr/bin/env bash
# Acceptance tests for `scripts/csid-prune`.
#
# This script deletes measurements. It is the only thing in the tree that does,
# and until 2026-08-18 it had no test at all — which is how it came to keep
# `capture.csiq` forever while its own header said the spool would be reclaimed.
#
# The invariants below are the ones whose violation loses data. They are asserted
# against a synthetic spool, so the tests need no bucket and no node:
#
#   1. A verified, out-of-grace session is stripped of ALL payload, not a
#      hardcoded list of two names.
#   2. `metadata.json` and `.synced` always survive — the on-node index.
#   3. A session with NO `.synced` marker is never touched, at any pressure.
#      This is the session root of a run in progress, and on 2026-08-17 it held
#      the only copy of 14.5 GB of time transfer.
#   4. A session inside its grace window is not stripped without pressure.
#   5. Under pressure, grace is ignored — oldest ship first.
#   6. When the bucket does not confirm a directory, it is skipped, not stripped.
#   7. An unreachable floor exits non-zero and still deletes nothing unverified.
#
# Usage: tests/prune.sh   (bash 3.2+, no dependencies)
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
PRUNE="${HERE}/../scripts/csid-prune"
[[ -x "$PRUNE" ]] || { echo "not executable: $PRUNE" >&2; exit 1; }

fails=0
pass() { printf '  ok   %s\n' "$1"; }
fail() { printf '  FAIL %s\n' "$1" >&2; fails=$(( fails + 1 )); }

check_exists() {
    if [[ -e "$1" ]]; then pass "$2"; else fail "$2 (missing: $1)"; fi
}
check_gone() {
    if [[ -e "$1" ]]; then fail "$2 (still present: $1)"; else pass "$2"; fi
}

# A spool with three shipped sessions of differing age and one unshipped root.
make_spool() {
    local s="$1"
    rm -rf "$s"; mkdir -p "$s"

    local d
    for d in old-seg0001 mid-seg0002 new-seg0003; do
        mkdir -p "$s/$d"
        printf '%*s' 4096 '' > "$s/$d/capture.raw"
        printf '%*s' 2048 '' > "$s/$d/capture.csiq"
        # A future artefact type. Invariant 1 says it goes without being named.
        printf '%*s' 512 '' > "$s/$d/ble_rssi.parquet"
        echo '{"status":"complete"}' > "$s/$d/metadata.json"
        echo shipped > "$s/$d/.synced"
    done

    # The session root of a run that never closed: no marker, and the only copy
    # of its time transfer.
    mkdir -p "$s/root-session"
    printf '%*s' 8192 '' > "$s/root-session/time_transfer.jsonl"
    printf '%*s' 1024 '' > "$s/root-session/ble_scan.jsonl"
    echo '{"status":"capturing"}' > "$s/root-session/metadata.json"

    # Ship times: old = 3 days ago, mid = 2 hours ago, new = now.
    touch_ago "$s/old-seg0001/.synced" 4320
    touch_ago "$s/mid-seg0002/.synced" 120
}

# Portable "set mtime N minutes ago" — GNU and BSD `touch` disagree on flags.
touch_ago() {
    local path="$1" mins="$2" stamp
    if stamp="$(date -u -d "@$(( $(date -u +%s) - mins * 60 ))" +%Y%m%d%H%M.%S 2>/dev/null)"; then
        touch -t "$stamp" "$path"
    else
        stamp="$(date -u -r "$(( $(date -u +%s) - mins * 60 ))" +%Y%m%d%H%M.%S)"
        touch -t "$stamp" "$path"
    fi
}

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# Verification is exercised separately in test 6; the rest run with it off so the
# suite needs no bucket.
run_prune() {
    env CSID_SPOOL="$1" \
        CSID_PRUNE_GRACE_DAYS="${GRACE:-1}" \
        CSID_PRUNE_MIN_FREE_GB="${FLOOR:-0}" \
        CSID_PRUNE_VERIFY="${VERIFY:-0}" \
        bash "$PRUNE"
}

echo "test 1+2+3+4: grace pass strips all payload, keeps the index, spares the unshipped root"
S="$TMP/a"; make_spool "$S"
GRACE=1 FLOOR=0 VERIFY=0 run_prune "$S" >/dev/null
check_gone   "$S/old-seg0001/capture.raw" "out-of-grace: capture.raw stripped"
check_gone   "$S/old-seg0001/capture.csiq" "out-of-grace: capture.csiq stripped (the 2026-08-18 bug)"
check_gone   "$S/old-seg0001/ble_rssi.parquet" "out-of-grace: unnamed future artefact stripped"
check_exists "$S/old-seg0001/metadata.json"          "out-of-grace: metadata.json kept"
check_exists "$S/old-seg0001/.synced"                "out-of-grace: .synced kept"
check_exists "$S/mid-seg0002/capture.raw"            "in-grace: not stripped without pressure"
check_exists "$S/new-seg0003/capture.raw"            "just-shipped: not stripped without pressure"
check_exists "$S/root-session/time_transfer.jsonl"   "unshipped root: time_transfer.jsonl survives"
check_exists "$S/root-session/ble_scan.jsonl"        "unshipped root: ble_scan.jsonl survives"

echo "test 5+7: an unreachable floor ignores grace, exits non-zero, and spares the unshipped root"
S="$TMP/b"; make_spool "$S"
set +e
GRACE=1 FLOOR=999999 VERIFY=0 run_prune "$S" >/dev/null 2>&1
rc=$?
set -e
if [[ $rc -ne 0 ]]; then pass "unreachable floor exits non-zero"; else fail "unreachable floor should exit non-zero (got $rc)"; fi
check_gone   "$S/mid-seg0002/capture.raw" "pressure: in-grace session stripped"
check_gone   "$S/new-seg0003/capture.raw" "pressure: just-shipped session stripped"
check_exists "$S/root-session/time_transfer.jsonl"   "pressure: unshipped root STILL survives"
check_exists "$S/root-session/ble_scan.jsonl"        "pressure: unshipped root ble_scan.jsonl STILL survives"
check_exists "$S/new-seg0003/metadata.json"          "pressure: index still kept"

echo "test 6: verification failure skips rather than strips"
S="$TMP/c"; make_spool "$S"
# Point at a bucket that cannot answer. Verification must fail, and failure must
# mean "keep the bytes".
set +e
env CSID_SPOOL="$S" CSID_PRUNE_GRACE_DAYS=1 CSID_PRUNE_MIN_FREE_GB=0 \
    CSID_PRUNE_VERIFY=1 \
    CSID_S3_BUCKET=nonexistent-bucket \
    CSID_S3_ENDPOINT=http://127.0.0.1:1 \
    CSID_S3_ACCESS_KEY=nope CSID_S3_SECRET_KEY=nope \
    bash "$PRUNE" >/dev/null 2>&1
set -e
check_exists "$S/old-seg0001/capture.raw"  "unconfirmed by the bucket: capture.raw kept"
check_exists "$S/old-seg0001/capture.csiq" "unconfirmed by the bucket: capture.csiq kept"

echo "test 8: no credentials means no deletion"
S="$TMP/d"; make_spool "$S"
set +e
env CSID_SPOOL="$S" CSID_PRUNE_GRACE_DAYS=1 CSID_PRUNE_MIN_FREE_GB=0 \
    CSID_PRUNE_VERIFY=1 bash "$PRUNE" >/dev/null 2>&1
set -e
check_exists "$S/old-seg0001/capture.raw" "no credentials: nothing deleted"

echo "test 9: idempotent, and an empty spool is not an error"
S="$TMP/e"; make_spool "$S"
GRACE=1 FLOOR=0 VERIFY=0 run_prune "$S" >/dev/null
GRACE=1 FLOOR=0 VERIFY=0 run_prune "$S" >/dev/null
pass "second pass over the same spool succeeds"
S="$TMP/f"; rm -rf "$S"; mkdir -p "$S"
GRACE=1 FLOOR=0 VERIFY=0 run_prune "$S" >/dev/null
pass "empty spool succeeds"

echo "test 10: malformed knobs are rejected before anything is touched"
S="$TMP/g"; make_spool "$S"
set +e
env CSID_SPOOL="$S" CSID_PRUNE_GRACE_DAYS=notanumber CSID_PRUNE_VERIFY=0 bash "$PRUNE" >/dev/null 2>&1
rc=$?
set -e
if [[ $rc -eq 2 ]]; then pass "bad grace rejected with exit 2"; else fail "bad grace should exit 2 (got $rc)"; fi
check_exists "$S/old-seg0001/capture.raw" "bad knob: nothing deleted"

echo
if [[ $fails -eq 0 ]]; then
    echo "csid-prune: all invariants hold"
else
    echo "csid-prune: $fails assertion(s) failed" >&2
    exit 1
fi
