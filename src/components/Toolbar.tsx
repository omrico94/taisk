import { useBoardSessions, useBoardTasks, useSessionStore } from "../store/sessionStore";
import { useShallow } from "zustand/react/shallow";
import { SearchField } from "./SearchField";
import { AskMemoryButton } from "./AskMemoryButton";
import { StatusPill } from "./StatusPill";
import { Logo } from "./Logo";
import styles from "./Toolbar.module.css";

export function Toolbar() {
  const query = useSessionStore((s) => s.query);
  const setQuery = useSessionStore((s) => s.setQuery);
  const setAskOpen = useSessionStore((s) => s.setAskOpen);
  // Counts reflect the board being viewed; other boards' counts sit on their tabs.
  const sessions = useBoardSessions();
  const hideIdle = useSessionStore((s) => s.hideIdle);
  const toggleHideIdle = useSessionStore((s) => s.toggleHideIdle);

  const workingCount = sessions.filter((s) => s.state === "Working").length;
  const waitingCount = sessions.filter((s) => s.state === "Waiting").length;
  // Only orphan idle sessions are hidden (see Board), so only those are counted.
  const assignments = useSessionStore(useShallow((s) => s.assignments));
  const tasks = useBoardTasks();
  const known = new Set(tasks.map((t) => t.id));
  const idleCount = sessions.filter((s) => s.state === "Idle" && !(assignments[s.id] && known.has(assignments[s.id]))).length;

  return (
    <div className={styles.toolbar}>
      <Logo />
      <SearchField value={query} onChange={setQuery} />
      <AskMemoryButton onClick={() => setAskOpen(true)} />
      <span className={styles.spacer} />
      {idleCount > 0 && (
        <button
          className={`${styles.idleToggle} ${hideIdle ? "" : styles.idleToggleActive}`}
          onClick={toggleHideIdle}
          title={hideIdle ? "Show idle sessions" : "Hide idle sessions"}
        >
          {hideIdle ? `${idleCount} idle hidden` : `Hide ${idleCount} idle`}
        </button>
      )}
      <StatusPill workingCount={workingCount} waitingCount={waitingCount} />
      <span className={styles.caption}>100% local · Ollama + LanceDB</span>
    </div>
  );
}
