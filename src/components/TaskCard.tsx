import { deleteTask } from "../api";
import { useSessionStore } from "../store/sessionStore";
import { taskMeta, taskRollup } from "../store/selectors";
import { fmtCost, fmtTokens, toolColorVar } from "../styles/sessionStyle";
import type { SessionView, Task } from "../types";
import { beginDrag, endDrag, getDrag, moveSession } from "./boardActions";
import { SessionRow } from "./SessionRow";
import styles from "./Kanban.module.css";

interface Props {
  task: Task;
  sessions: SessionView[];
}

export function TaskCard({ task, sessions }: Props) {
  const collapsed = useSessionStore((s) => s.collapsedTasks.has(task.id));
  const toggle = useSessionStore((s) => s.toggleTaskCollapsed);
  const drag = useSessionStore((s) => s.drag);
  const overTaskId = useSessionStore((s) => s.overTaskId);
  const setOver = useSessionStore((s) => s.setOver);

  const roll = taskRollup(sessions);
  const { project, tool } = taskMeta(sessions);
  const isDragging = drag.kind === "task" && drag.taskId === task.id;
  const sessionOver = drag.kind === "session" && overTaskId === task.id && drag.srcTaskId !== task.id;

  const cls = [
    styles.taskCard,
    roll.needsYou ? styles.taskNeeds : roll.anyWorking ? styles.taskWorking : "",
    task.stage === "done" ? styles.taskDone : "",
    sessionOver ? styles.taskDropTarget : "",
  ].join(" ");

  return (
    <div
      className={cls}
      data-testid="task-card"
      data-task-id={task.id}
      data-flip={`task:${task.id}`}
      style={{ opacity: isDragging ? 0.4 : undefined }}
      draggable
      onDragStart={(e) => {
        e.dataTransfer.setData("text/plain", task.id);
        e.dataTransfer.effectAllowed = "move";
        beginDrag({ kind: "task", taskId: task.id, sessId: null, srcTaskId: null });
      }}
      onDragEnd={endDrag}
      onDragOver={(e) => {
        if (getDrag().kind !== "session") return; // task drags fall through to the column
        e.preventDefault();
        e.stopPropagation();
        if (overTaskId !== task.id) setOver({ task: task.id, tray: false });
      }}
      onDragLeave={(e) => {
        if (!e.currentTarget.contains(e.relatedTarget as Node | null) && overTaskId === task.id) setOver({ task: null });
      }}
      onDrop={(e) => {
        const d = getDrag();
        if (d.kind !== "session" || !d.sessId) return;
        e.preventDefault();
        e.stopPropagation();
        moveSession(d.sessId, task.id);
        endDrag();
      }}
      onClick={() => toggle(task.id)}
    >
      <div className={styles.taskTopRow}>
        {tool && (
          <span className={styles.toolBadge} style={{ color: toolColorVar(tool) }}>
            {tool}
          </span>
        )}
        {sessions.length > 0 && <span className={styles.taskProject}>{project}</span>}
        <span className={styles.spacer} />
        {roll.needsYou && <span className={styles.needsPill}>● Needs you</span>}
        <button
          className={`${styles.iconButton} ${styles.iconDelete} ${styles.taskDelete}`}
          title="Delete task (its sessions return to the tray)"
          aria-label="Delete task"
          data-testid="delete-task"
          onClick={(e) => {
            e.stopPropagation();
            void deleteTask(task.id);
          }}
        >
          ✕
        </button>
      </div>
      <div className={styles.taskTitle}>{task.title}</div>

      {roll.total > 0 ? (
        <>
          <div className={styles.rollRow}>
            <div className={styles.rollTrack}>
              <div
                className={styles.rollFill}
                style={{ width: `${roll.pct}%`, background: task.stage === "done" ? "#34d399" : "#38bdf8" }}
              />
            </div>
            <span className={styles.rollLabel}>
              {roll.done}/{roll.total} done
            </span>
          </div>
          <div className={`${styles.sessionList} ${collapsed ? styles.sessionListCollapsed : ""}`}>
            <div className={styles.sessionListInner}>
              {sessions.map((s) => (
                <SessionRow key={s.id} session={s} taskId={task.id} />
              ))}
            </div>
          </div>
          <div className={styles.taskFooter}>
            <span className={styles.footerLabel}>
              {roll.total} {roll.total === 1 ? "session" : "sessions"}
            </span>
            <span className={styles.spacer} />
            <span className={styles.footerTokens}>{fmtTokens(roll.tokens)}</span>
            <span className={styles.footerCost}>{fmtCost(roll.cost)}</span>
          </div>
        </>
      ) : (
        <div className={styles.noSessions}>No sessions yet</div>
      )}
    </div>
  );
}
