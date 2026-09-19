//! The engine actor: single owner of live session state (plan §2). One task
//! owns the session map and applies every mutation itself via an mpsc
//! command channel — not a shared `Mutex<HashMap>` — so there's exactly one
//! place state changes happen (auditable, no lock contention) and every
//! change is broadcast to subscribers (the WS layer, M6) as a diff.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::collector::SubagentInfo;
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
    pub state: SessionState,
    /// Short, stable name for the session (e.g. "Auth middleware refactor"),
    /// set once at summary time and never refreshed after — distinct
    /// from `desc`, which is the live, changing "what's happening right
    /// now" line (Phase 2 design change: this used to be a single `task`
    /// field before the title/description split).
    pub title: String,
    pub desc: String,
    pub started_at_ms: i64,
    /// Real cumulative token/cost/context-window figures parsed from the
    /// transcript's own `usage` objects (Phase 2 design change — see
    /// `collector::extract_usage_metrics`). `ctx_max` is 0 until the first
    /// real assistant turn with usage data has been seen — the frontend
    /// treats that as "no metrics yet" rather than rendering a bogus 0%
    /// full bar.
    pub tokens: i64,
    pub cost: f64,
    pub ctx_used: i64,
    pub ctx_max: i64,
    /// Subagents spawned by this session (Phase 2 design change), re-derived
    /// wholesale on every `"stop"` hook from `collector::list_subagents` —
    /// never mutated in place. Empty for the overwhelming majority of
    /// sessions that never spawn one.
    #[serde(default)]
    pub subs: Vec<SubagentInfo>,
    /// Real task-tracking data (Phase 2 design change) for sessions that
    /// used `TaskCreate`/`TaskUpdate` — absence (`None`) is the normal case,
    /// not an error; most sessions never create tracked tasks.
    #[serde(default)]
    pub plan: Option<PlanView>,
    /// Last time a real hook/user-activity event touched this session.
    /// Drives the idle sweep (`EngineCommand::SweepIdle`): a `Working`
    /// session with no activity for longer than the configured TTL is
    /// presumed abandoned (terminal closed, process killed — anything that
    /// never gets to fire a clean `TurnEnd`/`SessionEnd`) and moved to
    /// `Done` rather than sitting at `Working` forever. Also used by
    /// `orchestrator`'s separate done-sweeper to age a quiet `Done` session
    /// into `Idle`. Not serialized to the frontend — it's purely an
    /// engine-internal bookkeeping field.
    #[serde(skip)]
    pub last_activity_ms: i64,
}

/// A single tracked step from `~/.claude/tasks/<session_id>/*.json`
/// (Phase 2 design change — real `TaskCreate`/`TaskUpdate` data, not the
/// deprecated `TodoWrite`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanStep {
    pub id: String,
    pub subject: String,
    pub done: bool,
}

/// A session's linked execution plan — present only for sessions that
/// actually used `TaskCreate`, which is most sessions' normal case of
/// having none at all (see `SessionView::plan`'s doc comment).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanView {
    pub title: String,
    pub steps: Vec<PlanStep>,
}

