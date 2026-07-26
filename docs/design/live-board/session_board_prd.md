# Product Requirements Document
## SessionBoard (working title) — A Local Board for Parallel AI Coding Sessions

**Status:** Draft v0.1
**Author:** [Your name]
**Date:** July 22, 2026

---

## 1. Overview

Developers running multiple AI coding agents in parallel (Claude Code, Aider, Cursor, etc.) lose track of what's running, which sessions need their attention, and how to find or return to related past work. SessionBoard is a local-first desktop app (plus an IDE extension) that shows every active session as a card on a single board — automatically categorized, visually signaling its state, and searchable by natural language over everything you've ever asked an agent to do. Everything runs on-device: categorization and summarization use a local LLM via Ollama, and session history is stored in a local vector database.

## 2. Goals

- At-a-glance visibility into every active session, across tools
- Immediate visual signal of state: **Working / Waiting for Input / Idle / Done**
- Automatic, meaningful grouping of sessions into categories — no manual tagging
- Natural-language / keyword search across current and historical sessions (local RAG)
- Zero cloud dependency — no session data, prompts, or code leave the machine
- Installation simple enough that it doesn't become its own support burden

## 3. Positioning (the wedge)

Your vision already implies a differentiator worth stating explicitly, since the space has real competition (native Claude Code session views, and several open-source multi-agent dashboards already exist):

- **100% local** — no cloud sync, no account, nothing leaves the machine. Most competitors either are cloud-synced or single-tool.
- **Tool-agnostic by design** — architected for any CLI-based agent, not tied to one vendor.
- **Semantic memory, not just a live view** — the RAG search over past sessions ("have I done something like this before?") is not something the tools found in competitive research offer. This is likely your strongest differentiator — lead with it.

## 4. Non-Goals (v1)

- Not a replacement for the agent's own approval/execution loop — SessionBoard observes and organizes; it does not have to drive the agent (see open question in §12 on whether replying from a card is in scope)
- Not a multi-device or team-sync tool (conflicts with the local-first positioning — could be a deliberate v2+ decision, not a v1 default)
- Not a token usage / cost / quota tracker (adjacent, but a different product)
- Not a git worktree manager

## 5. Core Concepts

| Term | Definition |
|---|---|
| **Session** | A single running instance of an agent (Claude Code, Aider, Cursor, …), tied to a project/working directory |
| **State** | Working (actively executing) / Waiting (blocked on user input or approval) / Idle (no activity beyond the configured TTL) / Done (exited/completed) |
| **Category** | An auto-assigned group label derived from semantic similarity of the session's initiating prompt; holds one or more sessions |
| **Current Task Summary** | A short, local-LLM-generated line describing what the session is doing right now |
| **Session Memory Store** | The local vector DB holding embeddings of prompts/summaries for categorization and search |

## 6. User Flows

### 6.1 Session ingestion & categorization
1. User starts an agent session (e.g. runs `claude` in a project, opens a Cursor session).
2. The collector detects the new session and captures the initiating prompt.
3. The prompt is sent to Ollama, requiring **two separate model calls**: an embedding model (e.g. `nomic-embed-text`) produces the vector, and a small instruct model (e.g. `qwen2.5:1.5b`/`llama3.2:3b`) produces the proposed category/task label. These are typically different models loaded in Ollama, not one call.
4. The embedding is compared against existing category exemplars in the vector DB:
   - Similarity above threshold → session joins that category.
   - Otherwise → a new category is created, seeded by this session.
5. The prompt and its embedding are persisted regardless of the categorization outcome, so it's searchable later.
6. A card appears on the board, under its category, in the **Working** state.

### 6.2 Ongoing state & task updates
1. The collector keeps observing the session's activity/output — for Claude Code, this should be a **hybrid**: Claude Code's hook system (`Notification`, `Stop`, `PreToolUse`/`PostToolUse`, `SessionEnd`) as the authoritative source for state transitions, plus tailing the session's JSONL transcript for content (prompts, summaries). Hooks fire deterministically on exactly the events that define our state machine (e.g. `Notification` fires specifically when Claude is blocked waiting on the user); inferring "Waiting" purely by pattern-matching or timing gaps in the transcript is a weaker fallback and should not be the primary mechanism where hooks are available.
2. On a state-changing signal (approval requested, tool call finished, session exits), the card's state updates.
3. Periodically, the local model re-summarizes recent activity into an updated "current task" line.
4. If no state-changing signal arrives within the configured TTL, the card moves to **Idle**.

