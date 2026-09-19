import { useState } from "react";
import { createTask } from "../api";
import { useSessionStore } from "../store/sessionStore";
import { WIP_LIMIT, sessionsOfTask } from "../store/selectors";
import type { SessionView, Stage, Task } from "../types";
import { moveTask } from "./boardActions";
import { TaskCard } from "./TaskCard";
import styles from "./Kanban.module.css";

interface Props {
  stage: Stage;
  label: string;
  accent: string;
  tasks: Task[];
  sessions: SessionView[];
  assignments: Record<string, string>;
}

export function Column({ stage, label, accent, tasks, sessions, assignments }: Props) {
  const drag = useSessionStore((s) => s.drag);
  const overCol = useSessionStore((s) => s.overCol);
  const setOver = useSessionStore((s) => s.setOver);
  const clearDrag = useSessionStore((s) => s.clearDrag);

  const [adding, setAdding] = useState(false);
  const [title, setTitle] = useState("");

  const hot = drag.kind === "task" && overCol === stage;
  const overLimit = stage === "inprogress" && tasks.length > WIP_LIMIT;

  const cancel = () => {
    setAdding(false);
    setTitle("");
  };
  const commit = () => {
    const t = title.trim();
    if (t) createTask(t, stage).catch((err) => console.error("Failed to create task:", err));
    cancel(); // empty submissions are discarded
  };

  return (
    <div className={styles.column} data-testid={`column-${stage}`}>
      <div className={styles.columnHeader} style={{ borderBottom: `2px solid ${accent}44` }}>
        <span
          className={styles.columnDot}
          style={{ background: accent, boxShadow: stage === "inprogress" ? `0 0 9px ${accent}99` : undefined }}
        />
        <span className={styles.columnLabel} style={{ color: accent }}>
          {label}
        </span>
        <span className={styles.spacer} />
        <span className={styles.columnCount} data-testid={`count-${stage}`}>
          {tasks.length}
        </span>
        {stage === "inprogress" && (
          <span className={`${styles.wipBadge} ${overLimit ? styles.wipOver : ""}`} data-testid="wip-badge">
            WIP {tasks.length}/{WIP_LIMIT}
          </span>
        )}
      </div>

      <div
        className={`${styles.columnBody} ${hot ? styles.columnHot : ""}`}
        onDragOver={(e) => {
          if (drag.kind !== "task") return;
          e.preventDefault();
          if (overCol !== stage) setOver({ col: stage });
        }}
        onDragLeave={(e) => {
          if (!e.currentTarget.contains(e.relatedTarget as Node | null) && overCol === stage) setOver({ col: null });
        }}
        onDrop={(e) => {
          if (drag.kind !== "task" || !drag.taskId) return;
          e.preventDefault();
          moveTask(drag.taskId, stage);
          clearDrag();
        }}
      >
        {tasks.map((t) => (
          <TaskCard key={t.id} task={t} sessions={sessionsOfTask(t.id, sessions, assignments)} />
        ))}
      </div>

      <div className={styles.columnFooter}>
        {adding ? (
          <div className={styles.addRow}>
            <input
              autoFocus
              className={styles.addInput}
              value={title}
              onChange={(e) => setTitle(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") commit();
                else if (e.key === "Escape") cancel();
              }}
              placeholder="Task title…"
              aria-label="New task title"
            />
            <button className={styles.addCommit} onClick={commit}>
              Add
            </button>
            <button className={styles.addCancel} onClick={cancel}>
              Esc
            </button>
          </div>
        ) : (
          <button className={styles.addButton} onClick={() => setAdding(true)}>
            + Add task
          </button>
        )}
      </div>
    </div>
  );
}
