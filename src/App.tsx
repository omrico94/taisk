import { useEffect, useState } from "react";
import { Toolbar } from "./components/Toolbar";
import { Board } from "./components/Board";
import { SessionOverlay } from "./components/SessionOverlay";
import { AssignMenu } from "./components/AssignMenu";
import { AskMemoryOverlay } from "./components/AskMemoryOverlay";
import { BoardTabs } from "./components/BoardTabs";
import { BoardDialog } from "./components/BoardDialog";
import { useSessionStore } from "./store/sessionStore";
import { useSessionEngine } from "./store/useSessionEngine";
import styles from "./App.module.css";

function App() {
  useSessionEngine();

  const askOpen = useSessionStore((s) => s.askOpen);
  const setAskOpen = useSessionStore((s) => s.setAskOpen);
  const selectCard = useSessionStore((s) => s.selectCard);

  const [nowMs, setNowMs] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNowMs(Date.now()), 30_000);
    return () => clearInterval(id);
  }, []);

  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      const key = e.key.toLowerCase();
      if ((e.metaKey || e.ctrlKey) && key === "k") {
        e.preventDefault();
        setAskOpen(!askOpen);
      } else if (key === "escape") {
        setAskOpen(false);
        useSessionStore.getState().setBoardDialogOpen(false);
        selectCard(null);
        useSessionStore.getState().openAssignMenu(null);
      }
    };
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  }, [askOpen, setAskOpen, selectCard]);

  return (
    <div className={styles.app}>
      <Toolbar />
      <BoardTabs />
      <div className={styles.body}>
        <Board />
        <SessionOverlay nowMs={nowMs} />
        <AssignMenu />
        <AskMemoryOverlay />
        <BoardDialog />
      </div>
    </div>
  );
}

export default App;
