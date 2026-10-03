#!/usr/bin/env bash
# pgtools docker-mode safety: no password in docker argv, loopback only. Uses a fake `docker`; needs no database.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
work="$(mktemp -d)"; trap 'rm -rf "$work"' EXIT
fail() { echo "pgtools test FAIL: $*" >&2; exit 1; }

mkdir -p "$work/bin"
cat > "$work/bin/docker" <<'SH'
#!/bin/sh
# Fake docker: `ps` reports a container; anything else records argv and the PGPASSWORD it inherited.
if [ "$1" = "ps" ]; then echo fakepg; exit 0; fi
printf '%s\n' "$*" >> "$FAKE_DOCKER_LOG"
printf 'env:%s\n' "${PGPASSWORD:-}" >> "$FAKE_DOCKER_LOG"
SH
chmod +x "$work/bin/docker"
export FAKE_DOCKER_LOG="$work/docker.log"
export PATH="$work/bin:$PATH" PAIR_PG_MODE=docker

# 1. password stays out of argv but reaches the client through the environment
out="$(DATABASE_URL='postgres://u:s3cretpw@127.0.0.1:55432/d' bash -c "source '$ROOT/scripts/lib/pgtools.sh'; pg_init; pg_psql d -c 'select 1'")" || fail "pg_psql under fake docker failed: $out"
if grep -q 's3cretpw' <(grep -v '^env:' "$FAKE_DOCKER_LOG"); then fail "PGPASSWORD leaked into docker argv"; fi
grep -q -- '-e PGPASSWORD ' "$FAKE_DOCKER_LOG" || fail "expected valueless -e PGPASSWORD in docker args"
grep -q '^env:s3cretpw$' "$FAKE_DOCKER_LOG" || fail "password not passed via environment"

# 2. non-loopback host: docker mode refuses
if DATABASE_URL='postgres://u:p@db.example.com:5432/d' bash -c "source '$ROOT/scripts/lib/pgtools.sh'; pg_init" 2>"$work/err"; then
  fail "docker mode accepted a non-loopback DB host"
fi
grep -q 'loopback' "$work/err" || fail "refusal does not mention loopback"
echo "pgtools test: PASS"
