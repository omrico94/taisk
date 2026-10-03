import { useEffect, useState } from "react";
import { getInferenceBackend, setAutoTask, type InferenceBackend } from "../api";
import { useAutoTaskOn, useBoardColumns, useBoardSessions, useBoardTasks, useSessionStore } from "../store/sessionStore";
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
  const ticketsOpen = useSessionStore((s) => s.ticketsOpen);
  const setTicketsOpen = useSessionStore((s) => s.setTicketsOpen);
  const activeBoardId = useSessionStore((s) => s.activeBoardId);
  const autoTask = useAutoTaskOn();
  const layout = useBoardColumns();
  // Auto-task files into the board's active column, else its intake column
  // (same fallback as `TaskStore::auto_task_for_session`).
  const autoColumnId = layout.active ?? layout.intake ?? layout.columns[0]?.id;
  const autoColumn = layout.columns.find((c) => c.id === autoColumnId)?.name ?? "In Progress";
  const autoTaskTip = autoTask
    ? `On: each new session becomes its own task in ${autoColumn}. Click to send new sessions to Unassigned instead.`
    : `Off: new sessions land in Unassigned. Click to make each new session its own task in ${autoColumn}.`;

  const workingCount = sessions.filter((s) => s.state === "Working").length;
  const waitingCount = sessions.filter((s) => s.state === "Waiting").length;
  // Only orphan idle sessions are hidden (see Board), so only those are counted.
  const assignments = useSessionStore(useShallow((s) => s.assignments));
  const tasks = useBoardTasks();
  const known = new Set(tasks.map((t) => t.id));
  const [backend, setBackend] = useState<InferenceBackend | null>(null);
  useEffect(() => {
    getInferenceBackend().then(setBackend, () => {});
  }, []);
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
      <button
        className={`${styles.idleToggle} ${autoTask ? styles.idleToggleActive : ""}`}
        onClick={() => setAutoTask(activeBoardId, !autoTask).catch((e) => console.error("auto-task toggle failed", e))}
        title={autoTaskTip}
        aria-pressed={autoTask}
        data-testid="auto-task-toggle"
      >
        Auto-task {autoTask ? "on" : "off"}
      </button>
      <button
        className={`${styles.idleToggle} ${ticketsOpen ? styles.idleToggleActive : ""}`}
        onClick={() => setTicketsOpen(!ticketsOpen)}
        title="Import tickets from GitHub and other trackers"
        data-testid="tickets-button"
      >
        Tickets
      </button>
      <StatusPill workingCount={workingCount} waitingCount={waitingCount} />
      <span className={styles.caption}>
        100% local{backend && ` · ${backend === "native" ? "Claude-native" : "Ollama"} + LanceDB`}
      </span>
    </div>
  );
}
