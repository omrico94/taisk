import { create } from "zustand";
import type { SearchResult, SessionView } from "../types";

interface SessionStoreState {
  sessions: Record<string, SessionView>;
  query: string;
  selectedId: string | null;
  askOpen: boolean;
  askQuery: string;
  replyText: string;
  askResults: SearchResult[];
  collapsedCategories: Set<string>;
  /** Per-session subagent-tree collapse state (design: default expanded). */
  collapsedSubs: Set<string>;
  /** Known categories in creation order (from `GET /categories`) — drives
   * lane order/empty-lane rendering, distinct from `sessions`' own
   * `.category` field which only reflects categories that have ≥1 session. */
  categories: string[];
  /** Set while a lane is a drop target during a drag — used to render the
   * hot-lane highlight; not persisted, purely transient UI state. */
  dragOverCategory: string | null;

  setSessions: (sessions: SessionView[]) => void;
  upsertSession: (s: SessionView) => void;
  removeSession: (id: string) => void;
  setQuery: (q: string) => void;
  selectCard: (id: string | null) => void;
  setAskOpen: (open: boolean) => void;
  setAskQuery: (q: string) => void;
  /** Opens the Ask-memory overlay seeded with `q` (e.g. from the toolbar's live filter). */
  openAskWithQuery: (q: string) => void;
  setReplyText: (t: string) => void;
  setAskResults: (r: SearchResult[]) => void;
  toggleCategoryCollapsed: (name: string) => void;
  toggleSubsCollapsed: (sessionId: string) => void;
  setCategories: (categories: string[]) => void;
  /** Appends locally after a successful `POST /categories` — no need to refetch. */
  addCategory: (name: string) => void;
  setDragOverCategory: (name: string | null) => void;
}

export const useSessionStore = create<SessionStoreState>((set) => ({
  sessions: {},
  query: "",
  selectedId: null,
  askOpen: false,
  askQuery: "",
  replyText: "",
  askResults: [],
  collapsedCategories: new Set(),
  collapsedSubs: new Set(),
  categories: [],
  dragOverCategory: null,

  setSessions: (sessions) => set({ sessions: Object.fromEntries(sessions.map((s) => [s.id, s])) }),
  upsertSession: (s) => set((state) => ({ sessions: { ...state.sessions, [s.id]: s } })),
  removeSession: (id) =>
    set((state) => {
      const next = { ...state.sessions };
      delete next[id];
      return { sessions: next };
    }),
  setQuery: (query) => set({ query }),
  selectCard: (selectedId) => set({ selectedId, replyText: "" }),
  setAskOpen: (askOpen) => set({ askOpen }),
  setAskQuery: (askQuery) => set({ askQuery }),
  openAskWithQuery: (q) => set({ askQuery: q, askOpen: true }),
  setReplyText: (replyText) => set({ replyText }),
  setAskResults: (askResults) => set({ askResults }),
  toggleCategoryCollapsed: (name) =>
    set((state) => {
      const next = new Set(state.collapsedCategories);
      if (next.has(name)) next.delete(name);
      else next.add(name);
      return { collapsedCategories: next };
    }),
  toggleSubsCollapsed: (sessionId) =>
    set((state) => {
      const next = new Set(state.collapsedSubs);
      if (next.has(sessionId)) next.delete(sessionId);
      else next.add(sessionId);
      return { collapsedSubs: next };
    }),
  setCategories: (categories) => set({ categories }),
  addCategory: (name) =>
    set((state) => (state.categories.includes(name) ? state : { categories: [...state.categories, name] })),
  setDragOverCategory: (dragOverCategory) => set({ dragOverCategory }),
}));
