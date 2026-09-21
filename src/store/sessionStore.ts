import { create } from "zustand";
import { openSessionTerminal, openTaskTerminal } from "../api";
import type { SearchResult, SessionView, Task, TasksSnapshot } from "../types";

/** A drag in flight. `srcTaskId` is `ORPHAN` (see selectors) for a session dragged out of the tray. */
export interface DragState {
  kind: "task" | "session" | null;
  taskId: string | null;
  sessId: string | null;
  srcTaskId: string | null;
}

export const NO_DRAG: DragState = { kind: null, taskId: null, sessId: null, srcTaskId: null };

/** The one terminal pane the board shows. The process behind it lives in the
 * backend, so detaching/collapsing/switching never interrupts a session. */
export interface TerminalState {
  ptyId: string | null;
  /** Null while a session started from a task hasn't reported its real id yet. */
  sessionId: string | null;
  /** Set only for that pending case, to label the pane until `sessionId` resolves. */
  taskId: string | null;
  collapsed: boolean;
  alive: boolean;
}

export const DEFAULT_TERMINAL_HEIGHT = 320;
export const MIN_TERMINAL_HEIGHT = 120;
const HEIGHT_KEY = "sessionboard.terminalHeight";

/** Pane height in px, remembered across launches (storage can be unavailable — never fatal). */
function loadTerminalHeight(): number {
  try {
    const n = Number(localStorage.getItem(HEIGHT_KEY));
    if (Number.isFinite(n) && n >= MIN_TERMINAL_HEIGHT) return n;
  } catch {
    /* fall through */
  }
  return DEFAULT_TERMINAL_HEIGHT;
}

export const NO_TERMINAL: TerminalState = { ptyId: null, sessionId: null, taskId: null, collapsed: false, alive: true };

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
  terminal: TerminalState;
  /** Height of the terminal pane's body, in px (user-resizable). */
  terminalHeight: number;

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
  /** Attaches the pane to `sessionId`'s process (spawning `claude --resume` if none is running). */
  openTerminalForSession: (sessionId: string) => Promise<void>;
  /** Spawns a new `claude` for the task and attaches the pane to it. */
  openTerminalForTask: (taskId: string, cwd: string | undefined) => Promise<void>;
  closeTerminal: () => void;
  toggleTerminalCollapsed: () => void;
  /** A pending task-spawned terminal learned its real session id. */
  resolveTerminalSession: (sessionId: string) => void;
  setTerminalAlive: (alive: boolean) => void;
  setTerminalHeight: (px: number) => void;
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
  terminal: NO_TERMINAL,
  terminalHeight: loadTerminalHeight(),

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
  openTerminalForSession: async (sessionId) => {
    const { pty_id } = await openSessionTerminal(sessionId);
    set((state) =>
      state.terminal.ptyId === pty_id
        ? { terminal: { ...state.terminal, collapsed: false } }
        : { terminal: { ptyId: pty_id, sessionId, taskId: null, collapsed: false, alive: true } },
    );
  },
  openTerminalForTask: async (taskId, cwd) => {
    const { pty_id } = await openTaskTerminal(taskId, cwd);
    set({ terminal: { ptyId: pty_id, sessionId: null, taskId, collapsed: false, alive: true } });
  },
  closeTerminal: () => set({ terminal: NO_TERMINAL }),
  toggleTerminalCollapsed: () => set((state) => ({ terminal: { ...state.terminal, collapsed: !state.terminal.collapsed } })),
  resolveTerminalSession: (sessionId) => set((state) => ({ terminal: { ...state.terminal, sessionId } })),
  setTerminalAlive: (alive) => set((state) => ({ terminal: { ...state.terminal, alive } })),
  setTerminalHeight: (px) => {
    const terminalHeight = Math.max(MIN_TERMINAL_HEIGHT, Math.round(px));
    try {
      localStorage.setItem(HEIGHT_KEY, String(terminalHeight));
    } catch {
      /* not persisted — still applies for this run */
    }
    set({ terminalHeight });
  },
}));
