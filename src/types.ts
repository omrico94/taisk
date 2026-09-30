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
  /** Board (Claude config directory / account) this session belongs to. */
  board: string;
  state: SessionState;
  /** Short, stable session name — set once at summary time. */
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

/** A column id on the task's board (see `BoardColumns`). */
export type Stage = string;

export interface Column {
  /** Stable across renames. */
  id: string;
  name: string;
  /** `#RRGGBB` — alpha bytes get appended to it for washes. */
  color: string;
}

/** One board's columns, in order, plus which column carries each role. */
export interface BoardColumns {
  columns: Column[];
  /** Cards here read as finished (dimmed). Display only — stages are manual. */
  done: string | null;
  /** Gets the strong accent (In Progress by default). Display only. */
  active: string | null;
  /** Where quick-add files new tasks; null = first column. */
  intake: string | null;
}

export type ColumnRole = "done" | "active" | "intake";

export interface Task {
  id: string;
  title: string;
  stage: Stage;
  created_at_ms: number;
  /** Board this task lives on. */
  board: string;
  /** Attached directories: a session started from the task runs in the first
   *  and gets the rest via `claude --add-dir`. */
  directories: string[];
  /** The tracker ticket this task was imported from, if any. */
  ticket?: TicketRef | null;
}

// Ticket trackers (GitHub, …). Everything here is provider-neutral — the UI
// renders whatever `GET /trackers` describes (see core-engine's trackers/mod.rs).

export type TicketState = "open" | "closed";

export interface TicketRef {
  provider: string;
  /** The provider's own id: "owner/repo#12", "ENG-42", … */
  key: string;
  title: string;
  url: string;
  state: TicketState;
}

export interface Ticket extends TicketRef {
  /** Repo / team / project it lives in. */
  container: string;
  labels: string[];
  assignee: string | null;
  updated_at: string;
  /** The task already linked to it on the requested board. */
  imported_task_id: string | null;
}

export interface AuthField {
  name: string;
  label: string;
  secret: boolean;
  help: string | null;
  help_url: string | null;
}

export interface ConnectionStatus {
  connected: boolean;
  account: string | null;
  /** Where the credentials came from, e.g. "keychain" or "gh". */
  source: string | null;
  error: string | null;
}

export interface TrackerInfo {
  id: string;
  name: string;
  container_label: string;
  container_placeholder: string;
  auth_fields: AuthField[];
  status: ConnectionStatus;
}

export interface TrackerLink {
  provider: string;
  container: string;
}

export interface TicketsResponse {
  tickets: Ticket[];
  errors: { provider: string; container: string; error: string }[];
}

/** session id → task id. A session absent from the map is unassigned. */
export interface TasksSnapshot {
  tasks: Task[];
  assignments: Record<string, string>;
  /** board id → layout; a board absent here uses `default_columns`. */
  columns: Record<string, BoardColumns>;
  default_columns: BoardColumns;
}

/** One WS message: a session diff, or a full snapshot of tasks/assignments
 * (the backend owns both — see `tasks.rs`). */
export type BoardMessage = { Upserted: SessionView } | { Removed: string } | { TasksChanged: TasksSnapshot };

export interface SearchResult {
  text: string;
  project: string;
  tool: string;
  session_id: string;
  distance: number;
  live: boolean;
  created_at: number;
}

/** A named board tied to one Claude config directory (one Claude account). */
export interface Board {
  id: string;
  name: string;
  config_dir: string;
}

export const DEFAULT_BOARD_ID = "default";
