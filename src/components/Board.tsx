import { useRef } from "react";
import { useShallow } from "zustand/react/shallow";
import { useBoardSessions, useBoardTasks, useSessionStore } from "../store/sessionStore";
import { STAGES, matchesQuery, orphanSessions, sessionsOfTask } from "../store/selectors";
import { Column } from "./Column";
import { UnassignedTray } from "./UnassignedTray";
import { useFlip } from "./useFlip";
import styles from "./Kanban.module.css";

export function Board() {
  const sessions = useBoardSessions();
  const tasks = useBoardTasks();
  const assignments = useSessionStore(useShallow((s) => s.assignments));
  const query = useSessionStore((s) => s.query);
  const hideIdle = useSessionStore((s) => s.hideIdle);
  const drag = useSessionStore((s) => s.drag);
  const openAskWithQuery = useSessionStore((s) => s.openAskWithQuery);

  const rootRef = useRef<HTMLDivElement>(null);
  useFlip(rootRef);

  const hasQuery = query.trim() !== "";

  // A query narrows the board: a task stays if its title or any of its
  // sessions match; the tray shows only matching orphans.
  const visibleTasks = hasQuery
    ? tasks.filter(
        (t) =>
          t.title.toLowerCase().includes(query.toLowerCase()) ||
          sessionsOfTask(t.id, sessions, assignments).some((s) => matchesQuery(s, query)),
      )
    : tasks;
  const orphans = orphanSessions(sessions, tasks, assignments)
    .filter((s) => !(hideIdle && s.state === "Idle"))
    .filter((s) => matchesQuery(s, query));

  // The tray is hidden when empty — except mid-drag of a session, so there's
  // always somewhere to drop it to unassign.
  // When there are no orphans it floats over the top of the columns instead of
  // pushing them down, so nothing shifts under the cursor mid-drag.
  const floatingTray = orphans.length === 0 && drag.kind === "session";
  const showTray = orphans.length > 0 || floatingTray;
  const noResults = hasQuery && visibleTasks.length === 0 && orphans.length === 0;

  return (
    <div className={styles.board} ref={rootRef}>
      {showTray && <UnassignedTray sessions={orphans} floating={floatingTray} />}
      {noResults && (
        <div className={styles.noResults}>
          <div>No tasks or sessions match &ldquo;{query}&rdquo;.</div>
          <button className={styles.noResultsButton} onClick={() => openAskWithQuery(query)}>
            ✦ Search your full history instead
          </button>
        </div>
      )}
      <div className={styles.columns}>
        {STAGES.map((st) => (
          <Column
            key={st.key}
            stage={st.key}
            label={st.label}
            accent={st.accent}
            tasks={visibleTasks.filter((t) => t.stage === st.key)}
            sessions={sessions}
            assignments={assignments}
          />
        ))}
      </div>
    </div>
  );
}