### 6.3 Search
1. User types a keyword or phrase into the board's search bar.
2. The query is embedded locally and matched against the vector DB.
3. Matching sessions — active or historical — are surfaced ranked by relevance; an active match can be jumped to, a historical one can be reviewed.

## 7. Functional Requirements

| ID | Requirement | Priority |
|---|---|---|
| FR1 | Detect and register a new session per supported tool: initiating prompt, working directory, timestamp | P0 |
| FR2 | Track and update session state near-real-time: Working / Waiting / Idle / Done | P0 |
| FR3 | Configurable idle TTL (global default + per-session override) | P0 |
| FR4 | Local categorization via Ollama: assign to existing category or create a new one, similarity threshold configurable | P0 |
| FR5 | Persist every session's prompt (and ideally periodic task summaries) as embeddings in the local vector DB | P0 |
| FR6 | Natural-language/keyword search across active + historical sessions | P0 |
| FR7 | Board UI: cards grouped by category, each showing tool, project, state, current task, elapsed time | P0 |
| FR8 | Animated transitions on card creation, category change, or state change | P1 |
| FR9 | Manual override: re-categorize or merge categories | P1 |
| FR10 | Click into a card for fuller detail / transcript tail | P1 |
| FR11 | Reply to a "Waiting" session directly from a card | P2 — open decision, see §12 |
| FR12 | Minimal-step installation: bundles/auto-detects Ollama and the local vector DB, no cloud account | P0 |
| FR13 | IDE extension (VS Code first) showing a lightweight board panel, backed by the same local service as the desktop app | P1 |

## 8. Non-Functional Requirements

- **Latency:** category assignment shouldn't noticeably delay session start — target ~1–2s using a small/fast local model. This is achievable for a warm/preloaded model on decent hardware, but is optimistic if Ollama has to cold-load the model on first request, or on CPU-only 8GB machines; recommend the Core Engine keep the model warm (periodic keep-alive ping) and, regardless, decouple card creation from categorization completion (see §10) so this latency target is a UX nicety, not a blocking dependency.
- **Privacy:** no telemetry, no network calls by default; all inference and storage stay local.
- **Resource footprint:** must run comfortably alongside real coding work. Default to a small quantized Ollama model (1–3B class — e.g. `qwen2.5:1.5b`/`llama3.2:3b` for categorization, `nomic-embed-text` for embeddings), with the option to swap in a larger one.
- **Extensibility:** adding a new agent tool should mean writing a new collector adapter, without touching the core engine, UI, or vector DB.
- **Reliability:** a restarted background service must not lose already-persisted session/category data. Collectors must also tolerate the underlying tool rewriting/rotating its log or transcript file without losing or duplicating events (see append-safe tailing note in §10).
- **Installability:** one command/installer; first run shouldn't require the user to hand-configure the model or DB schema. Note the vector DB (embedded library) and Ollama (separate system process) have meaningfully different installability profiles — see §10.
- **Cross-platform paths:** session transcript locations differ by OS (e.g. `~/.claude/projects/` on macOS/Linux vs. the Windows equivalent under the user profile); collector file-discovery logic needs to account for this rather than hard-coding a Unix-style path.

## 9. UX / Visual Requirements (high-level — full spec belongs in the design phase)

- Dark mode as the default (likely only) theme.
- Cards signal state at a glance via color/icon/motion — e.g. a subtle pulsing indicator for Working, a static highlighted border for Waiting, a muted/desaturated look for Idle.
- Categories read as clearly delineated zones on the board (swimlanes or clustered groups).
- Motion should be purposeful, not decorative: a new card animates into its category group; a card visibly moves if re-categorized or goes idle.
- Search should feel instant — live filtering/highlighting as the user types.

## 10. High-Level Architecture (for the design phase to formalize)

Four local components:

