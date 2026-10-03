#!/usr/bin/env bash
# scripts/restore safety tests against scratch databases (pair_t_restore_*). Never touches `pair`.
# Needs the shared Postgres (DATABASE_URL) via local pg tools or docker.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=../lib/pgtools.sh
source "$ROOT/scripts/lib/pgtools.sh"
pg_init

work="$(mktemp -d)"
suffix="$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
created=()
NEW_DB=""
fail() { echo "restore test FAIL: $*" >&2; exit 1; }
cleanup() {
  local d
  for d in "${created[@]+"${created[@]}"}"; do
    pg_psql postgres -c "DROP DATABASE IF EXISTS \"$d\" WITH (FORCE)" >/dev/null 2>&1 || true
    pg_psql postgres -c "DROP DATABASE IF EXISTS \"${d}_incoming\" WITH (FORCE)" >/dev/null 2>&1 || true
    pg_psql postgres -c "DROP DATABASE IF EXISTS \"${d}_old\" WITH (FORCE)" >/dev/null 2>&1 || true
  done
  rm -rf "$work"
}
trap cleanup EXIT

# new_db <suffix> <rows>: creates a scratch db holding table t with <rows> rows and sets NEW_DB.
# (Not command substitution: `created` must be updated in this shell so cleanup sees it.)
new_db() {
  NEW_DB="pair_t_restore_${suffix}_$1"
  created+=("$NEW_DB")
  pg_psql postgres -c "CREATE DATABASE \"$NEW_DB\"" >/dev/null
  pg_psql "$NEW_DB" -c "CREATE TABLE t (n int); INSERT INTO t SELECT generate_series(1, $2)" >/dev/null
}
rows() { pg_psql "$1" -At -c "SELECT count(*) FROM t"; }
db_exists() { [ "$(pg_psql postgres -At -c "SELECT 1 FROM pg_database WHERE datname = '$1'")" = "1" ]; }
run_restore() { "$ROOT/scripts/restore" "$@" >"$work/out" 2>&1; }

# A real (unencrypted) dump of a 7-row source.
new_db src 7
src="$NEW_DB"
PAIR_BACKUP_DIR="$work/bk" PAIR_BACKUP_DB="$src" PAIR_BACKUP_UNENCRYPTED=1 "$ROOT/scripts/backup" >/dev/null 2>&1
good="$(find "$work/bk" -name 'pair-*.dump' | head -n1)"
[ -n "$good" ] || fail "no good dump produced"

restore_failure_leaves_target_intact_corrupt_dump() {
  new_db corrupt 3
  local tgt="$NEW_DB"
  printf 'this is not a pg dump' > "$work/corrupt.dump"
  if PAIR_RESTORE_CONFIRM_DROP="$tgt" run_restore "$work/corrupt.dump" --target-db "$tgt" --drop-existing; then
    fail "corrupt dump restore succeeded"
  fi
  [ "$(rows "$tgt")" = "3" ] || fail "corrupt dump damaged target"
  if db_exists "${tgt}_incoming"; then fail "corrupt dump left _incoming behind"; fi
}

restore_failure_leaves_target_intact_bad_identity() {
  # An age shim that always fails to decrypt stands in for a wrong identity (age need not be installed).
  mkdir -p "$work/shim"
  printf '#!/bin/sh\necho "age: no identity matched" >&2\nexit 1\n' > "$work/shim/age"
  chmod +x "$work/shim/age"
  cp "$good" "$work/bad.dump.age"
  : > "$work/identity.txt"
  new_db badid 4
  local tgt="$NEW_DB"
  if PATH="$work/shim:$PATH" PAIR_BACKUP_AGE_IDENTITY="$work/identity.txt" PAIR_RESTORE_CONFIRM_DROP="$tgt" \
     run_restore "$work/bad.dump.age" --target-db "$tgt" --drop-existing; then
    fail "bad identity restore succeeded"
  fi
  [ "$(rows "$tgt")" = "4" ] || fail "bad identity damaged target"
  if db_exists "${tgt}_incoming"; then fail "bad identity left _incoming behind"; fi
  # identity not set / unreadable
  if PATH="$work/shim:$PATH" PAIR_BACKUP_AGE_IDENTITY="" PAIR_RESTORE_CONFIRM_DROP="$tgt" \
     run_restore "$work/bad.dump.age" --target-db "$tgt" --drop-existing; then
    fail "unset identity restore succeeded"
  fi
  [ "$(rows "$tgt")" = "4" ] || fail "unset identity damaged target"
  if PATH="$work/shim:$PATH" PAIR_BACKUP_AGE_IDENTITY="$work/nonexistent" PAIR_RESTORE_CONFIRM_DROP="$tgt" \
     run_restore "$work/bad.dump.age" --target-db "$tgt" --drop-existing; then
    fail "missing identity file restore succeeded"
  fi
  [ "$(rows "$tgt")" = "4" ] || fail "missing identity file damaged target"
}

restore_requires_confirmation_and_swaps_on_success() {
  new_db confirm 5
  local tgt="$NEW_DB"
  # --drop-existing without explicit confirmation is refused
  if run_restore "$good" --target-db "$tgt" --drop-existing; then fail "--drop-existing without confirmation succeeded"; fi
  [ "$(rows "$tgt")" = "5" ] || fail "unconfirmed drop damaged target"
  # non-empty target without --drop-existing is refused
  if run_restore "$good" --target-db "$tgt"; then fail "non-empty target restored without --drop-existing"; fi
  [ "$(rows "$tgt")" = "5" ] || fail "non-empty target damaged"
  # confirmed replace swaps the restored data in and leaves no stragglers
  if ! PAIR_RESTORE_CONFIRM_DROP="$tgt" run_restore "$good" --target-db "$tgt" --drop-existing; then
    cat "$work/out" >&2; fail "confirmed restore failed"
  fi
  [ "$(rows "$tgt")" = "7" ] || fail "restored target has wrong row count"
  if db_exists "${tgt}_incoming"; then fail "_incoming left after success"; fi
  if db_exists "${tgt}_old"; then fail "_old left after success"; fi
  RESTORED_DB="$tgt"
}

live_guard_keys_on_pair_live_db_not_database_url() {
  new_db live 2
  local live="$NEW_DB"
  if PAIR_LIVE_DB="$live" PAIR_RESTORE_CONFIRM_DROP="$live" run_restore "$good" --target-db "$live" --drop-existing; then
    fail "restore over PAIR_LIVE_DB succeeded without PAIR_RESTORE_CONFIRM_LIVE"
  fi
  [ "$(rows "$live")" = "2" ] || fail "live db damaged"
  # a DATABASE_URL naming a scratch db must not make that db "live"
  if ! DATABASE_URL="${DATABASE_URL%/*}/$RESTORED_DB" PAIR_LIVE_DB=pair PAIR_RESTORE_CONFIRM_DROP="$RESTORED_DB" \
     run_restore "$good" --target-db "$RESTORED_DB" --drop-existing; then
    cat "$work/out" >&2; fail "db named in DATABASE_URL wrongly treated as live"
  fi
}

RESTORED_DB=""
restore_failure_leaves_target_intact_corrupt_dump
restore_failure_leaves_target_intact_bad_identity
restore_requires_confirmation_and_swaps_on_success
live_guard_keys_on_pair_live_db_not_database_url
echo "restore test: PASS"
