#!/usr/bin/env bash
# Retention policy test: 7 daily + 4 weekly. Needs no database.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
dir="$(mktemp -d)"; trap 'rm -rf "$dir"' EXIT

fmt_epoch() { if date --version >/dev/null 2>&1; then date -u -d "@$1" +%Y%m%dT%H%M%SZ; else date -u -r "$1" +%Y%m%dT%H%M%SZ; fi; }
base=1790000000   # fixed epoch so the test is deterministic
day=86400
for d in $(seq 0 59); do
  touch "$dir/pair-$(fmt_epoch $((base - d * day))).dump"            # one per day for 60 days
  touch "$dir/pair-$(fmt_epoch $((base - d * day - 3600))).dump.age"  # older duplicate same day
done
touch "$dir/unrelated.txt" "$dir/pair-20200101T000000Z.dump.partial"

PAIR_BACKUP_DIR="$dir" "$ROOT/scripts/backup" --prune-only >/dev/null

fail() { echo "retention test FAIL: $*" >&2; exit 1; }
kept="$(find "$dir" -maxdepth 1 -name 'pair-????????T??????Z.dump*' ! -name '*.partial' | wc -l | tr -d ' ')"
[ "$kept" -eq 11 ] || fail "expected 11 retained backups (7 daily + 4 weekly), got $kept"
[ -e "$dir/unrelated.txt" ] || fail "unrelated file removed"
[ -e "$dir/pair-20200101T000000Z.dump.partial" ] || fail "partial file touched"
for d in $(seq 0 6); do
  [ -e "$dir/pair-$(fmt_epoch $((base - d * day))).dump" ] || fail "daily d-$d missing"
done
[ ! -e "$dir/pair-$(fmt_epoch $((base - 3600))).dump.age" ] || fail "same-day duplicate kept"
source "$ROOT/scripts/lib/retention.sh"
weeks="$(find "$dir" -maxdepth 1 -name 'pair-*' ! -name '*.partial' | sort -r | tail -n 4 | while read -r f; do b="$(basename "$f")"; iso_week "${b:5:8}"; done | sort -u | wc -l | tr -d ' ')"
[ "$weeks" -eq 4 ] || fail "expected 4 distinct weekly backups, got $weeks"
for w in 20260913 20260906 20260830 20260823; do
  ls "$dir" | grep -q "pair-${w}T" || fail "expected weekly backup for $w"
done
dweeks="$(for d in $(seq 0 6); do iso_week "$(fmt_epoch $((base - d * day)) | cut -c1-8)"; done | sort -u)"
for f in $(find "$dir" -maxdepth 1 -name 'pair-*' ! -name '*.partial' | sort -r | tail -n 4); do
  b="$(basename "$f")"
  [[ "$dweeks" != *"$(iso_week "${b:5:8}")"* ]] || fail "weekly $b shares an ISO week with a daily keep"
done
echo "retained: $(find "$dir" -maxdepth 1 -name 'pair-*' ! -name '*.partial' -exec basename {} \; | sort -r | tr '\n' ' ')"
echo "retention test: PASS"
