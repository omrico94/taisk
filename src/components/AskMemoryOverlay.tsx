import { useEffect } from "react";
import { AnimatePresence, motion } from "framer-motion";
import { useSessionStore } from "../store/sessionStore";
import { search } from "../api";
import styles from "./AskMemoryOverlay.module.css";

const SEARCH_DEBOUNCE_MS = 180;

// Matches the design's `overlayIn` spec (.3s cubic-bezier(.22,1,.36,1), from
// scale .96 + opacity 0) — Framer Motion here for the same reason as the
// drawer: one animation system instead of two (plan §8). AnimatePresence
// wraps this so the exit (click-scrim-to-dismiss) also animates rather than
// just vanishing.
const OVERLAY_TRANSITION = { duration: 0.3, ease: [0.22, 1, 0.36, 1] as const };

function relevancePercent(distance: number): number {
  return Math.round(Math.max(0, Math.min(1, 1 - distance)) * 100);
}

function formatWhen(createdAtMs: number): string {
  return new Date(createdAtMs).toLocaleDateString(undefined, { month: "short", day: "numeric" });
}

export function AskMemoryOverlay() {
  const askOpen = useSessionStore((s) => s.askOpen);
  const query = useSessionStore((s) => s.askQuery);
  const setQuery = useSessionStore((s) => s.setAskQuery);
  const setAskOpen = useSessionStore((s) => s.setAskOpen);
  const askResults = useSessionStore((s) => s.askResults);
  const setAskResults = useSessionStore((s) => s.setAskResults);
  const selectCard = useSessionStore((s) => s.selectCard);
  const activeBoardId = useSessionStore((s) => s.activeBoardId);

  // M10: real GET /search, debounced since — unlike M8's instant client-side
  // mock — this is a genuine network round trip (embeds the query, queries
  // LanceDB) that shouldn't fire on every keystroke.
  useEffect(() => {
    if (!askOpen) return;
    let cancelled = false;
    const id = setTimeout(() => {
      search(query, activeBoardId)
        .then((results) => {
          if (!cancelled) setAskResults(results);
        })
        .catch((err) => console.error("Search failed:", err));
    }, SEARCH_DEBOUNCE_MS);
    return () => {
      cancelled = true;
      clearTimeout(id);
    };
  }, [askOpen, query, activeBoardId, setAskResults]);

  return (
    <AnimatePresence>
      {askOpen && (
        <motion.div
          className={styles.scrim}
          onClick={() => setAskOpen(false)}
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          exit={{ opacity: 0 }}
          transition={{ duration: 0.2 }}
        >
          <motion.div
            className={styles.panel}
            onClick={(e) => e.stopPropagation()}
            initial={{ scale: 0.96, opacity: 0 }}
            animate={{ scale: 1, opacity: 1 }}
            exit={{ scale: 0.96, opacity: 0 }}
            transition={OVERLAY_TRANSITION}
          >
            <div className={styles.inputRow}>
              <span className={styles.icon}>✦</span>
              <input
                className={styles.input}
                autoFocus
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                placeholder="Search everything you've ever asked an agent…"
              />
              <span className={styles.escHint}>esc</span>
            </div>
            <div className={styles.results}>
              <div className={styles.resultsLabel}>Semantic matches · live + history</div>
              {askResults.map((r) => (
                <div
                  className={styles.row}
                  key={r.session_id}
                  onClick={() => {
                    setAskOpen(false);
                    if (r.live) selectCard(r.session_id);
                  }}
                >
                  <span className={styles.relevance}>{relevancePercent(r.distance)}%</span>
                  <div className={styles.body}>
                    <div className={styles.snippet}>{r.text}</div>
                    <div className={styles.meta}>
                      {r.tool} · {r.project}
                    </div>
                  </div>
                  <span className={`${styles.when} ${r.live ? styles.whenLive : styles.whenHistorical}`}>
                    {r.live ? "● live now" : formatWhen(r.created_at)}
                  </span>
                </div>
              ))}
              {askResults.length === 0 && query.trim() !== "" && (
                <div className={styles.emptyState}>Nothing in memory matches that yet — try different words.</div>
              )}
            </div>
          </motion.div>
        </motion.div>
      )}
    </AnimatePresence>
  );
}
