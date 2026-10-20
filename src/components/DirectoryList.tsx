import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import styles from "./Kanban.module.css";

interface Props {
  directories: string[];
  /** Persists the new list; throws with the backend's validation message. */
  save: (directories: string[]) => Promise<void>;
  editing: boolean;
  onDone: () => void;
  /** Shown under the input while the list is empty. */
  emptyHint?: string;
  /** Prefix for the `data-testid`s ("task" → `task-directories`, …). */
  testId?: string;
}

const basename = (p: string) => p.replace(/\/+$/, "").split("/").pop() || p;

// An editable list of directories — a task's, or a board's defaults. A session
// started from it runs in the first one and gets the rest via
// `claude --add-dir`. The backend validates and normalizes paths (`~`
// expansion, must exist); the caller's `save` pushes the change.
export function DirectoryList({
  directories,
  save: persist,
  editing,
  onDone,
  emptyHint = "New sessions start in the first directory; others are added with --add-dir.",
  testId = "task",
}: Props) {
  const [draft, setDraft] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [picking, setPicking] = useState(false);

  const save = async (next: string[]) => {
    try {
      await persist(next);
      setError(null);
      return true;
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
      return false;
    }
  };

  const add = async () => {
    const path = draft.trim();
    if (!path) return;
    if (await save([...directories, path])) setDraft("");
  };

  // Native picker (see `pick_directory` in src-tauri/src/lib.rs). Guarded so
  // repeated clicks can't stack panels.
  const browse = async () => {
    if (picking) return;
    setPicking(true);
    try {
      const picked = await invoke<string | null>("pick_directory");
      if (!picked || directories.includes(picked)) return; // cancelled / already there
      await save([...directories, picked]);
    } catch (err) {
      setError(`Couldn't open the folder picker: ${err instanceof Error ? err.message : String(err)}`);
    } finally {
      setPicking(false);
    }
  };

  return (
    // Clicks here must not toggle the card's collapse state or start a drag.
    <div data-testid={`${testId}-directories`} onClick={(e) => e.stopPropagation()} onMouseDown={(e) => e.stopPropagation()}>
      {directories.length > 0 && (
        <div className={styles.dirList}>
          {directories.map((dir, i) => (
            <span
              key={dir}
              className={`${styles.dirChip} ${i === 0 ? styles.dirChipPrimary : ""}`}
              title={i === 0 ? `${dir} (working directory)` : `${dir} (--add-dir)`}
              data-testid={`${testId}-directory`}
            >
              <span className={styles.dirChipName}>{basename(dir)}</span>
              {editing && (
                <button
                  className={styles.iconButton}
                  aria-label={`Remove ${dir}`}
                  onClick={() => void save(directories.filter((d) => d !== dir))}
                >
                  ✕
                </button>
              )}
            </span>
          ))}
        </div>
      )}
      {editing && (
        <>
          <div className={styles.dirAddRow}>
            <input
              className={styles.dirInput}
              placeholder="~/code/my-repo"
              value={draft}
              autoFocus
              draggable={false}
              onDragStart={(e) => {
                e.preventDefault();
                e.stopPropagation();
              }}
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") void add();
                if (e.key === "Escape") onDone();
              }}
              data-testid={`${testId}-directory-input`}
            />
            <button
              type="button"
              className={styles.dirBrowse}
              onClick={() => void browse()}
              disabled={picking}
              data-testid={`${testId}-directory-browse`}
            >
              {picking ? "Choosing…" : "Browse…"}
            </button>
          </div>
          {error ? (
            <div className={styles.dirError}>{error}</div>
          ) : (
            <div className={styles.dirHint}>
              {directories.length === 0 ? emptyHint : "Enter to add · Esc to close"}
            </div>
          )}
        </>
      )}
    </div>
  );
}
