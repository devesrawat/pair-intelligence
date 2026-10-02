#!/usr/bin/env bash
# Spike driver. Usage: spike.sh <init [true|false]|oc ARGS...|mock-start|mock-stop|gw-start|gw-stop|reset-out>
# Env: SPIKE_DIR (throwaway tree, required), OPENCLAW_DIR (built checkout, required), node 24 on PATH.
set -euo pipefail
: "${SPIKE_DIR:?}" "${OPENCLAW_DIR:?}"
here="$(cd "$(dirname "$0")" && pwd)"
# shellcheck disable=SC1091
source "$here/env.sh"
oc() { node "$OPENCLAW_DIR/openclaw.mjs" "$@"; }
cmd="${1:-}"; shift || true
case "$cmd" in
  init)
    conv="${1:-true}"
    mkdir -p "$OPENCLAW_HOME" "$OPENCLAW_STATE_DIR" "$SPIKE_DIR/out" "$SPIKE_DIR/workspace"
    sed -e "s#__SPIKE_DIR__#$SPIKE_DIR#g" \
        -e "s#__PLUGIN_DIR__#$here/../pair-spike#g" \
        -e "s#__GATEWAY_TOKEN__#$OPENCLAW_GATEWAY_TOKEN#" \
        -e "s#__CONV_ACCESS__#$conv#" \
        -e "s#__INJECT_ERR__#${2:-false}#" \
        "$here/openclaw.spike.template.json5" > "$OPENCLAW_CONFIG_PATH"
    oc telemetry off >/dev/null
    echo "config written: $OPENCLAW_CONFIG_PATH (allowConversationAccess=$conv injectPolicyError=${2:-false})";;
  oc) oc "$@";;
  mock-start)
    MOCK_LOG="$SPIKE_DIR/out/mock-requests.jsonl" nohup node "$here/mock-openai-server.ts" \
      > "$SPIKE_DIR/out/mock.log" 2>&1 &
    echo $! > "$SPIKE_DIR/mock.pid"; sleep 1; cat "$SPIKE_DIR/out/mock.log";;
  mock-stop) kill "$(cat "$SPIKE_DIR/mock.pid")" 2>/dev/null || true;;
  gw-start)
    nohup node "$OPENCLAW_DIR/openclaw.mjs" gateway run --port 18911 --bind loopback \
      > "$SPIKE_DIR/out/gateway.log" 2>&1 &
    echo $! > "$SPIKE_DIR/gw.pid"; echo "gateway pid $(cat "$SPIKE_DIR/gw.pid")";;
  gw-stop) kill "$(cat "$SPIKE_DIR/gw.pid")" 2>/dev/null || true;;
  reset-out) rm -f "$SPIKE_DIR"/out/*.jsonl;;
  save) mkdir -p "$SPIKE_DIR/evidence/$1"; cp "$SPIKE_DIR"/out/*.jsonl "$SPIKE_DIR"/out/*.json "$SPIKE_DIR/evidence/$1/" 2>/dev/null || true
        cp "$SPIKE_DIR/out/gateway.log" "$SPIKE_DIR/evidence/$1/" 2>/dev/null || true;;
  *) echo "unknown command: $cmd" >&2; exit 2;;
esac
