import { useState } from "react";
import { useSessionStore } from "../store/sessionStore";
import { useShallow } from "zustand/react/shallow";
import { createCategory } from "../api";
import { groupByCategory, matchesQuery } from "../store/selectors";
import { Swimlane } from "./Swimlane";
import styles from "./Board.module.css";

interface Props {
  nowMs: number;
}

export function Board({ nowMs }: Props) {
  const sessions = useSessionStore(useShallow((s) => Object.values(s.sessions)));
  const categories = useSessionStore(useShallow((s) => s.categories));
  const query = useSessionStore((s) => s.query);
  const selectCard = useSessionStore((s) => s.selectCard);
  const openAskWithQuery = useSessionStore((s) => s.openAskWithQuery);
  const hideIdle = useSessionStore((s) => s.hideIdle);

  const hasQuery = query.trim() !== "";
  const visible = hideIdle ? sessions.filter((s) => s.state !== "Idle") : sessions;
  const filtered = visible.filter((s) => matchesQuery(s, query));
  const allGroups = groupByCategory(filtered, categories);
  // Empty lanes stay visible so there's always somewhere to drag a session
  // into — but hide during an active search, matching the design's
  // `groupByCatDeep` (a query narrows the board down to actual matches).
  const groups = hasQuery ? allGroups.filter((g) => g.sessions.length > 0) : allGroups;
  const noResults = hasQuery && groups.length === 0;

  return (
    <div className={styles.board}>
      {(groups.length > 0 || !hasQuery) && (
        <div className={styles.lanes}>
          {groups.map((g) => (
            <Swimlane key={g.name} name={g.name} sessions={g.sessions} nowMs={nowMs} onSelect={selectCard} />
          ))}
          {!hasQuery && <AddCategoryRow />}
        </div>
      )}
      {noResults && (
        <div className={styles.empty}>
          <div className={styles.emptyMessage}>No live sessions match &ldquo;{query}&rdquo;.</div>
          <button className={styles.emptyButton} onClick={() => openAskWithQuery(query)}>
            ✦ Search your full history instead
          </button>
        </div>
      )}
    </div>
  );
}

// Phase 2 §5: the dashed "+ New category" control below the last lane.
// Inline input, Enter commits, Esc cancels — a brand-new category has no
// session to seed from, so `MemoryRepo::create_category` embeds the name
// itself as a placeholder exemplar.
function AddCategoryRow() {
  const addCategory = useSessionStore((s) => s.addCategory);
  const [adding, setAdding] = useState(false);
  const [name, setName] = useState("");

  const cancel = () => {
    setAdding(false);
    setName("");
  };

  const commit = () => {
    const trimmed = name.trim();
    if (!trimmed) {
      cancel();
      return;
    }
    createCategory(trimmed)
      .then(() => addCategory(trimmed))
      .catch((err) => console.error("Failed to create category:", err));
    cancel();
  };

  if (adding) {
    return (
      <div className={styles.addCategoryRow}>
        <div className={styles.addCategorySpacer} />
        <div className={styles.addCategoryInputWrap}>
          <span className={styles.addCategoryPlus}>+</span>
          <input
            autoFocus
            className={styles.addCategoryInput}
            value={name}
            onChange={(e) => setName(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") commit();
              else if (e.key === "Escape") cancel();
            }}
            placeholder="Category name…"
          />
          <button className={styles.addCategoryCommit} onClick={commit}>
            Add
          </button>
          <button className={styles.addCategoryCancel} onClick={cancel}>
            Esc
          </button>
        </div>
      </div>
    );
  }

  return (
    <div className={styles.addCategoryRow}>
      <div className={styles.addCategorySpacer} />
      <button className={styles.addCategoryButton} onClick={() => setAdding(true)}>
        <span className={styles.addCategoryButtonPlus}>+</span> New category
      </button>
    </div>
  );
}
