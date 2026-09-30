---
name: install-sessionboard
description: Installs and verifies everything SessionBoard needs to build and run locally — Rust, Node.js, npm dependencies, and optionally Ollama with its two models (nomic-embed-text, qwen2.5:1.5b — without them the app runs in Claude-native mode) — then confirms the Rust workspace compiles. Use this whenever the user wants to set up, install, bootstrap, or get SessionBoard running for the first time (or on a new machine), asks "what do I need to install for this project", hits a missing-dependency error (cargo/npm/ollama not found, model not pulled), or wants to verify their SessionBoard dev environment is complete. Safe to re-run any time — every step checks what's already installed before doing anything.
---

# Install SessionBoard

SessionBoard needs Rust (the core engine + Tauri shell) and Node.js (the
frontend build) to build or run. Ollama with two specific models is
**optional**: it powers LLM-written titles/summaries and semantic search, and
without it the app runs in Claude-native mode (Claude Code's own session
titles from the transcript, keyword search). If the user doesn't want Ollama,
skip it — the app is not broken without it.

## What to do

Run the bundled script — it does the real work, in order, and is safe to
re-run:

```bash
bash .claude/skills/install-sessionboard/scripts/setup.sh
```

It checks each dependency before installing it (via Homebrew), so running it
on a machine that already has some pieces in place only fills the gaps. It
finishes by running `npm install` and `cargo check --workspace`, so a clean
run is real evidence the project is ready to build — not just that the
individual tools exist.

Show the user the script's output as it runs rather than swallowing it —
each step prints `[ok]`/`[skip]`/`[fail]` so they can see exactly what
happened. If Homebrew itself isn't installed, the script stops immediately
with a link to https://brew.sh — that's the one prerequisite it can't
install for the user, since Homebrew's own installer needs an interactive
sudo prompt.

## After a successful run

Tell the user they can start the app with:

```bash
npm run tauri dev
```

(or the `Desktop/SessionBoard.command` shortcut, if one exists on their
machine). Don't launch it yourself as a background process unless the user
specifically asks to see it running — installing and running are different
asks, and `tauri dev` is a long-lived process the user will want to actually
watch/interact with.

## If something fails partway through

The script exits on the first real failure (`set -euo pipefail`) rather than
plowing ahead with a broken environment. Common cases:

- **`ollama pull` fails or hangs** — almost always a network issue; the
  models are a few hundred MB each. Suggest checking connectivity and
  re-running the script (already-pulled models are skipped, so nothing is
  wasted).
- **`cargo check` fails at the end** — dependencies are fine, but something
  in the Rust workspace itself doesn't compile. That's a real code problem,
  not a missing-dependency problem — don't re-run the installer for this,
  investigate the compiler error directly.
- **Ollama service won't come up** — the script waits up to 15 seconds and
  then reports the log at `/tmp/sessionboard-ollama.log`; check that file
  for what actually went wrong before assuming the script itself is broken.
