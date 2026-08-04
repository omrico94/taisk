// Mirrors core-engine's serde JSON shapes exactly (src-tauri/crates/core-engine/src/engine.rs,
// api.rs) — snake_case field names and externally-tagged enums are what serde_json produces by
// default, so these types are not renamed/camelCased on this side.

export type SessionState = "Working" | "Waiting" | "Idle" | "Done";

export interface SessionView {
  id: string;
  tool: string;
  project: string;
  cwd: string;
  entrypoint: string;
  category: string;
  state: SessionState;
  /** Short, stable session name — set once at categorization time. */
  title: string;
  /** Live, changing "what's happening right now" line (was `task` in Phase 1). */
  desc: string;
  started_at_ms: number;
  /** Real cumulative usage figures. `ctx_max === 0` means no metrics yet
   * (too early in the session — nothing to render). */
  tokens: number;
  cost: number;
  ctx_used: number;
  ctx_max: number;
  /** Subagents spawned by this session — empty for the common case. */
  subs: SubagentView[];
  /** Real `~/.claude/tasks/` data — absent for sessions that never used TaskCreate. */
  plan: PlanView | null;
}

export interface SubagentView {
  id: string;
  title: string;
  desc: string;
  /** `"Working"` or `"Done"` — a file-mtime heuristic, not a hook-driven guarantee. */
  state: string;
  tokens: number;
  cost: number;
  ctx_used: number;
  ctx_max: number;
}

export interface PlanStep {
  id: string;
  subject: string;
  done: boolean;
}

export interface PlanView {
  title: string;
  steps: PlanStep[];
}

export type SessionDiff = { Upserted: SessionView } | { Removed: string };

export interface SearchResult {
  text: string;
  project: string;
  tool: string;
  category: string;
  session_id: string;
  distance: number;
  live: boolean;
  created_at: number;
}
