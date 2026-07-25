import { useSessionStore } from "../store/sessionStore";
import { useShallow } from "zustand/react/shallow";
import { groupByCategory, matchesQuery } from "../store/selectors";
import { Swimlane } from "./Swimlane";
import styles from "./Board.module.css";

interface Props {
  nowMs: number;
}

export function Board({ nowMs }: Props) {
  const sessions = useSessionStore(useShallow((s) => Object.values(s.sessions)));
  const query = useSessionStore((s) => s.query);
  const selectCard = useSessionStore((s) => s.selectCard);
  const openAskWithQuery = useSessionStore((s) => s.openAskWithQuery);

  const filtered = sessions.filter((s) => matchesQuery(s, query));
  const groups = groupByCategory(filtered);
  const noResults = query.trim() !== "" && groups.length === 0;

  return (
    <div className={styles.board}>
      {groups.length > 0 && (
        <div className={styles.lanes}>
          {groups.map((g) => (
            <Swimlane key={g.name} name={g.name} sessions={g.sessions} nowMs={nowMs} onSelect={selectCard} />
          ))}
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
