import type { SessionState } from "../types";

// Small, centralized mapping so state/tool colors are declared once instead
// of repeated string literals across every component that needs them.
export const STATE_LABEL: Record<SessionState, string> = {
  Working: "Working",
  Waiting: "Waiting",
  Idle: "Idle",
  Done: "Done",
};

export const STATE_COLOR_VAR: Record<SessionState, string> = {
  Working: "var(--state-working)",
  Waiting: "var(--state-waiting)",
  Idle: "var(--state-idle)",
  Done: "var(--state-done)",
};

export const STATE_GLOW_VAR: Record<SessionState, string> = {
  Working: "var(--state-working-glow)",
  Waiting: "var(--state-waiting-glow)",
  Idle: "transparent",
  Done: "transparent",
};

const TOOL_COLOR_VAR: Record<string, string> = {
  "Claude Code": "var(--tool-claude-code)",
  Aider: "var(--tool-aider)",
  Cursor: "var(--tool-cursor)",
};

export function toolColorVar(tool: string): string {
  return TOOL_COLOR_VAR[tool] ?? "var(--text-muted)";
}

export function formatElapsed(startedAtMs: number, nowMs: number): string {
  const mins = Math.max(0, Math.floor((nowMs - startedAtMs) / 60000));
  if (mins < 60) return `${mins}m`;
  const h = Math.floor(mins / 60);
  const m = mins % 60;
  return `${h}h ${String(m).padStart(2, "0")}m`;
}
