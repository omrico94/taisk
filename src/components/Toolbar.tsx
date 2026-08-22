import { useSessionStore } from "../store/sessionStore";
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
  // Object.values(...) returns a new array every call — without useShallow,
  // useSyncExternalStore sees a "changed" snapshot on every render and loops.
  const sessions = useSessionStore(useShallow((s) => Object.values(s.sessions)));
  const hideIdle = useSessionStore((s) => s.hideIdle);
  const toggleHideIdle = useSessionStore((s) => s.toggleHideIdle);

  const workingCount = sessions.filter((s) => s.state === "Working").length;
  const waitingCount = sessions.filter((s) => s.state === "Waiting").length;
  const idleCount = sessions.filter((s) => s.state === "Idle").length;

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
      <span className={styles.caption}>100% local · Ollama + LanceDB on-device</span>
    </div>
  );
}
