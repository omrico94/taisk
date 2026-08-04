import { useEffect, useState } from "react";
import { motion } from "framer-motion";
import { invoke } from "@tauri-apps/api/core";
import { useShallow } from "zustand/react/shallow";
import { useSessionStore } from "../store/sessionStore";
import type { SessionState } from "../types";
import { approveSession, getTranscript, recategorizeSession, rejectSession, replySession, type TranscriptRow } from "../api";
import {
  STATE_COLOR_VAR,
  STATE_LABEL,
  ctxColor,
  ctxPercent,
  fmtCost,
  fmtTokens,
  formatElapsed,
  planPercent,
  toolColorVar,
} from "../styles/sessionStyle";
import styles from "./DetailDrawer.module.css";

// Matches the design's `slideIn` spec exactly (.42s cubic-bezier(.22,1,.36,1)
// from translateX(26px)+opacity 0). Framer Motion here instead of the CSS
// keyframe used elsewhere (breathe/waitGlow) so entrance motion is expressed
// in one system rather than two (plan §8).
const DRAWER_TRANSITION = { duration: 0.42, ease: [0.22, 1, 0.36, 1] as const };

const ROLE_COLOR: Record<string, string> = {
  You: "#7cc5ff",
  Claude: "#d98a6a",
  tool: "#7fbf7f",
};

interface Props {
  nowMs: number;
}

