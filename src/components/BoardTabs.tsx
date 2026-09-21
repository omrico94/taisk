import { useShallow } from "zustand/react/shallow";
import { useSessionStore } from "../store/sessionStore";
import styles from "./BoardTabs.module.css";

// One tab per board (Claude account). Counts come from the live session map,
// which holds every board's sessions, so a background board can still flag
// that something on it is waiting for you.
export function BoardTabs() {
  const boards = useSessionStore(useShallow((s) => s.boards));
  const activeBoardId = useSessionStore((s) => s.activeBoardId);
  const setActiveBoard = useSessionStore((s) => s.setActiveBoard);
  const setBoardDialogOpen = useSessionStore((s) => s.setBoardDialogOpen);
  const sessions = useSessionStore(useShallow((s) => Object.values(s.sessions)));

  return (
    <div className={styles.tabs} role="tablist" aria-label="Boards">
      {boards.map((board) => {
        const mine = sessions.filter((s) => s.board === board.id);
        const waiting = mine.filter((s) => s.state === "Waiting").length;
        const working = mine.filter((s) => s.state === "Working").length;
        const active = board.id === activeBoardId;
        return (
          <button
            key={board.id}
            role="tab"
            aria-selected={active}
            className={`${styles.tab} ${active ? styles.tabActive : ""}`}
            onClick={() => setActiveBoard(board.id)}
            title={board.config_dir}
          >
            {board.name}
            {waiting > 0 && <span className={`${styles.count} ${styles.countWaiting}`}>{waiting}</span>}
            {waiting === 0 && working > 0 && <span className={`${styles.count} ${styles.countWorking}`}>{working}</span>}
          </button>
        );
      })}
      <button className={styles.add} onClick={() => setBoardDialogOpen(true)} title="Add or manage boards">
        + Board
      </button>
    </div>
  );
}
