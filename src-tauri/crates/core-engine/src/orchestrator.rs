//! Wires the pieces built in M2–M7 into one running system: listens on the
//! hook UDS (`hook_socket`), maps each `HookEvent` to the state machine
//! (`state`/`engine`) and, on `session-start`, tails the transcript
//! (`collector`) and runs the categorization pipeline (`categorize`).
//!
//! This module didn't exist before M10 — the earlier milestones built and
//! tested each piece (the collector's tailing logic, the categorization
//! pipeline) in isolation; this is the actual production glue that makes a
//! real Claude Code hook firing end up moving a real card on the board.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, Mutex};

use crate::categorize::{categorize_session, refresh_task_summary, CategorizationConfig};
use crate::collector::{
    extract_entrypoint, extract_initiating_prompt, extract_latest_activity, extract_usage_metrics, list_subagents,
    project_name_from_cwd, read_lines_from_start, tail_new_lines, transcript_path, TailCheckpoints,
};
use crate::engine::{EngineCommand, EngineHandle};
use crate::hook_socket::{self, HookEvent};
use crate::memory_repo::MemoryRepo;
use crate::ollama::OllamaClient;
use crate::plan::read_session_plan;
use crate::state::SessionEvent;

pub struct OrchestratorConfig {
    pub claude_projects_dir: PathBuf,
    /// `~/.claude/tasks` — real `TaskCreate`/`TaskUpdate` data, read on every
    /// `"stop"` hook to populate a session's plan chip/panel (Phase 2 §4).
    pub tasks_dir: PathBuf,
    pub checkpoint_path: PathBuf,
    /// Unix domain socket the hook-bridge binary connects to. Defaults to
    /// the real shared path (`hook_socket::socket_path()`); tests override
    /// this with an isolated per-test temp path so parallel test runs don't
    /// collide on the same real socket.
    pub socket_path: PathBuf,
    /// How long to poll for the transcript file to appear after
    /// `SessionStart` fires, before giving up (plan §4).
    pub transcript_wait: Duration,
    /// On startup, a previously-seen session (durable row in `memories`) is
    /// only restored to the board if its transcript file was modified more
    /// recently than this — otherwise it's treated as long-finished rather
    /// than resurrected as a false "still working" ghost (see
    /// `reconstruct_live_sessions`).
    pub reconstruction_recency: Duration,
    /// A `Working` session with no hook activity for at least this long is
    /// presumed abandoned (closed terminal/window, killed process — anything
    /// that never fires a clean `TurnEnd`/`SessionEnd`) and moved to `Done`
    /// — the same safety-net role it always had, just aimed at `Done` now
    /// instead of `Idle` (user report: a session should read as "done" the
    /// moment it stops working, not sit at `Working` between turns — see
    /// `state::transition`'s doc comment).
    pub idle_ttl: Duration,
    /// A `Done` session — one that stopped working rather than one you
    /// explicitly closed (see `EndedSessions`, which is exempted from this)
    /// — that's sat quiet for at least this long ages into `Idle`. Checked
    /// by `run_done_sweeper`, a separate sweep from the one above because it
    /// needs `EndedSessions` to know which `Done` sessions to leave alone.
    pub done_ttl: Duration,
    /// How often the idle/done sweeps run. Independent of `idle_ttl`/
    /// `done_ttl` so the sweeps themselves can be cheap/frequent without
    /// making sessions age any faster than those TTLs allow.
    pub idle_sweep_interval: Duration,
    /// Durable record of session ids that have received a real `SessionEnd`
    /// hook (see `EndedSessions`), so a restart's reconstruction can tell a
    /// genuinely finished session apart from one that's still working —
    /// transcript-file recency alone can't distinguish those two cases.
    pub ended_sessions_path: PathBuf,
    /// Durable record of session ids currently blocked on the user (see
    /// `WaitingSessions`) — a `memories` row can't otherwise tell "genuinely
    /// waiting on a permission prompt/question" apart from "just idle", and
    /// nothing re-fires `Notification` on its own to correct a wrong guess,
    /// unlike `Working`/`Done` which self-correct from the next hook or the
    /// idle sweep.
    pub waiting_sessions_path: PathBuf,
}

/// Reads a `Duration` (in seconds) from an env var, falling back to
/// `default` when unset or unparseable. Lets e2e testing shorten the
/// otherwise-10-minute idle/done TTLs without touching source.
fn duration_from_env_secs(var: &str, default: Duration) -> Duration {
    std::env::var(var).ok().and_then(|v| v.parse().ok()).map(Duration::from_secs).unwrap_or(default)
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            claude_projects_dir: crate::first_run::claude_projects_dir(),
            tasks_dir: crate::first_run::claude_tasks_dir(),
            checkpoint_path: crate::first_run::app_data_dir().join("tail-checkpoints.json"),
            socket_path: hook_socket::socket_path(),
            // Bug fix (user report — real terminal sessions never appeared
            // on the board): 8s was nowhere near "a few seconds" in
            // practice. Measured directly against two real interactive `claude`
            // sessions: 22s and 27s elapsed between the process starting
            // (SessionStart firing) and the first message actually being
            // typed (reading the banner, thinking, typing) — both well past
            // the old 8s deadline, so the poll gave up and the session was
            // silently abandoned before the user ever sent anything. This
            // only affects genuinely idle real estate (a cheap file
            // stat+read every 150ms in one spawned task) while waiting, not
            // anything blocking, so erring long here is nearly free — a
            // session that's truly abandoned still eventually gets skipped.
            transcript_wait: Duration::from_secs(5 * 60),
            // A day comfortably covers "closed the app overnight, reopened
            // the next morning" without resurrecting genuinely old sessions.
            reconstruction_recency: Duration::from_secs(24 * 60 * 60),
            // Generous enough that normal thinking/typing pauses between
            // turns never trip it, but short enough that a closed terminal
            // doesn't sit looking "Working" for the rest of the day.
            idle_ttl: duration_from_env_secs("SESSIONBOARD_IDLE_TTL_SECS", Duration::from_secs(10 * 60)),
            // User-specified: a session that stopped working sits at `Done`
            // for 10 quiet minutes before aging into `Idle`.
            done_ttl: duration_from_env_secs("SESSIONBOARD_DONE_TTL_SECS", Duration::from_secs(10 * 60)),
            idle_sweep_interval: duration_from_env_secs("SESSIONBOARD_SWEEP_INTERVAL_SECS", Duration::from_secs(60)),
            ended_sessions_path: crate::first_run::app_data_dir().join("ended-sessions.json"),
            waiting_sessions_path: crate::first_run::app_data_dir().join("waiting-sessions.json"),
        }
    }
}

/// Durable record of which session ids have received a real `SessionEnd`
/// hook. `memories` rows (project/category/prompt) survive a restart, but
/// carry no notion of the session's lifecycle state — a session that ended
/// cleanly ten minutes ago and one still actively working both just look
/// like "a memory row with a recently-touched transcript file". This closes
/// that gap with the same flat-file pattern as `TailCheckpoints` (plan §4):
/// a handful of ids, not data that needs a real database.
pub struct EndedSessions {
    path: PathBuf,
    ids: HashSet<String>,
}

impl EndedSessions {
    pub fn load(path: &Path) -> Self {
        let ids = std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        Self { path: path.to_path_buf(), ids }
    }

    pub fn contains(&self, id: &str) -> bool {
        self.ids.contains(id)
    }

    pub fn mark_ended(&mut self, id: String) {
        self.ids.insert(id);
    }

    /// Evicts an id, if present. Called whenever any hook other than
    /// `SessionEnd` arrives for a session already recorded here — receiving
    /// a live hook at all is proof the session is alive again (a resumed
    /// conversation reusing its original session id), so the stale "ended"
    /// record must not keep forcing it back to `Done` on the next restart's
    /// reconstruction. Returns whether anything actually changed, so callers
    /// only pay for a save when needed.
    pub fn unmark_ended(&mut self, id: &str) -> bool {
        self.ids.remove(id)
    }

    pub fn save(&self) -> std::io::Result<()> {
        let pretty = serde_json::to_string_pretty(&self.ids)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, pretty)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// Durable record of session ids currently blocked on the user (a real
/// `Notification` — permission prompt/idle prompt — or the `AskUserQuestion`
/// special case). Same flat-file pattern as `EndedSessions`, for the same
/// reason: `memories` rows survive a restart but carry no lifecycle state,
/// and `Waiting` specifically can't be recovered by any other heuristic —
/// unlike a stale `Working` guess (corrected by the idle sweep) or a
/// finished session (recorded in `EndedSessions`), nothing fires a fresh
/// `Notification` on its own just because the app restarted. Without this,
/// a session that was genuinely waiting on the user when the app last
/// restarted permanently loses that status (reconstructed as a plain
/// `Working` guess, then promptly swept to `Idle` once its real staleness
/// is accounted for).
pub struct WaitingSessions {
    path: PathBuf,
    ids: HashSet<String>,
}

impl WaitingSessions {
    pub fn load(path: &Path) -> Self {
        let ids = std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        Self { path: path.to_path_buf(), ids }
    }

    pub fn contains(&self, id: &str) -> bool {
        self.ids.contains(id)
    }

    pub fn mark_waiting(&mut self, id: String) {
        self.ids.insert(id);
    }

    /// Evicts an id, if present — called whenever anything resolves the
    /// wait (further hook activity, or the user approving/rejecting/
    /// replying from the board). Returns whether anything changed, so
    /// callers only pay for a save when needed.
    pub fn unmark_waiting(&mut self, id: &str) -> bool {
        self.ids.remove(id)
    }

