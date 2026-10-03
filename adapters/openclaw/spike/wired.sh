#!/usr/bin/env bash
# Live harness: the real pair-api binary + the real OpenClaw gateway (v2026.9.7) + the pair-spike
# plugin + a loopback mock model server. Offline, loopback only, no real keys.
#
#   OPENCLAW_DIR=<built checkout> adapters/openclaw/spike/wired.sh [phase ...]
#
# Needs: Node 24 on PATH, jq, docker (Postgres client via scripts/lib/pgtools.sh), a pair-api debug
# build (built here with `cargo build --offline` if missing). Uses a uniquely named scratch
# database (pair_w2_<id>), never `pair`, and drops it at the end. Evidence is sanitized into
# adapters/openclaw/spike/evidence/wired/. Throwaway tokens are generated per run.
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
: "${OPENCLAW_DIR:?set OPENCLAW_DIR to the built OpenClaw checkout (read-only)}"
for tool in node jq curl docker; do command -v "$tool" >/dev/null || { echo "wired: need $tool" >&2; exit 2; }; done

RUN_ID="$(uuidgen | tr 'A-Z' 'a-z' | tr -d '-' | cut -c1-12)"
# The temp dir is created here (and removed at exit), resolved with pwd -P so the physical path
# (/private/var/... on macOS) is what the logs contain and what sanitize replaces.
SPIKE_RAW="$(mktemp -d "${TMPDIR:-/tmp}/pair-wired-XXXXXX")"
SPIKE_DIR="$(cd "$SPIKE_RAW" && pwd -P)"
export SPIKE_DIR
# Fixed on purpose: never taken from the environment, so the final replace cannot be aimed elsewhere.
EVID="$HERE/evidence/wired"
EVID_FAILED="$HERE/evidence/wired.failed"
STAGE="$SPIKE_DIR/evidence-stage"
OUT="$SPIKE_DIR/out"
WS="$SPIKE_DIR/workspace"
GW_PORT="${GW_PORT:-18921}"; MOCK_PORT="${MOCK_PORT:-18922}"; PAIR_PORT="${PAIR_PORT:-18923}"
SVC_TOKEN="$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')"
NEW_SVC_TOKEN="$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')"
export OPENCLAW_GATEWAY_TOKEN="$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')"
PAIR_BIN="$ROOT/service/target/debug/pair-api"
DB="pair_w2_${RUN_ID}"
SLOW_MS=25000
FAILS=0

# shellcheck source=../../../scripts/lib/pgtools.sh
source "$ROOT/scripts/lib/pgtools.sh"
# shellcheck source=env.sh
source "$HERE/env.sh"
pg_init || exit 2
[ "$DB" != "$PG_DEFAULT_DB" ] || { echo "wired: refusing to use the default database" >&2; exit 2; }

oc() { node "$OPENCLAW_DIR/openclaw.mjs" "$@"; }
log() { printf '[wired %s] %s\n' "$(date +%H:%M:%S)" "$*" >&2; }
dbq() { pg_psql "$DB" -At -c "$1"; }

mkdir -p "$SPIKE_DIR"; RESULTS="$SPIKE_DIR/results.txt"; : > "$RESULTS"
check() { # name, command...
  local name="$1"; shift
  if "$@"; then echo "PASS  $name" | tee -a "$RESULTS" >&2
  else echo "FAIL  $name" | tee -a "$RESULTS" >&2; FAILS=$((FAILS + 1)); fi
}

# ---------- processes ----------
PAIR_PID=""; MOCK_PID=""; GW_PID=""; TURN_PIDS=""
cleanup() {
  log "cleanup"
  local pid
  for pid in $TURN_PIDS; do pkill -P "$pid" 2>/dev/null; kill "$pid" 2>/dev/null; done
  for pid in "$GW_PID" "$MOCK_PID" "$PAIR_PID"; do [ -n "$pid" ] && kill "$pid" 2>/dev/null; done
  sleep 1
  pg_psql postgres -c "DROP DATABASE IF EXISTS \"$DB\" WITH (FORCE)" >/dev/null 2>&1 || true
  # The directory holds the gateway token (config file) and every raw log.
  case "$SPIKE_DIR" in */pair-wired-*) rm -rf "$SPIKE_DIR" ;; esac
}
trap cleanup EXIT

