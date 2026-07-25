# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

SessionBoard is a local-first Tauri (Rust) + React/TypeScript desktop app. It watches every running Claude Code session on the machine (CLI and Desktop), turns each into a live card grouped by category, drives that card's state from Claude Code's own hooks, and backs semantic search over session history with an embedded LanceDB vector store. All inference (categorization, embeddings) runs through a local Ollama instance — no cloud calls, no accounts, no API keys anywhere in this codebase.

There's a project-scoped setup skill at `.claude/skills/install-sessionboard/` — use it (or read it) before assuming a dependency is missing.

## Commands

### Frontend
```bash
npm install                 # install JS deps
npm run dev                 # Vite dev server only (frontend against nothing — mostly for CSS/component iteration)
npm run build                # tsc typecheck + vite build — this IS the lint/typecheck gate, there is no separate lint script
npm run tauri dev            # the real thing: builds+runs the Rust backend and opens the Tauri window
```

### Rust (run from `src-tauri/`)
```bash
cargo check --workspace                          # fast compile check, both crates
cargo test --workspace                           # full suite (~55-60 tests, sub-second)
cargo test -p core-engine engine::                # just the engine actor / state machine tests
cargo test -p core-engine orchestrator::           # just the hook-wiring / reconstruction tests
cargo test <exact_test_function_name>              # a single test, e.g. cargo test sweep_idle_moves_a_stale_working_session_to_idle
cargo test --workspace -- --ignored                # the one live-Ollama smoke test, skipped by default (needs Ollama actually running)
```

Rust changes to `src-tauri/` are picked up automatically while `npm run tauri dev` is running — its file watcher rebuilds and relaunches the backend process on its own. No manual restart needed after editing engine/orchestrator/API code.

**Gotcha:** `cargo`/`rustc` may not be on `PATH` in a fresh non-interactive shell even when Rust is genuinely installed via rustup — source `~/.cargo/env` first (`SessionBoard.command` and the install skill's script both do this explicitly; don't assume `command -v cargo` failing means Rust needs installing).

## Architecture

### The pipeline, end to end

```
Claude Code hook fires
  → hook-bridge (tiny bin, reads stdin, fire-and-forget)
  → Unix domain socket (~/Library/Application Support/SessionBoard/engine.sock)
  → orchestrator::handle_hook_event  (src-tauri/crates/core-engine/src/orchestrator.rs)
      - maps the hook's event name to a state::SessionEvent
      - session-start: polls for the transcript file, extracts the initiating
        prompt, then kicks off categorize_session (Ollama call, off the
        critical path — the card appears as "Uncategorized" immediately)
      - stop: refreshes the task-summary line from the latest transcript activity
  → engine::EngineHandle  (single-owner actor, owns the live session HashMap)
      - every mutation goes through state::transition(current, event) — the
        one place state changes happen, table-tested for every (state, event) pair
      - broadcasts a SessionDiff on every change
  → api::router (axum, localhost:37888)  — GET /sessions, WS /events, POST
    approve/reject/reply/recategorize, GET /search
  → React frontend (Zustand store + useSessionEngine() WS hook)
```

`core-engine` is a library crate consumed by both the real Tauri app (`src-tauri/src/lib.rs`) and a standalone dev-server example (`crates/core-engine/examples/dev_server.rs`) that shares the exact same `bootstrap::start()` — useful for driving the backend without the native window.

### The state machine is the source of truth — don't bypass it

Four states: `Working` → `Waiting` → `Idle` → `Done` (terminal). Every transition goes through `state::transition()` (`state.rs`) — never set `SessionView.state` directly anywhere else. Two non-obvious wrinkles worth knowing before touching hook handling:

