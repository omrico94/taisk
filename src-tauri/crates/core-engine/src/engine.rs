//! The engine actor: single owner of live session state (plan §2). One task
//! owns the session map and applies every mutation itself via an mpsc
//! command channel — not a shared `Mutex<HashMap>` — so there's exactly one
//! place state changes happen (auditable, no lock contention) and every
//! change is broadcast to subscribers (the WS layer, M6) as a diff.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::state::{self, SessionEvent, SessionState};

pub type SessionId = String;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionView {
    pub id: SessionId,
    pub tool: String,
    pub project: String,
    pub cwd: String,
    /// How this session was started (`"cli"` for a plain terminal `claude`
    /// invocation, `"claude-desktop"` for the Claude Desktop app — observed
    /// directly in real hook payloads/transcripts). Drives "Jump to
    /// session" (Phase 2 roadmap item 6): only a CLI-originated session can
    /// be reattached to via `claude --resume <id>`. Defaults to `"cli"` when
    /// a hook payload doesn't carry the field (or for sessions reconstructed
    /// from durable memory on restart, which has no record of the original
    /// entrypoint) — the common case, not a guess in the dark.
    pub entrypoint: String,
    pub category: String,
    pub state: SessionState,
    pub task: String,
    pub started_at_ms: i64,
    /// Last time a real hook/user-activity event touched this session.
    /// Drives the idle sweep (`EngineCommand::SweepIdle`): a session with no
    /// activity for longer than the configured TTL is presumed abandoned
    /// (terminal closed, process killed — anything that never gets to fire
    /// a clean `SessionEnd`) and moved to `Idle` rather than sitting at
    /// `Working` forever. Not serialized to the frontend — it's purely an
    /// engine-internal bookkeeping field.
    #[serde(skip)]
    pub last_activity_ms: i64,
}

