import { useCallback, useEffect, useRef, useState } from "react";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { createTask, getBoards } from "../api";
import type { Board } from "../types";
import styles from "./Popup.module.css";

const LAST_BOARD_KEY = "quickAdd.lastBoard";

function lastBoardId(): string | null {
  try {
    return localStorage.getItem(LAST_BOARD_KEY);
  } catch {
    return null;
  }
}

// Global-shortcut popup: type a title, Enter; with several boards a second
// keyboard-only step picks which one. Esc steps back / closes.
export function QuickAdd() {
  const [title, setTitle] = useState("");
  const [boards, setBoards] = useState<Board[]>([]);
  const [picking, setPicking] = useState(false);
  const [selected, setSelected] = useState(0);
  const [status, setStatus] = useState<"idle" | "saving" | "added">("idle");
  const [error, setError] = useState<string | null>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  const reset = useCallback(() => {
    setTitle("");
    setPicking(false);
    setStatus("idle");
    setError(null);
    getBoards()
      .then((b) => {
        setBoards(b);
        const i = b.findIndex((x) => x.id === lastBoardId());
        setSelected(i >= 0 ? i : 0);
      })
      .catch(() => setError("Can't reach the SessionBoard engine"));
    setTimeout(() => inputRef.current?.focus(), 0);
  }, []);

  useEffect(() => {
    reset();
    const un = listen("shortcut:shown", () => {
      window.focus();
      reset();
    });
    return () => {
      un.then((f) => f());
    };
  }, [reset]);

  const hide = () => void getCurrentWindow().hide();

  const submit = async (board: Board) => {
    setStatus("saving");
    try {
      await createTask(title.trim(), "todo", board.id);
      try {
        localStorage.setItem(LAST_BOARD_KEY, board.id);
      } catch {
        /* preference only */
      }
      setStatus("added");
      hide();
    } catch (e) {
      setStatus("idle");
      setError(e instanceof Error ? e.message : String(e));
    }
  };

  const onKeyDown = (e: KeyboardEvent) => {
    if (status !== "idle") return;
    if (e.key === "Escape") {
      e.preventDefault();
      if (picking) {
        setPicking(false);
        setTimeout(() => inputRef.current?.focus(), 0);
      } else hide();
      return;
    }
    if (!picking) {
      if (e.key === "Enter" && title.trim() && boards.length > 0) {
        e.preventDefault();
        if (boards.length === 1) void submit(boards[0]);
        else setPicking(true);
      }
      return;
    }
    if (e.key === "ArrowDown" || e.key === "j") {
      e.preventDefault();
      setSelected((i) => Math.min(i + 1, boards.length - 1));
    } else if (e.key === "ArrowUp" || e.key === "k") {
      e.preventDefault();
      setSelected((i) => Math.max(i - 1, 0));
    } else if (e.key === "Enter") {
      e.preventDefault();
      void submit(boards[selected]);
    } else if (/^[1-9]$/.test(e.key) && boards[Number(e.key) - 1]) {
      e.preventDefault();
      void submit(boards[Number(e.key) - 1]);
    }
  };

  // Document-level (re-bound every render so it sees fresh state): once the
  // input unmounts for the board picker nothing inside the panel has focus,
  // so an element-level handler would never fire.
  useEffect(() => {
    document.addEventListener("keydown", onKeyDown);
    return () => document.removeEventListener("keydown", onKeyDown);
  });

  return (
    <div className={styles.panel}>
      {!picking ? (
        <>
          <div className={styles.title}>New task</div>
          <input
            ref={inputRef}
            className={styles.input}
            placeholder="What needs doing?"
            value={title}
            onChange={(e) => setTitle(e.target.value)}
            autoFocus
            spellCheck={false}
          />
          {error ? (
            <div className={styles.error}>{error}</div>
          ) : (
            <div className={styles.hint}>
              {status === "added" ? "Added ✓" : boards.length > 1 ? "↵ choose board · esc close" : "↵ add to To Do · esc close"}
            </div>
          )}
        </>
      ) : (
        <>
          <div className={styles.title}>Add to board</div>
          <div className={styles.typed}>{title}</div>
          <ul className={styles.list}>
            {boards.map((b, i) => (
              <li key={b.id} className={`${styles.row} ${i === selected ? styles.rowActive : ""}`}>
                <span className={styles.key}>{i < 9 ? i + 1 : ""}</span>
                <span className={styles.name}>{b.name}</span>
              </li>
            ))}
          </ul>
          <div className={styles.hint}>
            {status === "added" ? "Added ✓" : error ?? "↑↓ select · ↵ add · esc back"}
          </div>
        </>
      )}
    </div>
  );
}
