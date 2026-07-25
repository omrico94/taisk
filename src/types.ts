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
  task: string;
  started_at_ms: number;
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
