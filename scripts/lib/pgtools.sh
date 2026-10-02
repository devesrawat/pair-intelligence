#!/usr/bin/env bash
# Source me. PostgreSQL client helpers that use local psql/pg_dump/pg_restore when
# installed, else `docker exec` into the container publishing the DATABASE_URL port.
#
# Provides: pg_init, pg_psql <db> [psql args], pg_dump_custom <db>, pg_restore_stdin <db>,
#           PG_DEFAULT_DB (database named in DATABASE_URL).

DATABASE_URL="${DATABASE_URL:-postgres://pair:pair@127.0.0.1:55432/pair}"

pg_init() {
  local re='^postgres(ql)?://([^:@/]+)(:([^@]*))?@([^:/]+)(:([0-9]+))?/([^?]+)'
  if [[ ! "$DATABASE_URL" =~ $re ]]; then
    echo "pgtools: cannot parse DATABASE_URL" >&2; return 1
  fi
  PGUSER_="${BASH_REMATCH[2]}"
  PGPASSWORD_="${BASH_REMATCH[4]}"
  PGHOST_="${BASH_REMATCH[5]}"
  PGPORT_="${BASH_REMATCH[7]:-5432}"
  PG_DEFAULT_DB="${BASH_REMATCH[8]}"
  if command -v psql >/dev/null && command -v pg_dump >/dev/null && command -v pg_restore >/dev/null; then
    PG_MODE=local
  elif command -v docker >/dev/null; then
    PG_MODE=docker
    PG_CONTAINER="${PAIR_PG_CONTAINER:-$(docker ps --filter "publish=${PGPORT_}" --format '{{.Names}}' | head -n1)}"
    if [ -z "$PG_CONTAINER" ]; then
      echo "pgtools: no local pg client tools and no container publishes port ${PGPORT_} (set PAIR_PG_CONTAINER)" >&2
      return 1
    fi
  else
    echo "pgtools: need psql/pg_dump/pg_restore or docker" >&2; return 1
  fi
}

# Run a client command against db $1. Remaining args are the tool + its args (no connection flags).
_pg_run() {
  local db="$1" tool="$2"; shift 2
  if [ "$PG_MODE" = local ]; then
    PGPASSWORD="$PGPASSWORD_" "$tool" -h "$PGHOST_" -p "$PGPORT_" -U "$PGUSER_" -d "$db" "$@"
  else
    docker exec -i -e PGPASSWORD="$PGPASSWORD_" "$PG_CONTAINER" "$tool" -U "$PGUSER_" -d "$db" "$@"
  fi
}

pg_psql() { local db="$1"; shift; _pg_run "$db" psql -X -v ON_ERROR_STOP=1 -q "$@"; }
pg_dump_custom() { _pg_run "$1" pg_dump -Fc --no-owner --no-privileges; }
pg_restore_stdin() { _pg_run "$1" pg_restore --no-owner --no-privileges --exit-on-error; }
