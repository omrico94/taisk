import { deleteSession } from "../api";
import { fmtTokens } from "../styles/sessionStyle";
import { ORPHAN } from "../store/selectors";
import { useSessionStore } from "../store/sessionStore";
import type { SessionView } from "../types";
import { beginDrag, endDrag, getDrag, moveSession } from "./boardActions";
import { SessionStateDot } from "./SessionStateDot";
import styles from "./Kanban.module.css";

interface Props {
  sessions: SessionView[];
  /** Empty tray shown only as a mid-drag drop target (overlays, no layout shift). */
  floating?: boolean;
}

/** Sessions with no task. Also a drop target: drop a session here to unassign it. */
export function UnassignedTray({ sessions, floating }: Props) {
  const drag = useSessionStore((s) => s.drag);
  const overTray = useSessionStore((s) => s.overTray);
  const setOver = useSessionStore((s) => s.setOver);

  const hot = drag.kind === "session" && overTray;

  return (
    <div
      className={`${styles.tray} ${hot ? styles.trayHot : ""} ${floating ? styles.trayFloating : ""}`}
      data-testid="tray"
      onDragOver={(e) => {
        if (getDrag().kind !== "session") return;
        e.preventDefault();
        if (!overTray) setOver({ tray: true, task: null });
      }}
      onDragLeave={(e) => {
        if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setOver({ tray: false });
      }}
      onDrop={(e) => {
        const d = getDrag();
        if (d.kind !== "session" || !d.sessId) return;
        e.preventDefault();
        moveSession(d.sessId, null);
        endDrag();
      }}
    >
      <div className={styles.trayHeader}>
        <span className={styles.trayLabel}>Unassigned sessions</span>
        <span className={styles.trayCount} data-testid="tray-count">
          {sessions.length}
        </span>
        <span className={styles.spacer} />
        <span className={styles.trayHint}>Drag onto a task, or drop one here to unassign</span>
      </div>
      <div className={styles.trayBody}>
        {sessions.map((s) => (
          <TrayChip key={s.id} session={s} />
        ))}
      </div>
    </div>
  );
}

function TrayChip({ session }: { session: SessionView }) {
  const selectCard = useSessionStore((s) => s.selectCard);
  const openAssignMenu = useSessionStore((s) => s.openAssignMenu);

  return (
    <div
      className={styles.chip}
      data-testid="tray-chip"
      data-session-id={session.id}
      data-flip={`session:${session.id}`}
      draggable
      onDragStart={(e) => {
        e.dataTransfer.setData("text/plain", session.id);
        e.dataTransfer.effectAllowed = "move";
        beginDrag({ kind: "session", taskId: null, sessId: session.id, srcTaskId: ORPHAN });
      }}
      onDragEnd={endDrag}
    >
      <SessionStateDot state={session.state} />
      <span className={styles.chipTitle} onClick={() => selectCard(session.id)}>
        {session.title}
      </span>
      {session.subs.length > 0 && <span className={styles.subCount}>⤷{session.subs.length}</span>}
      {session.tokens > 0 && <span className={styles.tokenCount}>{fmtTokens(session.tokens)}</span>}
      <button
        className={styles.assignButton}
        title="Assign to a task"
        aria-label="Assign to a task"
        onClick={(e) => {
          e.stopPropagation();
          const r = e.currentTarget.getBoundingClientRect();
          openAssignMenu({ sessionId: session.id, x: r.left, y: r.bottom + 6 });
        }}
      >
        ▾
      </button>
      <button
        className={`${styles.iconButton} ${styles.iconDelete}`}
        title="Delete session"
        aria-label="Delete session"
        onClick={() => void deleteSession(session.id)}
      >
        ✕
      </button>
    </div>
  );
}
