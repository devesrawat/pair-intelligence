#!/usr/bin/env bash
# Links the pinned OpenClaw checkout into the plugin's node_modules (types + runtime
# resolution of "openclaw/plugin-sdk/*") and compiles with the checkout's tsc.
# Read-only against the checkout. Usage: OPENCLAW_DIR=/path/to/openclaw ./setup-plugin.sh
set -euo pipefail
: "${OPENCLAW_DIR:?set OPENCLAW_DIR to the built OpenClaw checkout}"
here="$(cd "$(dirname "$0")/../pair-spike" && pwd)"
mkdir -p "$here/node_modules/@types"
ln -sfn "$OPENCLAW_DIR" "$here/node_modules/openclaw"
ln -sfn "$OPENCLAW_DIR/node_modules/@types/node" "$here/node_modules/@types/node"
cd "$here"
"$OPENCLAW_DIR/node_modules/.bin/tsc" -p tsconfig.json
echo "built $here/dist"
