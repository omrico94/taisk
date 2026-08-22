import type { SearchResult, SessionView } from "./types";

// Matches src-tauri/src/lib.rs's API_PORT constant — the desktop app's own
// Core Engine instance. A future Phase-3 VS Code extension would need real
// port discovery (a port file); not needed yet for desktop-only Phase 1.
const API_BASE = "http://127.0.0.1:37888";
export const WS_URL = "ws://127.0.0.1:37888/events";

export interface TranscriptRow {
  role: string;
  text: string;
}

export async function getSessions(): Promise<SessionView[]> {
  const resp = await fetch(`${API_BASE}/sessions`);
  return resp.json();
}

export async function getTranscript(sessionId: string): Promise<TranscriptRow[]> {
  const resp = await fetch(`${API_BASE}/sessions/${encodeURIComponent(sessionId)}/transcript`);
  return resp.json();
}

export async function approveSession(id: string): Promise<void> {
  await fetch(`${API_BASE}/sessions/${encodeURIComponent(id)}/approve`, { method: "POST" });
}

export async function rejectSession(id: string): Promise<void> {
  await fetch(`${API_BASE}/sessions/${encodeURIComponent(id)}/reject`, { method: "POST" });
}

export async function replySession(id: string, text: string): Promise<void> {
  await fetch(`${API_BASE}/sessions/${encodeURIComponent(id)}/reply`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ text }),
  });
}

export async function deleteSession(id: string): Promise<void> {
  await fetch(`${API_BASE}/sessions/${encodeURIComponent(id)}`, { method: "DELETE" });
}

export async function recategorizeSession(id: string, category: string): Promise<void> {
  await fetch(`${API_BASE}/sessions/${encodeURIComponent(id)}/recategorize`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ category }),
  });
}

export async function getCategories(): Promise<string[]> {
  const resp = await fetch(`${API_BASE}/categories`);
  return resp.json();
}

/** Throws on a rejected (e.g. blank-name) request so callers can surface it. */
export async function createCategory(name: string): Promise<void> {
  const resp = await fetch(`${API_BASE}/categories`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name }),
  });
  if (!resp.ok) {
    throw new Error(`Failed to create category "${name}" (${resp.status})`);
  }
}

/** Cascades server-side: every live session currently in this category is
 * durably deleted too (see the backend's `delete_category`), so removed
 * cards arrive here the same way a single-session delete does — over the
 * WS diff stream — rather than needing to be filtered out locally. */
export async function deleteCategory(name: string): Promise<void> {
  await fetch(`${API_BASE}/categories/${encodeURIComponent(name)}`, { method: "DELETE" });
}

export async function search(query: string): Promise<SearchResult[]> {
  const resp = await fetch(`${API_BASE}/search?q=${encodeURIComponent(query)}`);
  return resp.json();
}
