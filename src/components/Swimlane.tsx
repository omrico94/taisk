import { useState } from "react";
import type { SessionView } from "../types";
import { useSessionStore } from "../store/sessionStore";
import { deleteCategory, recategorizeSession } from "../api";
import { SessionCard } from "./SessionCard";
import styles from "./Swimlane.module.css";

interface Props {
  name: string;
  sessions: SessionView[];
  nowMs: number;
  onSelect: (id: string) => void;
}

// "Uncategorized" is a transient placeholder every new session starts in
// before categorization resolves (`SessionView::new_uncategorized`) — never
// created via `POST /categories`, so it isn't a real user lane to offer
// deleting.
const PROTECTED_CATEGORY = "Uncategorized";

export function Swimlane({ name, sessions, nowMs, onSelect }: Props) {
  const collapsed = useSessionStore((s) => s.collapsedCategories.has(name));
  const toggleCollapsed = useSessionStore((s) => s.toggleCategoryCollapsed);
  const dragOverCategory = useSessionStore((s) => s.dragOverCategory);
  const setDragOverCategory = useSessionStore((s) => s.setDragOverCategory);
  const removeCategory = useSessionStore((s) => s.removeCategory);
  const hot = dragOverCategory === name;
  const isEmpty = sessions.length === 0;
  // Arm/confirm instead of window.confirm() — Tauri's WebView doesn't
  // reliably show native dialogs (confirmed live with the per-session
  // delete button: it silently returned false without ever prompting).
  const [confirmingDelete, setConfirmingDelete] = useState(false);

  const armDelete = (e: React.MouseEvent) => {
    e.stopPropagation();
    setConfirmingDelete(true);
  };
  const cancelDelete = (e: React.MouseEvent) => {
    e.stopPropagation();
    setConfirmingDelete(false);
  };
  const confirmDelete = (e: React.MouseEvent) => {
    e.stopPropagation();
    deleteCategory(name);
    removeCategory(name);
    setConfirmingDelete(false);
  };

  return (
    <div
      className={`${styles.lane} ${hot ? styles.laneHot : ""}`}
      onDragOver={(e) => {
        // Native DnD only fires this while a real drag is in progress — no
        // separate "is anything being dragged" flag needed.
        e.preventDefault();
        if (dragOverCategory !== name) setDragOverCategory(name);
      }}
      onDrop={(e) => {
        e.preventDefault();
        const id = e.dataTransfer.getData("text/plain");
        if (id) recategorizeSession(id, name);
        setDragOverCategory(null);
      }}
    >
      <div
        className={styles.labelCol}
        onClick={() => toggleCollapsed(name)}
        role="button"
        tabIndex={0}
        onKeyDown={(e) => {
          // Ignore keydowns bubbling up from a nested interactive element
          // (the delete/cancel/confirm buttons) — only the label column's
          // own focus should toggle collapse.
          if (e.target !== e.currentTarget) return;
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            toggleCollapsed(name);
          }
        }}
      >
        <div className={styles.nameRow}>
          <span className={`${styles.chevron} ${collapsed ? styles.chevronCollapsed : ""}`}>▾</span>
          <div className={styles.name}>{name}</div>
          {name !== PROTECTED_CATEGORY && !confirmingDelete && (
            <button
              className={styles.deleteCategoryButton}
              onClick={armDelete}
              title={`Delete category "${name}"`}
            >
              🗑
            </button>
          )}
        </div>
        {confirmingDelete ? (
          <div className={styles.deleteConfirmRow}>
            <button className={styles.deleteConfirmButton} onClick={confirmDelete}>
              {sessions.length > 0 ? `Delete all ${sessions.length}` : "Delete"}
            </button>
            <button className={styles.deleteCancelButton} onClick={cancelDelete}>
              Cancel
            </button>
          </div>
        ) : (
          <div className={styles.count}>{sessions.length} live</div>
        )}
      </div>
      {!collapsed &&
        (isEmpty ? (
          <div className={`${styles.emptyHint} ${hot ? styles.emptyHintHot : ""}`}>Drop a session here</div>
        ) : (
          <div className={styles.cards}>
            {sessions.map((s) => (
              <SessionCard key={s.id} session={s} nowMs={nowMs} onSelect={onSelect} />
            ))}
          </div>
        ))}
    </div>
  );
}
