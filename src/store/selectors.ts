import type { SessionView, Stage, Task, TasksSnapshot } from "../types";

export const STAGES: { key: Stage; label: string; accent: string }[] = [
  { key: "backlog", label: "Backlog", accent: "#A08FC4" },
  { key: "todo", label: "To Do", accent: "#6FB9C9" },
  { key: "inprogress", label: "In Progress", accent: "#C8FF3D" },
  { key: "done", label: "Done", accent: "#86B98C" },
];

/** Design constant: In Progress column's WIP limit. */
export const WIP_LIMIT = 5;

/** Sentinel for `drag.srcTaskId` when a session is dragged out of the tray. */
export const ORPHAN = "__orphan__";

/** Case-insensitive substring match over everything a session shows. */
export function matchesQuery(s: SessionView, query: string): boolean {
  if (!query.trim()) return true;
  const q = query.toLowerCase();
  const subText = s.subs.map((sub) => `${sub.title} ${sub.desc}`).join(" ");
  return `${s.title} ${s.desc} ${s.project} ${s.tool} ${subText}`.toLowerCase().includes(q);
}

export interface BoardTask {
  task: Task;
  sessions: SessionView[];
}

export interface Rollup {
  total: number;
  /** Sessions that have stopped working (Done or Idle). */
  done: number;
  pct: number;
  tokens: number;
  cost: number;
  needsYou: boolean;
  anyWorking: boolean;
}

export function taskRollup(sessions: SessionView[]): Rollup {
  const total = sessions.length;
  const done = sessions.filter((s) => s.state === "Done" || s.state === "Idle").length;
  return {
    total,
    done,
    pct: total ? Math.round((done / total) * 100) : 0,
    tokens: sessions.reduce((a, s) => a + s.tokens, 0),
    cost: sessions.reduce((a, s) => a + s.cost, 0),
    needsYou: sessions.some((s) => s.state === "Waiting"),
    anyWorking: sessions.some((s) => s.state === "Working"),
  };
}

/** Project/tool on a task card are derived from its sessions (first wins). */
export function taskMeta(sessions: SessionView[]): { project: string; tool: string } {
  const first = sessions[0];
  return { project: first?.project ?? "—", tool: first?.tool ?? "" };
}

/** Sessions belonging to `taskId`, oldest first so the list is stable. */
export function sessionsOfTask(
  taskId: string,
  sessions: SessionView[],
  assignments: TasksSnapshot["assignments"],
): SessionView[] {
  return sessions.filter((s) => assignments[s.id] === taskId).sort((a, b) => a.started_at_ms - b.started_at_ms);
}

/** A task whose assignment points at a task that no longer exists counts as unassigned. */
export function orphanSessions(
  sessions: SessionView[],
  tasks: Task[],
  assignments: TasksSnapshot["assignments"],
): SessionView[] {
  const known = new Set(tasks.map((t) => t.id));
  return sessions
    .filter((s) => !assignments[s.id] || !known.has(assignments[s.id]))
    .sort((a, b) => a.started_at_ms - b.started_at_ms);
}
