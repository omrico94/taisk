import { useCallback, useState } from "react";
import { addColumn, createTask, updateColumn } from "../api";
import { useSessionStore } from "../store/sessionStore";
import { sessionsOfTask } from "../store/selectors";
import type { BoardColumns, Column as ColumnDef, SessionView, Task } from "../types";
import { beginDrag, dropTicket, endDrag, getDrag, moveColumn, moveTask } from "./boardActions";
import { ColumnMenu } from "./ColumnMenu";
import { TaskCard } from "./TaskCard";
import styles from "./Kanban.module.css";

interface Props {
  column: ColumnDef;
  layout: BoardColumns;
  tasks: Task[];
  sessions: SessionView[];
  assignments: Record<string, string>;
}

export function Column({ column, layout, tasks, sessions, assignments }: Props) {
  const drag = useSessionStore((s) => s.drag);
  const overCol = useSessionStore((s) => s.overCol);
  const setOver = useSessionStore((s) => s.setOver);

  const [adding, setAdding] = useState(false);
  const [title, setTitle] = useState("");
  const [renaming, setRenaming] = useState(false);
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null);
  const closeMenu = useCallback(() => setMenu(null), []);

  const { id, name, color: accent } = column;
  const board = () => useSessionStore.getState().activeBoardId;
  // The active-role column (In Progress by default) gets the stronger accent.
  const strong = layout.active === id;
  const hot = (drag.kind === "task" || drag.kind === "ticket") && overCol === id;
  const reorderTarget = drag.kind === "column" && overCol === id && drag.colId !== id;

  const cancel = () => {
    setAdding(false);
    setTitle("");
  };
  const commit = () => {
    const t = title.trim();
    if (t) createTask(t, id, board()).catch((err) => console.error("Failed to create task:", err));
    cancel(); // empty submissions are discarded
  };
  const rename = (next: string) => {
    setRenaming(false);
    const n = next.trim();
    if (n && n !== name) updateColumn(board(), id, { name: n }).catch((err) => console.error("Failed to rename column:", err));
  };

  return (
    <div
      className={`${styles.column} ${reorderTarget ? styles.columnReorderTarget : ""}`}
      data-testid={`column-${id}`}
      style={{ opacity: drag.kind === "column" && drag.colId === id ? 0.4 : undefined }}
      // Column reorder. Task drops are handled (and stopped) by the body below.
      onDragOver={(e) => {
        if (getDrag().kind !== "column") return;
        e.preventDefault();
        if (overCol !== id) setOver({ col: id });
      }}
      onDrop={(e) => {
        const d = getDrag();
        if (d.kind !== "column" || !d.colId) return;
        e.preventDefault();
        moveColumn(d.colId, layout.columns.findIndex((c) => c.id === id));
        endDrag();
      }}
    >
      <div
        className={styles.columnHeader}
        style={{ borderBottom: `2px solid ${accent}${strong ? "66" : "44"}` }}
        draggable={!renaming}
        onDragStart={(e) => {
          e.dataTransfer.setData("text/plain", id);
          e.dataTransfer.effectAllowed = "move";
          beginDrag({ kind: "column", colId: id, taskId: null, sessId: null, srcTaskId: null });
        }}
        onDragEnd={endDrag}
      >
        <span className={styles.columnDot} style={{ background: accent, boxShadow: strong ? `0 0 9px ${accent}99` : undefined }} />
        {renaming ? (
          <input
            autoFocus
            className={styles.columnRename}
            defaultValue={name}
            aria-label="Column name"
            maxLength={40}
            onFocus={(e) => e.currentTarget.select()}
            onBlur={(e) => rename(e.currentTarget.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") e.currentTarget.blur();
              else if (e.key === "Escape") setRenaming(false);
            }}
          />
        ) : (
          <span
            className={styles.columnLabel}
            style={{ color: accent }}
            title="Double-click to rename"
            onDoubleClick={() => setRenaming(true)}
            data-testid={`column-label-${id}`}
          >
            {name}
          </span>
        )}
        <span className={styles.spacer} />
        <span className={styles.columnCount} data-testid={`count-${id}`}>
          {tasks.length}
        </span>
        <button
          className={styles.columnMenuButton}
          aria-label={`${name} column options`}
          data-testid={`column-menu-${id}`}
          // No stopPropagation: the same click closes any other column's open menu.
          onClick={(e) => {
            const r = e.currentTarget.getBoundingClientRect();
            setMenu(menu ? null : { x: r.right, y: r.bottom + 4 });
          }}
        >
          ⋯
        </button>
      </div>
      {menu && (
        <ColumnMenu
          column={column}
          layout={layout}
          taskCount={tasks.length}
          anchor={menu}
          onClose={closeMenu}
          onRename={() => setRenaming(true)}
        />
      )}

      <div
        className={`${styles.columnBody} ${hot ? styles.columnHot : ""}`}
        data-testid={`column-body-${id}`}
        onDragOver={(e) => {
          const kind = getDrag().kind;
          if (kind !== "task" && kind !== "ticket") return;
          e.preventDefault();
          if (overCol !== id) setOver({ col: id });
        }}
        onDragLeave={(e) => {
          if (!e.currentTarget.contains(e.relatedTarget as Node | null) && overCol === id) setOver({ col: null });
        }}
        onDrop={(e) => {
          const d = getDrag();
          if (d.kind === "ticket" && d.ticket) {
            e.preventDefault();
            e.stopPropagation();
            dropTicket(d.ticket, id);
            endDrag();
            return;
          }
          if (d.kind !== "task" || !d.taskId) return;
          e.preventDefault();
          e.stopPropagation();
          moveTask(d.taskId, id);
          endDrag();
        }}
      >
        {tasks.map((t) => (
          <TaskCard key={t.id} task={t} sessions={sessionsOfTask(t.id, sessions, assignments)} done={layout.done === id} />
        ))}
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
    </div>
  );
}

/** Ghost column at the end of the board: "+ Add column" → name → Enter. */
export function AddColumn() {
  const [adding, setAdding] = useState(false);
  const [name, setName] = useState("");
  const [error, setError] = useState<string | null>(null);

  const cancel = () => {
    setAdding(false);
    setName("");
    setError(null);
  };
  const commit = () => {
    const n = name.trim();
    if (!n) return cancel();
    addColumn(useSessionStore.getState().activeBoardId, n)
      .then(cancel)
      .catch((err: Error) => setError(err.message));
  };

  return (
    <div className={`${styles.addColumn} ${adding ? styles.addColumnOpen : ""}`} data-testid="add-column">
      {adding ? (
        <>
          <div className={styles.addRow}>
            <input
              autoFocus
              className={styles.addInput}
              value={name}
              maxLength={40}
              onChange={(e) => setName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") commit();
                else if (e.key === "Escape") cancel();
              }}
              placeholder="Column name…"
              aria-label="New column name"
            />
            <button className={styles.addCommit} onClick={commit}>
              Add
            </button>
            <button className={styles.addCancel} onClick={cancel}>
              Esc
            </button>
          </div>
          {error && <div className={styles.columnError}>{error}</div>}
        </>
      ) : (
        <button className={styles.addButton} onClick={() => setAdding(true)}>
          + Add column
        </button>
      )}
    </div>
  );
}
