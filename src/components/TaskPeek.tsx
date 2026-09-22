import { useCallback, useEffect, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { getBoards, getTasks, WS_URL } from "../api";
import type { Board, BoardMessage, Stage, Task, TasksSnapshot } from "../types";
import styles from "./Popup.module.css";

const STAGES: { stage: Stage; label: string }[] = [
  { stage: "inprogress", label: "In Progress" },
  { stage: "todo", label: "To Do" },
  { stage: "backlog", label: "Backlog" },
  { stage: "done", label: "Done" },
];

// Global-shortcut popup: read-only list of every task with its stage,
// kept live over the same WS the main window uses.
export function TaskPeek() {
  const [snap, setSnap] = useState<TasksSnapshot>({ tasks: [], assignments: {} });
  const [boards, setBoards] = useState<Board[]>([]);
  const [error, setError] = useState(false);

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
    <div className={styles.panel}>
      <div className={styles.title}>Tasks</div>
      {error && <div className={styles.error}>Can't reach the SessionBoard engine</div>}
      <div className={styles.list} style={{ gap: 10 }}>
        {snap.tasks.length === 0 && !error && <div className={styles.empty}>No tasks yet.</div>}
        {STAGES.map(({ stage, label }) => {
          const tasks = snap.tasks.filter((t) => t.stage === stage);
          if (tasks.length === 0) return null;
          return (
            <div key={stage}>
              <div className={styles.hint}>
                {label} · {tasks.length}
              </div>
              {tasks.map((t) => (
                <div key={t.id} className={styles.row}>
                  <span className={`${styles.dot} ${styles[`stage_${stage}`]}`} />
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
