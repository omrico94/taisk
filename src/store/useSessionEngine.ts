import { useEffect } from "react";
import { useSessionStore } from "./sessionStore";
import { getBoards, getSessions, getTasks, WS_URL } from "../api";
import type { BoardMessage } from "../types";

// M10: the real data source, replacing useMockSessionEngine (M8). Seeds the
// store from GET /sessions on mount, then applies live WS diffs — same
// store, same components as the mock, only the source changed.
export function useSessionEngine(): void {
  const setSessions = useSessionStore((s) => s.setSessions);
  const upsertSession = useSessionStore((s) => s.upsertSession);
  const removeSession = useSessionStore((s) => s.removeSession);
  const setTasksSnapshot = useSessionStore((s) => s.setTasksSnapshot);
  const setBoards = useSessionStore((s) => s.setBoards);

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

      getBoards()
        .then((boards) => {
          if (!cancelled) setBoards(boards);
        })
        .catch((err) => console.error("Failed to load boards from Core Engine:", err));

      getTasks()
        .then((snap) => {
          if (!cancelled) setTasksSnapshot(snap);
        })
        .catch((err) => console.error("Failed to load tasks from Core Engine:", err));
    };

    const connect = () => {
      if (cancelled) return;
      loadSnapshot();

      ws = new WebSocket(WS_URL);
      ws.onopen = () => {
        retryDelayMs = 1000; // reset backoff once a connection actually succeeds
      };
      ws.onmessage = (event) => {
        const msg = JSON.parse(event.data) as BoardMessage;
        if ("Upserted" in msg) {
          upsertSession(msg.Upserted);
        } else if ("Removed" in msg) {
          removeSession(msg.Removed);
        } else {
          setTasksSnapshot(msg.TasksChanged);
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

    // Safety net: the live WS should make every add/update show up instantly,
    // but it's not reliably delivering messages in the real packaged webview
    // (reported bug — a session's own WS opens/reads fine from a plain
    // browser tab hitting the same engine, yet the board only ever picked up
    // a change on a manual reload). Rather than chase a WebKit-specific
    // root cause, fall back to plain polling regardless of what the socket
    // reports, so the board can't go stale for longer than one interval no
    // matter what's wrong with it.
    const pollTimer = setInterval(loadSnapshot, 5000);

    return () => {
      cancelled = true;
      clearTimeout(reconnectTimer);
      clearInterval(pollTimer);
      ws?.close();
    };
  }, [setSessions, upsertSession, removeSession, setTasksSnapshot, setBoards]);
}
