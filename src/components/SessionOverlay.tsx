import { useEffect, useState } from "react";
import { useSessionStore } from "../store/sessionStore";
import type { SessionState } from "../types";
import {
  answerSession,
  approveSession,
  deleteSession,
  getTranscript,
  rejectSession,
  replySession,
  type TranscriptRow,
} from "../api";
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
import { moveSession } from "./boardActions";
import styles from "./SessionOverlay.module.css";

const ROLE_COLOR: Record<string, string> = {
  You: "var(--tk-ink)",
  Claude: "var(--tk-tool-claude-code)",
  tool: "var(--tk-faint)",
};

interface Props {
  nowMs: number;
}

export function SessionOverlay({ nowMs }: Props) {
  const selectedId = useSessionStore((s) => s.selectedId);
  const session = useSessionStore((s) => (s.selectedId ? s.sessions[s.selectedId] : undefined));
  const selectCard = useSessionStore((s) => s.selectCard);
  const replyText = useSessionStore((s) => s.replyText);
  const setReplyText = useSessionStore((s) => s.setReplyText);
  // Non-default boards live in their own Claude config dir, so `--resume`
  // only finds the session if Terminal is launched with that same dir.
  const taskTitle = useSessionStore((s) => {
    const tid = s.selectedId ? s.assignments[s.selectedId] : undefined;
    return s.tasks.find((t) => t.id === tid)?.title ?? null;
  });

  const [transcript, setTranscript] = useState<TranscriptRow[]>([]);
  // Arm/confirm state for the delete button (see `del` below) — deliberately
  // not `window.confirm()`: that's a native browser dialog, and Tauri's
  // WebView doesn't reliably show it (confirmed live — it silently returns
  // `false` without ever prompting, so the delete button did nothing at
  // all). An in-UI two-click confirm needs no dialog support from the host.
  const [confirmingDelete, setConfirmingDelete] = useState(false);
  // Multi-select `AskUserQuestion` toggle state (labels currently checked) —
  // only meaningful while `session.waiting_question?.multi_select` is true;
  // a single-select question answers immediately on click instead.
  const [selectedLabels, setSelectedLabels] = useState<string[]>([]);
  // Surfaces the "couldn't actually reach the session" case (no known pty or
  // tty — see `api.ts`'s doc comments) instead of silently doing nothing,
  // which is the bug this whole delivery path exists to fix.
  const [deliveryError, setDeliveryError] = useState(false);

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

  // Selecting a different session (or closing/reopening the drawer) must not
  // carry over an armed delete state onto whatever's shown next.
  useEffect(() => {
    setConfirmingDelete(false);
    setSelectedLabels([]);
    setDeliveryError(false);
  }, [selectedId]);

  if (!session || !selectedId) return null;

  const color = STATE_COLOR_VAR[session.state];
  const isWaiting = session.state === "Waiting";
  const planDone = session.plan?.steps.filter((s) => s.done).length ?? 0;
  const planTotal = session.plan?.steps.length ?? 0;

  // These fire the real API calls (M10); the resulting state change comes
  // back authoritatively over the WS diff stream, not a local mutation —
  // the drawer only owns UI-only concerns (closing itself, clearing the
  // reply box) here. Each one now actually delivers into the session's real
  // terminal (see `api.ts`'s doc comments) — a `false` result means that
  // failed (no known pty/tty), so the overlay stays open and shows why
  // instead of closing on an action that silently did nothing.
  const approve = async () => {
    if (await approveSession(session.id)) selectCard(null);
    else setDeliveryError(true);
  };
  const reject = async () => {
    if (await rejectSession(session.id)) selectCard(null);
    else setDeliveryError(true);
  };
  const send = async () => {
    if (await replySession(session.id, replyText.trim())) selectCard(null);
    else setDeliveryError(true);
  };

  // `AskUserQuestion` answering: a single-select question answers the moment
  // an option is clicked (matching the real terminal's own immediate-select
  // behavior); a multi-select one toggles checkboxes and waits for an
  // explicit confirm, since more than one choice can be intended.
  const question = session.waiting_question;
  const toggleOption = (label: string) => {
    setSelectedLabels((prev) => (prev.includes(label) ? prev.filter((l) => l !== label) : [...prev, label]));
  };
  const chooseOption = async (label: string) => {
    if (await answerSession(session.id, { selected: [label] })) selectCard(null);
    else setDeliveryError(true);
  };
  const confirmMultiSelect = async () => {
    if (selectedLabels.length === 0) return;
    if (await answerSession(session.id, { selected: selectedLabels })) selectCard(null);
    else setDeliveryError(true);
  };
  const sendFreeTextAnswer = async () => {
    const text = replyText.trim();
    if (!text) return;
    if (await answerSession(session.id, { text })) selectCard(null);
    else setDeliveryError(true);
  };

  // Removes the card from the board entirely (durably, so it doesn't just
  // come back on the next restart — see the backend's `DismissedSessions`).
  // A resumed session reusing this id still revives normally, same as any
  // other dismissal in this codebase. Two clicks required (arm, then
  // confirm) instead of `window.confirm()` — see `confirmingDelete`'s doc
  // comment for why.
  const del = () => {
    if (!confirmingDelete) {
      setConfirmingDelete(true);
      return;
    }
    deleteSession(session.id);
    selectCard(null);
  };

  // Phase 2 roadmap item 6: only a CLI-originated session (a plain terminal
  // `claude` invocation) can be reattached to via `claude --resume <id>`.
  // There's no known deep-link for a specific Claude Desktop tab, so that
  // case gets an honest disabled button instead of a fallback gesture that
  // looks session-specific but isn't.
  const canJump = session.entrypoint === "cli";
  // Same underlying limitation as `canJump`: delivering an answer means
  // reaching the session's real terminal — an embedded pty the board opened
  // itself, or (the common case) the bare tty of an external one (see
  // `api.ts`'s doc comments) — neither of which exists for a Claude Desktop
  // session.
  const canAnswerFromBoard = session.entrypoint === "cli";
  const unassign = () => {
    moveSession(session.id, null);
    selectCard(null);
  };
  const jumpToSession = () => {
    useSessionStore
      .getState()
      .openTerminalForSession(session.id)
      .catch((err) => console.error("Failed to open a terminal for the session:", err));
    // The terminal pane becomes the focus; get the overlay out of the way.
    selectCard(null);
  };

  return (
    <div className={styles.scrim} data-testid="session-overlay" onClick={() => selectCard(null)}>
    <div className={styles.panel} role="dialog" aria-label={session.title} onClick={(e) => e.stopPropagation()}>
      <div className={styles.header}>
        <span className={styles.statePill} style={{ color, border: `1px solid color-mix(in srgb, ${color} 40%, transparent)` }}>
          <span className={styles.dot} style={{ background: color }} />
          {STATE_LABEL[session.state]}
        </span>
        <span className={styles.spacer} />
        <span className={styles.elapsed}>{formatElapsed(session.started_at_ms, nowMs)}</span>
        {taskTitle && (
          <button className={styles.unassignButton} onClick={unassign} title="Send this session back to the tray">
            ↩ Unassign
          </button>
        )}
        {confirmingDelete && (
          <button className={styles.deleteCancelButton} onClick={() => setConfirmingDelete(false)}>
            Cancel
          </button>
        )}
        <button
          className={`${styles.deleteButton} ${confirmingDelete ? styles.deleteButtonConfirming : ""}`}
          onClick={del}
          title={confirmingDelete ? "Click again to permanently delete" : "Delete this session from the board"}
        >
          {confirmingDelete ? "Confirm delete" : "Delete"}
        </button>
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
        {taskTitle && (
          <div className={styles.taskCaption}>
            part of task <strong>{taskTitle}</strong>
          </div>
        )}
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
                  <div className={styles.subCard} style={{ opacity: sub.state === "Done" ? 0.7 : 1 }}>
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
            <span className={styles.planGlyph}>✓</span>
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

      {isWaiting && !canAnswerFromBoard ? (
        <div className={`${styles.footer} ${styles.waitingFooter}`}>
          {question && <div className={styles.questionText}>{question.question}</div>}
          <div className={styles.deliveryError}>
            This session was started from Claude Desktop — there's no way to answer it from the board yet. Switch to
            that window to respond.
          </div>
        </div>
      ) : isWaiting && question ? (
        <div className={`${styles.footer} ${styles.waitingFooter}`}>
          <div className={styles.questionText}>{question.question}</div>
          <div className={styles.optionList}>
            {question.options.map((opt) => {
              const checked = selectedLabels.includes(opt.label);
              return (
                <button
                  key={opt.label}
                  className={`${styles.optionButton} ${checked ? styles.optionButtonSelected : ""}`}
                  onClick={() => (question.multi_select ? toggleOption(opt.label) : chooseOption(opt.label))}
                >
                  {question.multi_select && (
                    <span className={`${styles.optionCheckbox} ${checked ? styles.optionCheckboxChecked : ""}`}>
                      {checked ? "✓" : ""}
                    </span>
                  )}
                  <span className={styles.optionText}>
                    <span className={styles.optionLabel}>{opt.label}</span>
                    {opt.description && <span className={styles.optionDesc}>{opt.description}</span>}
                  </span>
                </button>
              );
            })}
          </div>
          {question.multi_select && (
            <button
              className={styles.approveButton}
              onClick={confirmMultiSelect}
              disabled={selectedLabels.length === 0}
            >
              Confirm ({selectedLabels.length} selected)
            </button>
          )}
          <textarea
            className={styles.textarea}
            value={replyText}
            onChange={(e) => setReplyText(e.target.value)}
            placeholder="Or type your own answer…"
          />
          <div className={styles.buttonRow}>
            <button className={styles.sendButton} onClick={sendFreeTextAnswer}>
              Send ↵
            </button>
          </div>
          {deliveryError && (
            <div className={styles.deliveryError}>
              Couldn't reach the session automatically — answer it directly in the terminal.
            </div>
          )}
        </div>
      ) : isWaiting ? (
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
          {deliveryError && (
            <div className={styles.deliveryError}>
              Couldn't reach the session automatically — answer it directly in the terminal.
            </div>
          )}
        </div>
      ) : (
        <div className={styles.footer}>
          {canJump ? (
            <button className={styles.jumpButton} onClick={jumpToSession}>
              Jump into this session ↗
            </button>
          ) : (
            <button
              className={styles.jumpButton}
              disabled
              title="This session was started from Claude Desktop — there's no way to jump to a specific Desktop tab yet."
            >
              Jump into this session ↗
            </button>
          )}
        </div>
      )}
    </div>
    </div>
  );
}
