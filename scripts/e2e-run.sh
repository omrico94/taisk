#!/usr/bin/env bash
# Starts an isolated dev_server for E2E: HOME points at a scratch dir, so the
# LanceDB store, tasks.json, engine.sock, ~/.claude/projects and ~/.claude/tasks
# are all throwaway (and the user's real ~/.claude/settings.json is untouched).
# Short TTLs make idle/done aging observable. A short path keeps the unix
# socket under macOS's 104-char limit.
#   scripts/e2e-run.sh start [--fresh]   scripts/e2e-run.sh stop
set -euo pipefail
E2E_HOME=${E2E_HOME:-/tmp/sbe2e}
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PIDFILE="$E2E_HOME/dev_server.pid"
case "${1:-}" in
  start)
    [[ "${2:-}" == "--fresh" ]] && rm -rf "$E2E_HOME"
    mkdir -p "$E2E_HOME/Library/Application Support"
    source "$HOME/.cargo/env"
    ( cd "$ROOT/src-tauri" && cargo build -q -p core-engine --example dev_server )
    BIN="$ROOT/src-tauri/target/debug/examples/dev_server"
    HOME="$E2E_HOME" SESSIONBOARD_IDLE_TTL_SECS=${IDLE:-600} SESSIONBOARD_DONE_TTL_SECS=${DONE:-600} \
      SESSIONBOARD_SWEEP_INTERVAL_SECS=${SWEEP:-5} SESSIONBOARD_FAKE_OLLAMA=1 SESSIONBOARD_API_PORT=${PORT:-37999} nohup "$BIN" > "$E2E_HOME/server.log" 2>&1 &
    echo $! > "$PIDFILE"
    for _ in $(seq 1 60); do curl -sf http://127.0.0.1:${PORT:-37999}/tasks >/dev/null && { echo "up (pid $(cat "$PIDFILE"))"; exit 0; }; sleep 0.5; done
    echo "server did not come up"; cat "$E2E_HOME/server.log"; exit 1 ;;
  stop)
    [[ -f "$PIDFILE" ]] && kill "$(cat "$PIDFILE")" 2>/dev/null || true
    sleep 0.5 ;;
  *) echo "usage: $0 start [--fresh] | stop"; exit 2 ;;
esac
