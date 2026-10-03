#!/usr/bin/env bash
# deploy/compose.yaml has no default credentials and hardens pair-api. Only renders config; starts nothing.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
fail() { echo "compose test FAIL: $*" >&2; exit 1; }
if ! command -v docker >/dev/null || ! docker compose version >/dev/null 2>&1; then
  echo "compose test: SKIP (docker compose not available)"; exit 0
fi

# 1. refuses to render without a password (no pair/pair fallback)
if env -u POSTGRES_PASSWORD docker compose -f "$ROOT/deploy/compose.yaml" --profile app config >/dev/null 2>&1; then
  fail "compose.yaml rendered without POSTGRES_PASSWORD"
fi
if grep -n 'POSTGRES_PASSWORD:-' "$ROOT/deploy/compose.yaml" >/dev/null; then
  fail "compose.yaml still carries a default password"
fi

# 2. with a password: pair-api is hardened and uses baked-in config
rendered="$(POSTGRES_PASSWORD=test-only-pw docker compose -f "$ROOT/deploy/compose.yaml" --profile app config)"
for needle in "read_only: true" "no-new-privileges:true" "- ALL" "/tmp" "PAIR_MODELS_CONFIG: /app/config/models.yaml" "PAIR_MIGRATIONS_DIR: /app/migrations"; do
  printf '%s\n' "$rendered" | grep -qF -- "$needle" || fail "rendered config lacks: $needle"
done

# 3. the dev file is self-contained and the only place with a fixed password
dev="$(env -u POSTGRES_PASSWORD docker compose -f "$ROOT/deploy/compose.dev.yaml" config)" || fail "compose.dev.yaml does not render without env"
printf '%s\n' "$dev" | grep -q 'POSTGRES_PASSWORD: pair' || fail "compose.dev.yaml lacks the dev password"
head -n 3 "$ROOT/deploy/compose.dev.yaml" | grep -q 'DEV ONLY' || fail "compose.dev.yaml is not clearly marked DEV ONLY"
echo "compose test: PASS"