    pub fn save(&self) -> std::io::Result<()> {
        let pretty = serde_json::to_string_pretty(&self.ids)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, pretty)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// Restores the board from durable memory on Core Engine startup (user
/// feedback: sessions should reappear with their real categories after a
/// restart, not vanish until the next hook event happens to touch them).
///
/// Live session state is deliberately never persisted (plan §2/§10) — it's
/// reconstructed here instead, from the one durable source that exists per
/// session: its `memories` row (category, project, cwd, original prompt),
/// plus `EndedSessions`/`WaitingSessions` for the bits of lifecycle state
/// that *are* persisted. A session is restored at all if — and only if — its
/// transcript file was modified more recently than `reconstruction_recency`,
/// treating that as evidence it's still relevant rather than long finished.
/// From there: `ended_sessions` wins first (a real close stays Done
/// forever), then `waiting_sessions` (genuinely blocked on you survives a
/// restart the same way). For anything else, rather than defaulting to a
/// `Working` guess and making every restart wait a full sweep interval to
/// correct it, the elapsed time since the transcript's real mtime is used to
/// compute directly where `idle_ttl`/`done_ttl` would already have taken it
/// (see `state::transition`'s doc comment) — `Working` if genuinely recent,
/// `Done` if quiet long enough to have stopped, `Idle` if quiet long enough
/// to have aged past that too. Still a heuristic for the truly ambiguous
/// case (a process killed mid-turn looks identical to one that's still
/// running until enough time passes) — the next real hook event corrects it
/// from here same as always.
async fn reconstruct_live_sessions(
    engine: &EngineHandle,
    repo: &MemoryRepo,
    orch_config: &OrchestratorConfig,
    ended_sessions: &EndedSessions,
    waiting_sessions: &WaitingSessions,
) {
    let Ok(memories) = repo.list_session_memories().await else { return };

    for memory in memories {
        if memory.cwd.is_empty() {
            continue; // backfilled historical entry with no recoverable cwd — see first_run.rs
        }

        let path = transcript_path(&orch_config.claude_projects_dir, &memory.cwd, &memory.session_id);
        let Ok(metadata) = std::fs::metadata(&path) else { continue };
        let Ok(modified) = metadata.modified() else { continue };
        let Ok(age) = std::time::SystemTime::now().duration_since(modified) else { continue };
        if age > orch_config.reconstruction_recency {
            continue; // long finished — don't resurrect as a false "still working" ghost
        }

        engine
            .dispatch(EngineCommand::SessionEvent {
                id: memory.session_id.clone(),
                event: SessionEvent::SessionStart,
                project: Some(memory.project.clone()),
                cwd: Some(memory.cwd.clone()),
                // No durable record of the original entrypoint exists in the
                // `memories` schema — defaults to "cli", a reasonable, low-
                // stakes approximation matching how this function already
                // approximates live `state` on restart.
                entrypoint: None,
                started_at_ms: Some(memory.created_at),
            })
            .await;
        // `SessionEvent` above just stamped `last_activity_ms` with "now" —
        // right for a live hook, wrong here: this is a replayed historical
        // event, and leaving it at "now" would reset the idle clock on every
        // restart, making a genuinely stale session sit as a "Working" ghost
        // for a full fresh `idle_ttl` each time before the sweep catches it.
        // Correct it to the transcript's real last-modified time instead.
        if let Ok(modified_ms) = modified.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64) {
            engine
                .dispatch(EngineCommand::SetLastActivity { id: memory.session_id.clone(), last_activity_ms: modified_ms })
                .await;
        }
        if ended_sessions.contains(&memory.session_id) {
            engine
                .dispatch(EngineCommand::SessionEvent {
                    id: memory.session_id.clone(),
                    event: SessionEvent::SessionEnd,
                    project: None,
                    cwd: None,
                    entrypoint: None,
                    started_at_ms: None,
                })
                .await;
        } else if waiting_sessions.contains(&memory.session_id) {
            // Restore Waiting too — see `WaitingSessions`'s doc comment for
            // why this can't just fall out of the transcript-recency
            // heuristic the way Working/Done can.
            engine
                .dispatch(EngineCommand::SessionEvent {
                    id: memory.session_id.clone(),
                    event: SessionEvent::Notification,
                    project: None,
                    cwd: None,
                    entrypoint: None,
                    started_at_ms: None,
                })
                .await;
        } else if age >= orch_config.idle_ttl {
            // Quiet long enough to have stopped working — dispatch `TurnEnd`
            // to take the freshly-created `Working` session (from the
            // `SessionStart` dispatch above, this reconstruction pass) to
            // `Done`, computed directly from the transcript's real elapsed
            // time instead of defaulting to Working and making every restart
            // wait a fresh sweep interval to catch up.
            engine
                .dispatch(EngineCommand::SessionEvent {
                    id: memory.session_id.clone(),
                    event: SessionEvent::TurnEnd,
                    project: None,
                    cwd: None,
                    entrypoint: None,
                    started_at_ms: None,
                })
                .await;
            if age >= orch_config.idle_ttl + orch_config.done_ttl {
                // Quiet long enough to have aged past Done into Idle too —
                // `IdleTimeout` is state-dependent (see `state::transition`'s
                // doc comment), so this must be dispatched *after* the
                // `TurnEnd` above reaches Done, not instead of it, or it'd
                // apply to the still-Working session and land on Done again.
                engine
                    .dispatch(EngineCommand::SessionEvent {
                        id: memory.session_id.clone(),
                        event: SessionEvent::IdleTimeout,
                        project: None,
                        cwd: None,
                        entrypoint: None,
                        started_at_ms: None,
                    })
                    .await;
            }
        }
        engine.dispatch(EngineCommand::SetCategory { id: memory.session_id.clone(), category: memory.category }).await;
        // No durable record of the original LLM-generated title exists in
        // the `memories` schema (same gap as `entrypoint` — see its doc
        // comment above) — approximate it from the same prompt text used
        // for `desc`, just trimmed to a title-length handful of words.
        let title = memory.text.split_whitespace().take(4).collect::<Vec<_>>().join(" ");
        engine.dispatch(EngineCommand::SetTitle { id: memory.session_id.clone(), title }).await;
        engine.dispatch(EngineCommand::SetDesc { id: memory.session_id.clone(), desc: memory.text.chars().take(80).collect() }).await;
        // Restore real metrics from the transcript too, so a reconstructed
        // card doesn't sit at zero until the next "stop" hook happens to fire.
        if let Some(m) = extract_usage_metrics(&path) {
            engine
                .dispatch(EngineCommand::SetMetrics {
                    id: memory.session_id,
                    tokens: m.tokens,
                    cost: m.cost,
                    ctx_used: m.ctx_used,
                    ctx_max: m.ctx_max,
                })
                .await;
        }
    }
}

/// Background loop, the `Done` counterpart to `engine::run_idle_sweeper`:
/// every `interval`, ages any `Done` session that's been quiet for at least
/// `done_ttl` into `Idle` (user report — a session that stopped working
/// should eventually settle into Idle, not sit at Done forever). Lives here
/// rather than in `engine.rs` because it needs `EndedSessions` to skip
/// sessions that reached `Done` via a real, explicit close — those stay
/// `Done` forever, since that's a genuine end, not just silence.
async fn run_done_sweeper(
    engine: EngineHandle,
    ended_sessions: Arc<Mutex<EndedSessions>>,
    done_ttl: Duration,
    interval: Duration,
) {
    let done_ttl_ms = done_ttl.as_millis() as i64;
    loop {
        tokio::time::sleep(interval).await;
        done_sweep_once(&engine, &ended_sessions, done_ttl_ms, crate::now_ms()).await;
    }
}

/// One pass of `run_done_sweeper`'s logic, split out so it's directly
/// testable without waiting on a real sleep interval.
async fn done_sweep_once(engine: &EngineHandle, ended_sessions: &Mutex<EndedSessions>, done_ttl_ms: i64, now_ms: i64) {
    let ended = ended_sessions.lock().await;
    for view in engine.snapshot().await {
        if view.state == crate::state::SessionState::Done
            && now_ms - view.last_activity_ms >= done_ttl_ms
            && !ended.contains(&view.id)
        {
            engine
                .dispatch(EngineCommand::SessionEvent {
                    id: view.id,
                    event: SessionEvent::IdleTimeout,
                    project: None,
                    cwd: None,
                    entrypoint: None,
                    started_at_ms: None,
                })
                .await;
        }
    }
}

/// Starts the UDS listener and processes hook events until the process
/// exits. Each event is handled on its own spawned task so a slow
/// categorization call for one session never delays state updates for
/// another.
pub async fn run(
    engine: EngineHandle,
    repo: Arc<MemoryRepo>,
    ollama: Arc<dyn OllamaClient>,
    cat_config: Arc<CategorizationConfig>,
    orch_config: Arc<OrchestratorConfig>,
    waiting_sessions: Arc<Mutex<WaitingSessions>>,
) {
    let ended_sessions = Arc::new(Mutex::new(EndedSessions::load(&orch_config.ended_sessions_path)));
    reconstruct_live_sessions(
        &engine,
        &repo,
        &orch_config,
        &*ended_sessions.lock().await,
        &*waiting_sessions.lock().await,
    )
    .await;

    tokio::spawn(run_done_sweeper(
        engine.clone(),
        ended_sessions.clone(),
        orch_config.done_ttl,
        orch_config.idle_sweep_interval,
    ));

    let (tx, mut rx) = mpsc::channel::<HookEvent>(256);
    tokio::spawn(hook_socket::listen_at(orch_config.socket_path.clone(), tx));

    let checkpoints = Arc::new(Mutex::new(TailCheckpoints::load(&orch_config.checkpoint_path)));

    while let Some(event) = rx.recv().await {
        let engine = engine.clone();
        let repo = repo.clone();
        let ollama = ollama.clone();
        let cat_config = cat_config.clone();
        let orch_config = orch_config.clone();
        let checkpoints = checkpoints.clone();
        let ended_sessions = ended_sessions.clone();
        let waiting_sessions = waiting_sessions.clone();

        tokio::spawn(async move {
            handle_hook_event(
                event,
                &engine,
                &repo,
                ollama.as_ref(),
                &cat_config,
                &orch_config,
                &checkpoints,
                &ended_sessions,
                &waiting_sessions,
            )
            .await;
        });
    }
}

async fn handle_hook_event(
    event: HookEvent,
    engine: &EngineHandle,
    repo: &MemoryRepo,
    ollama: &dyn OllamaClient,
    cat_config: &CategorizationConfig,
    orch_config: &OrchestratorConfig,
    checkpoints: &Arc<Mutex<TailCheckpoints>>,
    ended_sessions: &Arc<Mutex<EndedSessions>>,
    waiting_sessions: &Arc<Mutex<WaitingSessions>>,
) {
    let Some(session_id) = event.payload.get("session_id").and_then(|v| v.as_str()).map(str::to_string) else {
        return;
    };

    // Any real hook other than a fresh `session-end` is live proof this
    // session is not actually done — evict it from the durable ended-record
    // now, rather than waiting for a restart to correct it. Without this, a
    // resumed session stays correctly `Working`/`Waiting` in the running
    // engine (state.rs's Done is revivable), but the *next* restart's
    // reconstruction would immediately force it back to `Done` anyway,
    // since it's still sitting in `ended_sessions`.
    if event.event != "session-end" {
        let mut ended = ended_sessions.lock().await;
        if ended.unmark_ended(&session_id) {
            let _ = ended.save();
        }
    }

    // Mirror the same durable bookkeeping for `Waiting`: this hook either
    // drives the session into Waiting (a real blocking `Notification`, or
    // the `AskUserQuestion` special case below) or it's live proof of the
    // opposite — further activity that resolves any wait already recorded.
    // Kept as its own record (`WaitingSessions`) because, unlike Working/
    // Done, nothing about transcript recency or the idle sweep can ever
    // recover "was genuinely waiting on you" after a restart.
    //
    // `Notification`'s `notification_type` field matters here (regression —
    // user report: a session that had correctly settled into Done got
    // yanked back to Waiting ~60s later): Claude Code fires `Notification`
    // for several reasons, confirmed against the hooks docs —
    // `permission_prompt` (genuinely blocked on you), but also `idle_prompt`
    // (just sitting idle waiting for the next message — the same thing our
    // own Done/Idle timeout sweep already represents, not "blocked on a
    // decision"), plus `auth_success`/`elicitation_*`/`agent_needs_input`/
    // `agent_completed`. Only `idle_prompt` is excluded — every other type
    // (including any future/unrecognized one) still marks Waiting, same as
    // before this fix. Field name is `notification_type`, not `type` — a
    // prior version of this code read the wrong key, which silently made
    // `is_idle_prompt` always false against real payloads (confirmed against
    // code.claude.com/docs/en/hooks.md).
    let notification_type = event.payload.get("notification_type").and_then(|v| v.as_str());
    let is_idle_prompt = event.event == "notification" && notification_type == Some("idle_prompt");
    let drives_waiting = (event.event == "notification" && !is_idle_prompt)
        || (event.event == "pre-tool-use" && event.payload.get("tool_name").and_then(|v| v.as_str()) == Some("AskUserQuestion"));
    if !is_idle_prompt {
        let mut waiting = waiting_sessions.lock().await;
        if drives_waiting {
            waiting.mark_waiting(session_id.clone());
            let _ = waiting.save();
        } else if waiting.unmark_waiting(&session_id) {
            let _ = waiting.save();
        }
    }

    match event.event.as_str() {
        "session-start" => {
            let cwd = event.payload.get("cwd").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let project = project_name_from_cwd(&cwd);
            // "cli" (plain terminal `claude`) vs "claude-desktop" — carried
            // directly on real hook payloads (plan §Phase 2 item 6). Not
            // present -> default to "cli", the common case. Overridden below
            // by `extract_entrypoint` if the transcript itself carries a more
            // specific value (e.g. "claude-vscode") — see that fn's doc
            // comment for why the hook payload alone can't distinguish a
            // third-party VS Code integration like DevSwarm.
            let hook_entrypoint = event.payload.get("entrypoint").and_then(|v| v.as_str()).map(str::to_string);

            // Prefer the path Claude Code itself hands us in the hook
            // payload over recomputing it — more robust than trusting our
            // own sanitize_cwd reconstruction to match exactly in every
            // case, and it's authoritative when present.
            let path = event
                .payload
                .get("transcript_path")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| transcript_path(&orch_config.claude_projects_dir, &cwd, &session_id));

            // Wait for the transcript to actually have extractable content
            // *before* creating the card at all — a session that never
            // produces a prompt (opened and abandoned, or a background/
            // internal invocation) should never appear on the board rather
            // than showing up as a permanently "Uncategorized"/"Starting…"
            // ghost card. This does not reintroduce blocking on the model
            // call (plan §5/§10's actual concern) — only on cheap file
            // polling; categorization itself still runs async afterward.
            //
            // Reads fresh from the start every poll (`read_lines_from_start`,
            // not `tail_new_lines`) — the initiating prompt sits at a fixed
            // spot near the top of the file, not "whatever's new," and
            // `tail_new_lines`'s checkpoint is a single cursor shared with
            // `stop`'s incremental tailing of the same file. Consuming it
            // here too meant a `stop` (or a duplicate/retried `session-start`
            // delivery) that ran first could advance the checkpoint past the
            // prompt before this loop ever saw it, silently dropping the
            // session forever — a real bug, not a hypothetical one.
            let deadline = tokio::time::Instant::now() + orch_config.transcript_wait;
            let mut prompt = None;
            let mut transcript_entrypoint = None;
            while prompt.is_none() && tokio::time::Instant::now() < deadline {
                if let Ok(lines) = read_lines_from_start(&path) {
                    transcript_entrypoint = extract_entrypoint(&lines);
                    prompt = extract_initiating_prompt(&lines);
                }
                if prompt.is_none() {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
            }
            let Some(prompt) = prompt else {
                return; // genuinely empty/abandoned session — never surfaced
            };
            let entrypoint = transcript_entrypoint.or(hook_entrypoint);

            engine
                .dispatch(EngineCommand::SessionEvent {
                    id: session_id.clone(),
                    event: SessionEvent::SessionStart,
                    project: Some(project.clone()),
                    cwd: Some(cwd.clone()),
                    entrypoint,
                    started_at_ms: Some(crate::now_ms()),
                })
                .await;

            let _ = categorize_session(engine, repo, ollama, cat_config, &session_id, &project, &cwd, "Claude Code", &prompt).await;
        }
        // `idle_prompt` is deliberately excluded (see the `is_idle_prompt`
        // computation above) — it means "sitting idle," not "blocked on
        // you," and dispatching it here would revive a correctly-settled
        // Done/Idle session back to Waiting.
        "notification" if !is_idle_prompt => {
            engine
                .dispatch(EngineCommand::SessionEvent {
                    id: session_id,
                    event: SessionEvent::Notification,
                    project: None,
                    cwd: None,
                    entrypoint: None,
                    started_at_ms: None,
                })
                .await;
        }
        // `AskUserQuestion` blocks the turn on a real interactive choice from
        // the user, same as a permission prompt — but Claude Code doesn't
        // fire a `Notification` hook for it (that's reserved for
        // permission_prompt/idle_prompt/etc., confirmed against the hooks
        // docs), so without this special case the session sits at Working
        // for as long as the user takes to answer, showing "waiting for
        // approval" as a plain busy card instead of Waiting.
        "pre-tool-use"
            if event.payload.get("tool_name").and_then(|v| v.as_str()) == Some("AskUserQuestion") =>
        {
            engine
                .dispatch(EngineCommand::SessionEvent {
                    id: session_id,
                    event: SessionEvent::Notification,
                    project: None,
                    cwd: None,
                    entrypoint: None,
                    started_at_ms: None,
                })
                .await;
        }
        "pre-tool-use" | "post-tool-use" => {
            engine
                .dispatch(EngineCommand::SessionEvent {
                    id: session_id,
                    event: SessionEvent::ToolActivity,
                    project: None,
                    cwd: None,
                    entrypoint: None,
                    started_at_ms: None,
                })
                .await;
        }
        // "stop" fires once per assistant turn (unlike PreToolUse/PostToolUse,
        // which can fire many times per turn) — both the natural point to
        // refresh the "current task" line so it tracks the latest activity
        // instead of freezing after the initial prompt, AND (user report) the
        // real signal that the session just stopped working: dispatches
        // `TurnEnd` (-> Done) rather than `ToolActivity` (-> Working), so a
        // session reads as "done" the moment it finishes responding instead
        // of sitting at Working between turns. The next real activity (a new
        // message, a tool call) revives it to Working exactly like it would
        // from any other state — see `state::transition`'s doc comment.
        "stop" => {
            engine
                .dispatch(EngineCommand::SessionEvent {
                    id: session_id.clone(),
                    event: SessionEvent::TurnEnd,
                    project: None,
                    cwd: None,
                    entrypoint: None,
                    started_at_ms: None,
                })
                .await;

            let cwd = event.payload.get("cwd").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let path = event
                .payload
                .get("transcript_path")
                .and_then(|v| v.as_str())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| transcript_path(&orch_config.claude_projects_dir, &cwd, &session_id));

            if path.exists() {
                let lines = {
                    let mut cp = checkpoints.lock().await;
                    let lines = tail_new_lines(&path, &mut cp).unwrap_or_default();
                    let _ = cp.save();
                    lines
                };
                if let Some(activity) = extract_latest_activity(&lines) {
                    refresh_task_summary(engine, ollama, cat_config, &session_id, &activity).await;
                }

                // Re-derived fresh from the whole file on every "stop" (see
                // `extract_usage_metrics`'s doc comment for why this isn't
                // tracked incrementally) — cheap enough locally, and it's
                // the same trigger point everything else in this phase
                // (subagents, plan) piggybacks on.
                if let Some(m) = extract_usage_metrics(&path) {
                    engine
                        .dispatch(EngineCommand::SetMetrics {
                            id: session_id.clone(),
                            tokens: m.tokens,
                            cost: m.cost,
                            ctx_used: m.ctx_used,
                            ctx_max: m.ctx_max,
                        })
                        .await;
                }

                // Cheap directory listing (most sessions never spawn a
                // subagent, so this is almost always an empty read_dir) —
                // see `list_subagents`'s doc comment for the mtime-based
                // Working/Done heuristic this relies on.
                let subs = list_subagents(&orch_config.claude_projects_dir, &cwd, &session_id);
                engine.dispatch(EngineCommand::SetSubagents { id: session_id.clone(), subs }).await;
            }

            // Independent of the transcript file itself — reads
            // `~/.claude/tasks/<session_id>/` directly. `None` (no
            // `TaskCreate` ever used) is the normal case, not an error.
            let category = engine
                .snapshot()
                .await
                .into_iter()
                .find(|v| v.id == session_id)
                .map(|v| v.category)
                .unwrap_or_else(|| "General".to_string());
            let plan = read_session_plan(&orch_config.tasks_dir, &session_id, &category);
            engine.dispatch(EngineCommand::SetPlan { id: session_id.clone(), plan }).await;
        }
        "session-end" => {
            engine
                .dispatch(EngineCommand::SessionEvent {
                    id: session_id.clone(),
                    event: SessionEvent::SessionEnd,
                    project: None,
                    cwd: None,
                    entrypoint: None,
                    started_at_ms: None,
                })
                .await;

            // Durably remember this session ended, so a later restart's
            // reconstruction (see `reconstruct_live_sessions`) restores it
            // as Done rather than guessing Working from transcript recency.
            let mut ended = ended_sessions.lock().await;
            ended.mark_ended(session_id);
            let _ = ended.save();
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::SessionDiff;
    use crate::ollama::fake::FakeOllamaClient;
    use crate::memory_repo::{Memory, MemoryKind};
    use std::io::Write;
    use std::os::unix::net::UnixStream as StdUnixStream;

    /// Builders below use only field names confirmed against
    /// code.claude.com/docs/en/hooks.md. The `notification_type` fix this
    /// file's tests guard against came from a hand-rolled payload literal
    /// silently using the wrong key — reach for these instead of a raw
    /// `serde_json::json!` literal so a wrong field name has to be wrong
    /// here, once, rather than independently in every test.
    fn real_session_start_payload(session_id: &str, cwd: &str, transcript_path: &Path, source: &str) -> serde_json::Value {
        serde_json::json!({
            "session_id": session_id,
            "cwd": cwd,
            "transcript_path": transcript_path.to_str().unwrap(),
            "source": source,
        })
    }

    fn real_notification_payload(session_id: &str, notification_type: &str) -> serde_json::Value {
        serde_json::json!({"session_id": session_id, "notification_type": notification_type})
    }

    fn real_pre_tool_use_payload(session_id: &str, tool_name: &str, tool_use_id: &str) -> serde_json::Value {
        serde_json::json!({
            "session_id": session_id,
            "tool_name": tool_name,
            "tool_input": {},
            "tool_use_id": tool_use_id,
        })
    }

    fn real_stop_payload(session_id: &str, stop_reason: &str) -> serde_json::Value {
        serde_json::json!({"session_id": session_id, "stop_reason": stop_reason})
    }

    fn real_session_end_payload(session_id: &str, exit_reason: &str) -> serde_json::Value {
        serde_json::json!({"session_id": session_id, "exit_reason": exit_reason})
    }

    /// User feedback fix: sessions should reappear with their real category
    /// on Core Engine restart, reconstructed from the durable `memories` row
    /// — not vanish until the next hook event happens to touch them.
    #[tokio::test]
    async fn reconstructs_a_recent_session_from_durable_memory_on_startup() {
        let claude_dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();

        let cwd = "/Users/omricohen/api-gateway";
        let session_id = "restored-1";
        let path = transcript_path(claude_dir.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{}\n").unwrap(); // freshly written -> recent mtime

        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        repo.upsert_memory(&Memory {
            id: format!("{session_id}-prompt"),
            session_id: session_id.to_string(),
            kind: MemoryKind::Prompt,
            text: "Refactor auth middleware to async/await".to_string(),
            embedding: vec![1.0; 768],
            project: "api-gateway".to_string(),
            cwd: cwd.to_string(),
            tool: "Claude Code".to_string(),
            category: "Backend / API".to_string(),
            created_at: 1_700_000_000_000,
        })
        .await
        .unwrap();

        let engine = EngineHandle::spawn();
        let app_dir = tempfile::tempdir().unwrap();
        let orch_config = OrchestratorConfig {
            claude_projects_dir: claude_dir.path().to_path_buf(),
            ended_sessions_path: app_dir.path().join("ended-sessions.json"),
            waiting_sessions_path: app_dir.path().join("waiting-sessions.json"),
            ..OrchestratorConfig::default()
        };
        let ended_sessions = EndedSessions::load(&orch_config.ended_sessions_path);
        let waiting_sessions = WaitingSessions::load(&orch_config.waiting_sessions_path);

        reconstruct_live_sessions(&engine, &repo, &orch_config, &ended_sessions, &waiting_sessions).await;

        let snapshot = engine.snapshot().await;
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].id, session_id);
        assert_eq!(snapshot[0].category, "Backend / API");
        assert_eq!(snapshot[0].project, "api-gateway");
        assert_eq!(snapshot[0].state, crate::state::SessionState::Working);
        assert_eq!(snapshot[0].desc, "Refactor auth middleware to async/await");
        assert_eq!(snapshot[0].title, "Refactor auth middleware to");
        assert_eq!(snapshot[0].started_at_ms, 1_700_000_000_000);
    }

    /// Regression (user report — sessions that aren't actually being worked
    /// on show as "Working"): reconstruction dispatches a `SessionStart`
    /// through the same `SessionEvent` path a live hook uses, which stamps
    /// `last_activity_ms` with "now" — correct for a live hook, wrong for a
    /// replayed historical one. Left uncorrected, a session reconstructed as
    /// Working gets its idle clock reset to "now" on *every* restart, so the
    /// sweep waits a full fresh `idle_ttl` after each restart before
    /// correcting it, even though its transcript has been untouched for
    /// hours. Reconstruction must stamp the transcript's real mtime instead,
    /// so the sweep can act on it immediately. (`Working` + `IdleTimeout` ->
    /// `Done` now, not `Idle` — see `state::transition`'s doc comment.)
    #[tokio::test]
    async fn reconstruction_uses_the_transcripts_real_mtime_for_the_idle_clock() {
        let claude_dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();
        let app_dir = tempfile::tempdir().unwrap();

        let cwd = "/Users/omricohen/api-gateway";
        let session_id = "restored-stale-activity-1";
        let path = transcript_path(claude_dir.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{}\n").unwrap();
        // Backdate the transcript by 3 minutes — recent enough to pass the
        // 24h reconstruction-recency check, but well past a short idle_ttl.
        let three_min_ago = std::time::SystemTime::now() - Duration::from_secs(3 * 60);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(three_min_ago).unwrap();

        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        repo.upsert_memory(&Memory {
            id: format!("{session_id}-prompt"),
            session_id: session_id.to_string(),
            kind: MemoryKind::Prompt,
            text: "Refactor auth middleware to async/await".to_string(),
            embedding: vec![1.0; 768],
            project: "api-gateway".to_string(),
            cwd: cwd.to_string(),
            tool: "Claude Code".to_string(),
            category: "Backend / API".to_string(),
            created_at: 1_700_000_000_000,
        })
        .await
        .unwrap();

        let engine = EngineHandle::spawn();
        let orch_config = OrchestratorConfig {
            claude_projects_dir: claude_dir.path().to_path_buf(),
            ended_sessions_path: app_dir.path().join("ended-sessions.json"),
            waiting_sessions_path: app_dir.path().join("waiting-sessions.json"),
            ..OrchestratorConfig::default()
        };
        let ended_sessions = EndedSessions::load(&orch_config.ended_sessions_path);
        let waiting_sessions = WaitingSessions::load(&orch_config.waiting_sessions_path);

        reconstruct_live_sessions(&engine, &repo, &orch_config, &ended_sessions, &waiting_sessions).await;
        assert_eq!(engine.snapshot().await[0].state, crate::state::SessionState::Working);

        // A 1-minute idle_ttl comfortably fits inside the 3-minute-old
        // mtime: if the idle clock were wrongly reset to "now" during
        // reconstruction, this sweep would find no stale session and the
        // state would still be Working.
        engine
            .dispatch(crate::engine::EngineCommand::SweepIdle { now_ms: crate::now_ms(), ttl_ms: 60_000 })
            .await;

        assert_eq!(
            engine.snapshot().await[0].state,
            crate::state::SessionState::Done,
            "the sweep should have caught this session as stale using its real transcript mtime"
        );
    }

    /// Regression (user report — "it should move to done [after it stops
    /// working], idle is [after] this session is done more than X minutes"):
    /// reconstruction must compute Done/Idle directly from elapsed time
    /// against `idle_ttl`/`done_ttl`, not just default everything to a
    /// `Working` guess and wait for a live sweep to catch up.
    #[tokio::test]
    async fn reconstructs_a_quiet_session_as_done_and_a_long_quiet_one_as_idle() {
        let claude_dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();
        let app_dir = tempfile::tempdir().unwrap();

        async fn seed(repo: &MemoryRepo, claude_dir: &std::path::Path, session_id: &str, age: Duration) {
            let cwd = "/Users/omricohen/api-gateway";
            let path = transcript_path(claude_dir, cwd, session_id);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "{}\n").unwrap();
            let modified = std::time::SystemTime::now() - age;
            std::fs::File::options().write(true).open(&path).unwrap().set_modified(modified).unwrap();

            repo.upsert_memory(&Memory {
                id: format!("{session_id}-prompt"),
                session_id: session_id.to_string(),
                kind: MemoryKind::Prompt,
                text: "Refactor auth middleware to async/await".to_string(),
                embedding: vec![1.0; 768],
                project: "api-gateway".to_string(),
                cwd: cwd.to_string(),
                tool: "Claude Code".to_string(),
                category: "Backend / API".to_string(),
                created_at: 1_700_000_000_000,
            })
            .await
            .unwrap();
        }

        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        let idle_ttl = Duration::from_secs(10 * 60);
        let done_ttl = Duration::from_secs(10 * 60);
        // Past idle_ttl (stopped working) but not past idle_ttl + done_ttl (not yet aged into Idle).
        seed(&repo, claude_dir.path(), "quiet-done-1", idle_ttl + Duration::from_secs(60)).await;
        // Past idle_ttl + done_ttl entirely.
        seed(&repo, claude_dir.path(), "long-quiet-idle-1", idle_ttl + done_ttl + Duration::from_secs(60)).await;

        let engine = EngineHandle::spawn();
        let orch_config = OrchestratorConfig {
            claude_projects_dir: claude_dir.path().to_path_buf(),
            ended_sessions_path: app_dir.path().join("ended-sessions.json"),
            waiting_sessions_path: app_dir.path().join("waiting-sessions.json"),
            idle_ttl,
            done_ttl,
            ..OrchestratorConfig::default()
        };
        let ended_sessions = EndedSessions::load(&orch_config.ended_sessions_path);
        let waiting_sessions = WaitingSessions::load(&orch_config.waiting_sessions_path);

        reconstruct_live_sessions(&engine, &repo, &orch_config, &ended_sessions, &waiting_sessions).await;

        let snapshot = engine.snapshot().await;
        let get = |id: &str| snapshot.iter().find(|v| v.id == id).unwrap().state;
        assert_eq!(get("quiet-done-1"), crate::state::SessionState::Done);
        assert_eq!(get("long-quiet-idle-1"), crate::state::SessionState::Idle);
    }

    /// Regression (user report — a session genuinely waiting on the user
    /// wasn't shown as such): reconstruction can restore Working or Done
    /// from a `memories` row, but has no way to recover Waiting on its
    /// own — nothing re-fires `Notification` just because the app
    /// restarted. A session recorded in `WaitingSessions` (a real
    /// `Notification`/`AskUserQuestion` hook fired for it, never resolved)
    /// must be restored straight to Waiting.
    #[tokio::test]
    async fn reconstructs_a_waiting_session_as_waiting_not_working() {
        let claude_dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();
        let app_dir = tempfile::tempdir().unwrap();

        let cwd = "/Users/omricohen/api-gateway";
        let session_id = "restored-waiting-1";
        let path = transcript_path(claude_dir.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{}\n").unwrap(); // freshly written -> recent mtime

        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        repo.upsert_memory(&Memory {
            id: format!("{session_id}-prompt"),
            session_id: session_id.to_string(),
            kind: MemoryKind::Prompt,
            text: "Refactor auth middleware to async/await".to_string(),
            embedding: vec![1.0; 768],
            project: "api-gateway".to_string(),
            cwd: cwd.to_string(),
            tool: "Claude Code".to_string(),
            category: "Backend / API".to_string(),
            created_at: 1_700_000_000_000,
        })
        .await
        .unwrap();

        let engine = EngineHandle::spawn();
        let orch_config = OrchestratorConfig {
            claude_projects_dir: claude_dir.path().to_path_buf(),
            ended_sessions_path: app_dir.path().join("ended-sessions.json"),
            waiting_sessions_path: app_dir.path().join("waiting-sessions.json"),
            ..OrchestratorConfig::default()
        };
        let ended_sessions = EndedSessions::load(&orch_config.ended_sessions_path);
        let mut waiting_sessions = WaitingSessions::load(&orch_config.waiting_sessions_path);
        waiting_sessions.mark_waiting(session_id.to_string());

        reconstruct_live_sessions(&engine, &repo, &orch_config, &ended_sessions, &waiting_sessions).await;

        let snapshot = engine.snapshot().await;
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].id, session_id);
        assert_eq!(snapshot[0].state, crate::state::SessionState::Waiting);
    }

