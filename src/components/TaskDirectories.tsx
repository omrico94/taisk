import { useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { setTaskDirectories } from "../api";
import type { Task } from "../types";
import styles from "./Kanban.module.css";

interface Props {
  task: Task;
  editing: boolean;
  onDone: () => void;
}

const basename = (p: string) => p.replace(/\/+$/, "").split("/").pop() || p;

// Directories attached to a task. A session started from the task (the card's
// "+") runs in the first one and gets the rest via `claude --add-dir`. The
// backend validates and normalizes paths (`~` expansion, must exist) and the
// change comes back through the usual TasksChanged snapshot.
export function TaskDirectories({ task, editing, onDone }: Props) {
  const [draft, setDraft] = useState("");
  const [error, setError] = useState<string | null>(null);

  const save = async (directories: string[]) => {
    try {
      await setTaskDirectories(task.id, directories);
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
    if (await save([...task.directories, path])) setDraft("");
  };

  const browse = async () => {
    const picked = await open({ directory: true, multiple: false, title: "Choose a directory" });
    if (typeof picked !== "string") return; // cancelled
    if (task.directories.includes(picked)) return;
    await save([...task.directories, picked]);
  };

  return (
    // Clicks here must not toggle the card's collapse state or start a drag.
    <div data-testid="task-directories" onClick={(e) => e.stopPropagation()} onMouseDown={(e) => e.stopPropagation()}>
      {task.directories.length > 0 && (
        <div className={styles.dirList}>
          {task.directories.map((dir, i) => (
            <span
              key={dir}
              className={`${styles.dirChip} ${i === 0 ? styles.dirChipPrimary : ""}`}
              title={i === 0 ? `${dir} (working directory)` : `${dir} (--add-dir)`}
              data-testid="task-directory"
            >
              <span className={styles.dirChipName}>{basename(dir)}</span>
              {editing && (
                <button
                  className={styles.iconButton}
                  aria-label={`Remove ${dir}`}
                  onClick={() => void save(task.directories.filter((d) => d !== dir))}
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
              data-testid="task-directory-input"
            />
            <button
              type="button"
              className={styles.dirBrowse}
              onClick={() => void browse()}
              data-testid="task-directory-browse"
            >
              Browse…
            </button>
          </div>
          {error ? (
            <div className={styles.dirError}>{error}</div>
          ) : (
            <div className={styles.dirHint}>
              {task.directories.length === 0
                ? "New sessions start in the first directory; others are added with --add-dir."
                : "Enter to add · Esc to close"}
            </div>
          )}
        </>
      )}
    </div>
  );
}
