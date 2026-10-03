#!/usr/bin/env bash
# OpenClaw spike gate: allow-list policy and fail-closed behaviour (including an unwritable log path).
# Needs Node >= 22.18 (native TypeScript type stripping); needs no OpenClaw checkout and no network.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
MIN_NODE_MAJOR=22
if ! command -v node >/dev/null; then echo "openclaw gate test: SKIP (node not installed)"; exit 0; fi
major="$(node -p 'process.versions.node.split(".")[0]')"
if [ "$major" -lt "$MIN_NODE_MAJOR" ]; then echo "openclaw gate test: SKIP (node $major < $MIN_NODE_MAJOR)"; exit 0; fi
cd "$ROOT/adapters/openclaw/pair-spike"
node --test test/*.test.ts
echo "openclaw gate test: PASS"