    /// Regression (user report — sessions that had genuinely finished were
    /// showing back up as "Working" after a restart): transcript-file
    /// recency alone can't tell "still working" apart from "ended cleanly a
    /// few minutes ago", so a session recorded in `EndedSessions` (a real
    /// `SessionEnd` fired for it) must be reconstructed as Done, not Working.
    #[tokio::test]
    async fn reconstructs_an_ended_session_as_done_not_working() {
        let claude_dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();
        let app_dir = tempfile::tempdir().unwrap();

        let cwd = "/Users/omricohen/api-gateway";
        let session_id = "restored-done-1";
        let path = transcript_path(claude_dir.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{}\n").unwrap(); // freshly written -> recent mtime

        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        repo.upsert_memory(&Memory {
            id: format!("{session_id}-prompt"),
            session_id: session_id.to_string(),
            kind: MemoryKind::Prompt,
            text: "Refactor auth middleware to async/await".to_string(),
            embedding: vec![1.0; 768],
            project: "api-gateway".to_string(),
            cwd: cwd.to_string(),
            tool: "Claude Code".to_string(),
            category: "Backend / API".to_string(),
            created_at: 1_700_000_000_000,
        })
        .await
        .unwrap();

        let engine = EngineHandle::spawn();
        let orch_config = OrchestratorConfig {
            claude_projects_dir: claude_dir.path().to_path_buf(),
            ended_sessions_path: app_dir.path().join("ended-sessions.json"),
            waiting_sessions_path: app_dir.path().join("waiting-sessions.json"),
            ..OrchestratorConfig::default()
        };
        let mut ended_sessions = EndedSessions::load(&orch_config.ended_sessions_path);
        ended_sessions.mark_ended(session_id.to_string());
        let waiting_sessions = WaitingSessions::load(&orch_config.waiting_sessions_path);

        reconstruct_live_sessions(&engine, &repo, &orch_config, &ended_sessions, &waiting_sessions).await;

        let snapshot = engine.snapshot().await;
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].id, session_id);
        assert_eq!(snapshot[0].state, crate::state::SessionState::Done);
    }

    #[tokio::test]
    async fn does_not_resurrect_a_session_whose_transcript_is_stale() {
        let claude_dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();

        let cwd = "/Users/omricohen/old-project";
        let session_id = "stale-1";
        let path = transcript_path(claude_dir.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{}\n").unwrap();
        // Backdate the file well past the default 24h reconstruction window.
        let old_time = std::time::SystemTime::now() - Duration::from_secs(3 * 24 * 60 * 60);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(old_time).unwrap();

        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        repo.upsert_memory(&Memory {
            id: format!("{session_id}-prompt"),
            session_id: session_id.to_string(),
            kind: MemoryKind::Prompt,
            text: "Old finished task".to_string(),
            embedding: vec![1.0; 768],
            project: "old-project".to_string(),
            cwd: cwd.to_string(),
            tool: "Claude Code".to_string(),
            category: "Backend / API".to_string(),
            created_at: 0,
        })
        .await
        .unwrap();

        let engine = EngineHandle::spawn();
        let app_dir = tempfile::tempdir().unwrap();
        let orch_config = OrchestratorConfig {
            claude_projects_dir: claude_dir.path().to_path_buf(),
            ended_sessions_path: app_dir.path().join("ended-sessions.json"),
            waiting_sessions_path: app_dir.path().join("waiting-sessions.json"),
            ..OrchestratorConfig::default()
        };
        let ended_sessions = EndedSessions::load(&orch_config.ended_sessions_path);
        let waiting_sessions = WaitingSessions::load(&orch_config.waiting_sessions_path);

        reconstruct_live_sessions(&engine, &repo, &orch_config, &ended_sessions, &waiting_sessions).await;

        assert!(engine.snapshot().await.is_empty(), "a long-stale session must not be resurrected as a false ghost");
    }

    /// The M10 "full E2E" fixture harness (plan §11 M10): fake hook events
    /// written to a *real* UDS, plus a real scratch transcript, driven
    /// through the *real* orchestrator (not individual pieces called
    /// directly, as M5's harness did) — this is the first test exercising
    /// the actual production wiring.
    #[tokio::test]
    async fn real_uds_hook_events_and_scratch_transcript_drive_the_real_orchestrator() {
        let claude_dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();
        let app_dir = tempfile::tempdir().unwrap();

        let cwd = "/Users/omricohen/api-gateway";
        let session_id = "orch-e2e-1";
        let prompt_text = "Refactor auth middleware to async/await";

        let path = transcript_path(claude_dir.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({"type":"user","message":{"role":"user","content":prompt_text}}).to_string() + "\n",
        )
        .unwrap();

        let engine = EngineHandle::spawn();
        let mut diffs = engine.subscribe();
        let repo = Arc::new(MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap());
        let ollama: Arc<dyn OllamaClient> =
            Arc::new(FakeOllamaClient::new_categorizing("Backend / API", 90, true, "Refactoring auth middleware"));
        let cat_config = Arc::new(CategorizationConfig::default());
        // Isolated per-test socket path — parallel test runs must not share
        // the real default UDS path, or they collide with each other (and
        // with a real running instance) binding the same file.
        let socket_path = app_dir.path().join("engine.sock");
        let orch_config = Arc::new(OrchestratorConfig {
            claude_projects_dir: claude_dir.path().to_path_buf(),
            checkpoint_path: app_dir.path().join("tail-checkpoints.json"),
            socket_path: socket_path.clone(),
            transcript_wait: Duration::from_millis(500),
            waiting_sessions_path: app_dir.path().join("waiting-sessions.json"),
            ..OrchestratorConfig::default()
        });
        let waiting_sessions = Arc::new(Mutex::new(WaitingSessions::load(&orch_config.waiting_sessions_path)));

        tokio::spawn(run(engine.clone(), repo.clone(), ollama, cat_config, orch_config, waiting_sessions));
        // Give the orchestrator a moment to bind the UDS before the fake
        // hook-bridge client below tries to connect to it.
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Simulate hook-bridge exactly as the real compiled binary does:
        // connect, write the envelope, close.
        let envelope = serde_json::json!({
            "event": "session-start",
            "payload": real_session_start_payload(session_id, cwd, &path, "startup")
        });
        let socket_path_clone = socket_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut stream = StdUnixStream::connect(&socket_path_clone).unwrap();
            stream.write_all(envelope.to_string().as_bytes()).unwrap();
        })
        .await
        .unwrap();

        // First diff: Uncategorized card appears immediately.
        let diff = tokio::time::timeout(Duration::from_secs(2), diffs.recv())
            .await
            .expect("should receive the initial diff before timing out")
            .unwrap();
        let SessionDiff::Upserted(initial) = diff else { panic!("expected Upserted") };
        assert_eq!(initial.id, session_id);
        assert_eq!(initial.category, "Uncategorized");
        assert_eq!(initial.project, "api-gateway");

        // Categorization resolves via the real pipeline, followed by
        // SetTitle/SetDesc diffs (real title + task summary) — read forward past any
        // intermediate diffs rather than assuming an exact count, since
        // that's an implementation detail of the pipeline, not the contract
        // under test here.
        let categorized = recv_until(&mut diffs, |v| v.category != "Uncategorized")
            .await
            .expect("should receive a categorized diff before timing out");
        assert_eq!(categorized.id, session_id);
        assert_eq!(categorized.category, "Backend / API");

        // Now drive a Notification hook event through the same real path.
        let envelope = serde_json::json!({"event": "notification", "payload": real_notification_payload(session_id, "permission_prompt")});
        let socket_path_clone = socket_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut stream = StdUnixStream::connect(&socket_path_clone).unwrap();
            stream.write_all(envelope.to_string().as_bytes()).unwrap();
        })
        .await
        .unwrap();

        let waiting = recv_until(&mut diffs, |v| v.state == crate::state::SessionState::Waiting)
            .await
            .expect("should receive a Waiting diff before timing out");
        assert_eq!(waiting.state, crate::state::SessionState::Waiting);
    }

    /// Regression (user report — a real "best pet for you" AskUserQuestion
    /// session sat at Working instead of flipping to Waiting): Claude Code
    /// doesn't fire a `Notification` hook for `AskUserQuestion` even though
    /// it blocks the turn on a real user choice, so `pre-tool-use` for that
    /// specific tool must be special-cased to Waiting; any other tool still
    /// just marks Working as before.
    #[tokio::test]
    async fn pre_tool_use_ask_user_question_marks_the_session_waiting() {
        let claude_dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();
        let app_dir = tempfile::tempdir().unwrap();

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

        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        let ollama = FakeOllamaClient::new_categorizing("General", 90, true, "n/a");
        let cat_config = CategorizationConfig::default();
        let orch_config = OrchestratorConfig {
            claude_projects_dir: claude_dir.path().to_path_buf(),
            checkpoint_path: app_dir.path().join("tail-checkpoints.json"),
            ended_sessions_path: app_dir.path().join("ended-sessions.json"),
            waiting_sessions_path: app_dir.path().join("waiting-sessions.json"),
            ..OrchestratorConfig::default()
        };
        let checkpoints = Arc::new(Mutex::new(TailCheckpoints::load(&orch_config.checkpoint_path)));
        let ended_sessions = Arc::new(Mutex::new(EndedSessions::load(&orch_config.ended_sessions_path)));
        let waiting_sessions = Arc::new(Mutex::new(WaitingSessions::load(&orch_config.waiting_sessions_path)));

        let event = HookEvent {
            event: "pre-tool-use".into(),
            payload: real_pre_tool_use_payload("s1", "AskUserQuestion", "toolu_1"),
        };
        handle_hook_event(event, &engine, &repo, &ollama, &cat_config, &orch_config, &checkpoints, &ended_sessions, &waiting_sessions)
            .await;
        assert_eq!(engine.snapshot().await[0].state, crate::state::SessionState::Waiting);
        assert!(waiting_sessions.lock().await.contains("s1"), "AskUserQuestion should durably record the wait");

        // A normal tool call (no special-cased tool_name) still just marks Working.
        let event = HookEvent {
            event: "pre-tool-use".into(),
            payload: real_pre_tool_use_payload("s1", "Bash", "toolu_2"),
        };
        handle_hook_event(event, &engine, &repo, &ollama, &cat_config, &orch_config, &checkpoints, &ended_sessions, &waiting_sessions)
            .await;
        assert_eq!(engine.snapshot().await[0].state, crate::state::SessionState::Working);
        assert!(
            !waiting_sessions.lock().await.contains("s1"),
            "further activity should evict the durable wait record"
        );
    }

    /// Regression (user report — a terminal session showing "Waiting" when
    /// it was actually done): Claude Code's `Notification` hook fires for
    /// several reasons (`notification_type` field — confirmed against the
    /// hooks docs), not just "blocked on you". `idle_prompt` specifically
    /// means "sitting idle waiting for the next message" — nothing our board
    /// should treat as Waiting — but the old code treated every
    /// `Notification` the same, so a session that had correctly settled
    /// into `Done` got yanked back to `Waiting` by its own `idle_prompt`
    /// notification. That specific type must now be a true no-op: no state
    /// change, no durable record.
    #[tokio::test]
    async fn idle_prompt_notification_does_not_revive_a_done_session() {
        let claude_dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();
        let app_dir = tempfile::tempdir().unwrap();

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
                event: SessionEvent::TurnEnd,
                project: None,
                cwd: None,
                entrypoint: None,
                started_at_ms: None,
            })
            .await;
        assert_eq!(engine.snapshot().await[0].state, crate::state::SessionState::Done);

        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        let ollama = FakeOllamaClient::new_categorizing("General", 90, true, "n/a");
        let cat_config = CategorizationConfig::default();
        let orch_config = OrchestratorConfig {
            claude_projects_dir: claude_dir.path().to_path_buf(),
            checkpoint_path: app_dir.path().join("tail-checkpoints.json"),
            ended_sessions_path: app_dir.path().join("ended-sessions.json"),
            waiting_sessions_path: app_dir.path().join("waiting-sessions.json"),
            ..OrchestratorConfig::default()
        };
        let checkpoints = Arc::new(Mutex::new(TailCheckpoints::load(&orch_config.checkpoint_path)));
        let ended_sessions = Arc::new(Mutex::new(EndedSessions::load(&orch_config.ended_sessions_path)));
        let waiting_sessions = Arc::new(Mutex::new(WaitingSessions::load(&orch_config.waiting_sessions_path)));

        let event = HookEvent {
            event: "notification".into(),
            payload: real_notification_payload("s1", "idle_prompt"),
        };
        handle_hook_event(event, &engine, &repo, &ollama, &cat_config, &orch_config, &checkpoints, &ended_sessions, &waiting_sessions)
            .await;

        assert_eq!(
            engine.snapshot().await[0].state,
            crate::state::SessionState::Done,
            "an idle_prompt notification must not revive a Done session to Waiting"
        );
        assert!(
            !waiting_sessions.lock().await.contains("s1"),
            "an idle_prompt notification must not durably record a wait"
        );
    }

    /// Companion to the regression above: a genuine `permission_prompt`
    /// notification (or any type other than `idle_prompt`) must still mark
    /// Waiting exactly as before — including reviving a `Done` session, the
    /// earlier fix (`state::transition`'s doc comment) this must not regress.
    #[tokio::test]
    async fn permission_prompt_notification_still_revives_a_done_session_to_waiting() {
        let claude_dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();
        let app_dir = tempfile::tempdir().unwrap();

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
                event: SessionEvent::TurnEnd,
                project: None,
                cwd: None,
                entrypoint: None,
                started_at_ms: None,
            })
            .await;
        assert_eq!(engine.snapshot().await[0].state, crate::state::SessionState::Done);

        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        let ollama = FakeOllamaClient::new_categorizing("General", 90, true, "n/a");
        let cat_config = CategorizationConfig::default();
        let orch_config = OrchestratorConfig {
            claude_projects_dir: claude_dir.path().to_path_buf(),
            checkpoint_path: app_dir.path().join("tail-checkpoints.json"),
            ended_sessions_path: app_dir.path().join("ended-sessions.json"),
            waiting_sessions_path: app_dir.path().join("waiting-sessions.json"),
            ..OrchestratorConfig::default()
        };
        let checkpoints = Arc::new(Mutex::new(TailCheckpoints::load(&orch_config.checkpoint_path)));
        let ended_sessions = Arc::new(Mutex::new(EndedSessions::load(&orch_config.ended_sessions_path)));
        let waiting_sessions = Arc::new(Mutex::new(WaitingSessions::load(&orch_config.waiting_sessions_path)));

        let event = HookEvent {
            event: "notification".into(),
            payload: real_notification_payload("s1", "permission_prompt"),
        };
        handle_hook_event(event, &engine, &repo, &ollama, &cat_config, &orch_config, &checkpoints, &ended_sessions, &waiting_sessions)
            .await;

        assert_eq!(engine.snapshot().await[0].state, crate::state::SessionState::Waiting);
        assert!(waiting_sessions.lock().await.contains("s1"));
    }

    /// Regression (user report — "session goes to done once it stop
    /// working"): the `stop` hook (assistant's turn ends cleanly) must move
    /// a session straight to Done, not just refresh its task summary while
    /// leaving it pinned at Working. The next real activity still revives it
    /// to Working exactly like it would from any other state.
    #[tokio::test]
    async fn stop_hook_marks_the_session_done() {
        let claude_dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();
        let app_dir = tempfile::tempdir().unwrap();

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

        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        let ollama = FakeOllamaClient::new_categorizing("General", 90, true, "n/a");
        let cat_config = CategorizationConfig::default();
        let orch_config = OrchestratorConfig {
            claude_projects_dir: claude_dir.path().to_path_buf(),
            checkpoint_path: app_dir.path().join("tail-checkpoints.json"),
            ended_sessions_path: app_dir.path().join("ended-sessions.json"),
            waiting_sessions_path: app_dir.path().join("waiting-sessions.json"),
            ..OrchestratorConfig::default()
        };
        let checkpoints = Arc::new(Mutex::new(TailCheckpoints::load(&orch_config.checkpoint_path)));
        let ended_sessions = Arc::new(Mutex::new(EndedSessions::load(&orch_config.ended_sessions_path)));
        let waiting_sessions = Arc::new(Mutex::new(WaitingSessions::load(&orch_config.waiting_sessions_path)));

        let event = HookEvent { event: "stop".into(), payload: real_stop_payload("s1", "end_turn") };
        handle_hook_event(event, &engine, &repo, &ollama, &cat_config, &orch_config, &checkpoints, &ended_sessions, &waiting_sessions)
            .await;
        assert_eq!(engine.snapshot().await[0].state, crate::state::SessionState::Done);

        let event = HookEvent {
            event: "pre-tool-use".into(),
            payload: real_pre_tool_use_payload("s1", "Bash", "toolu_1"),
        };
        handle_hook_event(event, &engine, &repo, &ollama, &cat_config, &orch_config, &checkpoints, &ended_sessions, &waiting_sessions)
            .await;
        assert_eq!(engine.snapshot().await[0].state, crate::state::SessionState::Working);
    }

    /// Regression (user report — "idle is [after] this session is done more
    /// than X minutes"): a `Done` session that's sat quiet past `done_ttl`
    /// ages into `Idle`, but a session the user explicitly closed
    /// (`EndedSessions`) is exempt — that's a real close, not just silence,
    /// so it stays `Done` forever.
    #[tokio::test]
    async fn done_sweep_ages_a_quiet_done_session_into_idle_but_exempts_explicit_closes() {
        let engine = EngineHandle::spawn();
        for id in ["quiet-1", "closed-1"] {
            engine
                .dispatch(EngineCommand::SessionEvent {
                    id: id.into(),
                    event: SessionEvent::SessionStart,
                    project: None,
                    cwd: None,
                    entrypoint: None,
                    started_at_ms: Some(0),
                })
                .await;
            engine
                .dispatch(EngineCommand::SessionEvent {
                    id: id.into(),
                    event: SessionEvent::TurnEnd,
                    project: None,
                    cwd: None,
                    entrypoint: None,
                    started_at_ms: None,
                })
                .await;
        }
        assert_eq!(engine.snapshot().await.iter().find(|v| v.id == "quiet-1").unwrap().state, crate::state::SessionState::Done);

        let ended_sessions = Mutex::new(EndedSessions::load(&tempfile::tempdir().unwrap().path().join("ended-sessions.json")));
        ended_sessions.lock().await.mark_ended("closed-1".to_string());

        let now_ms = crate::now_ms() + 999_999_999;
        done_sweep_once(&engine, &ended_sessions, 10_000, now_ms).await;

        let snapshot = engine.snapshot().await;
        let get = |id: &str| snapshot.iter().find(|v| v.id == id).unwrap().state;
        assert_eq!(get("quiet-1"), crate::state::SessionState::Idle, "a quiet Done session should age into Idle");
        assert_eq!(get("closed-1"), crate::state::SessionState::Done, "an explicitly-closed session must stay Done forever");
    }

    /// Regression (user report — sessions that had genuinely finished
    /// showed back up as "Working" after a restart): a real `session-end`
    /// hook must durably record the session id, not just transition the
    /// live engine to Done, since a restart's reconstruction has no other
    /// way to tell "finished" apart from "still working" (see
    /// `reconstructs_an_ended_session_as_done_not_working`).
    #[tokio::test]
    async fn session_end_durably_records_the_session_as_ended() {
        let claude_dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();
        let app_dir = tempfile::tempdir().unwrap();

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

        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        let ollama = FakeOllamaClient::new_categorizing("General", 90, true, "n/a");
        let cat_config = CategorizationConfig::default();
        let orch_config = OrchestratorConfig {
            claude_projects_dir: claude_dir.path().to_path_buf(),
            checkpoint_path: app_dir.path().join("tail-checkpoints.json"),
            ended_sessions_path: app_dir.path().join("ended-sessions.json"),
            ..OrchestratorConfig::default()
        };
        let checkpoints = Arc::new(Mutex::new(TailCheckpoints::load(&orch_config.checkpoint_path)));
        let ended_sessions = Arc::new(Mutex::new(EndedSessions::load(&orch_config.ended_sessions_path)));
        let waiting_sessions = Arc::new(Mutex::new(WaitingSessions::load(&orch_config.waiting_sessions_path)));

        let event = HookEvent { event: "session-end".into(), payload: real_session_end_payload("s1", "other") };
        handle_hook_event(event, &engine, &repo, &ollama, &cat_config, &orch_config, &checkpoints, &ended_sessions, &waiting_sessions)
            .await;

        assert_eq!(engine.snapshot().await[0].state, crate::state::SessionState::Done);
        assert!(ended_sessions.lock().await.contains("s1"));

        // Reload from disk, simulating a restart, to confirm it was persisted.
        let reloaded = EndedSessions::load(&orch_config.ended_sessions_path);
        assert!(reloaded.contains("s1"));
    }

    /// Regression (user report — a session actively waiting on the user
    /// showed up as "Done"): a session previously recorded in
    /// `EndedSessions` (e.g. reconstructed straight to `Done` on the last
    /// restart) must be evicted from that durable record the moment ANY
    /// later real hook fires for it — proof it's alive again — so the
    /// *next* restart's reconstruction doesn't immediately force it back to
    /// `Done` before the live engine (already correctly revived per
    /// `state::transition`) gets a chance to reflect reality.
    #[tokio::test]
    async fn a_later_hook_evicts_the_session_from_ended_sessions() {
        let claude_dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();
        let app_dir = tempfile::tempdir().unwrap();

        let engine = EngineHandle::spawn();
        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        let ollama = FakeOllamaClient::new_categorizing("General", 90, true, "n/a");
        let cat_config = CategorizationConfig::default();
        let orch_config = OrchestratorConfig {
            claude_projects_dir: claude_dir.path().to_path_buf(),
            checkpoint_path: app_dir.path().join("tail-checkpoints.json"),
            ended_sessions_path: app_dir.path().join("ended-sessions.json"),
            ..OrchestratorConfig::default()
        };
        let checkpoints = Arc::new(Mutex::new(TailCheckpoints::load(&orch_config.checkpoint_path)));
        let ended_sessions = Arc::new(Mutex::new(EndedSessions::load(&orch_config.ended_sessions_path)));
        let waiting_sessions = Arc::new(Mutex::new(WaitingSessions::load(&orch_config.waiting_sessions_path)));
        ended_sessions.lock().await.mark_ended("s1".to_string());
        assert!(ended_sessions.lock().await.contains("s1"));

        // A resumed session's next real hook — a permission-prompt
        // notification in this case — arrives for an id still marked ended.
        let event = HookEvent {
            event: "pre-tool-use".into(),
            payload: real_pre_tool_use_payload("s1", "AskUserQuestion", "toolu_1"),
        };
        handle_hook_event(event, &engine, &repo, &ollama, &cat_config, &orch_config, &checkpoints, &ended_sessions, &waiting_sessions)
            .await;

        assert!(!ended_sessions.lock().await.contains("s1"), "the hook should evict the stale ended-record");
        let reloaded = EndedSessions::load(&orch_config.ended_sessions_path);
        assert!(!reloaded.contains("s1"), "the eviction should be durably persisted");
    }

    /// The core bug fix (user report): a `session-start` for a session that
    /// never produces any transcript content (opened and abandoned) must
    /// never create a card at all, rather than leaving a permanent
    /// "Uncategorized"/"Starting…" ghost on the board.
    #[tokio::test]
    async fn session_start_with_no_transcript_content_never_creates_a_card() {
        let claude_dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();
        let app_dir = tempfile::tempdir().unwrap();

        let session_id = "orch-ghost-1";
        // Deliberately no transcript file written anywhere for this session.

        let engine = EngineHandle::spawn();
        let repo = Arc::new(MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap());
        let ollama: Arc<dyn OllamaClient> = Arc::new(FakeOllamaClient::new_categorizing("General", 90, true, "n/a"));
        let cat_config = Arc::new(CategorizationConfig::default());
        let socket_path = app_dir.path().join("engine.sock");
        let orch_config = Arc::new(OrchestratorConfig {
            claude_projects_dir: claude_dir.path().to_path_buf(),
            checkpoint_path: app_dir.path().join("tail-checkpoints.json"),
            socket_path: socket_path.clone(),
            transcript_wait: Duration::from_millis(300),
            waiting_sessions_path: app_dir.path().join("waiting-sessions.json"),
            ..OrchestratorConfig::default()
        });
        let waiting_sessions = Arc::new(Mutex::new(WaitingSessions::load(&orch_config.waiting_sessions_path)));

        tokio::spawn(run(engine.clone(), repo.clone(), ollama, cat_config, orch_config, waiting_sessions));
        tokio::time::sleep(Duration::from_millis(150)).await;

        let missing_path = transcript_path(claude_dir.path(), "/Users/omricohen", session_id);
        let envelope = serde_json::json!({
            "event": "session-start",
            "payload": real_session_start_payload(session_id, "/Users/omricohen", &missing_path, "startup")
        });
        tokio::task::spawn_blocking(move || {
            let mut stream = StdUnixStream::connect(&socket_path).unwrap();
            stream.write_all(envelope.to_string().as_bytes()).unwrap();
        })
        .await
        .unwrap();

        // Wait comfortably past the orchestrator's own transcript_wait, then
        // confirm no session was ever created.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(engine.snapshot().await.is_empty(), "a session with no transcript content must never appear on the board");
    }

    /// Regression (user report — a real terminal session never appeared on
    /// the board): `tail_new_lines`'s checkpoint is a single cursor shared
    /// with `stop`'s incremental tailing of the same file. If anything else
    /// tailing this transcript (simulated here directly) advances that
    /// checkpoint past the initiating prompt before `session-start`'s own
    /// read happens, the old code would find nothing new and silently drop
    /// the session forever. `session-start` must read fresh from the start
    /// every time (`read_lines_from_start`), immune to that shared cursor.
    #[tokio::test]
    async fn session_start_finds_the_prompt_even_if_the_shared_checkpoint_already_passed_it() {
        let claude_dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();
        let app_dir = tempfile::tempdir().unwrap();

        let cwd = "/Users/omricohen";
        let session_id = "race-1";
        let path = transcript_path(claude_dir.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::json!({"type": "user", "message": {"content": "this is terminal"}}).to_string() + "\n")
            .unwrap();

        let engine = EngineHandle::spawn();
        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        let ollama = FakeOllamaClient::new_categorizing("General", 90, true, "n/a");
        let cat_config = CategorizationConfig::default();
        let orch_config = OrchestratorConfig {
            claude_projects_dir: claude_dir.path().to_path_buf(),
            checkpoint_path: app_dir.path().join("tail-checkpoints.json"),
            transcript_wait: Duration::from_millis(300),
            ..OrchestratorConfig::default()
        };
        let checkpoints = Arc::new(Mutex::new(TailCheckpoints::load(&orch_config.checkpoint_path)));
        let ended_sessions = Arc::new(Mutex::new(EndedSessions::load(&orch_config.ended_sessions_path)));
        let waiting_sessions = Arc::new(Mutex::new(WaitingSessions::load(&orch_config.waiting_sessions_path)));

        // Simulate a `stop` (or a duplicate `session-start` delivery) already
        // having tailed this exact file to EOF before the real `session-start`
        // handling below ever runs.
        {
            let mut cp = checkpoints.lock().await;
            let _ = tail_new_lines(&path, &mut cp).unwrap();
        }

        let event = HookEvent {
            event: "session-start".into(),
            payload: real_session_start_payload(session_id, cwd, &path, "startup"),
        };
        handle_hook_event(event, &engine, &repo, &ollama, &cat_config, &orch_config, &checkpoints, &ended_sessions, &waiting_sessions)
            .await;

        let snapshot = engine.snapshot().await;
        assert_eq!(snapshot.len(), 1, "the session must still appear despite the shared checkpoint already being past the prompt");
        assert_eq!(snapshot[0].id, session_id);
        assert_eq!(snapshot[0].category, "General");
    }

    /// Feedback fix: the task line should keep tracking the latest activity,
    /// not freeze after the initial prompt. A `stop` hook after new
    /// transcript content has appeared should refresh it.
    #[tokio::test]
    async fn stop_hook_refreshes_the_task_summary_with_latest_activity() {
        let claude_dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();
        let app_dir = tempfile::tempdir().unwrap();

        let cwd = "/Users/omricohen/api-gateway";
        let session_id = "orch-refresh-1";
        let path = transcript_path(claude_dir.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({"type":"user","message":{"content":"Refactor auth middleware"}}).to_string() + "\n",
        )
        .unwrap();

        let engine = EngineHandle::spawn();
        let mut diffs = engine.subscribe();
        let repo = Arc::new(MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap());
        // Kept as a concretely-typed Arc (not just Arc<dyn OllamaClient>) so
        // the test can reach into `canned_labels` below to give the "stop"
        // refresh prompt a distinct plain-text response, separate from the
        // categorization call's JSON response (`generate()` on this fake
        // otherwise always returns the same `default_label` regardless of
        // which prompt it's called with).
        let fake = Arc::new(FakeOllamaClient::new_categorizing("Backend / API", 90, true, "Refactoring auth middleware"));
        let ollama: Arc<dyn OllamaClient> = fake.clone();
        let cat_config = Arc::new(CategorizationConfig::default());
        let socket_path = app_dir.path().join("engine.sock");
        let orch_config = Arc::new(OrchestratorConfig {
            claude_projects_dir: claude_dir.path().to_path_buf(),
            checkpoint_path: app_dir.path().join("tail-checkpoints.json"),
            socket_path: socket_path.clone(),
            transcript_wait: Duration::from_millis(500),
            waiting_sessions_path: app_dir.path().join("waiting-sessions.json"),
            ..OrchestratorConfig::default()
        });
        let waiting_sessions = Arc::new(Mutex::new(WaitingSessions::load(&orch_config.waiting_sessions_path)));

        tokio::spawn(run(engine.clone(), repo.clone(), ollama, cat_config, orch_config, waiting_sessions));
        tokio::time::sleep(Duration::from_millis(150)).await;

        let refresh_prompt = format!(
            "In one short sentence (under 12 words), describe what is currently being worked on, based on this recent activity: \"{}\"",
            "Now running the auth test suite."
        );
        fake.canned_labels.lock().await.insert(refresh_prompt, "Now running the auth test suite.".to_string());

        let send = |event: &str, extra_cwd: bool| {
            let socket_path = socket_path.clone();
            let payload = if extra_cwd {
                real_session_start_payload(session_id, cwd, &path, "startup")
            } else {
                real_stop_payload(session_id, "end_turn")
            };
            let envelope = serde_json::json!({"event": event, "payload": payload});
            async move {
                tokio::task::spawn_blocking(move || {
                    let mut stream = StdUnixStream::connect(&socket_path).unwrap();
                    stream.write_all(envelope.to_string().as_bytes()).unwrap();
                })
                .await
                .unwrap();
            }
        };

        send("session-start", true).await;
        recv_until(&mut diffs, |v| v.category != "Uncategorized")
            .await
            .expect("should receive a categorized diff before timing out");

        // New activity appears in the transcript before the next turn ends.
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        use std::io::Write as _;
        f.write_all(
            (serde_json::json!({"type":"assistant","message":{"content":"Now running the auth test suite."}}).to_string() + "\n")
                .as_bytes(),
        )
        .unwrap();
        drop(f);

        send("stop", true).await;
        let refreshed = recv_until(&mut diffs, |v| v.desc == "Now running the auth test suite.")
            .await
            .expect("should receive a refreshed task diff before timing out");
        assert_eq!(refreshed.desc, "Now running the auth test suite.");
    }

    /// End-to-end: a real `~/.claude/tasks/<session_id>/*.json` fixture on
    /// disk must surface as a populated `plan` field after a "stop" hook —
    /// the same trigger point metrics/subagents piggyback on (plan §4).
    #[tokio::test]
    async fn stop_hook_populates_the_plan_from_real_tasks_files() {
        let claude_dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();
        let app_dir = tempfile::tempdir().unwrap();
        let tasks_dir = tempfile::tempdir().unwrap();

        let cwd = "/Users/omricohen/api-gateway";
        let session_id = "orch-plan-1";
        let path = transcript_path(claude_dir.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::json!({"type":"user","message":{"content":"Refactor auth middleware"}}).to_string() + "\n")
            .unwrap();

        let session_tasks_dir = tasks_dir.path().join(session_id);
        std::fs::create_dir_all(&session_tasks_dir).unwrap();
        std::fs::write(
            session_tasks_dir.join("1.json"),
            serde_json::json!({"id":"1","subject":"Write the plan","status":"completed","blocks":[],"blockedBy":[]}).to_string(),
        )
        .unwrap();
        std::fs::write(
            session_tasks_dir.join("2.json"),
            serde_json::json!({"id":"2","subject":"Implement it","status":"pending","blocks":[],"blockedBy":[]}).to_string(),
        )
        .unwrap();

        let engine = EngineHandle::spawn();
        let mut diffs = engine.subscribe();
        let repo = Arc::new(MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap());
        let ollama: Arc<dyn OllamaClient> = Arc::new(FakeOllamaClient::new_categorizing("Backend / API", 90, true, "Refactoring"));
        let cat_config = Arc::new(CategorizationConfig::default());
        let socket_path = app_dir.path().join("engine.sock");
        let orch_config = Arc::new(OrchestratorConfig {
            claude_projects_dir: claude_dir.path().to_path_buf(),
            tasks_dir: tasks_dir.path().to_path_buf(),
            checkpoint_path: app_dir.path().join("tail-checkpoints.json"),
            socket_path: socket_path.clone(),
            transcript_wait: Duration::from_millis(500),
            waiting_sessions_path: app_dir.path().join("waiting-sessions.json"),
            ..OrchestratorConfig::default()
        });
        let waiting_sessions = Arc::new(Mutex::new(WaitingSessions::load(&orch_config.waiting_sessions_path)));

        tokio::spawn(run(engine.clone(), repo.clone(), ollama, cat_config, orch_config, waiting_sessions));
        tokio::time::sleep(Duration::from_millis(150)).await;

        let send = |event: &str| {
            let socket_path = socket_path.clone();
            let payload = if event == "session-start" {
                real_session_start_payload(session_id, cwd, &path, "startup")
            } else {
                real_stop_payload(session_id, "end_turn")
            };
            let envelope = serde_json::json!({"event": event, "payload": payload});
            async move {
                tokio::task::spawn_blocking(move || {
                    let mut stream = StdUnixStream::connect(&socket_path).unwrap();
                    stream.write_all(envelope.to_string().as_bytes()).unwrap();
                })
                .await
                .unwrap();
            }
        };

        send("session-start").await;
        recv_until(&mut diffs, |v| v.category != "Uncategorized")
            .await
            .expect("should receive a categorized diff before timing out");

        send("stop").await;
        let with_plan = recv_until(&mut diffs, |v| v.plan.is_some())
            .await
            .expect("should receive a diff carrying the real plan before timing out");
        let plan = with_plan.plan.expect("checked by predicate");
        assert_eq!(plan.title, "Backend / API plan");
        assert_eq!(plan.steps.len(), 2);
        assert!(plan.steps[0].done);
        assert!(!plan.steps[1].done);
    }

    /// Reads diffs off `rx` until one satisfies `pred`, ignoring the exact
    /// number/order of intermediate diffs in between (an implementation
    /// detail of the pipeline being exercised, not the contract under test).
    async fn recv_until(
        rx: &mut tokio::sync::broadcast::Receiver<SessionDiff>,
        pred: impl Fn(&crate::engine::SessionView) -> bool,
    ) -> Option<crate::engine::SessionView> {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let SessionDiff::Upserted(view) = rx.recv().await.ok()? {
                    if pred(&view) {
                        return Some(view);
                    }
                }
            }
        })
        .await
        .ok()
        .flatten()
    }
}
