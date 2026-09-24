import type { SessionState } from "../types";

// Small, centralized mapping so state/tool colors are declared once instead
// of repeated string literals across every component that needs them.
export const STATE_LABEL: Record<SessionState, string> = {
  Working: "Working",
  Waiting: "Needs you",
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

// `session.entrypoint` mostly matters for distinguishing where a session
// actually came from — plain "cli" is the common case and doesn't need
// calling out, but the notable ones get a small badge on the card.
const ENTRYPOINT_LABEL: Record<string, string> = {
  "claude-desktop": "Desktop",
  "claude-vscode": "DevSwarm",
};

export function entrypointLabel(entrypoint: string): string | null {
  return ENTRYPOINT_LABEL[entrypoint] ?? null;
}

export function formatElapsed(startedAtMs: number, nowMs: number): string {
  const mins = Math.max(0, Math.floor((nowMs - startedAtMs) / 60000));
  if (mins < 60) return `${mins}m`;
  const h = Math.floor(mins / 60);
  const m = mins % 60;
  return `${h}h ${String(m).padStart(2, "0")}m`;
}

// Phase 2: real token/cost/context-window metrics (design §2's ctxColor/
// fmtTokens/fmtCost, ported from the prototype's JS to TS).

/** <70% cyan, 70-89% amber, >=90% red — the "signal at a glance" urgency ramp. */
export function ctxColor(pct: number): string {
  if (pct >= 90) return "var(--state-danger)";
  if (pct >= 70) return "var(--state-waiting)";
  return "var(--state-working)";
}

export function ctxPercent(ctxUsed: number, ctxMax: number): number {
  if (ctxMax <= 0) return 0;
  return Math.min(100, Math.round((ctxUsed / ctxMax) * 100));
}

/** 128400 -> "128K", 1_200_000 -> "1.2M". */
export function fmtTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${Math.round(n / 1000)}K`;
  return `${n}`;
}

export function fmtCost(cost: number): string {
  return `$${cost.toFixed(2)}`;
}

export function planPercent(done: number, total: number): number {
  if (total <= 0) return 0;
  return Math.round((done / total) * 100);
}
