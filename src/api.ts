import type { Board, SearchResult, SessionView, Stage, Task, TasksSnapshot } from "./types";

// Matches src-tauri/src/lib.rs's API_PORT constant — the desktop app's own
// Core Engine instance. A future Phase-3 VS Code extension would need real
// port discovery (a port file); not needed yet for desktop-only Phase 1.
// `VITE_API_PORT` only exists so an E2E run can point a scratch frontend at a
// scratch backend next to a real app instance; unset, it's the fixed port.
const API_PORT = import.meta.env.VITE_API_PORT ?? "37888";
const API_BASE = `http://127.0.0.1:${API_PORT}`;
export const WS_URL = `ws://127.0.0.1:${API_PORT}/events`;

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
export async function createTask(title: string, stage: Stage, board: string): Promise<Task> {
  const resp = await fetch(`${API_BASE}/tasks`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ title, stage, board }),
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

export async function search(query: string, board: string): Promise<SearchResult[]> {
  const resp = await fetch(`${API_BASE}/search?q=${encodeURIComponent(query)}&board=${encodeURIComponent(board)}`);
  return resp.json();
}

export async function getBoards(): Promise<Board[]> {
  const resp = await fetch(`${API_BASE}/boards`);
  return resp.json();
}

// Board mutations surface the backend's own message (e.g. "another board
// already uses that config directory") so the dialog can show it verbatim.
async function boardRequest(url: string, init: RequestInit): Promise<Response> {
  const resp = await fetch(url, init);
  if (!resp.ok) {
    const body = (await resp.json().catch(() => null)) as { error?: string } | null;
    throw new Error(body?.error ?? `Request failed (${resp.status})`);
  }
  return resp;
}

export async function createBoard(name: string, configDir: string): Promise<Board> {
  const resp = await boardRequest(`${API_BASE}/boards`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name, config_dir: configDir }),
  });
  return resp.json();
}

export async function renameBoard(id: string, name: string): Promise<Board> {
  const resp = await boardRequest(`${API_BASE}/boards/${encodeURIComponent(id)}`, {
    method: "PATCH",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ name }),
  });
  return resp.json();
}

export async function deleteBoard(id: string): Promise<void> {
  await boardRequest(`${API_BASE}/boards/${encodeURIComponent(id)}`, { method: "DELETE" });
}

/** A pty is a real `claude` process kept alive by the backend (see terminal.rs). */
export interface OpenedTerminal {
  pty_id: string;
  /** True when a running process for that session was reattached, not respawned. */
  reused: boolean;
}

/** Reattaches to the running process for `sessionId`, else spawns `claude --resume`. */
export async function openSessionTerminal(sessionId: string): Promise<OpenedTerminal> {
  const resp = await fetch(`${API_BASE}/terminals/sessions/${encodeURIComponent(sessionId)}`, { method: "POST" });
  if (!resp.ok) throw new Error(`Failed to open a terminal for session ${sessionId} (${resp.status})`);
  return resp.json();
}

/** Always spawns a fresh `claude`; the new session is filed under `taskId` once it appears. */
export async function openTaskTerminal(taskId: string, cwd: string | undefined): Promise<OpenedTerminal> {
  const resp = await fetch(`${API_BASE}/terminals/tasks/${encodeURIComponent(taskId)}`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ cwd: cwd ?? null }),
  });
  if (!resp.ok) throw new Error(`Failed to start a session for task ${taskId} (${resp.status})`);
  return resp.json();
}

export function terminalWsUrl(ptyId: string): string {
  return `ws://127.0.0.1:${API_PORT}/terminals/${encodeURIComponent(ptyId)}/ws`;
}
