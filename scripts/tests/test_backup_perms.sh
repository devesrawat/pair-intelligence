#!/usr/bin/env bash
# scripts/backup creates a private (0700) directory and private (0600) files even under a permissive umask.
# Uses a scratch database; never the live one.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
# shellcheck source=../lib/pgtools.sh
source "$ROOT/scripts/lib/pgtools.sh"
pg_init
work="$(mktemp -d)"
src="pair_t_bkperm_$(od -An -N6 -tx1 /dev/urandom | tr -d ' \n')"
cleanup() { pg_psql postgres -c "DROP DATABASE IF EXISTS \"$src\" WITH (FORCE)" >/dev/null 2>&1 || true; rm -rf "$work"; }
trap cleanup EXIT
fail() { echo "backup perms test FAIL: $*" >&2; exit 1; }

pg_psql postgres -c "CREATE DATABASE \"$src\"" >/dev/null
pg_psql "$src" -c "CREATE TABLE t (n int); INSERT INTO t VALUES (1)" >/dev/null

umask 022
dir="$work/new/backups"   # does not exist yet
PAIR_BACKUP_DIR="$dir" PAIR_BACKUP_DB="$src" PAIR_BACKUP_UNENCRYPTED=1 "$ROOT/scripts/backup" >/dev/null 2>&1 || fail "backup failed"
mode_of() { ls -ld "$1" | cut -c1-10; }
[ "$(mode_of "$dir")" = "drwx------" ] || fail "backup dir mode is $(mode_of "$dir"), want drwx------"
[ "$(mode_of "$work/new")" = "drwx------" ] || fail "parent created by backup is $(mode_of "$work/new"), want drwx------"
f="$(find "$dir" -name 'pair-*.dump' | head -n1)"
[ -n "$f" ] || fail "no dump written"
[ "$(mode_of "$f")" = "-rw-------" ] || fail "dump mode is $(mode_of "$f"), want -rw-------"
echo "backup perms test: PASS"
