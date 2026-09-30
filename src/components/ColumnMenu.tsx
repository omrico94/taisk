import { useEffect, useState } from "react";
import { createPortal } from "react-dom";
import { addColumn, deleteColumn, reorderColumns, setColumnRoles, updateColumn } from "../api";
import { useSessionStore } from "../store/sessionStore";
import { COLUMN_PALETTE } from "../store/selectors";
import type { BoardColumns, Column, ColumnRole } from "../types";
import { moveColumn } from "./boardActions";
import styles from "./Kanban.module.css";

const MENU_W = 250;

const ROLES: { role: ColumnRole; label: string; hint: string }[] = [
  { role: "done", label: "Finished column", hint: "Cards here are shown as done (dimmed)" },
  { role: "active", label: "Highlighted column", hint: "Gets the bright accent, like In Progress" },
  { role: "intake", label: "Quick-add column", hint: "⌥⌘N files new tasks here" },
];

interface Props {
  column: Column;
  layout: BoardColumns;
  taskCount: number;
  /** Screen point the menu's top-right corner is anchored to. */
  anchor: { x: number; y: number };
  onClose: () => void;
  onRename: () => void;
}

/** A column's ⋯ menu: rename, color, reorder, roles, delete.
 * `position: fixed` in a portal so it escapes the board's overflow clipping;
 * closes on any outside click or Escape (same pattern as `AssignMenu`). */
export function ColumnMenu({ column, layout, taskCount, anchor, onClose, onRename }: Props) {
  const board = useSessionStore((s) => s.activeBoardId);
  const others = layout.columns.filter((c) => c.id !== column.id);
  const index = layout.columns.findIndex((c) => c.id === column.id);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [moveTo, setMoveTo] = useState(others[Math.max(0, index - 1)]?.id ?? "");
  const [addSide, setAddSide] = useState<"left" | "right" | null>(null);
  const [newName, setNewName] = useState("");
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    // Deferred a tick so the click that opened the menu doesn't close it.
    const t = setTimeout(() => document.addEventListener("click", onClose), 0);
    document.addEventListener("keydown", onKey);
    return () => {
      clearTimeout(t);
      document.removeEventListener("click", onClose);
      document.removeEventListener("keydown", onKey);
    };
  }, [onClose]);

  const run = (p: Promise<unknown>, close = true) =>
    p.then(() => close && onClose()).catch((err: Error) => setError(err.message));

  const move = (delta: number) => {
    moveColumn(column.id, index + delta);
    onClose();
  };

  // New column is created (appended at the end by the backend), then moved
  // next to this one — same two-step dance as a drag-reorder.
  const commitAdd = () => {
    const n = newName.trim();
    if (!n) return setAddSide(null);
    addColumn(board, n)
      .then((created) => {
        const ids = layout.columns.map((c) => c.id);
        ids.splice(addSide === "left" ? index : index + 1, 0, created.id);
        return reorderColumns(board, ids);
      })
      .then(() => {
        setAddSide(null);
        setNewName("");
        onClose();
      })
      .catch((err: Error) => setError(err.message));
  };

  const left = Math.max(8, Math.min(anchor.x - MENU_W, window.innerWidth - MENU_W - 8));

  return createPortal(
    <div
      className={`${styles.assignMenu} ${styles.columnMenu}`}
      role="menu"
      data-testid="column-menu"
      style={{ left, top: anchor.y, width: MENU_W }}
      onClick={(e) => e.stopPropagation()}
    >
      {addSide ? (
        <>
          <div className={styles.assignHeader}>
            Add a column to the {addSide} of “{column.name}”
          </div>
          <div className={styles.addRow}>
            <input
              autoFocus
              className={styles.addInput}
              value={newName}
              maxLength={40}
              onChange={(e) => setNewName(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") commitAdd();
                else if (e.key === "Escape") setAddSide(null);
              }}
              placeholder="Column name…"
              aria-label="New column name"
            />
            <button className={styles.addCommit} onClick={commitAdd}>
              Add
            </button>
            <button className={styles.addCancel} onClick={() => setAddSide(null)}>
              Esc
            </button>
          </div>
        </>
      ) : confirmDelete ? (
        <>
          <div className={styles.assignHeader}>Delete “{column.name}”?</div>
          {taskCount > 0 && (
            <label className={styles.menuField}>
              <span>
                Move {taskCount} task{taskCount === 1 ? "" : "s"} to
              </span>
              <select className={styles.menuSelect} value={moveTo} onChange={(e) => setMoveTo(e.target.value)} aria-label="Move tasks to">
                {others.map((c) => (
                  <option key={c.id} value={c.id}>
                    {c.name}
                  </option>
                ))}
              </select>
            </label>
          )}
          <div className={styles.menuActions}>
            <button className={styles.addCancel} onClick={() => setConfirmDelete(false)}>
              Cancel
            </button>
            <button
              className={styles.menuDanger}
              data-testid="confirm-delete-column"
              onClick={() => run(deleteColumn(board, column.id, moveTo))}
            >
              Delete column
            </button>
          </div>
        </>
      ) : (
        <>
          <div className={styles.assignHeader}>{column.name}</div>
          <button
            role="menuitem"
            className={styles.menuItem}
            onClick={() => {
              onRename();
              onClose();
            }}
          >
            Rename
          </button>

          <div className={styles.swatches} role="group" aria-label="Column color">
            {COLUMN_PALETTE.map((c) => (
              <button
                key={c}
                className={`${styles.swatch} ${c.toUpperCase() === column.color.toUpperCase() ? styles.swatchOn : ""}`}
                style={{ background: c }}
                aria-label={`Color ${c}`}
                onClick={() => run(updateColumn(board, column.id, { color: c }), false)}
              />
            ))}
          </div>


          <div className={styles.menuRow}>
            <button className={styles.menuItem} disabled={index <= 0} onClick={() => move(-1)}>
              ← Move left
            </button>
            <button className={styles.menuItem} disabled={index >= layout.columns.length - 1} onClick={() => move(1)}>
              Move right →
            </button>
          </div>
          <div className={styles.menuRow}>
            <button className={styles.menuItem} data-testid="add-left" onClick={() => setAddSide("left")}>
              + Add left
            </button>
            <button className={styles.menuItem} data-testid="add-right" onClick={() => setAddSide("right")}>
              + Add right
            </button>
          </div>

          <div className={styles.menuDivider} />
          {ROLES.map(({ role, label, hint }) => {
            const on = layout[role] === column.id;
            return (
              <button
                key={role}
                role="menuitemcheckbox"
                aria-checked={on}
                className={styles.menuItem}
                title={hint}
                data-testid={`role-${role}`}
                onClick={() => run(setColumnRoles(board, { [role]: on ? null : column.id }), false)}
              >
                <span className={styles.menuCheck}>{on ? "✓" : ""}</span>
                {label}
              </button>
            );
          })}

          <div className={styles.menuDivider} />
          <button
            role="menuitem"
            className={`${styles.menuItem} ${styles.menuItemDanger}`}
            disabled={others.length === 0}
            title={others.length === 0 ? "A board needs at least one column" : undefined}
            onClick={() => setConfirmDelete(true)}
          >
            Delete column…
          </button>
        </>
      )}
      {error && <div className={styles.columnError}>{error}</div>}
    </div>,
    document.body,
  );
}
