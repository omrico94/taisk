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
}));