start_pair() { # budget.yaml token
  stop_pair
  ( cd "$ROOT"; DATABASE_URL="${DATABASE_URL%/*}/$DB" PAIR_SERVICE_TOKEN="$2" PAIR_BIND="127.0.0.1:$PAIR_PORT" \
      PAIR_MIGRATIONS_DIR="$ROOT/migrations" PAIR_DATA_DIR="$ROOT" \
      PAIR_MODELS_CONFIG="$HERE/wired/models.test.yaml" PAIR_BUDGET_CONFIG="$HERE/wired/$1" \
      PAIR_POLICY_CONFIG="$ROOT/config/policy.yaml" PAIR_CONTEXT_CONFIG="$ROOT/config/context.yaml" \
      PAIR_WORKSPACE_ROOT="$WS" RUST_LOG=info \
      nohup "$PAIR_BIN" >> "$OUT/pair-api.log" 2>&1 & echo $! > "$SPIKE_DIR/pair.pid" )
  PAIR_PID="$(cat "$SPIKE_DIR/pair.pid")"
  local i
  for i in $(seq 1 60); do
    curl -fs "http://127.0.0.1:$PAIR_PORT/healthz" >/dev/null 2>&1 && return 0
    kill -0 "$PAIR_PID" 2>/dev/null || { log "pair-api exited early"; return 1; }
    sleep 0.5
  done
  return 1
}
stop_pair() {
  [ -n "$PAIR_PID" ] && kill "$PAIR_PID" 2>/dev/null
  [ -n "$PAIR_PID" ] && wait "$PAIR_PID" 2>/dev/null
  PAIR_PID=""
}

start_mock() {
  MOCK_PORT="$MOCK_PORT" MOCK_LOG="$OUT/mock-requests.jsonl" MOCK_ALL_LOG="$OUT/mock-all-requests.log" \
    MOCK_WORKSPACE="$WS" MOCK_SLOW_MS="$SLOW_MS" nohup node "$HERE/mock-openai-server.ts" > "$OUT/mock.log" 2>&1 &
  MOCK_PID=$!
  sleep 1; touch "$OUT/mock-requests.jsonl" "$OUT/mock-all-requests.log"
}

# The gateway restores a "last good" backup when it finds an unstamped config, so the rendered file
# is stamped by `openclaw telemetry off` (which rewrites it with metadata) after stale backups are
# removed. Verified by reading the allowConversationAccess value back out of the live file.
render_config() { # conv_access
  rm -f "$OPENCLAW_CONFIG_PATH".bak* "$OPENCLAW_CONFIG_PATH".clobbered.*
  sed -e "s#__SPIKE_DIR__#$SPIKE_DIR#g" -e "s#__PLUGIN_DIR__#$HERE/../pair-spike#g" \
      -e "s#__GATEWAY_TOKEN__#$OPENCLAW_GATEWAY_TOKEN#" -e "s#__GATEWAY_PORT__#$GW_PORT#" \
      -e "s#__MOCK_PORT__#$MOCK_PORT#" -e "s#__CONV_ACCESS__#$1#" \
      "$HERE/wired/openclaw.wired.template.json5" > "$OPENCLAW_CONFIG_PATH"
  oc telemetry off >/dev/null 2>&1
  [ "$(grep -c "allowConversationAccess\"*: $1" "$OPENCLAW_CONFIG_PATH")" -ge 1 ] || { echo "wired: config did not keep allowConversationAccess=$1" >&2; exit 2; }
}

