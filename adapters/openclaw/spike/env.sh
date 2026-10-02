#!/usr/bin/env bash
# Source this. Isolates OpenClaw into a throwaway tree; never touches ~/.openclaw.
# Requires SPIKE_DIR. OPENCLAW_HOME relocates the home dir OpenClaw derives paths from.
: "${SPIKE_DIR:?set SPIKE_DIR to a throwaway directory}"
export OPENCLAW_HOME="$SPIKE_DIR/home"
export OPENCLAW_STATE_DIR="$SPIKE_DIR/state"
export OPENCLAW_CONFIG_PATH="$SPIKE_DIR/state/openclaw.json"
export OPENCLAW_NO_AUTO_UPDATE=1
export CLAWHUB_DISABLE_TELEMETRY=1
# Bonjour/mDNS multicasts to 224.0.0.251 on the LAN by default; the spike must stay on loopback.
export OPENCLAW_DISABLE_BONJOUR=1
export OPENCLAW_GATEWAY_TOKEN="${OPENCLAW_GATEWAY_TOKEN:-spike-token-local-only}"
export NO_PROXY=127.0.0.1,localhost
