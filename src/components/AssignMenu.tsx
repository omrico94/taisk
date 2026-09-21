import { useEffect } from "react";
import { createPortal } from "react-dom";
import { useShallow } from "zustand/react/shallow";
import { useBoardSessions, useBoardTasks, useSessionStore } from "../store/sessionStore";
import { taskMeta, sessionsOfTask } from "../store/selectors";
import { moveSession } from "./boardActions";
import styles from "./Kanban.module.css";

const MENU_W = 210;

/** "Assign to…" dropdown for a tray chip. `position: fixed` (anchored to the
 * chip's button rect) so it escapes the board's `overflow` clipping. Closes on
 * any outside click or Escape. */
export function AssignMenu() {
  const menu = useSessionStore((s) => s.assignMenu);
  const open = useSessionStore((s) => s.openAssignMenu);
  const tasks = useBoardTasks();
  const sessions = useBoardSessions();
  const assignments = useSessionStore(useShallow((s) => s.assignments));

  useEffect(() => {
    if (!menu) return;
    const close = () => open(null);
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") close();
    };
    // Deferred a tick so the click that opened the menu doesn't close it.
    const t = setTimeout(() => document.addEventListener("click", close), 0);
    document.addEventListener("keydown", onKey);
    return () => {
      clearTimeout(t);
      document.removeEventListener("click", close);
      document.removeEventListener("keydown", onKey);
    };
  }, [menu, open]);

  if (!menu) return null;
  const left = Math.max(8, Math.min(menu.x, window.innerWidth - MENU_W - 8));

  return createPortal(
    <div
      className={styles.assignMenu}
      role="menu"
      data-testid="assign-menu"
      style={{ left, top: menu.y, width: MENU_W }}
      onClick={(e) => e.stopPropagation()}
    >
      <div className={styles.assignHeader}>Assign to…</div>
      {tasks.length === 0 && <div className={styles.assignEmpty}>No tasks yet — add one first.</div>}
      {tasks.map((t) => (
        <button
          key={t.id}
          role="menuitem"
          className={styles.assignItem}
          onClick={() => {
            moveSession(menu.sessionId, t.id);
            open(null);
          }}
        >
          <span className={styles.assignTitle}>{t.title}</span>
          <span className={styles.assignProject}>{taskMeta(sessionsOfTask(t.id, sessions, assignments)).project}</span>
        </button>
      ))}
    </div>,
    document.body,
  );
}
