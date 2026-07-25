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
    /// No state-changing signal arrived within the configured TTL.
    IdleTimeout,
    /// Claude Code's `SessionEnd` hook fired.
    SessionEnd,
}

/// Encodes every (state, event) transition rule. `Done` is terminal: once a
/// session has ended, no later hook (which could arrive out of order, or as a
/// stray race) should be able to revive it.
pub fn transition(current: SessionState, event: SessionEvent) -> SessionState {
    use SessionEvent::*;
    use SessionState::*;

    if current == Done {
        return Done;
    }

    match event {
        SessionEnd => Done,
        SessionStart => Working,
        ToolActivity => Working,
        UserReply => Working,
        Notification => Waiting,
        IdleTimeout => Idle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SessionEvent::*;
    use SessionState::*;

    const ALL_STATES: [SessionState; 4] = [Working, Waiting, Idle, Done];
    const ALL_EVENTS: [SessionEvent; 6] = [
        SessionStart,
        Notification,
        ToolActivity,
        UserReply,
        IdleTimeout,
        SessionEnd,
    ];

    /// Table-driven: every (state, event) pair must be covered here, and this
    /// test asserts the enumeration itself is complete (4 states * 6 events),
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
            (Working, IdleTimeout, Idle),
            (Working, SessionEnd, Done),
            // Waiting
            (Waiting, SessionStart, Working),
            (Waiting, Notification, Waiting),
            (Waiting, ToolActivity, Working),
            (Waiting, UserReply, Working),
            (Waiting, IdleTimeout, Idle),
            (Waiting, SessionEnd, Done),
            // Idle
            (Idle, SessionStart, Working),
            (Idle, Notification, Waiting),
            (Idle, ToolActivity, Working),
            (Idle, UserReply, Working),
            (Idle, IdleTimeout, Idle),
            (Idle, SessionEnd, Done),
            // Done (terminal — every event is a no-op)
            (Done, SessionStart, Done),
            (Done, Notification, Done),
            (Done, ToolActivity, Done),
            (Done, UserReply, Done),
            (Done, IdleTimeout, Done),
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

    #[test]
    fn done_is_terminal_for_every_event() {
        for event in ALL_EVENTS {
            assert_eq!(transition(Done, event), Done);
        }
    }
}
