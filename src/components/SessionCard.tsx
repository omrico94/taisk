import { useState, type CSSProperties } from "react";
import { motion } from "framer-motion";
import type { SessionState, SessionView } from "../types";
import { useSessionStore } from "../store/sessionStore";
import {
  STATE_COLOR_VAR,
  STATE_GLOW_VAR,
  STATE_LABEL,
  ctxColor,
  ctxPercent,
  entrypointLabel,
  fmtCost,
  fmtTokens,
  formatElapsed,
  planPercent,
  toolColorVar,
} from "../styles/sessionStyle";
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
  const subsCollapsed = useSessionStore((s) => s.collapsedSubs.has(session.id));
  const toggleSubsCollapsed = useSessionStore((s) => s.toggleSubsCollapsed);
  const hasSubs = session.subs.length > 0;
  const planDone = session.plan?.steps.filter((s) => s.done).length ?? 0;
  const planTotal = session.plan?.steps.length ?? 0;
  const setDragOverCategory = useSessionStore((s) => s.setDragOverCategory);
  const [isDragging, setIsDragging] = useState(false);
  const origin = entrypointLabel(session.entrypoint);

  const cardStyle: CSSProperties =
    session.state === "Waiting" ? ({ "--w": glow } as CSSProperties) : {};

  const dotBoxShadow =
    session.state === "Working" ? `0 0 12px 1px ${glow}` : session.state === "Waiting" ? `0 0 10px 0 ${glow}` : "none";

  return (
    // Wraps the card and its (optional) subagent tree as one flex item —
    // mirrors the design's `groupStyle` column so the tree wraps together
    // with its parent card inside the lane's flex-wrap layout, instead of
    // the two becoming independent wrapping items.
    <div
      className={styles.group}
      style={{ opacity: isDragging ? 0.4 : 1 }}
      draggable
      onDragStart={(e) => {
        e.dataTransfer.effectAllowed = "move";
        e.dataTransfer.setData("text/plain", session.id);
        setIsDragging(true);
      }}
      onDragEnd={() => {
        setIsDragging(false);
        // Mirrors the design's `endDrag()`: the drag SOURCE clears the
        // target lane's hot-highlight too, so a drop outside any lane (or a
        // cancelled drag) doesn't leave a lane stuck highlighted.
        setDragOverCategory(null);
      }}
    >
      {/* motion.div's own onDragStart/onDragEnd props are Framer's pointer-drag
          gesture system (different signature from native DragEvent), so native
          HTML5 DnD is wired on the plain wrapper above instead of here. */}
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

        <p className={styles.title}>{session.title}</p>
        <p className={styles.desc}>{session.desc}</p>

        {session.ctx_max > 0 && (
          <div className={styles.ctxRow}>
            <span className={styles.ctxLabel}>CTX</span>
            <div className={styles.ctxTrack}>
              <div
                className={styles.ctxFill}
                style={{ width: `${ctxPercent(session.ctx_used, session.ctx_max)}%`, background: ctxColor(ctxPercent(session.ctx_used, session.ctx_max)) }}
              />
            </div>
            <span className={styles.ctxPct} style={{ color: ctxColor(ctxPercent(session.ctx_used, session.ctx_max)) }}>
              {ctxPercent(session.ctx_used, session.ctx_max)}%
            </span>
          </div>
        )}

        {session.ctx_max > 0 ? (
          <div className={styles.metricsRow}>
            <span className={styles.tokensBadge}>
              <span className={styles.tokensGlyph}>◇</span>
              {fmtTokens(session.tokens)}
            </span>
            <span className={styles.costBadge}>{fmtCost(session.cost)}</span>
            <span className={styles.spacer} />
            {origin && <span className={styles.toolBadge}>{origin}</span>}
            <span className={styles.toolBadge} style={{ color: toolColorVar(session.tool) }}>
              {session.tool}
            </span>
          </div>
        ) : (
          // No usage data yet (session just started) — fall back to the
          // Phase 1 tool + project row rather than showing nothing.
          <div className={styles.row}>
            {origin && <span className={styles.toolBadge}>{origin}</span>}
            <span className={styles.toolBadge} style={{ color: toolColorVar(session.tool) }}>
              {session.tool}
            </span>
            <span className={styles.project}>{session.project}</span>
          </div>
        )}

        {session.plan && (
          <div className={styles.planRow}>
            <span className={styles.planGlyph}>▧</span>
            <span className={styles.planLabel}>
              Plan · {planDone}/{planTotal}
            </span>
            <div className={styles.planTrack}>
              <div className={styles.planFill} style={{ width: `${planPercent(planDone, planTotal)}%` }} />
            </div>
          </div>
        )}

        {hasSubs && (
          <div
            className={styles.subsToggle}
            onClick={(e) => {
              e.stopPropagation();
              toggleSubsCollapsed(session.id);
            }}
          >
            <span className={styles.subsChevron}>{subsCollapsed ? "›" : "‹"}</span>
            <span className={styles.subsGlyph}>⤷</span>
            {session.subs.length} {session.subs.length === 1 ? "subagent" : "subagents"}
          </div>
        )}
      </motion.div>

      {hasSubs && !subsCollapsed && (
        <div className={styles.subWrap}>
          {session.subs.map((sub) => {
            const subState = sub.state as SessionState;
            const subColor = STATE_COLOR_VAR[subState];
            const pct = ctxPercent(sub.ctx_used, sub.ctx_max);
            return (
              <div className={styles.subRow} key={sub.id}>
                <span className={styles.subConnector} />
                <div
                  className={styles.subCard}
                  style={{ opacity: sub.state === "Done" ? 0.72 : 1 }}
                  onClick={() => onSelect(session.id)}
                >
                  <div className={styles.row}>
                    <span
                      className={`${styles.subDot} ${sub.state === "Working" ? styles.dotWorking : ""}`}
                      style={{ background: subColor }}
                    />
                    <span className={styles.subTitle}>{sub.title}</span>
                    <span className={styles.spacer} />
                    <span className={styles.subStateLabel} style={{ color: subColor }}>
                      {STATE_LABEL[subState]}
                    </span>
                  </div>
                  <div className={styles.subDesc}>{sub.desc}</div>
                  {sub.ctx_max > 0 && (
                    <div className={styles.row}>
                      <div className={styles.subCtxTrack}>
                        <div className={styles.subCtxFill} style={{ width: `${pct}%`, background: ctxColor(pct) }} />
                      </div>
                      <span className={styles.subCtxPct} style={{ color: ctxColor(pct) }}>
                        {pct}%
                      </span>
                      <span className={styles.subTokens}>{fmtTokens(sub.tokens)}</span>
                      <span className={styles.subCost}>{fmtCost(sub.cost)}</span>
                    </div>
                  )}
                </div>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
