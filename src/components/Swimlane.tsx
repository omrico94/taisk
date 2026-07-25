import type { SessionView } from "../types";
import { useSessionStore } from "../store/sessionStore";
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

  return (
    <div className={styles.lane}>
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
      {!collapsed && (
        <div className={styles.cards}>
          {sessions.map((s) => (
            <SessionCard key={s.id} session={s} nowMs={nowMs} onSelect={onSelect} />
          ))}
        </div>
      )}
    </div>
  );
}
