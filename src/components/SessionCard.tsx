import type { CSSProperties } from "react";
import { motion } from "framer-motion";
import type { SessionView } from "../types";
import { STATE_COLOR_VAR, STATE_GLOW_VAR, STATE_LABEL, formatElapsed, toolColorVar } from "../styles/sessionStyle";
import styles from "./SessionCard.module.css";

// The design's signature "cards physically move" motion (680ms,
// cubic-bezier(.22,1,.36,1)) — the prototype hand-rolls this via
// getBoundingClientRect + the Web Animations API (FLIP) because it's a
// dependency-free static HTML demo. In a real React app, Framer Motion's
// `layout`/`layoutId` implements the same FLIP technique with far better
// edge-case coverage (unmount/reflow, concurrent updates), so it's used here
// instead of reimplementing FLIP by hand (plan §8).
const MOVE_TRANSITION = { duration: 0.68, ease: [0.22, 1, 0.36, 1] as const };

interface Props {
  session: SessionView;
  nowMs: number;
  onSelect: (id: string) => void;
}

const STATE_CLASS: Record<SessionView["state"], string> = {
  Working: styles.working,
  Waiting: styles.waiting,
  Idle: styles.idle,
  Done: styles.done,
};

export function SessionCard({ session, nowMs, onSelect }: Props) {
  const color = STATE_COLOR_VAR[session.state];
  const glow = STATE_GLOW_VAR[session.state];

  const cardStyle: CSSProperties =
    session.state === "Waiting" ? ({ "--w": glow } as CSSProperties) : {};

  const dotBoxShadow =
    session.state === "Working" ? `0 0 12px 1px ${glow}` : session.state === "Waiting" ? `0 0 10px 0 ${glow}` : "none";

  return (
    <motion.div
      layout
      layoutId={session.id}
      transition={MOVE_TRANSITION}
      className={`${styles.card} ${STATE_CLASS[session.state]}`}
      style={cardStyle}
      onClick={() => onSelect(session.id)}
      data-testid={`session-card-${session.id}`}
    >
      <div className={styles.row}>
        <span
          className={`${styles.dot} ${session.state === "Working" ? styles.dotWorking : ""}`}
          style={{ background: color, boxShadow: dotBoxShadow }}
        />
        <span className={styles.stateLabel} style={{ color }}>
          {STATE_LABEL[session.state]}
        </span>
        <span className={styles.spacer} />
        <span className={styles.elapsed}>{formatElapsed(session.started_at_ms, nowMs)}</span>
      </div>

      <p className={styles.task}>{session.task}</p>

      <div className={styles.row}>
        <span className={styles.toolBadge} style={{ color: toolColorVar(session.tool) }}>
          {session.tool}
        </span>
        <span className={styles.project}>{session.project}</span>
      </div>
    </motion.div>
  );
}
