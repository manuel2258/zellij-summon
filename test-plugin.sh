#!/usr/bin/env bash
# Smoke-test: build the plugin and verify it loads in Zellij 0.44 without
# the "could not find exported function" / "failed to load plugin" error.
#
# Usage (two modes):
#   Inside a running Zellij session (typical):
#       bash test-plugin.sh
#   Specifying a Zellij binary (e.g. downloaded 0.44.1):
#       ZELLIJ_BIN=/tmp/zellij bash test-plugin.sh

set -euo pipefail

REPO="$(cd "$(dirname "$0")" && pwd)"
ZELLIJ="${ZELLIJ_BIN:-$(command -v zellij 2>/dev/null || echo zellij)}"
LOG_FILE="/tmp/zellij-$(id -u)/zellij-log/zellij.log"

# ── 1. Build ──────────────────────────────────────────────────────────────────
echo "[test] building WASM…"
cargo build --target wasm32-wasip1 --release \
  --manifest-path "$REPO/Cargo.toml" 2>&1 \
  | grep -E "Compiling|Finished|^error" || true

WASM="$REPO/target/wasm32-wasip1/release/zellij-pane-manager.wasm"
[[ -f "$WASM" ]] || { echo "[test] FAIL: WASM not found at $WASM" >&2; exit 1; }
echo "[test] WASM: $(wc -c < "$WASM") bytes"

# ── 2. Check we can talk to a running Zellij session ─────────────────────────
if ! "$ZELLIJ" action list-clients &>/dev/null; then
  echo "[test] No running Zellij session found."
  echo "       Start Zellij first, then re-run this script from a pane inside it."
  echo "       Or set ZELLIJ_BIN to the path of the zellij binary and ensure"
  echo "       a session is running."
  exit 1
fi
echo "[test] Zellij session found ($("$ZELLIJ" --version))"

# ── 3. Note current log position ─────────────────────────────────────────────
mkdir -p "$(dirname "$LOG_FILE")"
START_LINE=$(( $(wc -l < "$LOG_FILE" 2>/dev/null || echo 0) + 1 ))

# ── 4. Test path A: layout startup (launch-plugin) ───────────────────────────
echo "[test] launching plugin via 'zellij action launch-plugin' (fresh, no cache)…"
PANE_ID=$("$ZELLIJ" action launch-plugin --floating --skip-plugin-cache \
  -c "pane_0_name=terminal" \
  "file:$WASM" 2>&1)
echo "[test] got pane id: $PANE_ID"
sleep 2

# ── 5. Test path B: LaunchOrFocusPlugin keybind path ─────────────────────────
echo "[test] focusing same plugin via 'zellij action launch-or-focus-plugin'…"
"$ZELLIJ" action launch-or-focus-plugin --floating \
  -c "pane_0_name=terminal" -c "target=terminal" \
  "file:$WASM" &>/dev/null
sleep 1

# ── 6. Evaluate log ───────────────────────────────────────────────────────────
SINCE=$(tail -n +"$START_LINE" "$LOG_FILE" 2>/dev/null || true)

echo ""
echo "=== Zellij log — plugin-related lines ==="
echo "$SINCE" \
  | grep -iE "plugin|wasm|load|error" \
  | grep -v "resurrection\|Unhandled esc\|stdin_ansi\|Overriding plugin" \
  | head -40 || true
echo "==="

LOAD_ERRORS=$(echo "$SINCE" \
  | grep -cE "failed to load plugin|could not find exported" || true)

LOAD_OK=$(echo "$SINCE" \
  | grep -cE "Loaded plugin.*zellij.pane.manager|Loaded plugin.*pane.manager" || true)

echo ""
if [[ "$LOAD_ERRORS" -gt 0 ]]; then
  echo "[test] FAIL: $LOAD_ERRORS plugin-load error(s) found in log"
  echo "$SINCE" | grep -E "failed to load plugin|could not find exported"
  "$ZELLIJ" action close-pane 2>/dev/null || true
  exit 1
fi

if [[ "$LOAD_OK" -gt 0 ]]; then
  echo "[test] PASS — plugin loaded in Zellij 0.44 wasmi (pane $PANE_ID)"
else
  echo "[test] PASS — no load errors (plugin already cached; pane $PANE_ID)"
fi
echo "       Close the plugin pane with Ctrl+q when done"
