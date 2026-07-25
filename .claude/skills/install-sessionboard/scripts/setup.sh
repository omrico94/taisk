#!/usr/bin/env bash
# Idempotent installer for SessionBoard's local dependencies. Every step
# checks before acting, so re-running this after a partial/failed run only
# does the remaining work, not everything from scratch.
set -euo pipefail

# A non-interactive shell (or a fresh terminal that hasn't sourced its rc
# file yet) doesn't necessarily have rustup's cargo on PATH even when it's
# genuinely installed — the same reason SessionBoard's own desktop launcher
# sources this file before running `tauri dev`. Doing it here too means the
# Rust check below reflects reality instead of just this shell's current
# PATH, and can't wrongly conclude Rust is missing and install a second,
# conflicting copy via Homebrew.
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

step() { printf '\n\033[1;36m==> %s\033[0m\n' "$1"; }
ok()   { printf '    \033[1;32m[ok]\033[0m %s\n' "$1"; }
skip() { printf '    \033[2m[skip]\033[0m %s\n' "$1"; }
die()  { printf '    \033[1;31m[fail]\033[0m %s\n' "$1" >&2; exit 1; }

PROJECT_ROOT="$(git -C "$(dirname "${BASH_SOURCE[0]}")" rev-parse --show-toplevel 2>/dev/null || true)"
if [ -z "$PROJECT_ROOT" ]; then
  die "couldn't find the SessionBoard git repo root — run this from inside the sessionboard checkout."
fi
cd "$PROJECT_ROOT"

step "Homebrew"
if ! command -v brew >/dev/null 2>&1; then
  die "Homebrew not found. Install it from https://brew.sh, then re-run this script."
fi
ok "$(brew --version | head -1)"

step "Rust toolchain"
if command -v cargo >/dev/null 2>&1; then
  skip "already installed ($(cargo --version))"
else
  brew install rust
  ok "installed"
fi

step "Node.js"
if command -v node >/dev/null 2>&1; then
  skip "already installed ($(node --version))"
else
  brew install node
  ok "installed"
fi

step "Ollama"
if command -v ollama >/dev/null 2>&1; then
  skip "already installed ($(ollama --version))"
else
  brew install ollama
  ok "installed"
fi

step "Ollama background service"
if curl -fsS http://127.0.0.1:11434 >/dev/null 2>&1; then
  skip "already running"
else
  brew services start ollama >/dev/null 2>&1 || nohup ollama serve >/tmp/sessionboard-ollama.log 2>&1 &
  for _ in $(seq 1 15); do
    curl -fsS http://127.0.0.1:11434 >/dev/null 2>&1 && break
    sleep 1
  done
  curl -fsS http://127.0.0.1:11434 >/dev/null 2>&1 || die "Ollama didn't come up — check /tmp/sessionboard-ollama.log"
  ok "started"
fi

step "Ollama models"
installed_models="$(ollama list | awk 'NR>1{print $1}')"
for model in nomic-embed-text qwen2.5:1.5b; do
  # `ollama list` always shows an explicit tag (defaulting to `:latest` when
  # one wasn't given at pull time), so an untagged name here must be
  # normalized the same way before comparing — otherwise "nomic-embed-text"
  # never matches the installed "nomic-embed-text:latest" and gets re-pulled
  # every single run.
  target="$model"
  [[ "$target" == *:* ]] || target="$target:latest"
  if grep -qx "$target" <<<"$installed_models"; then
    skip "$model already pulled"
  else
    ollama pull "$model"
    ok "$model pulled"
  fi
done

step "npm dependencies"
npm install
ok "installed"

step "Rust workspace (compile check)"
(cd src-tauri && cargo check --workspace --quiet)
ok "workspace compiles"

step "All set"
echo "    Run 'npm run tauri dev' from $PROJECT_ROOT to start SessionBoard,"
echo "    or double-click Desktop/SessionBoard.command if that shortcut exists."
