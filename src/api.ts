import type { SearchResult, SessionView, Stage, Task, TasksSnapshot } from "./types";

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

export async function getTasks(): Promise<TasksSnapshot> {
  const resp = await fetch(`${API_BASE}/tasks`);
  return resp.json();
}

/** Throws on a rejected (e.g. blank-title) request so callers can surface it. */
export async function createTask(title: string, stage: Stage): Promise<Task> {
  const resp = await fetch(`${API_BASE}/tasks`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ title, stage }),
  });
  if (!resp.ok) throw new Error(`Failed to create task "${title}" (${resp.status})`);
  return resp.json();
}

export async function updateTask(id: string, patch: { title?: string; stage?: Stage }): Promise<void> {
  await fetch(`${API_BASE}/tasks/${encodeURIComponent(id)}`, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(patch),
  });
}

/** Its sessions become unassigned (they return to the tray). */
export async function deleteTask(id: string): Promise<void> {
  await fetch(`${API_BASE}/tasks/${encodeURIComponent(id)}`, { method: "DELETE" });
}

/** `taskId: null` unassigns the session (back to the tray). */
export async function assignSession(sessionId: string, taskId: string | null): Promise<void> {
  await fetch(`${API_BASE}/sessions/${encodeURIComponent(sessionId)}/task`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ task_id: taskId }),
  });
}

export async function search(query: string): Promise<SearchResult[]> {
  const resp = await fetch(`${API_BASE}/search?q=${encodeURIComponent(query)}`);
  return resp.json();
}