- **`AskUserQuestion` is special-cased.** Claude Code does *not* fire a `Notification` hook for it even though it blocks the turn on a real user choice (`Notification` is reserved for `permission_prompt`/`idle_prompt`/etc., confirmed against the hooks docs). `orchestrator.rs`'s `pre-tool-use` handler checks `tool_name` and fires `SessionEvent::Notification` (→ Waiting) for that one tool specifically. If a future Claude Code tool has similar blocking-on-user-input behavior without its own Notification, it likely needs the same treatment.
- **Idle is TTL-driven, not hook-driven.** `engine::run_idle_sweeper` ticks every `idle_sweep_interval` (default 60s) and moves any `Working` session with no activity for `idle_ttl` (default 10 min) to `Idle` — this is what catches a killed process/closed terminal that never fires a clean `SessionEnd`. `Waiting` sessions are deliberately exempt (blocked-on-you isn't abandoned).

### Reconstruction on restart is a heuristic, not a replay

Live session state is intentionally never persisted wholesale — `orchestrator::reconstruct_live_sessions()` rebuilds the board on startup from two durable sources instead:

1. `memories` LanceDB rows (category, project, cwd, original prompt) — recency of the session's transcript file (`reconstruction_recency`, default 24h) gates whether a session gets resurrected at all.
2. `EndedSessions` (`~/Library/Application Support/SessionBoard/ended-sessions.json`, same atomic-write flat-file pattern as `TailCheckpoints`) — the *only* durable record of which sessions actually reached `Done`. Without checking this, every restart would reconstruct every recent session as `Working`, silently erasing real `Done`/`Waiting` history (this was a real, shipped bug — see `reconstructs_an_ended_session_as_done_not_working` in `orchestrator.rs` for the regression test). Any session not in this file gets restored as `Working` and corrected later by a real hook event or the idle sweep — it's a best-effort guess, not a guarantee.

### LanceDB: two tables, on purpose

`memory_repo.rs` wraps exactly two tables — `memories` (session text + embedding, doubles as both the categorization lookup and the search index) and `category_exemplars` (one row per known category, seeded from that category's first session). `MemoryRepo::open()` self-heals a stale on-disk schema (drops and recreates a table missing an expected column) rather than panicking — relevant if you add a column to `Memory` or `CategoryExemplar`.

### Categorization: embedding lookup first, LLM only when needed

`categorize.rs`'s `categorize_session()` embeds the prompt (`nomic-embed-text`), checks cosine similarity against existing `category_exemplars`. Above the confidence threshold → joins that category, no generation call. Below it → calls the small instruct model (`qwen2.5:1.5b`) for a label, and — critically — the confidence threshold is enforced in *our* code, not just trusted from the model's own `is_new` flag in its JSON response.

### Frontend: two independent search surfaces, don't merge their state again

`sessionStore.ts` deliberately keeps two separate query fields: `query` (the toolbar's instant client-side substring filter over already-loaded sessions — no network call) and `askQuery` (the ⌘K `AskMemoryOverlay`'s semantic search, debounced, hits `GET /search`). These were briefly merged into one shared field during a refactor and it broke both search surfaces simultaneously (typing in one silently overwrote the other's results) — keep them separate. `Board.tsx`'s "search your full history instead" fallback button uses `openAskWithQuery(query)` to hand the typed text across intentionally, which is the one place they're allowed to talk to each other.

### Ports, sockets, and data paths

- Frontend ↔ Core Engine: `http://127.0.0.1:37888` (HTTP + WS), constant `API_PORT` in `lib.rs`.
- hook-bridge → Core Engine: Unix domain socket, `~/Library/Application Support/SessionBoard/engine.sock`.
- Durable data: `~/Library/Application Support/SessionBoard/` (LanceDB dir, tail checkpoints, ended-sessions record).
- Claude Code hooks are registered idempotently into `~/.claude/settings.json` (`settings_merge.rs` — safe to re-run, identifies its own entries by the `hook-bridge` binary path in `command`).
- Session transcripts are read from `~/.claude/projects/<sanitized-cwd>/<session_id>.jsonl` — sanitization logic lives in `collector.rs`'s `transcript_path()`.

### `entrypoint` drives what "Jump to session" can do

`SessionView.entrypoint` is `"cli"` (plain terminal `claude`) or `"claude-desktop"` — captured from the real hook payload at session-start, defaulting to `"cli"` when absent (durable `memories` rows have no record of it, so reconstruction can't recover the original value either). Only `"cli"` sessions can be reattached to (`claude --resume <id>` via a new Terminal window, `src-tauri/src/lib.rs`'s `jump_to_cli_session` command) — there's no known deep-link for a specific Claude Desktop tab, so that case is an honest disabled button, not a fallback guess.
