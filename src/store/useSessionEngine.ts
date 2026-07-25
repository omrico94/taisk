import { useEffect } from "react";
import { useSessionStore } from "./sessionStore";
import { getSessions, WS_URL } from "../api";
import type { SessionDiff } from "../types";

// M10: the real data source, replacing useMockSessionEngine (M8). Seeds the
// store from GET /sessions on mount, then applies live WS diffs — same
// store, same components as the mock, only the source changed.
export function useSessionEngine(): void {
  const setSessions = useSessionStore((s) => s.setSessions);
  const upsertSession = useSessionStore((s) => s.upsertSession);
  const removeSession = useSessionStore((s) => s.removeSession);

  useEffect(() => {
    let cancelled = false;
    let ws: WebSocket | undefined;

    getSessions()
      .then((sessions) => {
        if (!cancelled) setSessions(sessions);
      })
      .catch((err) => console.error("Failed to load initial sessions from Core Engine:", err));

    ws = new WebSocket(WS_URL);
    ws.onmessage = (event) => {
      const diff = JSON.parse(event.data) as SessionDiff;
      if ("Upserted" in diff) {
        upsertSession(diff.Upserted);
      } else {
        removeSession(diff.Removed);
      }
    };
    ws.onerror = (err) => console.error("Core Engine WS error:", err);

    return () => {
      cancelled = true;
      ws?.close();
    };
  }, [setSessions, upsertSession, removeSession]);
}
