import { useState } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { invoke } from "@tauri-apps/api/core";
import { useShallow } from "zustand/react/shallow";
import { createBoard, deleteBoard, renameBoard } from "../api";
import { useSessionStore } from "../store/sessionStore";
import { DEFAULT_BOARD_ID, type Board } from "../types";
import styles from "./BoardDialog.module.css";

function suggestConfigDir(name: string): string {
  const slug = name
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-|-$/g, "");
  return slug ? `~/.claude-${slug}` : "";
}

// Manage boards: each one is a name plus a Claude config directory, and each
// config directory holds its own Claude login. Adding a board installs
// taisk's hooks into that directory; signing in is done in a Terminal
// with CLAUDE_CONFIG_DIR set to it (the "Log in" button opens one).
export function BoardDialog() {
  const open = useSessionStore((s) => s.boardDialogOpen);
  const setOpen = useSessionStore((s) => s.setBoardDialogOpen);
  const boards = useSessionStore(useShallow((s) => s.boards));
  const upsertBoard = useSessionStore((s) => s.upsertBoard);
  const removeBoard = useSessionStore((s) => s.removeBoard);
  const setActiveBoard = useSessionStore((s) => s.setActiveBoard);

  const [name, setName] = useState("");
  const [configDir, setConfigDir] = useState("");
  // Once the user types their own path, stop overwriting it from the name.
  const [dirEdited, setDirEdited] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [renamingId, setRenamingId] = useState<string | null>(null);
  const [renameText, setRenameText] = useState("");
  // Arm/confirm instead of window.confirm(), which Tauri's WebView swallows.
  const [confirmingId, setConfirmingId] = useState<string | null>(null);

  const close = () => {
    setOpen(false);
    setError(null);
    setConfirmingId(null);
    setRenamingId(null);
  };

  const add = async () => {
    setBusy(true);
    setError(null);
    try {
      const board = await createBoard(name, configDir);
      upsertBoard(board);
      setActiveBoard(board.id);
      setName("");
      setConfigDir("");
      setDirEdited(false);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  };

  const commitRename = async (board: Board) => {
    try {
      upsertBoard(await renameBoard(board.id, renameText));
      setRenamingId(null);
      setError(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  };

  const remove = async (board: Board) => {
    if (confirmingId !== board.id) {
      setConfirmingId(board.id);
      return;
    }
    try {
      await deleteBoard(board.id);
      removeBoard(board.id);
      setConfirmingId(null);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  };

  const login = (board: Board) => {
    invoke("login_board_terminal", { configDir: board.config_dir }).catch((err) =>
      setError(`Couldn't open Terminal: ${String(err)}`),
    );
  };

  return (
    <AnimatePresence>
      {open && (
        <motion.div
          className={styles.scrim}
          onClick={close}
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          exit={{ opacity: 0 }}
          transition={{ duration: 0.2 }}
        >
          <motion.div
            className={styles.panel}
            role="dialog"
            aria-label="Manage boards"
            onClick={(e) => e.stopPropagation()}
            initial={{ scale: 0.96, opacity: 0 }}
            animate={{ scale: 1, opacity: 1 }}
            exit={{ scale: 0.96, opacity: 0 }}
            transition={{ duration: 0.25 }}
          >
            <div className={styles.header}>
              <div className={styles.title}>Boards</div>
              <button className={styles.close} onClick={close} aria-label="Close">
                ✕
              </button>
            </div>
            <p className={styles.hint}>
              Each board reads sessions from one Claude config directory, and each directory has its own login — that is
              how one board works with your work account and another with your home account.
            </p>

            <div className={styles.list}>
              {boards.map((board) => (
                <div className={styles.row} key={board.id}>
                  <div className={styles.rowMain}>
                    {renamingId === board.id ? (
                      <input
                        autoFocus
                        className={styles.inlineInput}
                        value={renameText}
                        onChange={(e) => setRenameText(e.target.value)}
                        onKeyDown={(e) => {
                          if (e.key === "Enter") void commitRename(board);
                          else if (e.key === "Escape") setRenamingId(null);
                        }}
                      />
                    ) : (
                      <div className={styles.rowName}>{board.name}</div>
                    )}
                    <div className={styles.rowDir}>{board.config_dir}</div>
                  </div>
                  <div className={styles.rowActions}>
                    {board.id !== DEFAULT_BOARD_ID && (
                      <button className={styles.small} onClick={() => login(board)} title="Open Terminal to log in">
                        Log in
                      </button>
                    )}
                    {renamingId === board.id ? (
                      <button className={styles.small} onClick={() => void commitRename(board)}>
                        Save
                      </button>
                    ) : (
                      <button
                        className={styles.small}
                        onClick={() => {
                          setRenamingId(board.id);
                          setRenameText(board.name);
                        }}
                      >
                        Rename
                      </button>
                    )}
                    {board.id !== DEFAULT_BOARD_ID && (
                      <button
                        className={`${styles.small} ${confirmingId === board.id ? styles.dangerArmed : styles.danger}`}
                        onClick={() => void remove(board)}
                        onBlur={() => setConfirmingId(null)}
                      >
                        {confirmingId === board.id ? "Confirm remove" : "Remove"}
                      </button>
                    )}
                  </div>
                </div>
              ))}
            </div>

            <div className={styles.form}>
              <div className={styles.formTitle}>Add a board</div>
              <input
                className={styles.input}
                placeholder="Name (e.g. Work)"
                value={name}
                onChange={(e) => {
                  setName(e.target.value);
                  if (!dirEdited) setConfigDir(suggestConfigDir(e.target.value));
                }}
              />
              <input
                className={styles.input}
                placeholder="Config directory (e.g. ~/.claude-work)"
                value={configDir}
                onChange={(e) => {
                  setConfigDir(e.target.value);
                  setDirEdited(true);
                }}
              />
              <div className={styles.formFoot}>
                <span className={styles.note}>
                  After adding, press “Log in” and sign in to that account in the Terminal that opens.
                </span>
                <button className={styles.primary} disabled={busy || !name.trim() || !configDir.trim()} onClick={() => void add()}>
                  Add board
                </button>
              </div>
            </div>
            {error && <div className={styles.error}>{error}</div>}
            <p className={styles.footnote}>
              Removing a board only takes its cards off taisk and removes our hooks from its directory. Your Claude
              files and transcripts are never deleted.
            </p>
          </motion.div>
        </motion.div>
      )}
    </AnimatePresence>
  );
}