impl SessionView {
    pub fn new_starting(id: SessionId, project: String, cwd: String, entrypoint: String, started_at_ms: i64) -> Self {
        Self {
            id,
            tool: "Claude Code".to_string(),
            project,
            cwd,
            entrypoint,
            state: SessionState::Working,
            title: "Starting…".to_string(),
            desc: "Starting…".to_string(),
            started_at_ms,
            tokens: 0,
            cost: 0.0,
            ctx_used: 0,
            ctx_max: 0,
            subs: Vec::new(),
            plan: None,
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
    /// Creates the session (as Starting…/Working) if it doesn't exist yet.
    SessionEvent {
        id: SessionId,
        event: SessionEvent,
        project: Option<String>,
        cwd: Option<String>,
        entrypoint: Option<String>,
        started_at_ms: Option<i64>,
    },
    /// Set once, at summary time — see `SessionView::title`'s doc
    /// comment for why this is separate from `SetDesc`.
    SetTitle { id: SessionId, title: String },
    SetDesc { id: SessionId, desc: String },
    /// Real usage-derived metrics (`collector::extract_usage_metrics`),
    /// re-sent wholesale on every refresh rather than incrementally updated.
    SetMetrics { id: SessionId, tokens: i64, cost: f64, ctx_used: i64, ctx_max: i64 },
    /// Re-derived wholesale on every `"stop"` hook from
    /// `collector::list_subagents` — replaces the entire vec rather than
    /// patching individual entries.
    SetSubagents { id: SessionId, subs: Vec<SubagentInfo> },
    /// Re-derived wholesale on every `"stop"` hook from the real
    /// `~/.claude/tasks/` files. `None` is the normal case for a session
    /// that never used `TaskCreate`.
    SetPlan { id: SessionId, plan: Option<PlanView> },
    /// Corrects `last_activity_ms` after the fact — used only by
    /// reconstruction on startup, which otherwise has `SessionEvent`
    /// unconditionally stamp it with "now" (right, for a live hook; wrong
    /// for a replayed historical event, which would reset a genuinely stale
    /// session's idle clock and make the sweep wait a full fresh `idle_ttl`
    /// after every restart before correcting a "Working" ghost that hasn't
    /// actually been touched in hours). Bookkeeping-only: no diff broadcast,
    /// since the field is `#[serde(skip)]` and invisible to the frontend.
    SetLastActivity { id: SessionId, last_activity_ms: i64 },
    /// Approve/Reject/Send from the drawer (plan §7) — resolves a Waiting
    /// session back to Working with an updated desc line.
    ResolveWaiting { id: SessionId, desc: String },
    /// Periodic tick (see `run_idle_sweeper`): any session still `Working`
    /// with no activity for at least `ttl_ms` is presumed to have stopped
    /// (crashed/closed without a clean `TurnEnd`/`SessionEnd`) and moved to
    /// `Done`. Sessions that are `Waiting` (genuinely blocked on the user,
    /// not abandoned) or already `Done`/`Idle` are left alone — aging a
    /// quiet `Done` session into `Idle` is a separate sweep, in
    /// `orchestrator.rs`, since it needs `EndedSessions` to exempt
    /// explicitly-closed sessions.
    SweepIdle { now_ms: i64, ttl_ms: i64 },
    /// Explicit user-initiated delete (the board's "Delete" action) — drops
    /// the session from the live map entirely and broadcasts `Removed`
    /// rather than transitioning it through the state machine, since there's
    /// no state that means "gone." The caller (`api::delete_session`) is
    /// responsible for durably recording the dismissal too, or this id would
    /// simply be reconstructed again on the next restart.
    RemoveSession { id: SessionId },
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
                                SessionView::new_starting(
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
                    EngineCommand::SetTitle { id, title } => {
                        if let Some(view) = sessions.get_mut(&id) {
                            view.title = title;
                            let _ = diff_tx_actor.send(SessionDiff::Upserted(view.clone()));
                        }
                    }
                    EngineCommand::SetDesc { id, desc } => {
                        if let Some(view) = sessions.get_mut(&id) {
                            view.desc = desc;
                            let _ = diff_tx_actor.send(SessionDiff::Upserted(view.clone()));
                        }
                    }
                    EngineCommand::SetMetrics { id, tokens, cost, ctx_used, ctx_max } => {
                        if let Some(view) = sessions.get_mut(&id) {
                            view.tokens = tokens;
                            view.cost = cost;
                            view.ctx_used = ctx_used;
                            view.ctx_max = ctx_max;
                            let _ = diff_tx_actor.send(SessionDiff::Upserted(view.clone()));
                        }
                    }
                    EngineCommand::SetSubagents { id, subs } => {
                        if let Some(view) = sessions.get_mut(&id) {
                            view.subs = subs;
                            let _ = diff_tx_actor.send(SessionDiff::Upserted(view.clone()));
                        }
                    }
                    EngineCommand::SetPlan { id, plan } => {
                        if let Some(view) = sessions.get_mut(&id) {
                            view.plan = plan;
                            let _ = diff_tx_actor.send(SessionDiff::Upserted(view.clone()));
                        }
                    }
                    EngineCommand::SetLastActivity { id, last_activity_ms } => {
                        if let Some(view) = sessions.get_mut(&id) {
                            view.last_activity_ms = last_activity_ms;
                        }
                    }
                    EngineCommand::ResolveWaiting { id, desc } => {
                        if let Some(view) = sessions.get_mut(&id) {
                            view.state = state::transition(view.state, SessionEvent::UserReply);
                            view.desc = desc;
                            view.last_activity_ms = crate::now_ms();
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
                    EngineCommand::RemoveSession { id } => {
                        if sessions.remove(&id).is_some() {
                            let _ = diff_tx_actor.send(SessionDiff::Removed(id));
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
/// had no hook activity for at least `ttl` and moves them to `Done` (see
/// `EngineCommand::SweepIdle`). Started once from `bootstrap::start` and runs
/// for the lifetime of the app — this is what catches a session whose
/// process was killed or window closed without ever firing a clean
/// `TurnEnd`/`SessionEnd` (a hook is cooperative; nothing fires it for you).
/// The separate `Done` -> `Idle` aging sweep lives in `orchestrator.rs`.
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
    async fn session_start_creates_a_starting_working_session() {
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
        assert_eq!(view.title, "Starting…");
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
            .dispatch(EngineCommand::ResolveWaiting { id: "s1".into(), desc: "Resumed".into() })
            .await;
        let snapshot = engine.snapshot().await;
        assert_eq!(snapshot[0].state, SessionState::Working);
        assert_eq!(snapshot[0].desc, "Resumed");
    }

    /// A session that's gone quiet (no hook activity) for longer than the
    /// TTL is presumed abandoned and moved to Done — the crash-safety
    /// fallback for a closed terminal/killed process that never fires a
    /// clean `TurnEnd`/`SessionEnd` (see `state::transition`'s doc comment:
    /// `Working` + `IdleTimeout` now means "presumably stopped working",
    /// same conclusion a clean `Stop` hook would reach, just detected by
    /// silence instead).
    #[tokio::test]
    async fn sweep_idle_moves_a_stale_working_session_to_done() {
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
        assert_eq!(engine.snapshot().await[0].state, SessionState::Done);
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

    /// Activity (a real hook event) after a session has already gone Done
    /// (via the idle-sweep safety net) must bring it back to Working, same
    /// as any other state per the transition table — the sweep isn't a
    /// one-way door.
    #[tokio::test]
    async fn activity_after_done_returns_to_working() {
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
        assert_eq!(engine.snapshot().await[0].state, SessionState::Done);

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

    #[tokio::test]
    async fn set_subagents_replaces_the_whole_vec_and_set_plan_replaces_the_option() {
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
        assert!(engine.snapshot().await[0].subs.is_empty());
        assert!(engine.snapshot().await[0].plan.is_none());

        let sub = SubagentInfo {
            id: "agent-1".into(),
            title: "Fix flaky test".into(),
            desc: "Investigating retries".into(),
            state: "Working".into(),
            tokens: 100,
            cost: 0.01,
            ctx_used: 500,
            ctx_max: 1_000_000,
        };
        engine.dispatch(EngineCommand::SetSubagents { id: "s1".into(), subs: vec![sub.clone()] }).await;
        assert_eq!(engine.snapshot().await[0].subs, vec![sub]);

        let plan = PlanView {
            title: "Backend / API plan".into(),
            steps: vec![PlanStep { id: "1".into(), subject: "Do the thing".into(), done: true }],
        };
        engine.dispatch(EngineCommand::SetPlan { id: "s1".into(), plan: Some(plan.clone()) }).await;
        assert_eq!(engine.snapshot().await[0].plan, Some(plan));
    }

    #[tokio::test]
    async fn remove_session_drops_it_from_the_snapshot_and_broadcasts_removed() {
        let engine = EngineHandle::spawn();
        let mut diffs = engine.subscribe();
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
        diffs.recv().await.unwrap(); // the SessionStart upsert

        engine.dispatch(EngineCommand::RemoveSession { id: "s1".into() }).await;
        let diff = diffs.recv().await.unwrap();
        assert!(matches!(diff, SessionDiff::Removed(id) if id == "s1"));
        assert!(engine.snapshot().await.is_empty());

        // Removing an id that isn't present is a no-op, not a spurious broadcast.
        engine.dispatch(EngineCommand::RemoveSession { id: "ghost".into() }).await;
        assert!(engine.snapshot().await.is_empty());
    }
}
