import { deleteSession } from "../api";
import { fmtTokens } from "../styles/sessionStyle";
import { ORPHAN } from "../store/selectors";
import { useSessionStore } from "../store/sessionStore";
import type { SessionView } from "../types";
import { moveSession } from "./boardActions";
import { SessionStateDot } from "./SessionStateDot";
import styles from "./Kanban.module.css";

interface Props {
  sessions: SessionView[];
}

/** Sessions with no task. Also a drop target: drop a session here to unassign it. */
export function UnassignedTray({ sessions }: Props) {
  const drag = useSessionStore((s) => s.drag);
  const overTray = useSessionStore((s) => s.overTray);
  const setOver = useSessionStore((s) => s.setOver);
  const clearDrag = useSessionStore((s) => s.clearDrag);

  const hot = drag.kind === "session" && overTray;

  return (
    <div
      className={`${styles.tray} ${hot ? styles.trayHot : ""}`}
      data-testid="tray"
      onDragOver={(e) => {
        if (drag.kind !== "session") return;
        e.preventDefault();
        if (!overTray) setOver({ tray: true, task: null });
      }}
      onDragLeave={(e) => {
        if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setOver({ tray: false });
      }}
      onDrop={(e) => {
        if (drag.kind !== "session" || !drag.sessId) return;
        e.preventDefault();
        moveSession(drag.sessId, null);
        clearDrag();
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
  const setDrag = useSessionStore((s) => s.setDrag);
  const clearDrag = useSessionStore((s) => s.clearDrag);
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
        setDrag({ kind: "session", taskId: null, sessId: session.id, srcTaskId: ORPHAN });
      }}
      onDragEnd={clearDrag}
    >
      <SessionStateDot state={session.state} />
      <span className={styles.chipTitle} onClick={() => selectCard(session.id)}>
        {session.title}
      </span>
      {session.subs.length > 0 && <span className={styles.subCount}>⤷{session.subs.length}</span>}
      <span className={styles.tokenCount}>{fmtTokens(session.tokens)}</span>
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
