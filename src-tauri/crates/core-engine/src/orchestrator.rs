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
    extract_initiating_prompt, extract_latest_activity, project_name_from_cwd, tail_new_lines, transcript_path,
    TailCheckpoints,
};
use crate::engine::{EngineCommand, EngineHandle};
use crate::hook_socket::{self, HookEvent};
use crate::memory_repo::MemoryRepo;
use crate::ollama::OllamaClient;
use crate::state::SessionEvent;

pub struct OrchestratorConfig {
    pub claude_projects_dir: PathBuf,
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
    /// that never fires a clean `SessionEnd`) and moved to `Idle`.
    pub idle_ttl: Duration,
    /// How often the idle sweep runs. Independent of `idle_ttl` so the sweep
    /// itself can be cheap/frequent without making sessions go idle any
    /// faster than `idle_ttl` allows.
    pub idle_sweep_interval: Duration,
    /// Durable record of session ids that have received a real `SessionEnd`
    /// hook (see `EndedSessions`), so a restart's reconstruction can tell a
    /// genuinely finished session apart from one that's still working —
    /// transcript-file recency alone can't distinguish those two cases.
    pub ended_sessions_path: PathBuf,
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            claude_projects_dir: crate::first_run::claude_projects_dir(),
            checkpoint_path: crate::first_run::app_data_dir().join("tail-checkpoints.json"),
            socket_path: hook_socket::socket_path(),
            // Generous but still bounded: a session where the user takes a
            // few seconds to type their first message shouldn't be treated
            // as empty, but a session that's genuinely abandoned (opened,
            // never used) must eventually give up rather than poll forever.
            transcript_wait: Duration::from_secs(8),
            // A day comfortably covers "closed the app overnight, reopened
            // the next morning" without resurrecting genuinely old sessions.
            reconstruction_recency: Duration::from_secs(24 * 60 * 60),
            // Generous enough that normal thinking/typing pauses between
            // turns never trip it, but short enough that a closed terminal
            // doesn't sit looking "Working" for the rest of the day.
            idle_ttl: Duration::from_secs(10 * 60),
            idle_sweep_interval: Duration::from_secs(60),
            ended_sessions_path: crate::first_run::app_data_dir().join("ended-sessions.json"),
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
/// plus `EndedSessions` for the one bit of lifecycle state that *is*
/// persisted. A session is restored as Working if — and only if — its
/// transcript file was modified more recently than `reconstruction_recency`,
/// treating that as evidence it's still relevant rather than long finished.
/// That recency check alone can't tell "still working" apart from "finished
/// cleanly a few minutes ago" (both just look like a recently-touched
/// transcript), which is what `ended_sessions` is for: a session recorded
/// there is restored straight to Done instead of Working. This is still a
/// heuristic for anything not in `ended_sessions` (e.g. a session whose
/// process was killed rather than exited cleanly) — the next real hook
/// event, or the idle sweep, is what corrects it from here.
async fn reconstruct_live_sessions(
    engine: &EngineHandle,
    repo: &MemoryRepo,
    orch_config: &OrchestratorConfig,
    ended_sessions: &EndedSessions,
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
        }
        engine.dispatch(EngineCommand::SetCategory { id: memory.session_id.clone(), category: memory.category }).await;
        engine.dispatch(EngineCommand::SetTask { id: memory.session_id, task: memory.text.chars().take(80).collect() }).await;
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
) {
    let ended_sessions = Arc::new(Mutex::new(EndedSessions::load(&orch_config.ended_sessions_path)));
    reconstruct_live_sessions(&engine, &repo, &orch_config, &*ended_sessions.lock().await).await;

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

        tokio::spawn(async move {
            handle_hook_event(event, &engine, &repo, ollama.as_ref(), &cat_config, &orch_config, &checkpoints, &ended_sessions)
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
) {
    let Some(session_id) = event.payload.get("session_id").and_then(|v| v.as_str()).map(str::to_string) else {
        return;
    };

    match event.event.as_str() {
        "session-start" => {
            let cwd = event.payload.get("cwd").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let project = project_name_from_cwd(&cwd);
            // "cli" (plain terminal `claude`) vs "claude-desktop" — carried
            // directly on real hook payloads (plan §Phase 2 item 6). Not
            // present -> default to "cli", the common case.
            let entrypoint = event.payload.get("entrypoint").and_then(|v| v.as_str()).map(str::to_string);

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
            let deadline = tokio::time::Instant::now() + orch_config.transcript_wait;
            let mut prompt = None;
            while prompt.is_none() && tokio::time::Instant::now() < deadline {
                if path.exists() {
                    let mut cp = checkpoints.lock().await;
                    let lines = tail_new_lines(&path, &mut cp).unwrap_or_default();
                    let _ = cp.save();
                    drop(cp);
                    prompt = extract_initiating_prompt(&lines);
                }
                if prompt.is_none() {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
            }
            let Some(prompt) = prompt else {
                return; // genuinely empty/abandoned session — never surfaced
            };

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
        "notification" => {
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
        // which can fire many times per turn) — the natural point to refresh
        // the "current task" line so it tracks the latest activity instead
        // of freezing after the initial prompt, without being too chatty.
        "stop" => {
            engine
                .dispatch(EngineCommand::SessionEvent {
                    id: session_id.clone(),
                    event: SessionEvent::ToolActivity,
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
            }
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
            ..OrchestratorConfig::default()
        };
        let ended_sessions = EndedSessions::load(&orch_config.ended_sessions_path);

        reconstruct_live_sessions(&engine, &repo, &orch_config, &ended_sessions).await;

        let snapshot = engine.snapshot().await;
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].id, session_id);
        assert_eq!(snapshot[0].category, "Backend / API");
        assert_eq!(snapshot[0].project, "api-gateway");
        assert_eq!(snapshot[0].state, crate::state::SessionState::Working);
        assert_eq!(snapshot[0].task, "Refactor auth middleware to async/await");
        assert_eq!(snapshot[0].started_at_ms, 1_700_000_000_000);
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
            ..OrchestratorConfig::default()
        };
        let mut ended_sessions = EndedSessions::load(&orch_config.ended_sessions_path);
        ended_sessions.mark_ended(session_id.to_string());

        reconstruct_live_sessions(&engine, &repo, &orch_config, &ended_sessions).await;

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
            ..OrchestratorConfig::default()
        };
        let ended_sessions = EndedSessions::load(&orch_config.ended_sessions_path);

        reconstruct_live_sessions(&engine, &repo, &orch_config, &ended_sessions).await;

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
            ..OrchestratorConfig::default()
        });

        tokio::spawn(run(engine.clone(), repo.clone(), ollama, cat_config, orch_config));
        // Give the orchestrator a moment to bind the UDS before the fake
        // hook-bridge client below tries to connect to it.
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Simulate hook-bridge exactly as the real compiled binary does:
        // connect, write the envelope, close.
        let envelope = serde_json::json!({
            "event": "session-start",
            "payload": {"session_id": session_id, "cwd": cwd}
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

        // Categorization resolves via the real pipeline, followed by a
        // SetTask diff (real task summary) — read forward past any
        // intermediate diffs rather than assuming an exact count, since
        // that's an implementation detail of the pipeline, not the contract
        // under test here.
        let categorized = recv_until(&mut diffs, |v| v.category != "Uncategorized")
            .await
            .expect("should receive a categorized diff before timing out");
        assert_eq!(categorized.id, session_id);
        assert_eq!(categorized.category, "Backend / API");

        // Now drive a Notification hook event through the same real path.
        let envelope = serde_json::json!({"event":"notification","payload":{"session_id": session_id}});
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
            ..OrchestratorConfig::default()
        };
        let checkpoints = Arc::new(Mutex::new(TailCheckpoints::load(&orch_config.checkpoint_path)));
        let ended_sessions = Arc::new(Mutex::new(EndedSessions::load(&orch_config.ended_sessions_path)));

        let event = HookEvent {
            event: "pre-tool-use".into(),
            payload: serde_json::json!({"session_id": "s1", "tool_name": "AskUserQuestion"}),
        };
        handle_hook_event(event, &engine, &repo, &ollama, &cat_config, &orch_config, &checkpoints, &ended_sessions).await;
        assert_eq!(engine.snapshot().await[0].state, crate::state::SessionState::Waiting);

        // A normal tool call (no special-cased tool_name) still just marks Working.
        let event = HookEvent {
            event: "pre-tool-use".into(),
            payload: serde_json::json!({"session_id": "s1", "tool_name": "Bash"}),
        };
        handle_hook_event(event, &engine, &repo, &ollama, &cat_config, &orch_config, &checkpoints, &ended_sessions).await;
        assert_eq!(engine.snapshot().await[0].state, crate::state::SessionState::Working);
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

        let event = HookEvent { event: "session-end".into(), payload: serde_json::json!({"session_id": "s1"}) };
        handle_hook_event(event, &engine, &repo, &ollama, &cat_config, &orch_config, &checkpoints, &ended_sessions).await;

        assert_eq!(engine.snapshot().await[0].state, crate::state::SessionState::Done);
        assert!(ended_sessions.lock().await.contains("s1"));

        // Reload from disk, simulating a restart, to confirm it was persisted.
        let reloaded = EndedSessions::load(&orch_config.ended_sessions_path);
        assert!(reloaded.contains("s1"));
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
            ..OrchestratorConfig::default()
        });

        tokio::spawn(run(engine.clone(), repo.clone(), ollama, cat_config, orch_config));
        tokio::time::sleep(Duration::from_millis(150)).await;

        let envelope = serde_json::json!({
            "event": "session-start",
            "payload": {"session_id": session_id, "cwd": "/Users/omricohen"}
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
            ..OrchestratorConfig::default()
        });

        tokio::spawn(run(engine.clone(), repo.clone(), ollama, cat_config, orch_config));
        tokio::time::sleep(Duration::from_millis(150)).await;

        let refresh_prompt = format!(
            "In one short sentence (under 12 words), describe what is currently being worked on, based on this recent activity: \"{}\"",
            "Now running the auth test suite."
        );
        fake.canned_labels.lock().await.insert(refresh_prompt, "Now running the auth test suite.".to_string());

        let send = |event: &str, extra_cwd: bool| {
            let socket_path = socket_path.clone();
            let payload = if extra_cwd {
                serde_json::json!({"session_id": session_id, "cwd": cwd})
            } else {
                serde_json::json!({"session_id": session_id})
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
        let refreshed = recv_until(&mut diffs, |v| v.task == "Now running the auth test suite.")
            .await
            .expect("should receive a refreshed task diff before timing out");
        assert_eq!(refreshed.task, "Now running the auth test suite.");
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
