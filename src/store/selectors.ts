import type { SessionView } from "../types";

export function matchesQuery(s: SessionView, query: string): boolean {
  if (!query.trim()) return true;
  const q = query.toLowerCase();
  return `${s.task} ${s.project} ${s.tool} ${s.category}`.toLowerCase().includes(q);
}

export interface CategoryGroup {
  name: string;
  sessions: SessionView[];
}

// Real categories are emergent (LLM-assigned), not the prototype's fixed
// four-category demo list — sorted alphabetically for a stable, simple
// ordering, with the transient "Uncategorized" placeholder pinned last since
// it's a loading state, not a real category.
export function groupByCategory(sessions: SessionView[]): CategoryGroup[] {
  const map = new Map<string, SessionView[]>();
  for (const s of sessions) {
    const list = map.get(s.category) ?? [];
    list.push(s);
    map.set(s.category, list);
  }
  const names = Array.from(map.keys()).sort((a, b) => {
    if (a === "Uncategorized" && b !== "Uncategorized") return 1;
    if (b === "Uncategorized" && a !== "Uncategorized") return -1;
    return a.localeCompare(b);
  });
  return names.map((name) => ({ name, sessions: map.get(name)! }));
}