export function DetailDrawer({ nowMs }: Props) {
  const selectedId = useSessionStore((s) => s.selectedId);
  const session = useSessionStore((s) => (s.selectedId ? s.sessions[s.selectedId] : undefined));
  const selectCard = useSessionStore((s) => s.selectCard);
  const replyText = useSessionStore((s) => s.replyText);
  const setReplyText = useSessionStore((s) => s.setReplyText);
  const knownCategories = useSessionStore(
    useShallow((s) => Array.from(new Set(Object.values(s.sessions).map((x) => x.category)))),
  );

  const [transcript, setTranscript] = useState<TranscriptRow[]>([]);

  // M10: real transcript fetch (replacing M8's MOCK_TRANSCRIPTS). Re-fetches
  // whenever the selected session changes — this is a point-in-time read,
  // not a subscription, matching the plan's GET /sessions/:id/transcript.
  useEffect(() => {
    if (!selectedId) {
      setTranscript([]);
      return;
    }
    let cancelled = false;
    getTranscript(selectedId)
      .then((rows) => {
        if (!cancelled) setTranscript(rows);
      })
      .catch((err) => console.error("Failed to load transcript:", err));
    return () => {
      cancelled = true;
    };
  }, [selectedId]);

  if (!session || !selectedId) return null;

  const color = STATE_COLOR_VAR[session.state];
  const isWaiting = session.state === "Waiting";
  const planDone = session.plan?.steps.filter((s) => s.done).length ?? 0;
  const planTotal = session.plan?.steps.length ?? 0;

  // These fire the real API calls (M10); the resulting state change comes
  // back authoritatively over the WS diff stream, not a local mutation —
  // the drawer only owns UI-only concerns (closing itself, clearing the
  // reply box) here.
  const approve = () => {
    approveSession(session.id);
    selectCard(null);
  };
  const reject = () => {
    rejectSession(session.id);
    selectCard(null);
  };
  const send = () => {
    replySession(session.id, replyText.trim());
    selectCard(null);
  };

  // FR9 manual override: cycle to the next known category. This is what
  // actually exercises the "cards physically move between swimlanes"
  // motion (plan §8/§9) — Board re-groups once the WS diff lands and the
  // card's layoutId carries it smoothly to its new lane.
  const recategorize = () => {
    const others = knownCategories.filter((c) => c !== session.category && c !== "Uncategorized");
    const next = others[0] ?? "General";
    recategorizeSession(session.id, next);
  };

  // Phase 2 roadmap item 6: only a CLI-originated session (a plain terminal
  // `claude` invocation) can be reattached to via `claude --resume <id>`.
  // There's no known deep-link for a specific Claude Desktop tab, so that
  // case gets an honest disabled button instead of a fallback gesture that
  // looks session-specific but isn't.
  const canJump = session.entrypoint === "cli";
  const jumpToSession = () => {
    invoke("jump_to_cli_session", { cwd: session.cwd, sessionId: session.id }).catch((err) =>
      console.error("Failed to jump to session:", err),
    );
  };

  return (
    <motion.div
      className={styles.drawer}
      initial={{ x: 26, opacity: 0 }}
      animate={{ x: 0, opacity: 1 }}
      transition={DRAWER_TRANSITION}
    >
      <div className={styles.header}>
        <span className={styles.statePill} style={{ color, border: `1px solid ${color}` }}>
          <span className={styles.dot} style={{ background: color }} />
          {STATE_LABEL[session.state]}
        </span>
        <span className={styles.spacer} />
        <span className={styles.elapsed}>{formatElapsed(session.started_at_ms, nowMs)}</span>
        <button className={styles.closeButton} onClick={() => selectCard(null)}>
          ✕
        </button>
      </div>

      <div className={styles.identity}>
        <div className={styles.identityRow}>
          <span style={{ color: toolColorVar(session.tool), fontSize: "10.5px", fontWeight: 600 }}>{session.tool}</span>
          <span className={styles.projectName}>{session.project}</span>
        </div>
        <div className={styles.titleLine}>{session.title}</div>
        <div className={styles.descLine}>{session.desc}</div>
      </div>

      {session.ctx_max > 0 && (
        <div className={styles.statBlock}>
          <div className={styles.statPair}>
            <div className={styles.statCol}>
              <span className={styles.statLabel}>Tokens</span>
              <span className={styles.statValue}>{fmtTokens(session.tokens)}</span>
            </div>
            <div className={styles.statDivider} />
            <div className={styles.statCol}>
              <span className={styles.statLabel}>Cost</span>
              <span className={`${styles.statValue} ${styles.statValueCyan}`}>{fmtCost(session.cost)}</span>
            </div>
          </div>
          <div className={styles.ctxBlock}>
            <div className={styles.ctxHeaderRow}>
              <span className={styles.statLabel}>Context window</span>
              <span className={styles.spacer} />
              <span className={styles.ctxUsedLabel}>
                {fmtTokens(session.ctx_used)} / {fmtTokens(session.ctx_max)}
              </span>
            </div>
            <div className={styles.ctxBarRow}>
              <div className={styles.ctxTrack}>
                <div
                  className={styles.ctxFill}
                  style={{
                    width: `${ctxPercent(session.ctx_used, session.ctx_max)}%`,
                    background: ctxColor(ctxPercent(session.ctx_used, session.ctx_max)),
                  }}
                />
              </div>
              <span className={styles.ctxPct} style={{ color: ctxColor(ctxPercent(session.ctx_used, session.ctx_max)) }}>
                {ctxPercent(session.ctx_used, session.ctx_max)}%
              </span>
            </div>
          </div>
        </div>
      )}

      {session.subs.length > 0 && (
        <div className={styles.subsSection}>
          <div className={styles.subsSectionLabel}>
            <span className={styles.subsSectionGlyph}>⤷</span>
            {session.subs.length} {session.subs.length === 1 ? "subagent" : "subagents"}
          </div>
          <div className={styles.subWrap}>
            {session.subs.map((sub) => {
              const subState = sub.state as SessionState;
              const subColor = STATE_COLOR_VAR[subState];
              return (
                <div className={styles.subRow} key={sub.id}>
                  <span className={styles.subConnector} />
                  <div className={styles.subCard} style={{ opacity: sub.state === "Done" ? 0.72 : 1 }}>
                    <div className={styles.identityRow}>
                      <span
                        className={`${styles.subDot} ${sub.state === "Working" ? styles.subDotWorking : ""}`}
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
                      <div className={styles.subMetricsRow}>
                        <span className={styles.subTokens}>◇ {fmtTokens(sub.tokens)}</span>
                        <span className={styles.subCost}>{fmtCost(sub.cost)}</span>
                      </div>
                    )}
                  </div>
                </div>
              );
            })}
          </div>
        </div>
      )}

      {session.plan && (
        <div className={styles.planPanel}>
          <div className={styles.planHeaderRow}>
            <span className={styles.planGlyph}>◧</span>
            <span className={styles.planTitle}>{session.plan.title}</span>
          </div>
          <div className={styles.planProgressRow}>
            <div className={styles.planTrack}>
              <div className={styles.planFill} style={{ width: `${planPercent(planDone, planTotal)}%` }} />
            </div>
            <span className={styles.planLabel}>
              {planDone}/{planTotal}
            </span>
          </div>
          <div className={styles.planSteps}>
            {session.plan.steps.map((step) => (
              <div className={styles.planStepRow} key={step.id}>
                <span className={`${styles.planMark} ${step.done ? styles.planMarkDone : ""}`}>{step.done ? "✓" : ""}</span>
                <span className={`${styles.planStepText} ${step.done ? styles.planStepTextDone : ""}`}>{step.subject}</span>
              </div>
            ))}
          </div>
        </div>
      )}

      <div className={styles.memoryChip}>
        <span className={styles.memoryChipIcon}>✦</span>
        <span className={styles.memoryChipText}>
          In <strong className={styles.memoryChipStrong}>{session.category}</strong>
        </span>
      </div>

      <div className={styles.transcript}>
        <div className={styles.transcriptLabel}>Transcript tail</div>
        <div className={styles.transcriptRows}>
          {transcript.map((t, i) => (
            <div className={styles.transcriptRow} key={i}>
              <span className={styles.roleTag} style={{ color: ROLE_COLOR[t.role] ?? "var(--text-faint)" }}>
                {t.role}
              </span>
              <p className={styles.transcriptText}>{t.text}</p>
            </div>
          ))}
        </div>
      </div>

      {isWaiting ? (
        <div className={`${styles.footer} ${styles.waitingFooter}`}>
          <textarea
            className={styles.textarea}
            value={replyText}
            onChange={(e) => setReplyText(e.target.value)}
            placeholder="Reply, or just approve…"
          />
          <div className={styles.buttonRow}>
            <button className={styles.approveButton} onClick={approve}>
              Approve
            </button>
            <button className={styles.rejectButton} onClick={reject}>
              Reject
            </button>
            <button className={styles.sendButton} onClick={send}>
              Send ↵
            </button>
          </div>
        </div>
      ) : (
        <div className={styles.footer}>
          {canJump ? (
            <button className={styles.jumpButton} onClick={jumpToSession}>
              Jump to session ↗
            </button>
          ) : (
            <button
              className={styles.jumpButton}
              disabled
              title="This session was started from Claude Desktop — there's no way to jump to a specific Desktop tab yet."
            >
              Jump to session ↗
            </button>
          )}
          <button className={styles.recatButton} onClick={recategorize}>
            Re-categorize
          </button>
        </div>
      )}
    </motion.div>
  );
}
