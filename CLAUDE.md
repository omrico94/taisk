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

Four states: `Working` → `Done` → `Idle`, with `Waiting` as a side-branch. Every transition goes through `state::transition()` (`state.rs`) — never set `SessionView.state` directly anywhere else. `IdleTimeout` is the one event whose result depends on `current`, not just the event itself: `Working` + `IdleTimeout` → `Done`, but `Done` + `IdleTimeout` → `Idle`. Every other event maps to a fixed target regardless of `current`, *including* reviving a `Done` session (a resumed session reusing its old session id) — otherwise a session marked ended and later resumed stays stuck on the board forever, since reconstruction re-forces `Done` on every restart before a new hook gets a chance to correct it. Non-obvious wrinkles worth knowing before touching hook handling:

- **A session reads as `Done` the moment it stops working, not just when explicitly closed** (user-driven design choice, not the original one). `orchestrator.rs`'s `"stop"` handler fires `SessionEvent::TurnEnd` (→ `Done`) instead of `ToolActivity` — the assistant finishing a turn cleanly is treated the same as the session being "done" for now, distinct from a real `SessionEnd` hook (closing the conversation), which still also lands on `Done` but is durably recorded in `EndedSessions` so it never ages further.
- **`Done` ages into `Idle` after `done_ttl` (default 10 min) of continued silence — unless it was an explicit close.** `orchestrator::run_done_sweeper` (`done_sweep_once`) ticks every `idle_sweep_interval` and dispatches `IdleTimeout` to any `Done` session quiet that long, *except* ids in `EndedSessions` — those stay `Done` forever, since that's a real close, not just silence. This is a separate sweep from `engine::run_idle_sweeper` specifically because it needs `EndedSessions`, which is orchestrator-layer state `engine.rs` doesn't know about.
- **`Working` → `Done` also has a TTL-driven safety net.** `engine::run_idle_sweeper` ticks every `idle_sweep_interval` (default 60s) and moves any `Working` session with no activity for `idle_ttl` (default 10 min) to `Done` — this is what catches a killed process/closed terminal that never fires a clean `TurnEnd`/`SessionEnd`. `Waiting` sessions are deliberately exempt (blocked-on-you isn't abandoned). `idle_ttl`/`done_ttl`/`idle_sweep_interval` can each be shortened via `SESSIONBOARD_IDLE_TTL_SECS`/`SESSIONBOARD_DONE_TTL_SECS`/`SESSIONBOARD_SWEEP_INTERVAL_SECS` env vars for faster manual e2e iteration — unset, defaults are unchanged.
- **`AskUserQuestion` is special-cased.** Claude Code does *not* fire a `Notification` hook for it even though it blocks the turn on a real user choice (`Notification` is reserved for `permission_prompt`/`idle_prompt`/etc., confirmed against the hooks docs). `orchestrator.rs`'s `pre-tool-use` handler checks `tool_name` and fires `SessionEvent::Notification` (→ Waiting) for that one tool specifically. If a future Claude Code tool has similar blocking-on-user-input behavior without its own Notification, it likely needs the same treatment.
- **Ordinary permission dialogs (Bash, Edit, Write, …) drive Waiting off `PermissionRequest`, not `Notification`'s `permission_prompt`.** Real, reproduced bug (user report, with a screenshot of a live "Allow Claude to run?" dialog): the card sat on `Working` for the whole approval. Root cause, confirmed against code.claude.com/docs/en/hooks.md: `Notification`'s `permission_prompt` type only fires once the user has gone **~6 seconds without typing** — most dialogs get answered well inside that window, so the signal never fires at all for the common case. `PermissionRequest` fires the instant Claude Code is about to show the dialog, no idle gating, and is now registered as its own hook event (`settings_merge.rs`'s `HOOK_EVENTS`, `hook-bridge permission-request`) — `orchestrator.rs` treats it exactly like a driving `Notification` (marks `Waiting`, durably recorded in `WaitingSessions` same as always) without ever writing a decision to stdout, so the real permission flow (including genuine deny rules) is untouched; it's a pure observer. `Notification`'s `permission_prompt` handling is left in place too (harmless — by the time it might fire 6s in, the session is usually already `Waiting`), it's just no longer the only signal. Existing users need SessionBoard to re-run its idempotent hook merge (app restart) before `PermissionRequest` actually gets registered in `~/.claude/settings.json`.
- **Not every `Notification` means "blocked on you."** Its payload carries a `notification_type` field (confirmed against code.claude.com/docs/en/hooks.md) — confirmed values include `permission_prompt`, `idle_prompt`, `auth_success`, `elicitation_dialog`/`elicitation_complete`/`elicitation_response`, `agent_needs_input`, `agent_completed`. Only `idle_prompt` is currently excluded from driving Waiting (`orchestrator.rs`'s `is_idle_prompt` check) — it means "sitting idle waiting for the next message," which is exactly what the Done→Idle timeout sweep already represents, not "blocked on a decision." This was a real, reproduced bug, and it shipped *twice*: the original fix read the field as `payload.get("type")`, which doesn't exist on the real wire format (the actual key is `notification_type`) — so `is_idle_prompt` was silently always `false` against real Claude Code, and every `idle_prompt` kept yanking a settled `Done` session back to `Waiting` exactly as before the "fix." Caught by replaying a real-shaped payload (`{"notification_type": "idle_prompt"}`) against a live running instance, not by the existing unit tests, which had made the identical wrong-key mistake in their own fake payloads and so passed either way. Every other type still marks Waiting (permissive default, preserves the original `permission_prompt` fix). **DevSwarm sessions genuinely cannot be detected as Waiting during a pending tool approval — confirmed, not a bug in this codebase.** Live-tested against a real DevSwarm-orchestrated session sitting on an actual "Do you want to proceed?" approval prompt: no hook of any kind reached the engine during that window (board showed `Working` the whole time), and the transcript itself carries no distinguishing marker either — the last line while pending is just a normal `tool_use` block indistinguishable from any brief, fast-completing tool call, so a polling-based workaround isn't viable (it would misfire on every ordinary tool use). DevSwarm's `AskUserQuestion` handling is unaffected by this — that path goes through `PreToolUse`, not `Notification`, and works identically to CLI/Desktop. If DevSwarm ever exposes its own hook or log for this, that's the only clean way to close this gap; don't build a heuristic without one.

### Reconstruction on restart is a heuristic, not a replay

Live session state is intentionally never persisted wholesale — `orchestrator::reconstruct_live_sessions()` rebuilds the board on startup from two durable sources instead:

1. `memories` LanceDB rows (category, project, cwd, original prompt) — recency of the session's transcript file (`reconstruction_recency`, default 24h) gates whether a session gets resurrected at all.
2. `EndedSessions` (`~/Library/Application Support/SessionBoard/ended-sessions.json`, same atomic-write flat-file pattern as `TailCheckpoints`) — the *only* durable record of which sessions actually reached `Done`. Without checking this, every restart would reconstruct every recent session as `Working`, silently erasing real `Done`/`Waiting` history (this was a real, shipped bug — see `reconstructs_an_ended_session_as_done_not_working` in `orchestrator.rs` for the regression test). An id is evicted from this file the moment any later hook fires for it (proof it's alive again, e.g. a resumed conversation) — otherwise the *next* restart would immediately force it back to `Done` even though the live engine had already correctly revived it.
3. `WaitingSessions` (`waiting-sessions.json`, same pattern) — the only durable record of which sessions are genuinely blocked on the user (a `Notification` hook or the `AskUserQuestion` special case, see below). Reconstruction can fall back to a `Working` guess for anything else and let the next hook or the idle sweep correct it, but nothing re-fires `Notification` on its own just because the app restarted — without this file, a session that was legitimately `Waiting` when the app last restarted loses that status permanently (reconstructed as `Working`, then promptly swept to `Done` once `last_activity_ms` — also restored from the transcript's real mtime, not "now" — shows how stale it really is). Kept in sync from two places: `orchestrator::handle_hook_event` (any hook resolves or (re)marks the wait) and `api`'s approve/reject/reply handlers (`AppState::waiting_sessions`, shared with the orchestrator) — resolving a wait from the board has to update the same durable record a real hook would, or a restart shortly after would restore it as `Waiting` again.

For any session in neither file, reconstruction computes `Working`/`Done`/`Idle` directly from elapsed time against `idle_ttl`/`done_ttl` (using the transcript's real mtime, not "now") rather than always guessing `Working` and waiting a full sweep interval to correct — still a best-effort guess for the truly ambiguous case (a process killed mid-turn looks identical to one still running until enough time passes), corrected by the next real hook event same as always.

### `session-start` fires exactly once — losing that race means the session never appears, ever

There's no retry and no fallback: if the `session-start` handler in `orchestrator.rs` doesn't manage to find the initiating prompt and dispatch `SessionStart` before it gives up, that session id is never surfaced by any later hook (`engine.rs` treats any non-`SessionStart` event for an unknown id as a no-op — deliberately, so a stray late hook can't fabricate a blank ghost card). Two real, shipped bugs came from this:

- **The prompt-read must not share `tail_new_lines`'s checkpoint.** That checkpoint is a single cursor keyed by file path, also used by `stop`'s incremental "what's new" tailing of the same transcript. `session-start` is looking for something at a *fixed* position (the first prompt), not "what's new" — if `stop` (or a duplicate `session-start` delivery) advances that shared cursor past the prompt before `session-start`'s own read happens, the read comes back empty and the session is dropped forever. Fixed by having `session-start` read via `collector::read_lines_from_start` (a plain, non-checkpointed full-file read) instead.
- **`transcript_wait` (how long the poll waits for that prompt to appear) needs to reflect real human typing speed, not best-guess "a few seconds."** Measured directly against two real interactive terminal sessions: 22s and 27s elapsed between the process starting and the user's first message actually landing in the transcript (reading the banner, thinking, typing) — both well past the previous 8s default, so completely normal sessions were silently abandoned before ever getting a chance. Now 5 minutes — cheap to wait that long (one spawned task doing a stat+read every 150ms), and a genuinely abandoned session still eventually gets skipped, just later.

If a session still doesn't show up, check `~/.claude/projects/<sanitized-cwd>/<session_id>.jsonl` exists and has a real `"type":"user"` line — if it does, the fastest way to confirm/fix it live is replaying the real `session-start` hook by hand against the running engine's socket (`~/Library/Application Support/SessionBoard/engine.sock`), with `session_id`/`cwd`/`transcript_path` read straight off that transcript's own lines.

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