1. **Collectors** — one per supported tool, translating tool-specific signals into a common event schema (session started / output chunk / waiting-for-input / exited). For Claude Code: hooks for authoritative state transitions (see §6.2) *plus* tailing the transcript JSONL that Claude Code already writes locally per session (confirmed on a real machine during PRD review: files exist at `~/.claude/projects/<project>/<session-uuid>.jsonl`, one file per session, append-only, structured JSON per line). This file-tailing approach is the same mechanism used by the open-source project [agent-office](https://github.com/belle05/agent-office), which reads these same Claude Code transcripts (and, per its README, Cursor transcripts at `~/.cursor/projects/*/agent-transcripts/`) to build a similar local session-visibility tool — a useful existence proof that local, hook-free observation of Claude Code sessions is viable, and a starting point (not a confirmed fact — see Cursor risk below) for the collector implementation. Tools without hooks will need PTY-wrapping or log-tailing instead.
   - **Engineering note:** tailing a file that the agent CLI is actively appending to needs append-safe reads (track byte offset / inode, handle rotation), not a naive re-read-whole-file-on-change loop.
2. **Core Engine** (background service) — owns the session state machine and TTL logic, talks to Ollama for categorization/summarization, talks to the vector DB for storage and search. To keep session-start latency off the model's critical path, the card should render immediately (e.g. "Uncategorized" state) and be updated in place once the async categorization call returns, rather than blocking card creation on a ~1–2s model round-trip (which is optimistic for a cold-started model — see NFR notes below).
3. **Local Vector DB** — stores prompt/task embeddings and metadata (category, project, timestamps) for both live categorization and historical search. Recommend an **embedded (in-process) vector DB** rather than one requiring a separate running server, since that removes an entire class of install/auto-detect problems: **LanceDB** (Apache-2.0, embedded, serverless, Python/Node bindings, built for exactly this "local app with a vector index" use case) is a strong default; **sqlite-vec** (single-file, minimal footprint, plenty for the expected scale — hundreds to low thousands of sessions) or **Chroma** in persistent local mode are viable alternatives. All are free/open-source and require no account or cloud dependency, satisfying Goal #5.
4. **Front ends** — the desktop board and the IDE extension are both thin clients against the same Core Engine over a local API (e.g. localhost HTTP/WebSocket) — one source of truth, no duplicated detection logic.

**Flagged risk:** Cursor is a full IDE rather than a CLI agent with an obvious hook/log surface — whether (and how) its sessions can be observed locally needs a technical spike before it's committed to the tool list. The agent-office README claims Cursor also writes local per-project transcript files under `~/.cursor/projects/*/agent-transcripts/`; **this is an unverified lead, not a confirmed fact** — it couldn't be checked during this review (Cursor isn't installed on the reviewing machine, and Cursor's storage format has changed across versions historically). Treat it as the first thing the Phase 3 spike checks, before assuming PTY-wrapping is necessary.

**Free/local infra stack (concrete recommendation, addressing "zero cloud dependency" and "free infra" requirements):**
- Local LLM runtime: **Ollama** (free, MIT-adjacent license, runs as a local HTTP server on `127.0.0.1:11434`).
- Categorization/summarization model: a small instruct model in the 1–3B class, e.g. `qwen2.5:1.5b` or `llama3.2:3b` — both free, open-weight, small enough to run acceptably on CPU-only 8GB laptops.
- Embedding model: `nomic-embed-text` or `all-minilm` via Ollama's `/api/embeddings` endpoint — both free, open-weight, and purpose-built for this (note: this is a *different* model from the categorization model, see §6.1 correction).
- Vector DB: **LanceDB** (recommended) / sqlite-vec / Chroma — all free, open-source, embeddable, no server process, no account.
- **Installability caveat:** unlike the vector DB (which can ship as an embedded library inside the app binary with zero extra install step), Ollama is a separate system-level process with its own installer per OS. "Bundles/auto-detects Ollama" (FR12) is achievable as *detect-if-running, else guide the user through the official Ollama installer*, but fully silent bundling of a third-party runtime inside your own installer is a nontrivial additional engineering effort — worth scoping explicitly rather than assuming it's a checkbox.

## 11. MVP Scope / Phasing (recommended)

- **Phase 1:** Desktop app only, single tool — Claude Code (best-documented hook surface). Full categorization, search, and board UX built and validated against one real integration first.
- **Phase 2:** Add a second tool via PTY-wrapping/log-tailing — Aider is a good candidate (plain CLI, verbose logs) — to prove the collector abstraction generalizes.
- **Phase 3:** VS Code extension on the same backend; spike Cursor feasibility.
- **Phase 4:** Additional tools, reply-from-card, category merge/rename polish.

This is a recommendation to de-risk the build — narrow it further or widen it based on your own timeline constraints.

## 12. Open Questions & Risks

- Can Cursor sessions be observed locally at all, given it isn't a CLI-hook-based tool? (needs a spike — start by checking whether Cursor still writes local per-project transcripts, as an external OSS project claims; unverified, see §10)
- Does the exact JSONL schema Claude Code writes (message roles, tool-call/tool-result markers, timestamps) reliably distinguish "assistant still working" from "waiting on user" without hooks, or is transcript content alone ambiguous here? (needs verification against the current Claude Code CLI version during the Phase 1 spike — schemas can and do change across CLI versions)
- What similarity threshold should trigger "new category" vs. "join existing" — likely needs tuning against real prompts, possibly a user-adjustable setting
- Should users be able to reply to a Waiting session directly from a card? (adds scope, but matches the expectation set by existing tools)
- What's the right default idle TTL? (needs a sensible default, e.g. 10–15 min, validated with real usage)
- How does the local model choice scale across hardware (8GB laptop vs. GPU workstation) — may need a "lite" vs. "full" model tier

## 13. Success Metrics

- Time-to-notice: how fast a user reacts to a session going Waiting, vs. their current manual-checking baseline
- % of sessions correctly auto-categorized without manual correction
- Search usage rate and perceived relevance
- Install-to-first-board-view time
- Daily/weekly active use after install

## 14. Out of Scope for v1

- Multi-device or team sync
- Usage/cost/quota tracking
- Git worktree management
- Accounts, licensing, or paywall infrastructure

## 15. Next Steps

1. Take this PRD into the design phase: wireframe the board layout, card anatomy, category grouping, and the state/motion language.
2. Before design locks in, run a short technical spike validating Claude Code detection end-to-end — combining hooks (state transitions) and transcript-tailing (content) — the state machine and "current task" feature both depend on this working reliably.
3. Hand off to implementation: Phase 1 collector (Claude Code) + Core Engine + desktop board UI.

## 16. Technical Validation Addendum (Engineering Review Pass)

This section documents what was checked during technical review, prior to sending this PRD to design. Design-relevant content (§9) was intentionally left untouched by this pass.

**Verified locally (2026-07-22):**
- Claude Code does write local, per-session JSON-lines transcript files under `~/.claude/projects/<project>/<session-uuid>.jsonl` — confirmed present on a real machine, one file per session, structured JSON per line. This supports the file-tailing collector approach as technically viable, independent of hooks.
- Cursor's equivalent local storage could **not** be verified (Cursor not installed on the reviewing machine); the claim that Cursor writes similar transcripts is sourced from a third-party OSS project's README and should be treated as an unverified lead for the Phase 3 spike, not a given.

**Gaps closed in this pass (previously implicit or unspecified):**
- Named concrete free/local models for the two distinct Ollama calls categorization needs (embedding model vs. instruct model) — the original draft implied one call could do both.
- Named a concrete embedded vector DB option (LanceDB, or sqlite-vec/Chroma as alternatives) — the original draft referred to "the local vector DB" without naming a candidate, which left FR5/FR12/installability underspecified.
- Called out that Claude Code hooks (not transcript-tailing alone) should be the authoritative source for state transitions (Working/Waiting/Idle/Done), since that's the core promise of the product and heuristic inference from log content/timing is a weaker fallback.
- Flagged that "bundle/auto-detect Ollama" (FR12) and embedding-the-vector-DB have very different installability profiles (external system process vs. in-process library) — worth budgeting separately.
- Added a recommendation to decouple card creation from categorization completion, so the ~1–2s latency NFR is a UX target rather than a hard blocking dependency on cold-start model latency.
- Added cross-platform path handling and append-safe/rotation-safe file tailing as explicit reliability requirements.

**Not changed:** positioning (§3), non-goals (§4), UX/visual requirements (§9), success metrics (§13), and overall phasing (§11) — these are product/design decisions, not technical feasibility questions, and were out of scope for this pass.

**Bottom line:** the core technical premise — local-only categorization via Ollama, local vector search, and Claude-Code-first detection — holds up. The two things that most need a spike before implementation locks in are (1) confirming the JSONL-vs-hooks split for state detection against the real, current Claude Code CLI schema, and (2) validating whether Cursor is observable at all before committing to it in the tool roadmap.
