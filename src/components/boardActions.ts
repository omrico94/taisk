import { assignSession, updateTask } from "../api";
import { ORPHAN } from "../store/selectors";
import { NO_DRAG, useSessionStore } from "../store/sessionStore";
import type { Stage } from "../types";

// Every mutation goes to the Core Engine, which owns tasks/assignments;
// the resulting state (incl. the Done rollup) comes back over the WS as a
// `TasksChanged` snapshot, so nothing here mutates the store locally.

/** Session → task, or `null` to unassign (back to the tray). */
export function moveSession(sessionId: string, toTaskId: string | null): void {
  const { assignments } = useSessionStore.getState();
  if ((assignments[sessionId] ?? null) === toTaskId) return; // already there
  void assignSession(sessionId, toTaskId);
}

export function moveTask(taskId: string, stage: Stage): void {
  const task = useSessionStore.getState().tasks.find((t) => t.id === taskId);
  if (!task || task.stage === stage) return;
  void updateTask(taskId, { stage });
}

export { NO_DRAG, ORPHAN };
