import { deleteSession } from "../api";
import { fmtTokens } from "../styles/sessionStyle";
import { useSessionStore } from "../store/sessionStore";
import type { SessionView } from "../types";
import { moveSession } from "./boardActions";
import { SessionStateDot } from "./SessionStateDot";
import styles from "./Kanban.module.css";

interface Props {
  session: SessionView;
  taskId: string;
}

/** A session inside a task card: draggable to another task or the tray. */
export function SessionRow({ session, taskId }: Props) {
  const selectCard = useSessionStore((s) => s.selectCard);
  const setDrag = useSessionStore((s) => s.setDrag);
  const clearDrag = useSessionStore((s) => s.clearDrag);

  return (
    <div
      className={styles.sessionRow}
      data-testid="session-row"
      data-session-id={session.id}
      data-flip={`session:${session.id}`}
      draggable
      onDragStart={(e) => {
        // Sessions live inside a draggable task card — keep this drag from
        // also starting a task drag.
        e.stopPropagation();
        e.dataTransfer.setData("text/plain", session.id);
        e.dataTransfer.effectAllowed = "move";
        setDrag({ kind: "session", taskId: null, sessId: session.id, srcTaskId: taskId });
      }}
      onDragEnd={clearDrag}
      onClick={(e) => {
        e.stopPropagation();
        selectCard(session.id);
      }}
    >
      <SessionStateDot state={session.state} />
      <span className={styles.sessionTitle}>{session.title}</span>
      {session.subs.length > 0 && <span className={styles.subCount}>⤷{session.subs.length}</span>}
      <span className={styles.tokenCount}>{fmtTokens(session.tokens)}</span>
      <button
        className={`${styles.iconButton} ${styles.iconUnassign}`}
        title="Unassign (send to tray)"
        aria-label="Unassign session"
        onClick={(e) => {
          e.stopPropagation();
          moveSession(session.id, null);
        }}
      >
        ↩
      </button>
      <button
        className={`${styles.iconButton} ${styles.iconDelete}`}
        title="Delete session"
        aria-label="Delete session"
        onClick={(e) => {
          e.stopPropagation();
          void deleteSession(session.id);
        }}
      >
        ✕
      </button>
    </div>
  );
}
