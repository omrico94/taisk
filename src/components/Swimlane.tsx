import type { SessionView } from "../types";
import { useSessionStore } from "../store/sessionStore";
import { recategorizeSession } from "../api";
import { SessionCard } from "./SessionCard";
import styles from "./Swimlane.module.css";

interface Props {
  name: string;
  sessions: SessionView[];
  nowMs: number;
  onSelect: (id: string) => void;
}

export function Swimlane({ name, sessions, nowMs, onSelect }: Props) {
  const collapsed = useSessionStore((s) => s.collapsedCategories.has(name));
  const toggleCollapsed = useSessionStore((s) => s.toggleCategoryCollapsed);
  const dragOverCategory = useSessionStore((s) => s.dragOverCategory);
  const setDragOverCategory = useSessionStore((s) => s.setDragOverCategory);
  const hot = dragOverCategory === name;
  const isEmpty = sessions.length === 0;

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
          if (e.key === "Enter" || e.key === " ") {
            e.preventDefault();
            toggleCollapsed(name);
          }
        }}
      >
        <div className={styles.nameRow}>
          <span className={`${styles.chevron} ${collapsed ? styles.chevronCollapsed : ""}`}>▾</span>
          <div className={styles.name}>{name}</div>
        </div>
        <div className={styles.count}>{sessions.length} live</div>
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
