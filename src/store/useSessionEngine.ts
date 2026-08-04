import { useEffect } from "react";
import { useSessionStore } from "./sessionStore";
import { getCategories, getSessions, WS_URL } from "../api";
import type { SessionDiff } from "../types";

// M10: the real data source, replacing useMockSessionEngine (M8). Seeds the
// store from GET /sessions on mount, then applies live WS diffs — same
// store, same components as the mock, only the source changed.
export function useSessionEngine(): void {
  const setSessions = useSessionStore((s) => s.setSessions);
  const upsertSession = useSessionStore((s) => s.upsertSession);
  const removeSession = useSessionStore((s) => s.removeSession);
  const setCategories = useSessionStore((s) => s.setCategories);

  useEffect(() => {
    let cancelled = false;
    let ws: WebSocket | undefined;
    let reconnectTimer: ReturnType<typeof setTimeout> | undefined;
    // Backend restarts kill this socket (e.g. the Rust dev-server rebuilding
    // on every save), and a plain WebSocket never reconnects on its own —
    // without retry logic, a tab left open across a restart silently freezes
    // on whatever it last saw, never learning about sessions that started
    // afterward. Refetch the full snapshot on every reconnect (not just
    // resume the diff stream) since diffs broadcast while disconnected are
    // simply lost — there's no server-side backlog to replay.
    let retryDelayMs = 1000;

    const loadSnapshot = () => {
      getSessions()
        .then((sessions) => {
          if (!cancelled) setSessions(sessions);
        })
        .catch((err) => console.error("Failed to load sessions from Core Engine:", err));

      getCategories()
        .then((categories) => {
          if (!cancelled) setCategories(categories);
        })
        .catch((err) => console.error("Failed to load categories from Core Engine:", err));
    };

    const connect = () => {
      if (cancelled) return;
      loadSnapshot();

      ws = new WebSocket(WS_URL);
      ws.onopen = () => {
        retryDelayMs = 1000; // reset backoff once a connection actually succeeds
      };
      ws.onmessage = (event) => {
        const diff = JSON.parse(event.data) as SessionDiff;
        if ("Upserted" in diff) {
          upsertSession(diff.Upserted);
        } else {
          removeSession(diff.Removed);
        }
      };
      ws.onerror = (err) => console.error("Core Engine WS error:", err);
      ws.onclose = () => {
        if (cancelled) return;
        reconnectTimer = setTimeout(connect, retryDelayMs);
        retryDelayMs = Math.min(retryDelayMs * 2, 15000);
      };
    };

    connect();

    return () => {
      cancelled = true;
      clearTimeout(reconnectTimer);
      ws?.close();
    };
  }, [setSessions, upsertSession, removeSession, setCategories]);
}
