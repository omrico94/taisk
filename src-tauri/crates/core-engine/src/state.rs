//! Session lifecycle state machine.
//!
//! `SessionState` transitions are driven exclusively by `SessionEvent`s sourced
//! from Claude Code hooks (see the implementation plan §2/§3) — never inferred
//! from transcript content/timing alone. `transition` is the single place that
//! encodes the rules, so the whole state machine is auditable and unit-testable
//! in one spot rather than scattered across the collector/engine actor.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SessionState {
    Working,
    Waiting,
    Idle,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionEvent {
    /// Claude Code's `SessionStart` hook fired (new session or resume).
    SessionStart,
    /// Claude Code's `Notification` hook fired — Claude is blocked on the user
    /// (permission prompt, or idle-waiting-for-input per Claude Code's own definition).
    Notification,
    /// `PreToolUse` / `PostToolUse` fired — the agent is actively doing work.
    ToolActivity,
    /// The user approved, rejected, or replied to a Waiting session from the board.
    UserReply,
    /// Claude Code's `Stop` hook fired — the assistant's turn just ended
    /// cleanly (user report: a session should read as "done" the moment it
    /// stops actively working, not stay pinned at Working between turns).
    TurnEnd,
    /// No state-changing signal arrived within the configured TTL. Means two
    /// different things depending on `current` (see `transition`): a safety
    /// net for a `Working` session that never got a clean `TurnEnd`/
    /// `SessionEnd` (crashed process, closed terminal), or the "sat quiet
    /// long enough" signal that ages a `Done` session into `Idle`.
    IdleTimeout,
    /// Claude Code's `SessionEnd` hook fired.
    SessionEnd,
}

/// Encodes every (state, event) transition rule.
///
/// `IdleTimeout` is the one event whose result genuinely depends on
/// `current`, not just `event` — everything else maps to a fixed target
/// state regardless of where the session was:
/// - `Working` + `IdleTimeout` -> `Done`: the crash-safety fallback for a
///   session that never fires a clean `TurnEnd`/`SessionEnd` (closed
///   terminal, killed process) — same signal a clean stop would have given,
///   just detected by silence instead of a hook.
/// - `Done` + `IdleTimeout` -> `Idle`: a session that's been sitting `Done`
///   (not currently working) with no further activity for long enough ages
///   into `Idle`. The caller (`orchestrator`'s done-sweeper) is responsible
///   for only dispatching this to sessions that reached `Done` by going
///   quiet — a session explicitly ended by the user (`EndedSessions`) is
///   filtered out there and stays `Done` forever, since that's a real close,
///   not just silence.
/// - `Idle`/`Waiting` + `IdleTimeout` -> `Idle`: already-settled states, a
///   no-op (the sweep never actually targets `Waiting` — see
///   `engine::run_idle_sweeper` — this exists only so the table stays total).
///
/// Every other event is real, current evidence from Claude Code itself that
/// the session is alive (or just finished a turn) and should transition it
/// accordingly regardless of `current` — including reviving a `Done`
/// session (a resumed conversation reusing its original session id — see
/// `SessionStart`'s doc comment). Without that revival, a session that was
/// previously marked ended (`EndedSessions`) and then resumed would stay
/// stuck showing `Done` forever, because reconstruction on every restart
/// re-forces it to `Done` before any new hook has a chance to correct it.
pub fn transition(current: SessionState, event: SessionEvent) -> SessionState {
    use SessionEvent::*;
    use SessionState::*;

    if event == IdleTimeout {
        return if current == Working { Done } else { Idle };
    }

    match event {
        SessionEnd => Done,
        SessionStart => Working,
        ToolActivity => Working,
        UserReply => Working,
        Notification => Waiting,
        TurnEnd => Done,
        IdleTimeout => unreachable!("handled above"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SessionEvent::*;
    use SessionState::*;

    const ALL_STATES: [SessionState; 4] = [Working, Waiting, Idle, Done];
    const ALL_EVENTS: [SessionEvent; 7] = [
        SessionStart,
        Notification,
        ToolActivity,
        UserReply,
        TurnEnd,
        IdleTimeout,
        SessionEnd,
    ];

    /// Table-driven: every (state, event) pair must be covered here, and this
    /// test asserts the enumeration itself is complete (4 states * 7 events),
    /// not just that "some" transitions pass — matching the plan's M1 done
    /// condition ("test count checked against that enumeration").
    #[test]
    fn covers_every_state_event_pair() {
        let cases: Vec<(SessionState, SessionEvent, SessionState)> = vec![
            // Working
            (Working, SessionStart, Working),
            (Working, Notification, Waiting),
            (Working, ToolActivity, Working),
            (Working, UserReply, Working),
            (Working, TurnEnd, Done),
            (Working, IdleTimeout, Done),
            (Working, SessionEnd, Done),
            // Waiting
            (Waiting, SessionStart, Working),
            (Waiting, Notification, Waiting),
            (Waiting, ToolActivity, Working),
            (Waiting, UserReply, Working),
            (Waiting, TurnEnd, Done),
            (Waiting, IdleTimeout, Idle),
            (Waiting, SessionEnd, Done),
            // Idle
            (Idle, SessionStart, Working),
            (Idle, Notification, Waiting),
            (Idle, ToolActivity, Working),
            (Idle, UserReply, Working),
            (Idle, TurnEnd, Done),
            (Idle, IdleTimeout, Idle),
            (Idle, SessionEnd, Done),
            // Done (a resumed session reviving is real evidence, not a stray
            // race — every event revives it except SessionEnd, which is
            // idempotent. IdleTimeout now genuinely transitions Done to Idle
            // — see `transition`'s doc comment for why that's safe: the
            // caller filters out explicitly-ended sessions before dispatching it.)
            (Done, SessionStart, Working),
            (Done, Notification, Waiting),
            (Done, ToolActivity, Working),
            (Done, UserReply, Working),
            (Done, TurnEnd, Done),
            (Done, IdleTimeout, Idle),
            (Done, SessionEnd, Done),
        ];

        assert_eq!(
            cases.len(),
            ALL_STATES.len() * ALL_EVENTS.len(),
            "test table must cover every (state, event) pair exactly once"
        );

        for (state, event, expected) in cases {
            assert_eq!(
                transition(state, event),
                expected,
                "transition({state:?}, {event:?}) should be {expected:?}"
            );
        }
    }

    /// Regression (user report — a session that stopped working stayed
    /// pinned at Working instead of reading as "done"): a clean `Stop` hook
    /// must move a session to `Done` immediately, not just refresh its task
    /// summary while leaving it looking busy.
    #[test]
    fn turn_end_moves_a_working_session_straight_to_done() {
        assert_eq!(transition(Working, TurnEnd), Done);
    }

    /// Regression (user report — the reverse bug: a genuinely finished
    /// session needs a way to eventually settle into Idle): `IdleTimeout` on
    /// `Done` is a real transition now, not the no-op it used to be.
    #[test]
    fn idle_timeout_ages_a_done_session_into_idle() {
        assert_eq!(transition(Done, IdleTimeout), Idle);
    }

    #[test]
    fn session_end_is_idempotent_on_done() {
        assert_eq!(transition(Done, SessionEnd), Done);
    }
}
