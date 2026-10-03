#!/usr/bin/env bash
# Retention safety: only exact backup names are ever candidates; pruning failures are loud. No database needed.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
dir="$(mktemp -d)"; trap 'rm -rf "$dir"' EXIT
fail() { echo "retention safety test FAIL: $*" >&2; exit 1; }

# 1. look-alike names are never deleted, even though they are old and numerous.
for d in 01 02 03 04 05 06 07 08 09 10 11 12; do
  touch "$dir/pair-202001${d}T000000Z.dump"
done
lookalikes=(
  "pair-20190101T000000Z.dump.bak"
  "pair-20190101T000000Z.dumpx"
  "pair-20190101T000000Z.dump.age.old"
  "pair-20190101T000000Z.dump.age.partial"
  "pair-2019010T000000Z.dump"
  "pair-20190101T000000Z.dump.age.age"
  "xpair-20190101T000000Z.dump"
)
for f in "${lookalikes[@]}"; do touch "$dir/$f"; done
PAIR_BACKUP_DIR="$dir" "$ROOT/scripts/backup" --prune-only >/dev/null
for f in "${lookalikes[@]}"; do [ -e "$dir/$f" ] || fail "look-alike $f was deleted"; done

# 2. a failure while computing victims must fail the prune and delete nothing.
before="$(ls "$dir" | wc -l | tr -d ' ')"
touch "$dir/pair-99999999T000000Z.dump"   # exact name shape, impossible date: iso_week fails
if PAIR_BACKUP_DIR="$dir" "$ROOT/scripts/backup" --prune-only >/dev/null 2>&1; then
  fail "prune reported success although victim computation failed"
fi
after="$(ls "$dir" | wc -l | tr -d ' ')"
[ "$after" -eq $((before + 1)) ] || fail "prune deleted files despite failing ($before -> $after)"
echo "retention safety test: PASS"
