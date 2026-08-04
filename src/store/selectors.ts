import type { SessionView } from "../types";

export function matchesQuery(s: SessionView, query: string): boolean {
  if (!query.trim()) return true;
  const q = query.toLowerCase();
  const subText = s.subs.map((sub) => `${sub.title} ${sub.desc}`).join(" ");
  return `${s.title} ${s.desc} ${s.project} ${s.tool} ${s.category} ${subText}`.toLowerCase().includes(q);
}

export interface CategoryGroup {
  name: string;
  sessions: SessionView[];
}

// Lane order follows `knownCategories` (creation order, from `GET
// /categories` — matches the design's `cats.push(name)`: additive, never
// resorted) rather than alphabetical. `knownCategories` is the source of
// truth for which lanes exist at all (Phase 2 §5 — a freshly created
// category has zero sessions and must still render its own empty lane).
// Any category present on a session but missing from `knownCategories`
// (only "Uncategorized", a transient loading placeholder that's never
// created via `POST /categories`) is appended and pinned last.
export function groupByCategory(sessions: SessionView[], knownCategories: string[]): CategoryGroup[] {
  const map = new Map<string, SessionView[]>();
  for (const s of sessions) {
    const list = map.get(s.category) ?? [];
    list.push(s);
    map.set(s.category, list);
  }
  const names = [...knownCategories];
  for (const name of map.keys()) {
    if (!names.includes(name)) names.push(name);
  }
  names.sort((a, b) => {
    if (a === "Uncategorized" && b !== "Uncategorized") return 1;
    if (b === "Uncategorized" && a !== "Uncategorized") return -1;
    return 0; // stable sort: preserve knownCategories' creation order otherwise
  });
  return names.map((name) => ({ name, sessions: map.get(name) ?? [] }));
}