start_gateway() { # conv_access [pair_url]
  stop_gateway
  render_config "$1"
  : > "$OUT/gateway.log"
  PAIR_API_URL="${2:-http://127.0.0.1:$PAIR_PORT}" PAIR_SERVICE_TOKEN="$SVC_TOKEN" \
    nohup node "$OPENCLAW_DIR/openclaw.mjs" gateway run --port "$GW_PORT" --bind loopback \
    > "$OUT/gateway.log" 2>&1 &
  GW_PID=$!
  local i
  for i in $(seq 1 240); do
    grep -q "plugin" "$OUT/gateway.log" 2>/dev/null && (exec 3<>"/dev/tcp/127.0.0.1/$GW_PORT") 2>/dev/null && { sleep 8; return 0; }
    kill -0 "$GW_PID" 2>/dev/null || { log "gateway exited early"; return 1; }
    sleep 1
  done
  return 1
}
stop_gateway() { [ -n "$GW_PID" ] && kill "$GW_PID" 2>/dev/null && wait "$GW_PID" 2>/dev/null; GW_PID=""; }

# ---------- turns and evidence ----------
turn() { # name session message  -> $OUT/turn-<name>.json (+ .exit)
  oc agent --agent main --session-key "agent:main:$2" --message "$3" --timeout 120 --json \
    > "$OUT/turn-$1.json" 2> "$OUT/turn-$1.err"
  echo $? > "$OUT/turn-$1.exit"
}
completions() { grep -c . "$OUT/mock-requests.jsonl" 2>/dev/null || true; }
all_requests() { grep -c . "$OUT/mock-all-requests.log" 2>/dev/null || true; }
hooks() { jq -c "select($1)" "$OUT/hooks.jsonl" 2>/dev/null; }
turn_text() { cat "$OUT/turn-$1.json" "$OUT/turn-$1.err" 2>/dev/null; }

