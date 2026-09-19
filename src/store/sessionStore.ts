import { create } from "zustand";
import type { SearchResult, SessionView, Task, TasksSnapshot } from "../types";

/** A drag in flight. `srcTaskId` is `ORPHAN` (see selectors) for a session dragged out of the tray. */
export interface DragState {
  kind: "task" | "session" | null;
  taskId: string | null;
  sessId: string | null;
  srcTaskId: string | null;
}

export const NO_DRAG: DragState = { kind: null, taskId: null, sessId: null, srcTaskId: null };

interface SessionStoreState {
  sessions: Record<string, SessionView>;
  tasks: Task[];
  /** session id → task id (absent = unassigned). */
  assignments: Record<string, string>;
  query: string;
  selectedId: string | null;
  askOpen: boolean;
  askQuery: string;
  replyText: string;
  askResults: SearchResult[];
  /** Per-task session-list expand state; a task not in the map is expanded (design default). */
  collapsedTasks: Set<string>;
  drag: DragState;
  overCol: string | null;
  overTaskId: string | null;
  overTray: boolean;
  /** Which tray chip's ▾ assign menu is open, anchored to its screen rect (position:fixed). */
  assignMenu: { sessionId: string; x: number; y: number } | null;
  /** Idle sessions are hidden from the tray by default — a toolbar toggle reveals them. */
  hideIdle: boolean;

  setSessions: (sessions: SessionView[]) => void;
  upsertSession: (s: SessionView) => void;
  removeSession: (id: string) => void;
  setTasksSnapshot: (snap: TasksSnapshot) => void;
  setQuery: (q: string) => void;
  selectCard: (id: string | null) => void;
  setAskOpen: (open: boolean) => void;
  setAskQuery: (q: string) => void;
  /** Opens the Ask-memory overlay seeded with `q` (e.g. from the toolbar's live filter). */
  openAskWithQuery: (q: string) => void;
  setReplyText: (t: string) => void;
  setAskResults: (r: SearchResult[]) => void;
  toggleTaskCollapsed: (taskId: string) => void;
  setDrag: (d: DragState) => void;
  setOver: (o: { col?: string | null; task?: string | null; tray?: boolean }) => void;
  clearDrag: () => void;
  openAssignMenu: (m: { sessionId: string; x: number; y: number } | null) => void;
  toggleHideIdle: () => void;
}

export const useSessionStore = create<SessionStoreState>((set) => ({
  sessions: {},
  tasks: [],
  assignments: {},
  query: "",
  selectedId: null,
  askOpen: false,
  askQuery: "",
  replyText: "",
  askResults: [],
  collapsedTasks: new Set(),
  drag: NO_DRAG,
  overCol: null,
  overTaskId: null,
  overTray: false,
  assignMenu: null,
  hideIdle: true,

  setSessions: (sessions) => set({ sessions: Object.fromEntries(sessions.map((s) => [s.id, s])) }),
  upsertSession: (s) => set((state) => ({ sessions: { ...state.sessions, [s.id]: s } })),
  removeSession: (id) =>
    set((state) => {
      const next = { ...state.sessions };
      delete next[id];
      return { sessions: next, selectedId: state.selectedId === id ? null : state.selectedId };
    }),
  setTasksSnapshot: (snap) => set({ tasks: snap.tasks, assignments: snap.assignments }),
  setQuery: (query) => set({ query }),
  selectCard: (selectedId) => set({ selectedId, replyText: "" }),
  setAskOpen: (askOpen) => set({ askOpen }),
  setAskQuery: (askQuery) => set({ askQuery }),
  openAskWithQuery: (q) => set({ askQuery: q, askOpen: true }),
  setReplyText: (replyText) => set({ replyText }),
  setAskResults: (askResults) => set({ askResults }),
  toggleTaskCollapsed: (taskId) =>
    set((state) => {
      const next = new Set(state.collapsedTasks);
      if (next.has(taskId)) next.delete(taskId);
      else next.add(taskId);
      return { collapsedTasks: next };
    }),
  setDrag: (drag) => set({ drag }),
  setOver: (o) =>
    set((state) => ({
      overCol: o.col !== undefined ? o.col : state.overCol,
      overTaskId: o.task !== undefined ? o.task : state.overTaskId,
      overTray: o.tray !== undefined ? o.tray : state.overTray,
    })),
  clearDrag: () => set({ drag: NO_DRAG, overCol: null, overTaskId: null, overTray: false }),
  openAssignMenu: (assignMenu) => set({ assignMenu }),
  toggleHideIdle: () => set((state) => ({ hideIdle: !state.hideIdle })),
}));
