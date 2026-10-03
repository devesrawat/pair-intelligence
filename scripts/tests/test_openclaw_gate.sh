#!/usr/bin/env bash
# OpenClaw adapter: local rules, fail-closed gate, pair-api client, tool mapping, budget gate,
# against a loopback mock pair-api. Needs Node >= 22.18 (native TypeScript type stripping); needs
# no OpenClaw checkout and no network beyond loopback.
# If OPENCLAW_DIR points at a built checkout, the plugin is also type-checked with its tsc.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
MIN_NODE_MAJOR=22
if ! command -v node >/dev/null; then echo "openclaw gate test: SKIP (node not installed)"; exit 0; fi
major="$(node -p 'process.versions.node.split(".")[0]')"
if [ "$major" -lt "$MIN_NODE_MAJOR" ]; then echo "openclaw gate test: SKIP (node $major < $MIN_NODE_MAJOR)"; exit 0; fi
cd "$ROOT/adapters/openclaw/pair-spike"
node --test test/*.test.ts
if [ -n "${OPENCLAW_DIR:-}" ] && [ -x "$OPENCLAW_DIR/node_modules/.bin/tsc" ]; then
  "$ROOT/adapters/openclaw/spike/setup-plugin.sh"
  echo "openclaw gate test: type-check against upstream tsc OK"
else
  echo "openclaw gate test: upstream type-check SKIPPED (set OPENCLAW_DIR to a built checkout)"
fi
echo "openclaw gate test: PASS"