# Order matters: the longest, most specific prefixes first. SPIKE_DIR (physical and raw spelling)
# and ROOT / OPENCLAW_DIR all live under HOME, so HOME must come last.
sanitize() {
  sed -e "s#$SPIKE_DIR#<SPIKE_DIR>#g" -e "s#$SPIKE_RAW#<SPIKE_DIR>#g" \
      -e "s#$SVC_TOKEN#<service-token>#g" -e "s#$NEW_SVC_TOKEN#<rotated-token>#g" \
      -e "s#$OPENCLAW_GATEWAY_TOKEN#<gateway-token>#g" \
      -e "s#$ROOT#<REPO>#g" -e "s#$OPENCLAW_DIR#<OPENCLAW>#g" -e "s#$HOME#<HOME>#g"
}
snap() { # phase  (copies sanitized outputs, then truncates the per-phase logs)
  local d="$STAGE/$1" f; mkdir -p "$d"
  for f in "$OUT"/*.jsonl "$OUT"/*.log "$OUT"/turn-*.json "$OUT"/turn-*.err "$OUT"/turn-*.exit "$SPIKE_DIR"/db-*.txt; do
    [ -f "$f" ] && sanitize < "$f" > "$d/$(basename "$f")"
  done
  rm -f "$OUT"/turn-* "$SPIKE_DIR"/db-*.txt
  : > "$OUT/hooks.jsonl"; : > "$OUT/usage.jsonl"; : > "$OUT/mock-requests.jsonl"; : > "$OUT/mock-all-requests.log"
  : > "$OUT/pair-api.log"
}

# ---------- setup ----------
setup() {
  local port
  for port in "$GW_PORT" "$MOCK_PORT" "$PAIR_PORT"; do
    if lsof -nP -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then echo "wired: port $port already in use (stale process?)" >&2; exit 2; fi
  done
  mkdir -p "$OUT" "$WS" "$OPENCLAW_HOME" "$OPENCLAW_STATE_DIR" "$STAGE"
  # Build the plugin from HEAD's src with the checkout's tsc, so the gateway loads what is recorded.
  OPENCLAW_DIR="$OPENCLAW_DIR" "$HERE/setup-plugin.sh" >&2 || { echo "wired: plugin build failed" >&2; exit 2; }
  {
    echo "head $(git -C "$ROOT" rev-parse HEAD)"
    echo "node $(node -v)"
  } >> "$RESULTS"
  check "plugin source tree is clean (evidence matches HEAD)" test -z "$(git -C "$ROOT" status --porcelain -- adapters/openclaw/pair-spike adapters/openclaw/spike)"
  printf 'HELLO-CANARY-7731\n' > "$WS/hello.txt"
  printf 'TFVARS-CANARY-4410\n' > "$WS/prod.tfvars"
  cat > "$WS/write_canary.py" <<EOF
open("$SPIKE_DIR/canary-executed", "w").write("python3 ran\n")
EOF
  : > "$OUT/hooks.jsonl"; : > "$OUT/usage.jsonl"; : > "$OUT/pair-api.log"
  [ -x "$PAIR_BIN" ] || ( cd "$ROOT/service" && cargo build --offline -p pair-api --bin pair-api ) || exit 2
  log "scratch db $DB; spike dir $SPIKE_DIR"
  pg_psql postgres -c "CREATE DATABASE \"$DB\"" || exit 2
  start_pair budget.open.yaml "$SVC_TOKEN" || { cat "$OUT/pair-api.log" >&2; exit 2; }
  start_mock
  start_gateway true || { tail -30 "$OUT/gateway.log" >&2; exit 2; }
  oc plugins inspect pair-spike --runtime --json > "$OUT/plugins-inspect.json" 2>&1
  local nmig files
  nmig="$(dbq "SELECT count(*) FROM _sqlx_migrations WHERE success")"
  files="$(find "$ROOT/migrations" -name '*.sql' | wc -l | tr -d ' ')"
  echo "migrations applied=$nmig files=$files" > "$SPIKE_DIR/db-migrations.txt"
  check "every migration applied to the scratch database ($nmig of $files)" test "$nmig" = "$files"
  curl -s -o "$SPIKE_DIR/db-readyz.txt" -w '%{http_code}\n' "http://127.0.0.1:$PAIR_PORT/readyz" -H "Authorization: Bearer $SVC_TOKEN" -H "X-Actor: wired-harness" >> "$SPIKE_DIR/db-readyz.txt"
  snap p0-setup
}

# ---------- phases ----------
phase_normal() { # (iv) plus allow positive control
  log "phase normal"
  turn plain w-plain "say hello SCENARIO:plain"
  local rid st
  rid="$(hooks '.hook=="before_model_resolve" and .phase=="reserved"' | jq -r .reservationId | head -1)"
  check "plain turn exits 0" test "$(cat "$OUT/turn-plain.exit")" = 0
  check "plain turn reached the mock with the routed model (reserve then override)" \
    test "$(jq -r .model "$OUT/mock-requests.jsonl" | sort -u)" = "mock-routed"
  check "a reservation id was recorded for the turn" test -n "$rid"
  hooks '.hook=="llm_output"' > "$SPIKE_DIR/hooks-llm.txt"
  check "llm_output reconciled the reservation as settled" \
    test "$(hooks '.hook=="llm_output" and .phase=="reconciled"' | jq -r .state | head -1)" = settled
  dbq "SELECT id, state, reserved_micros, counted_micros, price_version, task_kind FROM budget_reservations WHERE id='$rid'" > "$SPIKE_DIR/db-reservation.txt"
  dbq "SELECT reservation_id, settled, input_tokens, output_tokens, amount_micros, price_version FROM budget_ledger WHERE reservation_id='$rid'" > "$SPIKE_DIR/db-ledger.txt"
  check "DB: reservation row is settled" test "$(dbq "SELECT state FROM budget_reservations WHERE id='$rid'")" = settled
  check "DB: one settled ledger row for the reservation" test "$(dbq "SELECT count(*) FROM budget_ledger WHERE reservation_id='$rid' AND settled")" = 1
  check "DB: ledger amount equals the adapter's computed actual cost" \
    test "$(dbq "SELECT amount_micros FROM budget_ledger WHERE reservation_id='$rid'")" = "$(hooks '.hook=="llm_output" and .phase=="reconciled"' | jq -r .costMicros | head -1)"
  check "DB: reservation uses the explicit kind and price version" \
    test "$(dbq "SELECT task_kind||','||price_version FROM budget_reservations WHERE id='$rid'")" = "default,mock-2026-10-03"
  hooks '.hook=="before_model_resolve" and .phase=="reserve-attempt"' | jq -c '{promptChars, maxCostMicros}' > "$SPIKE_DIR/db-estimate.txt"
  jq -c '{bodyBytes, messageCount}' "$OUT/mock-requests.jsonl" >> "$SPIKE_DIR/db-estimate.txt"
  snap p1-normal-turn

  turn allow w-allow "read the file SCENARIO:allowcat"
  check "allowed tool: exec reached the service and was allowed" \
    test "$(hooks '.hook=="before_tool_call" and .source=="pair-api"' | jq -r .decision | head -1)" = allow
  check "allowed tool: the command executed (canary content in the tool result)" grep -q HELLO-CANARY-7731 "$OUT/turn-allow.json"
  snap p2-allowed-tool
}

phase_deny() { # (i)
  log "phase deny"
  rm -f "$SPIKE_DIR/canary-executed"
  turn denysecret w-secret "do it SCENARIO:svcsecret"
  check "service denial: decision deny from pair-api" \
    test "$(hooks '.hook=="before_tool_call" and .source=="pair-api"' | jq -r .decision | head -1)" = deny
  check "service denial: reason recorded (denied name)" test -n "$(hooks '.hook=="before_tool_call" and .decision=="deny"' | jq -r .reason | head -1)"
  check "service denial: the model saw the block reason" grep -q "pair policy:" "$OUT/turn-denysecret.json"
  check "service denial: secret file content never reached the model or the output" \
    sh -c "! grep -q TFVARS-CANARY-4410 '$OUT/turn-denysecret.json' '$OUT/mock-requests.jsonl'"
  snap p3-denied-secret-read

  turn denypy w-py "do it SCENARIO:svcpython"
  check "service denial: python3 denied by the real engine" \
    test "$(hooks '.hook=="before_tool_call" and .source=="pair-api"' | jq -r .decision | head -1)" = deny
  check "service denial: python3 reason names the executable" \
    sh -c "hooks_reason=\$(jq -r 'select(.decision==\"deny\") | .reason' '$OUT/hooks.jsonl' | head -1); echo \"\$hooks_reason\" | grep -q python3"
  check "service denial: nothing executed (canary file absent)" test ! -e "$SPIKE_DIR/canary-executed"
  snap p4-denied-code-exec

  turn gitpush w-push "do it SCENARIO:gitpush"
  check "git push: sent to pair-api as tool git.push (not shell.exec)" \
    test "$(hooks '.hook=="before_tool_call" and .source=="pair-api"' | jq -r .pairTool | head -1)" = git.push
  check "git push: not allowed (denied by egress or held for approval, which the adapter cannot grant)" \
    test "$(hooks '.hook=="before_tool_call" and .source=="pair-api"' | jq -r .decision | head -1)" != allow
  check "git push: the model saw a block reason" grep -q "pair" "$OUT/turn-gitpush.json"
  snap p4b-denied-git-push
}

wait_for_new_completion() { # baseline
  local i
  for i in $(seq 1 120); do
    [ "$(all_requests)" -gt "$1" ] && return 0
    sleep 0.5
  done
  return 1
}

phase_pair_down() { # (iii)
  log "phase pair down"
  rm -f "$SPIKE_DIR/canary-executed"
  local base; base="$(all_requests)"
  turn slowdown w-down "go SCENARIO:slowtool" &
  local tp=$!; TURN_PIDS="$TURN_PIDS $tp"
  wait_for_new_completion "$base" || log "mock never saw the slow request"
  log "stopping pair-api while the model response is delayed"
  stop_pair
  wait "$tp"
  check "pair-api down mid-run: reservation had succeeded before the stop" \
    test "$(hooks '.hook=="before_model_resolve" and .phase=="reserved"' | wc -l | tr -d ' ')" -ge 1
  check "pair-api down mid-run: tool call blocked (deny-on-error, service unreachable)" \
    test "$(hooks '.hook=="before_tool_call" and .decision=="deny-on-error"' | jq -r .errorKind | head -1)" = unreachable
  check "pair-api down mid-run: tool did not execute (no canary content in output)" \
    sh -c "! grep -q HELLO-CANARY-7731 '$OUT/turn-slowdown.json'"
  check "pair-api down mid-run: the model saw the fail-closed reason" grep -q "policy service unavailable" "$OUT/turn-slowdown.json"
  snap p5a-pair-down-mid-run

  base="$(all_requests)"
  turn newdown w-down2 "say hi SCENARIO:plain"
  check "pair-api down at run start: mock model server saw ZERO requests" test "$(all_requests)" = "$base"
  check "pair-api down at run start: reserve failed as unreachable" \
    test "$(hooks '.hook=="before_model_resolve" and .phase=="refused"' | jq -r .errorKind | head -1)" = unreachable
  check "pair-api down at run start: run blocked (non-zero exit or block message)" \
    sh -c "[ \"\$(cat '$OUT/turn-newdown.exit')\" != 0 ] || grep -qi 'blocked' '$OUT/turn-newdown.json'"
  snap p5b-pair-down-new-run
  start_pair budget.open.yaml "$SVC_TOKEN" || log "pair-api restart failed"
}

phase_token_rotation() { # kill-switch level 3
  log "phase token rotation"
  rm -f "$SPIKE_DIR/canary-executed"
  local base; base="$(all_requests)"
  turn slowrot w-rot "go SCENARIO:slowtool" &
  local tp=$!; TURN_PIDS="$TURN_PIDS $tp"
  wait_for_new_completion "$base" || log "mock never saw the slow request"
  log "rotating PAIR_SERVICE_TOKEN (restart pair-api with a new token) while the response is delayed"
  start_pair budget.open.yaml "$NEW_SVC_TOKEN" || log "pair-api restart failed"
  wait "$tp"
  check "rotation mid-run: tool call blocked (service answered 401 to the old token)" \
    test "$(hooks '.hook=="before_tool_call" and .decision=="deny-on-error"' | jq -r .status | head -1)" = 401
  check "rotation mid-run: tool did not execute" sh -c "! grep -q HELLO-CANARY-7731 '$OUT/turn-slowrot.json'"
  snap p6a-token-rotated-mid-run

  base="$(all_requests)"
  turn newrot w-rot2 "say hi SCENARIO:plain"
  check "rotation at run start: mock model server saw ZERO requests" test "$(all_requests)" = "$base"
  check "rotation at run start: reserve refused with HTTP 401" \
    test "$(hooks '.hook=="before_model_resolve" and .phase=="refused"' | jq -r .status | head -1)" = 401
  snap p6b-token-rotated-new-run
  start_pair budget.open.yaml "$SVC_TOKEN" || log "pair-api restart failed"
}

phase_budget_refused() { # (ii)
  log "phase budget refused"
  start_pair budget.zero.yaml "$SVC_TOKEN" || log "pair-api restart failed"
  local before base; before="$(dbq "SELECT count(*) FROM budget_reservations")"; base="$(all_requests)"
  turn refused w-zero "say hi SCENARIO:plain"
  check "zero caps: mock model server saw ZERO requests (no model call happened)" test "$(all_requests)" = "$base"
  check "zero caps: reserve refused with budget_exceeded" \
    test "$(hooks '.hook=="before_model_resolve" and .phase=="refused"' | jq -r .errorCode | head -1)" = budget_exceeded
  check "zero caps: no override was applied and the run was blocked at before_agent_run" \
    test "$(hooks '.hook=="before_agent_run" and .phase=="pass"' | wc -l | tr -d ' ')" = 0
  check "zero caps: no reservation row was created" test "$(dbq "SELECT count(*) FROM budget_reservations")" = "$before"
  dbq "SELECT count(*) FROM budget_reservations" > "$SPIKE_DIR/db-reservation-count.txt"
  snap p7-budget-refused
  start_pair budget.open.yaml "$SVC_TOKEN" || log "pair-api restart failed"
}

phase_noconv() { # (v)
  log "phase allowConversationAccess=false"
  start_gateway false || { tail -30 "$OUT/gateway.log" >&2; return 1; }
  local before base; before="$(dbq "SELECT count(*) FROM budget_reservations")"; base="$(completions)"
  grep -i "allowConversationAccess" "$OUT/gateway.log" > "$SPIKE_DIR/load-warnings.log" || true
  cp "$SPIKE_DIR/load-warnings.log" "$OUT/load-warnings.log"
  check "loud ERROR at start from the adapter (model gate cannot run)" grep -q "pair-spike: plugins.entries.pair-spike.hooks.allowConversationAccess is not true" "$OUT/gateway.log"
  check "no model hooks were registered, so OpenClaw has nothing to WARN about" \
    sh -c "! grep -q 'typed hook \"before_model_resolve\" blocked' '$OUT/gateway.log'"
  turn noconv w-noconv "say hi SCENARIO:plain"
  check "no opt-in: the model call HAPPENS (the adapter cannot gate it; the ERROR above is the signal)" test "$(completions)" -gt "$base"
  check "no opt-in: model override not applied (default model used)" test "$(jq -r .model "$OUT/mock-requests.jsonl" | sort -u)" = "mock-small"
  check "no opt-in: no reservation was created" test "$(dbq "SELECT count(*) FROM budget_reservations")" = "$before"
  turn noconvdeny w-noconv2 "do it SCENARIO:allowcat"
  check "no opt-in: the tool gate is deny-all (even a read the policy would allow)" grep -q "misconfigured, tool calls are denied" "$OUT/turn-noconvdeny.json"
  check "no opt-in: the tool did not execute" sh -c "! grep -q HELLO-CANARY-7731 '$OUT/turn-noconvdeny.json'"
  snap p8-no-conversation-access
}

phase_misconfig() {
  log "phase misconfigured adapter (non-loopback PAIR_API_URL)"
  local base; base="$(all_requests)"
  start_gateway true "http://10.255.255.1:9" || { tail -30 "$OUT/gateway.log" >&2; return 1; }
  check "non-loopback PAIR_API_URL refused at startup (logged, deny-all installed)" grep -q "refusing to run ungated" "$OUT/gateway.log"
  turn misconf w-misconf "say hi SCENARIO:plain"
  check "misconfigured adapter: mock model server saw ZERO requests" test "$(all_requests)" = "$base"
  snap p9-misconfigured-adapter
}

main() {
  setup
  local phases=("$@"); [ ${#phases[@]} -gt 0 ] || phases=(normal deny pair_down token_rotation budget_refused noconv misconfig)
  local p
  for p in "${phases[@]}"; do "phase_$p"; done
  # Final scans of the evidence: no tokens and no host paths anywhere.
  check "no token appears in the evidence" sh -c "! grep -rqF -e '$SVC_TOKEN' -e '$NEW_SVC_TOKEN' -e '$OPENCLAW_GATEWAY_TOKEN' '$STAGE'"
  check "no host path appears in the evidence (HOME, temp dir, repo)" \
    sh -c "! grep -rqF -e '$HOME' -e '$SPIKE_DIR' -e '$SPIKE_RAW' -e '$ROOT' '$STAGE'"
  cp "$RESULTS" "$STAGE/results.txt"
  publish_evidence
  [ "$FAILS" -eq 0 ]
}

# The committed evidence is replaced only by a run in which every check passed; a failed run
# leaves it untouched and writes its own (sanitized) evidence next to it.
publish_evidence() {
  if [ "$FAILS" -eq 0 ]; then
    rm -rf "$EVID" && mkdir -p "$EVID" && cp -R "$STAGE"/. "$EVID"/
    log "done: all checks passed; evidence replaced in $EVID"
  else
    rm -rf "$EVID_FAILED" && mkdir -p "$EVID_FAILED" && cp -R "$STAGE"/. "$EVID_FAILED"/
    log "done: $FAILS failed check(s); committed evidence untouched; failed run in $EVID_FAILED"
  fi
}
main "$@"
