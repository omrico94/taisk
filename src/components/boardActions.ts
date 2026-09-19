import { assignSession, updateTask } from "../api";
import { ORPHAN } from "../store/selectors";
import { NO_DRAG, useSessionStore, type DragState } from "../store/sessionStore";
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

// The drag in flight, readable synchronously by drag handlers. The store copy
// (`useSessionStore.drag`) drives *visuals* only and is published one tick after
// `dragstart`: mutating the DOM (fading the card, floating the tray in) inside
// the `dragstart` handler itself can make Chrome abort the drag or snapshot a
// half-updated drag image. Handlers must not wait for that render, so they read
// this instead.
let liveDrag: DragState = NO_DRAG;

export function getDrag(): DragState {
  return liveDrag;
}

export function beginDrag(d: DragState): void {
  liveDrag = d;
  setTimeout(() => {
    // Ignore if the drag already ended before the tick fired.
    if (liveDrag === d) useSessionStore.getState().setDrag(d);
  }, 0);
}

export function endDrag(): void {
  liveDrag = NO_DRAG;
  useSessionStore.getState().clearDrag();
}

export { NO_DRAG, ORPHAN };
