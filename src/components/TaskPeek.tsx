import { useCallback, useEffect, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { getBoards, getTasks, WS_URL } from "../api";
import type { Board, BoardColumns, BoardMessage, Task, TasksSnapshot } from "../types";
import { useAutoResizeWindow } from "./useAutoResizeWindow";
import styles from "./Popup.module.css";

const PANEL_WIDTH = 360;
const PANEL_MAX_HEIGHT = 520;

const EMPTY: TasksSnapshot = {
  tasks: [],
  assignments: {},
  columns: {},
  default_columns: { columns: [], done: null, active: null, intake: null },
};

/** One group per column name across boards (a column two boards share, e.g.
 * both boards' "Done", is one group), ordered what's-live-first: active-role
 * columns ("In Progress"), then the rest in board order, done-role last. */
function groups(snap: TasksSnapshot, boards: Board[]): { key: string; label: string; color: string; strong: boolean; tasks: Task[] }[] {
  const layoutOf = (board: string): BoardColumns => snap.columns[board] ?? snap.default_columns;
  const boardIds = boards.length ? boards.map((b) => b.id) : [...new Set(snap.tasks.map((t) => t.board))];
  const out = new Map<string, { key: string; label: string; color: string; strong: boolean; tasks: Task[]; rank: number }>();
  boardIds.forEach((board, bi) => {
    const layout = layoutOf(board);
    layout.columns.forEach((col, ci) => {
      const key = col.name.trim().toLowerCase();
      const strong = layout.active === col.id;
      const tasks = snap.tasks.filter((t) => t.board === board && t.stage === col.id);
      const g = out.get(key);
      if (g) {
        g.tasks.push(...tasks);
        g.strong ||= strong;
      } else {
        const rank = (strong ? 0 : layout.done === col.id ? 2e6 : 1e6) + bi * 1000 + ci;
        out.set(key, { key, label: col.name, color: col.color, strong, tasks, rank });
      }
    });
  });
  return [...out.values()].sort((a, b) => a.rank - b.rank);
}

// Global-shortcut popup: read-only list of every task with its stage,
// kept live over the same WS the main window uses.
export function TaskPeek() {
  const [snap, setSnap] = useState<TasksSnapshot>(EMPTY);
  const [boards, setBoards] = useState<Board[]>([]);
  const [error, setError] = useState(false);
  const panelRef = useRef<HTMLDivElement>(null);
  useAutoResizeWindow(panelRef, PANEL_WIDTH, PANEL_MAX_HEIGHT);

  const refresh = useCallback(() => {
    setError(false);
    Promise.all([getTasks(), getBoards()])
      .then(([s, b]) => {
        setSnap(s);
        setBoards(b);
      })
      .catch(() => setError(true));
  }, []);

  useEffect(() => {
    refresh();
    const un = listen("shortcut:shown", () => {
      window.focus();
      refresh();
    });
    let ws: WebSocket | undefined;
    let timer: ReturnType<typeof setTimeout>;
    let closed = false;
    const connect = () => {
      ws = new WebSocket(WS_URL);
      ws.onmessage = (ev) => {
        const msg = JSON.parse(ev.data) as BoardMessage;
        if ("TasksChanged" in msg) setSnap(msg.TasksChanged);
      };
      ws.onclose = () => {
        if (!closed) timer = setTimeout(connect, 2000);
      };
    };
    connect();
    // Same safety net as the main board (useSessionEngine.ts): the WS isn't
    // reliably delivering in the real packaged webview, so poll too rather
    // than trust it alone.
    const pollTimer = setInterval(refresh, 4000);
    return () => {
      closed = true;
      clearTimeout(timer);
      clearInterval(pollTimer);
      ws?.close();
      un.then((f) => f());
    };
  }, [refresh]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") void getCurrentWindow().hide();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, []);

  const boardName = (t: Task) => boards.find((b) => b.id === t.board)?.name;
  const sessionCount = (t: Task) => Object.values(snap.assignments).filter((id) => id === t.id).length;

  return (
    <div className={styles.panel} ref={panelRef}>
      <div className={styles.title}>Tasks</div>
      {error && <div className={styles.error}>Can't reach the taisk engine</div>}
      <div className={styles.list} style={{ gap: 10, maxHeight: 440 }}>
        {snap.tasks.length === 0 && !error && <div className={styles.empty}>No tasks yet.</div>}
        {groups(snap, boards).map(({ key, label, color, strong, tasks }) => {
          if (tasks.length === 0) return null;
          return (
            <div key={key}>
              <div className={styles.stageHeader} style={{ color }}>
                {label} · {tasks.length}
              </div>
              {tasks.map((t) => (
                <div key={t.id} className={styles.row}>
                  <span className={styles.dot} style={{ background: color, boxShadow: strong ? `0 0 8px ${color}99` : undefined }} />
                  <span className={styles.name}>{t.title}</span>
                  {sessionCount(t) > 0 && <span className={styles.meta}>{sessionCount(t)} sess.</span>}
                  {boards.length > 1 && <span className={styles.meta}>{boardName(t)}</span>}
                </div>
              ))}
            </div>
          );
        })}
      </div>
      <div className={styles.hint}>esc close</div>
    </div>
  );
}