impl SessionView {
    pub fn new_uncategorized(id: SessionId, project: String, cwd: String, entrypoint: String, started_at_ms: i64) -> Self {
        Self {
            id,
            tool: "Claude Code".to_string(),
            project,
            cwd,
            entrypoint,
            category: "Uncategorized".to_string(),
            state: SessionState::Working,
            task: "Starting…".to_string(),
            started_at_ms,
            last_activity_ms: crate::now_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionDiff {
    Upserted(SessionView),
    Removed(SessionId),
}

#[derive(Debug)]
pub enum EngineCommand {
    /// A session-lifecycle hook fired; advance that session's state machine.
    /// Creates the session (as Uncategorized/Working) if it doesn't exist yet.
    SessionEvent {
        id: SessionId,
        event: SessionEvent,
        project: Option<String>,
        cwd: Option<String>,
        entrypoint: Option<String>,
        started_at_ms: Option<i64>,
    },
    SetCategory { id: SessionId, category: String },
    SetTask { id: SessionId, task: String },
    /// Approve/Reject/Send from the drawer (plan §7) — resolves a Waiting
    /// session back to Working with an updated task line.
    ResolveWaiting { id: SessionId, task: String },
    Recategorize { id: SessionId, category: String },
    /// Periodic tick (see `run_idle_sweeper`): any session still `Working`
    /// with no activity for at least `ttl_ms` is presumed abandoned and
    /// moved to `Idle`. Sessions that are `Waiting` (genuinely blocked on
    /// the user, not abandoned) or already `Done`/`Idle` are left alone.
    SweepIdle { now_ms: i64, ttl_ms: i64 },
    Snapshot { respond_to: oneshot::Sender<Vec<SessionView>> },
}

#[derive(Clone)]
pub struct EngineHandle {
    cmd_tx: mpsc::Sender<EngineCommand>,
    diff_tx: broadcast::Sender<SessionDiff>,
}

impl EngineHandle {
    /// Spawns the actor task and returns a cloneable handle to it. Multiple
    /// producers (the hook collector, the API server's approve/reject
    /// handlers) share one handle; only the actor task itself ever touches
    /// the session map directly.
    pub fn spawn() -> Self {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<EngineCommand>(256);
        let (diff_tx, _) = broadcast::channel::<SessionDiff>(256);
        let diff_tx_actor = diff_tx.clone();

        tokio::spawn(async move {
            let mut sessions: HashMap<SessionId, SessionView> = HashMap::new();

            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    EngineCommand::SessionEvent { id, event, project, cwd, entrypoint, started_at_ms } => {
                        // Only `SessionStart` may create a new entry. Any
                        // other event (notification, tool activity, stop,
                        // session-end) arriving for a session_id the engine
                        // has never seen a SessionStart for must be a no-op,
                        // not a fallback-created blank ghost card — this was
                        // a real bug: the orchestrator can legitimately
                        // decide not to create a card at session-start (no
                        // content yet), and a later hook event for that same
                        // session used to fabricate one anyway via this
                        // `.or_insert_with`, with project/cwd empty and
                        // started_at_ms defaulting to 0.
                        if !sessions.contains_key(&id) {
                            if event != SessionEvent::SessionStart {
                                continue;
                            }
                            sessions.insert(
                                id.clone(),
                                SessionView::new_uncategorized(
                                    id.clone(),
                                    project.unwrap_or_default(),
                                    cwd.unwrap_or_default(),
                                    entrypoint.unwrap_or_else(|| "cli".to_string()),
                                    started_at_ms.unwrap_or(0),
                                ),
                            );
                        }
                        let view = sessions.get_mut(&id).expect("just inserted or already present");
                        view.state = state::transition(view.state, event);
                        view.last_activity_ms = crate::now_ms();
                        let _ = diff_tx_actor.send(SessionDiff::Upserted(view.clone()));
                    }
                    EngineCommand::SetCategory { id, category } => {
                        if let Some(view) = sessions.get_mut(&id) {
                            view.category = category;
                            let _ = diff_tx_actor.send(SessionDiff::Upserted(view.clone()));
                        }
                    }
                    EngineCommand::SetTask { id, task } => {
                        if let Some(view) = sessions.get_mut(&id) {
                            view.task = task;
                            let _ = diff_tx_actor.send(SessionDiff::Upserted(view.clone()));
                        }
                    }
                    EngineCommand::ResolveWaiting { id, task } => {
                        if let Some(view) = sessions.get_mut(&id) {
                            view.state = state::transition(view.state, SessionEvent::UserReply);
                            view.task = task;
                            view.last_activity_ms = crate::now_ms();
                            let _ = diff_tx_actor.send(SessionDiff::Upserted(view.clone()));
                        }
                    }
                    EngineCommand::Recategorize { id, category } => {
                        if let Some(view) = sessions.get_mut(&id) {
                            view.category = category;
                            let _ = diff_tx_actor.send(SessionDiff::Upserted(view.clone()));
                        }
                    }
                    EngineCommand::SweepIdle { now_ms, ttl_ms } => {
                        for view in sessions.values_mut() {
                            if view.state == SessionState::Working && now_ms - view.last_activity_ms >= ttl_ms {
                                view.state = state::transition(view.state, SessionEvent::IdleTimeout);
                                let _ = diff_tx_actor.send(SessionDiff::Upserted(view.clone()));
                            }
                        }
                    }
                    EngineCommand::Snapshot { respond_to } => {
                        let _ = respond_to.send(sessions.values().cloned().collect());
                    }
                }
            }
        });

        Self { cmd_tx, diff_tx }
    }

    pub async fn dispatch(&self, cmd: EngineCommand) {
        let _ = self.cmd_tx.send(cmd).await;
    }

    pub fn subscribe(&self) -> broadcast::Receiver<SessionDiff> {
        self.diff_tx.subscribe()
    }

    pub async fn snapshot(&self) -> Vec<SessionView> {
        let (tx, rx) = oneshot::channel();
        self.dispatch(EngineCommand::Snapshot { respond_to: tx }).await;
        rx.await.unwrap_or_default()
    }
}

/// Background loop: every `interval`, sweeps for `Working` sessions that have
/// had no hook activity for at least `ttl` and moves them to `Idle` (see
/// `EngineCommand::SweepIdle`). Started once from `bootstrap::start` and runs
/// for the lifetime of the app — this is what catches a session whose
/// process was killed or window closed without ever firing a clean
/// `SessionEnd` (a hook is cooperative; nothing fires it for you).
pub async fn run_idle_sweeper(engine: EngineHandle, ttl: std::time::Duration, interval: std::time::Duration) {
    let ttl_ms = ttl.as_millis() as i64;
    loop {
        tokio::time::sleep(interval).await;
        engine.dispatch(EngineCommand::SweepIdle { now_ms: crate::now_ms(), ttl_ms }).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn session_start_creates_an_uncategorized_working_session() {
        let engine = EngineHandle::spawn();
        let mut diffs = engine.subscribe();

        engine
            .dispatch(EngineCommand::SessionEvent {
                id: "s1".into(),
                event: SessionEvent::SessionStart,
                project: Some("api-gateway".into()),
                cwd: Some("/Users/omricohen/api-gateway".into()),
                entrypoint: None,
                started_at_ms: Some(1000),
            })
            .await;

        let diff = diffs.recv().await.unwrap();
        let SessionDiff::Upserted(view) = diff else { panic!("expected Upserted") };
        assert_eq!(view.id, "s1");
        assert_eq!(view.category, "Uncategorized");
        assert_eq!(view.state, SessionState::Working);

        let snapshot = engine.snapshot().await;
        assert_eq!(snapshot.len(), 1);
    }

    /// Bug fix: a non-SessionStart event (e.g. `stop`, `post-tool-use`)
    /// arriving for a session_id the engine has never seen a SessionStart
    /// for must be silently ignored, not fabricate a blank ghost card
    /// (project/cwd empty, started_at_ms 0). This happens legitimately when
    /// the orchestrator deliberately skips creating a card because no
    /// content was found yet, but later hook events still arrive.
    #[tokio::test]
    async fn non_session_start_event_for_unknown_session_is_ignored() {
        let engine = EngineHandle::spawn();
        let mut diffs = engine.subscribe();

        engine
            .dispatch(EngineCommand::SessionEvent {
                id: "ghost".into(),
                event: SessionEvent::ToolActivity,
                project: None,
                cwd: None,
                entrypoint: None,
                started_at_ms: None,
            })
            .await;
        engine
            .dispatch(EngineCommand::SessionEvent {
                id: "ghost".into(),
                event: SessionEvent::SessionEnd,
                project: None,
                cwd: None,
                entrypoint: None,
                started_at_ms: None,
            })
            .await;

        assert!(engine.snapshot().await.is_empty(), "no ghost session should have been created");

        // A real SessionStart afterward for a *different* id still works normally.
        engine
            .dispatch(EngineCommand::SessionEvent {
                id: "real".into(),
                event: SessionEvent::SessionStart,
                project: Some("api-gateway".into()),
                cwd: Some("/x".into()),
                entrypoint: None,
                started_at_ms: Some(1000),
            })
            .await;
        let diff = diffs.recv().await.unwrap();
        let SessionDiff::Upserted(view) = diff else { panic!("expected Upserted") };
        assert_eq!(view.id, "real");
        assert_eq!(engine.snapshot().await.len(), 1);
    }

    #[tokio::test]
    async fn categorization_updates_in_place_without_a_new_session() {
        let engine = EngineHandle::spawn();
        engine
            .dispatch(EngineCommand::SessionEvent {
                id: "s1".into(),
                event: SessionEvent::SessionStart,
                project: Some("api-gateway".into()),
                cwd: Some("/x".into()),
                entrypoint: None,
                started_at_ms: Some(0),
            })
            .await;
        engine
            .dispatch(EngineCommand::SetCategory { id: "s1".into(), category: "Backend / API".into() })
            .await;

        let snapshot = engine.snapshot().await;
        assert_eq!(snapshot.len(), 1, "categorization must not create a duplicate session");
        assert_eq!(snapshot[0].category, "Backend / API");
    }

    #[tokio::test]
    async fn notification_then_resolve_waiting_round_trip() {
        let engine = EngineHandle::spawn();
        engine
            .dispatch(EngineCommand::SessionEvent {
                id: "s1".into(),
                event: SessionEvent::SessionStart,
                project: None,
                cwd: None,
                entrypoint: None,
                started_at_ms: None,
            })
            .await;
        engine
            .dispatch(EngineCommand::SessionEvent {
                id: "s1".into(),
                event: SessionEvent::Notification,
                project: None,
                cwd: None,
                entrypoint: None,
                started_at_ms: None,
            })
            .await;
        assert_eq!(engine.snapshot().await[0].state, SessionState::Waiting);

        engine
            .dispatch(EngineCommand::ResolveWaiting { id: "s1".into(), task: "Resumed".into() })
            .await;
        let snapshot = engine.snapshot().await;
        assert_eq!(snapshot[0].state, SessionState::Working);
        assert_eq!(snapshot[0].task, "Resumed");
    }

    /// A session that's gone quiet (no hook activity) for longer than the
    /// TTL is presumed abandoned and moved to Idle — this is what catches a
    /// closed terminal/killed process that never fires `SessionEnd`.
    #[tokio::test]
    async fn sweep_idle_moves_a_stale_working_session_to_idle() {
        let engine = EngineHandle::spawn();
        engine
            .dispatch(EngineCommand::SessionEvent {
                id: "s1".into(),
                event: SessionEvent::SessionStart,
                project: None,
                cwd: None,
                entrypoint: None,
                started_at_ms: Some(0),
            })
            .await;

        // `last_activity_ms` is stamped with the real wall clock at creation
        // (`crate::now_ms()`), so the sweep's `now_ms` must be on that same
        // scale, not an arbitrary small test number.
        let started = crate::now_ms();

        // Not stale yet: "now" is within the TTL of last activity.
        engine.dispatch(EngineCommand::SweepIdle { now_ms: started + 1_000, ttl_ms: 10_000 }).await;
        assert_eq!(engine.snapshot().await[0].state, SessionState::Working);

        // Stale: TTL has elapsed with no activity in between.
        engine.dispatch(EngineCommand::SweepIdle { now_ms: started + 20_000, ttl_ms: 10_000 }).await;
        assert_eq!(engine.snapshot().await[0].state, SessionState::Idle);
    }

    /// A Waiting session (genuinely blocked on the user, not abandoned) must
    /// never be swept into Idle just because time has passed — only a
    /// Working session can go stale this way.
    #[tokio::test]
    async fn sweep_idle_never_touches_a_waiting_session() {
        let engine = EngineHandle::spawn();
        engine
            .dispatch(EngineCommand::SessionEvent {
                id: "s1".into(),
                event: SessionEvent::SessionStart,
                project: None,
                cwd: None,
                entrypoint: None,
                started_at_ms: Some(0),
            })
            .await;
        engine
            .dispatch(EngineCommand::SessionEvent {
                id: "s1".into(),
                event: SessionEvent::Notification,
                project: None,
                cwd: None,
                entrypoint: None,
                started_at_ms: None,
            })
            .await;

        engine.dispatch(EngineCommand::SweepIdle { now_ms: crate::now_ms() + 999_999_999, ttl_ms: 10_000 }).await;
        assert_eq!(engine.snapshot().await[0].state, SessionState::Waiting);
    }

    /// Activity (a real hook event) after a session has already gone Idle
    /// must bring it back to Working, same as any other state per the
    /// transition table — the sweep isn't a one-way door.
    #[tokio::test]
    async fn activity_after_idle_returns_to_working() {
        let engine = EngineHandle::spawn();
        engine
            .dispatch(EngineCommand::SessionEvent {
                id: "s1".into(),
                event: SessionEvent::SessionStart,
                project: None,
                cwd: None,
                entrypoint: None,
                started_at_ms: Some(0),
            })
            .await;
        engine.dispatch(EngineCommand::SweepIdle { now_ms: crate::now_ms() + 999_999_999, ttl_ms: 10_000 }).await;
        assert_eq!(engine.snapshot().await[0].state, SessionState::Idle);

        engine
            .dispatch(EngineCommand::SessionEvent {
                id: "s1".into(),
                event: SessionEvent::ToolActivity,
                project: None,
                cwd: None,
                entrypoint: None,
                started_at_ms: None,
            })
            .await;
        assert_eq!(engine.snapshot().await[0].state, SessionState::Working);
    }

    /// Phase 2 roadmap item 6 ("Jump to session"): a session's `entrypoint`
    /// must be captured at creation time, defaulting to `"cli"` when the
    /// caller doesn't supply one (the common case), and preserved as-is when
    /// it does (e.g. `"claude-desktop"`).
    #[tokio::test]
    async fn session_start_captures_entrypoint_defaulting_to_cli() {
        let engine = EngineHandle::spawn();
        engine
            .dispatch(EngineCommand::SessionEvent {
                id: "s1".into(),
                event: SessionEvent::SessionStart,
                project: None,
                cwd: None,
                entrypoint: None,
                started_at_ms: None,
            })
            .await;
        assert_eq!(engine.snapshot().await[0].entrypoint, "cli");

        engine
            .dispatch(EngineCommand::SessionEvent {
                id: "s2".into(),
                event: SessionEvent::SessionStart,
                project: None,
                cwd: None,
                entrypoint: Some("claude-desktop".into()),
                started_at_ms: None,
            })
            .await;
        let snapshot = engine.snapshot().await;
        let s2 = snapshot.iter().find(|v| v.id == "s2").unwrap();
        assert_eq!(s2.entrypoint, "claude-desktop");
    }
}
