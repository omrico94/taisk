//! The local HTTP/WS API (plan §7): the one surface both the desktop
//! frontend and (later, Phase 3) a VS Code extension talk to — "one source
//! of truth, no duplicated detection logic." Deliberately minimal: no
//! GraphQL, no generic CRUD layer, just the finite set of endpoints the
//! approved design's interactions actually need.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, patch, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex};
use tower_http::cors::CorsLayer;

use crate::boards::{Board, BoardStore, DEFAULT_BOARD_ID};
use crate::summarize::SummarizeConfig;
use crate::collector::{parse_transcript_for_display, transcript_path, TranscriptRow};
use crate::engine::{EngineCommand, EngineHandle, SessionView};
use crate::memory_repo::{MemoryRepo, PurgeScope};
use crate::ollama::OllamaClient;
use crate::orchestrator::{DismissedSessions, WaitingSessions};
use crate::tasks::{Stage, Task, TaskHub, TasksSnapshot};

#[derive(Clone)]
pub struct AppState {
    pub engine: EngineHandle,
    pub repo: Arc<MemoryRepo>,
    pub ollama: Arc<dyn OllamaClient>,
    pub config: Arc<SummarizeConfig>,
    pub claude_projects_dir: PathBuf,
    /// Shared with the orchestrator: boards and their session assignments.
    pub boards: Arc<BoardStore>,
    /// Path of the `hook-bridge` binary, used to (un)register hooks in a
    /// board's config directory. `None` (dev server, tests) means boards can
    /// be managed but their settings.json is never touched.
    pub hook_bridge_path: Option<String>,
    /// Shared with `orchestrator::run` — approve/reject/reply from the board
    /// resolve a `Waiting` session the same way a real hook would, so they
    /// must evict it from the same durable record reconstruction reads on
    /// the next restart (see `WaitingSessions`'s doc comment).
    pub waiting_sessions: Arc<Mutex<WaitingSessions>>,
    /// Shared with `orchestrator::run` for the same reason as
    /// `waiting_sessions` — a delete from the board has to durably record
    /// the dismissal in the same place reconstruction checks it, or the
    /// session just reappears on the next restart (see
    /// `DismissedSessions`'s doc comment).
    pub dismissed_sessions: Arc<Mutex<DismissedSessions>>,
    /// Kanban tasks + session→task assignments (see `tasks.rs`).
    pub tasks: TaskHub,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/sessions", get(get_sessions))
        .route("/events", get(ws_events))
        .route("/boards", get(list_boards).post(create_board))
        .route("/boards/{id}", patch(update_board).delete(delete_board))
        .route("/tasks", get(get_tasks).post(create_task))
        .route("/tasks/{id}", patch(update_task).delete(delete_task))
        .route("/sessions/{id}/task", put(assign_session))
        .route("/sessions/{id}/approve", post(approve))
        .route("/sessions/{id}/reject", post(reject))
        .route("/sessions/{id}/reply", post(reply))
        .route("/sessions/{id}/transcript", get(get_transcript))
        .route("/sessions/{id}", delete(delete_session))
        .route("/search", get(search))
        .route("/memories", delete(purge_memories))
        // Frontend (Tauri webview / Vite dev server) and this API are
        // different origins (different ports), so browser fetch() calls
        // need CORS even though it's all localhost. No auth/credentials
        // are involved and nothing here is reachable off 127.0.0.1, so a
        // permissive policy is the right amount of restriction, not none.
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn get_sessions(State(state): State<AppState>) -> Json<Vec<SessionView>> {
    Json(state.engine.snapshot().await)
}

async fn ws_events(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

/// Multiplexes the two live streams onto one socket: session diffs
/// (`{"Upserted": …}` / `{"Removed": …}`) and full task snapshots
/// (`{"TasksChanged": {tasks, assignments}}`). The frontend discriminates on
/// the single top-level key.
async fn handle_ws(mut socket: WebSocket, state: AppState) {
    let mut diffs = state.engine.subscribe();
    let mut tasks = state.tasks.subscribe();
    loop {
        let text = tokio::select! {
            diff = diffs.recv() => match diff {
                Ok(diff) => serde_json::to_string(&diff),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            },
            snap = tasks.recv() => match snap {
                Ok(snap) => serde_json::to_string(&serde_json::json!({ "TasksChanged": snap })),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            },
        };
        let Ok(text) = text else { continue };
        if socket.send(Message::Text(text.into())).await.is_err() {
            break;
        }
    }
}

async fn get_tasks(State(state): State<AppState>) -> Json<TasksSnapshot> {
    Json(state.tasks.snapshot().await)
}

#[derive(Deserialize)]
struct CreateTaskBody {
    title: String,
    stage: Stage,
    /// Board the task belongs to; omitted means the default board.
    board: Option<String>,
}

async fn create_task(State(state): State<AppState>, Json(body): Json<CreateTaskBody>) -> Result<Json<Task>, StatusCode> {
    let board = body.board.as_deref().unwrap_or(DEFAULT_BOARD_ID);
    if state.boards.get(board).is_none() {
        return Err(StatusCode::BAD_REQUEST);
    }
    state.tasks.create_on_board(&body.title, body.stage, board).await.map(Json).ok_or(StatusCode::BAD_REQUEST)
}

#[derive(Serialize)]
struct ApiError {
    error: String,
}

fn api_error(status: StatusCode, message: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (status, Json(ApiError { error: message.into() }))
}

async fn list_boards(State(state): State<AppState>) -> Json<Vec<Board>> {
    Json(state.boards.list())
}

#[derive(Deserialize)]
struct CreateBoardBody {
    name: String,
    config_dir: String,
}

/// Adds a board and installs our hooks into its config directory (created if
/// it doesn't exist yet, so the user can log in there afterwards). If the hook
/// install fails the board is rolled back, so a listed board always works.
async fn create_board(
    State(state): State<AppState>,
    Json(body): Json<CreateBoardBody>,
) -> Result<Json<Board>, (StatusCode, Json<ApiError>)> {
    let board = state.boards.add(&body.name, &body.config_dir).map_err(|e| api_error(StatusCode::BAD_REQUEST, e))?;
    if let Some(bridge) = &state.hook_bridge_path {
        if let Err(e) = crate::first_run::register_board_hooks(bridge, &board) {
            let _ = state.boards.remove(&board.id);
            return Err(api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("couldn't write hooks into {}: {e}", board.config_dir.display()),
            ));
        }
    }
    Ok(Json(board))
}

#[derive(Deserialize)]
struct UpdateBoardBody {
    name: String,
}

async fn update_board(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateBoardBody>,
) -> Result<Json<Board>, (StatusCode, Json<ApiError>)> {
    state.boards.rename(&id, &body.name).map(Json).map_err(|e| api_error(StatusCode::BAD_REQUEST, e))
}

/// Removes a board: takes its live cards and its tasks off SessionBoard and
/// strips our hooks from its config directory. Never touches the directory's
/// other contents or any transcripts.
async fn delete_board(State(state): State<AppState>, Path(id): Path<String>) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    let board = state.boards.remove(&id).map_err(|e| api_error(StatusCode::BAD_REQUEST, e))?;
    state.tasks.delete_board(&id).await;
    for session in state.engine.snapshot().await.into_iter().filter(|s| s.board == id) {
        unmark_waiting(&state, &session.id).await;
        state.tasks.forget_session(&session.id).await;
        state.engine.dispatch(EngineCommand::RemoveSession { id: session.id }).await;
    }
    if let Some(bridge) = &state.hook_bridge_path {
        let _ = crate::first_run::unregister_board_hooks(bridge, &board);
    }
    Ok(StatusCode::OK)
}

#[derive(Deserialize)]
struct UpdateTaskBody {
    title: Option<String>,
    stage: Option<Stage>,
}

async fn update_task(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateTaskBody>,
) -> StatusCode {
    if state.tasks.update(&id, body.title.as_deref(), body.stage).await {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn delete_task(State(state): State<AppState>, Path(id): Path<String>) -> StatusCode {
    if state.tasks.delete(&id).await {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

#[derive(Deserialize)]
struct AssignBody {
    task_id: Option<String>,
}

/// Assign a session to a task, or unassign it with `task_id: null`.
async fn assign_session(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AssignBody>,
) -> StatusCode {
    let sessions = state.engine.snapshot().await;
    if state.tasks.assign(&id, body.task_id.as_deref(), &sessions).await {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

#[derive(Deserialize, Default)]
struct ReplyBody {
    text: Option<String>,
}

/// Evicts `id` from the durable waiting-record, if present — resolving a
/// wait from the board must keep that record in sync with the live engine
/// the same way a real hook does (see `WaitingSessions`), or a restart
/// shortly after would incorrectly restore it as `Waiting` again.
async fn unmark_waiting(state: &AppState, id: &str) {
    let mut waiting = state.waiting_sessions.lock().await;
    if waiting.unmark_waiting(id) {
        let _ = waiting.save();
    }
}

async fn approve(State(state): State<AppState>, Path(id): Path<String>) -> StatusCode {
    unmark_waiting(&state, &id).await;
    state
        .engine
        .dispatch(EngineCommand::ResolveWaiting { id, desc: "Resumed — applying your approval".into() })
        .await;
    StatusCode::OK
}

async fn reject(State(state): State<AppState>, Path(id): Path<String>) -> StatusCode {
    unmark_waiting(&state, &id).await;
    state
        .engine
        .dispatch(EngineCommand::ResolveWaiting { id, desc: "Continuing without that change".into() })
        .await;
    StatusCode::OK
}

async fn reply(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<ReplyBody>>,
) -> StatusCode {
    unmark_waiting(&state, &id).await;
    let text = body.and_then(|b| b.0.text).filter(|t| !t.trim().is_empty());
    let desc = match text {
        Some(t) => format!("Working on: {t}"),
        None => "Working on your reply…".into(),
    };
    state.engine.dispatch(EngineCommand::ResolveWaiting { id, desc }).await;
    StatusCode::OK
}

/// The board's "Delete" action — drops the session's live card immediately
/// and durably records the dismissal (see `DismissedSessions`) so it doesn't
/// simply reappear the next time the app restarts and reconstructs the board
/// from `memories`. This only removes the card; it doesn't touch the
/// underlying transcript/memory data, and a later real hook for the same id
/// (a resumed conversation) revives it same as any other dismissal-eviction
/// path in this codebase.
async fn delete_session(State(state): State<AppState>, Path(id): Path<String>) -> StatusCode {
    {
        let mut dismissed = state.dismissed_sessions.lock().await;
        dismissed.mark_dismissed(id.clone());
        let _ = dismissed.save();
    }
    unmark_waiting(&state, &id).await;
    state.tasks.forget_session(&id).await;
    state.engine.dispatch(EngineCommand::RemoveSession { id }).await;
    StatusCode::OK
}

/// Backs the design's "Transcript tail" panel — re-reads the session's
/// transcript file fresh on each request (not the append-only tailing/
/// checkpoint path used for ingestion, which is a different concern: this
/// is a point-in-time read for display, not incremental consumption).
/// Returns an empty list for an unknown session or missing file rather than
/// an error — a session that hasn't produced a transcript yet isn't a
/// failure case.
async fn get_transcript(State(state): State<AppState>, Path(id): Path<String>) -> Json<Vec<TranscriptRow>> {
    let sessions = state.engine.snapshot().await;
    let Some(session) = sessions.into_iter().find(|s| s.id == id) else {
        return Json(vec![]);
    };
    let projects_dir = match state.boards.get(&session.board) {
        Some(b) if b.id != DEFAULT_BOARD_ID => b.config_dir.join("projects"),
        _ => state.claude_projects_dir.clone(),
    };
    let path = transcript_path(&projects_dir, &session.cwd, &session.id);
    Json(parse_transcript_for_display(&path))
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    /// Restrict results to one board. Omitted searches every board.
    board: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct SearchResult {
    text: String,
    project: String,
    tool: String,
    session_id: String,
    distance: f32,
    /// Distinguishes a live session's content from purely historical memory
    /// — the ⌘K overlay renders these differently (design: "● live now" vs a
    /// date) and clicking a live result opens its drawer.
    live: bool,
    created_at: i64,
}

/// Phase 2 roadmap item 5a originally shipped a hard server-side floor
/// (`distance <= 0.3`, i.e. only results the frontend would display as
/// "70%+ relevant" — see AskMemoryOverlay.tsx's `1 - distance` formula).
/// Real-world nomic-embed-text distances turned out not to support that:
/// a search for "todo" against a memory whose text literally contains "Todo
/// list" scored a 0.4999 distance (50%) — nowhere near 70% — and gradually
/// weaker matches trail off from there rather than clustering near 0. A
/// fixed cutoff calibrated against one query breaks the next one, so there's
/// no single "right" threshold to hard-code here. Returning the nearest
/// `limit` matches ranked by relevance and letting the frontend's visible
/// percentage badge communicate confidence per result — rather than
/// silently hiding anything below a guessed-at bar — is the fix: a
/// low-relevance result the user can *see* is low-relevance is more useful
/// than an empty list.
async fn search(State(state): State<AppState>, Query(q): Query<SearchQuery>) -> Json<Vec<SearchResult>> {
    let Ok(embedding) = state.ollama.embed(&state.config.embedding_model, &q.q).await else {
        return Json(vec![]);
    };
    let live_ids: HashSet<String> = state.engine.snapshot().await.into_iter().map(|s| s.id).collect();
    // Over-fetch when filtering by board so a board with few matches isn't
    // starved by other boards' rows in the global top-N.
    let fetch = if q.board.is_some() { 50 } else { 10 };
    let results = state.repo.search(&embedding, fetch).await.unwrap_or_default();
    Json(
        results
            .into_iter()
            .filter(|r| q.board.as_deref().is_none_or(|b| state.boards.board_of(&r.memory.session_id) == b))
            .take(10)
            .map(|r| SearchResult {
                live: live_ids.contains(&r.memory.session_id),
                text: r.memory.text,
                project: r.memory.project,
                tool: r.memory.tool,
                session_id: r.memory.session_id,
                distance: r.distance,
                created_at: r.memory.created_at,
            })
            .collect(),
    )
}

#[derive(Deserialize)]
struct PurgeQuery {
    project: Option<String>,
    all: Option<bool>,
}

/// Explicit, user-triggered purge (plan §6 retention policy) — deliberately
/// not exposed anywhere in the main board UI, only behind a settings panel.
async fn purge_memories(State(state): State<AppState>, Query(q): Query<PurgeQuery>) -> StatusCode {
    let scope = if q.all.unwrap_or(false) {
        PurgeScope::All
    } else if let Some(project) = q.project {
        PurgeScope::Project(project)
    } else {
        return StatusCode::BAD_REQUEST;
    };
    match state.repo.purge(scope).await {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_repo::{Memory, MemoryKind};
    use crate::ollama::fake::FakeOllamaClient;
    use crate::state::SessionEvent;
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    async fn spawn_test_server() -> (String, AppState) {
        let lance_dir = tempfile::tempdir().unwrap();
        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        let claude_dir = tempfile::tempdir().unwrap();
        let claude_projects_dir = claude_dir.path().to_path_buf();
        let app_dir = tempfile::tempdir().unwrap();
        let waiting_sessions_path = app_dir.path().join("waiting-sessions.json");
        let dismissed_sessions_path = app_dir.path().join("dismissed-sessions.json");
        let app_dir_path = app_dir.path().to_path_buf();
        // Leak the tempdirs so they aren't cleaned up while the server is
        // running for the lifetime of the test.
        std::mem::forget(lance_dir);
        std::mem::forget(claude_dir);
        std::mem::forget(app_dir);

        let state = AppState {
            engine: EngineHandle::spawn(),
            repo: Arc::new(repo),
            ollama: Arc::new(FakeOllamaClient::new("Backend / API")),
            config: Arc::new(SummarizeConfig::default()),
            claude_projects_dir,
            boards: Arc::new(BoardStore::in_memory()),
            hook_bridge_path: None,
            waiting_sessions: Arc::new(Mutex::new(WaitingSessions::load(&waiting_sessions_path))),
            dismissed_sessions: Arc::new(Mutex::new(DismissedSessions::load(&dismissed_sessions_path))),
            tasks: TaskHub::load(&app_dir_path.join("tasks.json")),
        };

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(state.clone());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        (format!("http://{addr}"), state)
    }

    #[tokio::test]
    async fn get_sessions_reflects_engine_state() {
        let (base, state) = spawn_test_server().await;
        let client = reqwest::Client::new();

        let sessions: Vec<SessionView> = client
            .get(format!("{base}/sessions"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(sessions.is_empty());

        state
            .engine
            .dispatch(EngineCommand::SessionEvent {
                id: "s1".into(),
                event: SessionEvent::SessionStart,
                project: Some("api-gateway".into()),
                cwd: Some("/x".into()),
                started_at_ms: Some(0),
                entrypoint: None,
            })
            .await;

        let sessions: Vec<SessionView> = client
            .get(format!("{base}/sessions"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "s1");
    }

    #[tokio::test]
    async fn ws_events_delivers_a_diff_on_a_real_state_change() {
        let (base, state) = spawn_test_server().await;
        let ws_url = base.replace("http://", "ws://") + "/events";

        let (mut ws_stream, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();

        state
            .engine
            .dispatch(EngineCommand::SessionEvent {
                id: "s1".into(),
                event: SessionEvent::SessionStart,
                project: Some("api-gateway".into()),
                cwd: Some("/x".into()),
                started_at_ms: Some(0),
                entrypoint: None,
            })
            .await;

        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ws_stream.next())
            .await
            .expect("should receive a diff before timing out")
            .unwrap()
            .unwrap();
        let WsMessage::Text(text) = msg else { panic!("expected a text frame") };
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["Upserted"]["id"], "s1");

        ws_stream.close(None).await.ok();
    }

    #[tokio::test]
    async fn tasks_crud_assign_unassign_and_delete_orphans_sessions() {
        let (base, state) = spawn_test_server().await;
        let client = reqwest::Client::new();

        // Blank title rejected.
        let r = client.post(format!("{base}/tasks")).json(&serde_json::json!({"title": " ", "stage": "todo"})).send().await.unwrap();
        assert_eq!(r.status(), 400);

        let task: Task = client
            .post(format!("{base}/tasks"))
            .json(&serde_json::json!({"title": "Ship auth", "stage": "todo"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(task.stage, Stage::Todo);

        let r = client.patch(format!("{base}/tasks/{}", task.id)).json(&serde_json::json!({"stage": "inprogress"})).send().await.unwrap();
        assert_eq!(r.status(), 200);
        let r = client.patch(format!("{base}/tasks/nope")).json(&serde_json::json!({"stage": "done"})).send().await.unwrap();
        assert_eq!(r.status(), 404);

        // Assign, then reject an unknown task, then unassign.
        let r = client.put(format!("{base}/sessions/s1/task")).json(&serde_json::json!({"task_id": task.id})).send().await.unwrap();
        assert_eq!(r.status(), 200);
        let r = client.put(format!("{base}/sessions/s1/task")).json(&serde_json::json!({"task_id": "nope"})).send().await.unwrap();
        assert_eq!(r.status(), 404);
        let snap: TasksSnapshot = client.get(format!("{base}/tasks")).send().await.unwrap().json().await.unwrap();
        assert_eq!(snap.tasks[0].stage, Stage::InProgress);
        assert_eq!(snap.assignments.get("s1"), Some(&task.id));
        client.put(format!("{base}/sessions/s1/task")).json(&serde_json::json!({"task_id": null})).send().await.unwrap();
        assert!(state.tasks.snapshot().await.assignments.is_empty());

        // Deleting a task drops its assignments; deleting a session drops its own.
        client.put(format!("{base}/sessions/s2/task")).json(&serde_json::json!({"task_id": task.id})).send().await.unwrap();
        client.delete(format!("{base}/sessions/s2")).send().await.unwrap();
        assert!(state.tasks.snapshot().await.assignments.is_empty());
        client.put(format!("{base}/sessions/s3/task")).json(&serde_json::json!({"task_id": task.id})).send().await.unwrap();
        assert_eq!(client.delete(format!("{base}/tasks/{}", task.id)).send().await.unwrap().status(), 200);
        let snap = state.tasks.snapshot().await;
        assert!(snap.tasks.is_empty() && snap.assignments.is_empty());
    }

    #[tokio::test]
    async fn ws_delivers_tasks_changed_snapshots() {
        let (base, _state) = spawn_test_server().await;
        let ws_url = base.replace("http://", "ws://") + "/events";
        let (mut ws_stream, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        reqwest::Client::new()
            .post(format!("{base}/tasks"))
            .json(&serde_json::json!({"title": "A", "stage": "backlog"}))
            .send()
            .await
            .unwrap();

        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ws_stream.next()).await.unwrap().unwrap().unwrap();
        let WsMessage::Text(text) = msg else { panic!("expected text") };
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["TasksChanged"]["tasks"][0]["title"], "A");
        assert_eq!(value["TasksChanged"]["tasks"][0]["stage"], "backlog");
    }

    #[tokio::test]
    async fn approve_reject_and_reply_resolve_via_engine() {
        let (base, state) = spawn_test_server().await;
        let client = reqwest::Client::new();

        state
            .engine
            .dispatch(EngineCommand::SessionEvent {
                id: "s1".into(),
                event: SessionEvent::SessionStart,
                project: None,
                cwd: None,
                started_at_ms: None,
                entrypoint: None,
            })
            .await;
        state
            .engine
            .dispatch(EngineCommand::SessionEvent {
                id: "s1".into(),
                event: SessionEvent::Notification,
                project: None,
                cwd: None,
                started_at_ms: None,
                entrypoint: None,
            })
            .await;

        let resp = client.post(format!("{base}/sessions/s1/approve")).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let snapshot = state.engine.snapshot().await;
        assert_eq!(snapshot[0].state, crate::state::SessionState::Working);
        assert_eq!(snapshot[0].desc, "Resumed — applying your approval");

        let resp = client
            .post(format!("{base}/sessions/s1/reply"))
            .json(&serde_json::json!({"text": "please also add tests"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(state.engine.snapshot().await[0].desc, "Working on: please also add tests");

    }

    #[tokio::test]
    async fn delete_session_removes_it_and_durably_marks_it_dismissed() {
        let (base, state) = spawn_test_server().await;
        let client = reqwest::Client::new();

        state
            .engine
            .dispatch(EngineCommand::SessionEvent {
                id: "s1".into(),
                event: SessionEvent::SessionStart,
                project: None,
                cwd: None,
                started_at_ms: None,
                entrypoint: None,
            })
            .await;
        assert_eq!(state.engine.snapshot().await.len(), 1);

        let resp = client.delete(format!("{base}/sessions/s1")).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert!(state.engine.snapshot().await.is_empty(), "deleting must remove the live card");
        assert!(
            state.dismissed_sessions.lock().await.contains("s1"),
            "deleting must durably mark the session dismissed so it doesn't reappear on restart"
        );
    }

    #[tokio::test]
    async fn search_marks_live_sessions_and_purge_removes_them() {
        let (base, state) = spawn_test_server().await;
        let client = reqwest::Client::new();

        state
            .engine
            .dispatch(EngineCommand::SessionEvent {
                id: "s1".into(),
                event: SessionEvent::SessionStart,
                project: Some("api-gateway".into()),
                cwd: Some("/x".into()),
                started_at_ms: Some(0),
                entrypoint: None,
            })
            .await;

        let embedding = state.ollama.embed("nomic-embed-text", "refactor auth middleware").await.unwrap();
        state
            .repo
            .upsert_memory(&Memory {
                id: "s1-prompt".into(),
                session_id: "s1".into(),
                kind: MemoryKind::Prompt,
                text: "refactor auth middleware".into(),
                embedding: embedding.clone(),
                project: "api-gateway".into(),
                cwd: "/x".into(),
                tool: "Claude Code".into(),
                title: "Refactor auth middleware".into(),
                created_at: 0,
            })
            .await
            .unwrap();

        let results: Vec<SearchResult> = client
            .get(format!("{base}/search?q=refactor auth middleware"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].live, "session s1 is active, result should be marked live");

        let resp = client.delete(format!("{base}/memories?all=true")).send().await.unwrap();
        assert_eq!(resp.status(), 200);

        let results: Vec<SearchResult> = client
            .get(format!("{base}/search?q=refactor auth middleware"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(results.is_empty(), "purge(all) should remove every memory row");
    }

    /// Regression (user report, twice — search for a query as literal as
    /// "todo" against a memory whose text contains "Todo list" came back
    /// empty): there is no hard relevance floor. Both a near-exact match and
    /// a genuinely weak one come back, ranked by relevance, so the frontend
    /// always has something to show — its percentage badge is what
    /// communicates confidence per result, not a server-side cutoff.
    #[tokio::test]
    async fn search_returns_both_strong_and_weak_matches_ranked_by_relevance() {
        let (base, state) = spawn_test_server().await;
        let client = reqwest::Client::new();

        let query_embedding = state.ollama.embed("nomic-embed-text", "refactor auth middleware").await.unwrap();

        // High-relevance memory: identical embedding -> cosine distance ~0.
        state
            .repo
            .upsert_memory(&Memory {
                id: "high".into(),
                session_id: "s-high".into(),
                kind: MemoryKind::Prompt,
                text: "refactor auth middleware".into(),
                embedding: query_embedding.clone(),
                project: "api-gateway".into(),
                cwd: "/x".into(),
                tool: "Claude Code".into(),
                title: "Refactor auth middleware".into(),
                created_at: 0,
            })
            .await
            .unwrap();

        // Low-relevance memory: flip half the embedding's dimensions so cosine
        // distance lands much further away — a weak match, but still a real
        // one that must be returned, not silently dropped.
        let mut low_embedding = query_embedding.clone();
        for v in low_embedding.iter_mut().take(400) {
            *v = -*v;
        }
        state
            .repo
            .upsert_memory(&Memory {
                id: "low".into(),
                session_id: "s-low".into(),
                kind: MemoryKind::Prompt,
                text: "totally unrelated topic".into(),
                embedding: low_embedding,
                project: "api-gateway".into(),
                cwd: "/x".into(),
                tool: "Claude Code".into(),
                title: "Refactor auth middleware".into(),
                created_at: 0,
            })
            .await
            .unwrap();

        let results: Vec<SearchResult> = client
            .get(format!("{base}/search?q=refactor auth middleware"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        assert_eq!(results.len(), 2, "both the strong and weak match must come back — no hard floor");
        assert_eq!(results[0].session_id, "s-high", "the closer match must rank first");
        assert!(results[0].distance < results[1].distance);
    }

    #[tokio::test]
    async fn transcript_endpoint_reads_the_sessions_real_file() {
        let (base, state) = spawn_test_server().await;
        let client = reqwest::Client::new();

        let cwd = "/Users/omricohen/api-gateway".to_string();
        state
            .engine
            .dispatch(EngineCommand::SessionEvent {
                id: "s1".into(),
                event: SessionEvent::SessionStart,
                project: Some("api-gateway".into()),
                cwd: Some(cwd.clone()),
                started_at_ms: Some(0),
                entrypoint: None,
            })
            .await;

        // Unknown session -> empty, not an error.
        let rows: Vec<TranscriptRow> = client
            .get(format!("{base}/sessions/does-not-exist/transcript"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(rows.is_empty());

        // Write the real transcript file at exactly the path the endpoint computes.
        let path = transcript_path(&state.claude_projects_dir, &cwd, "s1");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({"type":"user","message":{"content":"refactor auth"}}).to_string() + "\n",
        )
        .unwrap();

        let rows: Vec<TranscriptRow> = client
            .get(format!("{base}/sessions/s1/transcript"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(rows, vec![TranscriptRow { role: "You".into(), text: "refactor auth".into() }]);
    }

    #[tokio::test]
    async fn boards_scope_tasks_and_removing_one_clears_its_cards() {
        let (base, state) = spawn_test_server().await;
        let client = reqwest::Client::new();

        let boards: Vec<Board> = client.get(format!("{base}/boards")).send().await.unwrap().json().await.unwrap();
        assert_eq!(boards.len(), 1);
        assert_eq!(boards[0].id, DEFAULT_BOARD_ID);

        let created = client
            .post(format!("{base}/boards"))
            .json(&serde_json::json!({"name": "Work", "config_dir": "/tmp/sessionboard-test-work"}))
            .send()
            .await
            .unwrap();
        assert_eq!(created.status(), 200);
        let work: Board = created.json().await.unwrap();
        assert_eq!(work.id, "work");

        let dup = client
            .post(format!("{base}/boards"))
            .json(&serde_json::json!({"name": "Work", "config_dir": "/tmp/elsewhere"}))
            .send()
            .await
            .unwrap();
        assert_eq!(dup.status(), 400);
        let body: serde_json::Value = dup.json().await.unwrap();
        assert!(body["error"].as_str().unwrap().contains("already exists"));

        // Tasks belong to a board (default when omitted); unknown board is rejected.
        let on_work: Task = client
            .post(format!("{base}/tasks"))
            .json(&serde_json::json!({"title": "Billing", "stage": "todo", "board": "work"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let on_default: Task = client
            .post(format!("{base}/tasks"))
            .json(&serde_json::json!({"title": "Ops", "stage": "todo"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(on_work.board, "work");
        assert_eq!(on_default.board, DEFAULT_BOARD_ID);
        let bad = client
            .post(format!("{base}/tasks"))
            .json(&serde_json::json!({"title": "X", "stage": "todo", "board": "ghost"}))
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 400);

        let renamed: Board = client
            .patch(format!("{base}/boards/work"))
            .json(&serde_json::json!({"name": "Job"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(renamed.name, "Job");

        // A live card on the work board, assigned to its task, goes away with
        // the board — along with the task. Default is untouched and permanent.
        state
            .engine
            .dispatch(EngineCommand::SessionEvent {
                id: "w1".into(),
                event: SessionEvent::SessionStart,
                project: None,
                cwd: None,
                entrypoint: None,
                started_at_ms: Some(0),
            })
            .await;
        state.engine.dispatch(EngineCommand::SetBoard { id: "w1".into(), board: "work".into() }).await;
        client.put(format!("{base}/sessions/w1/task")).json(&serde_json::json!({"task_id": on_work.id})).send().await.unwrap();

        assert_eq!(client.delete(format!("{base}/boards/default")).send().await.unwrap().status(), 400);
        assert_eq!(client.delete(format!("{base}/boards/work")).send().await.unwrap().status(), 200);
        assert!(state.engine.snapshot().await.is_empty());
        let snap = state.tasks.snapshot().await;
        assert_eq!(snap.tasks.len(), 1);
        assert_eq!(snap.tasks[0].id, on_default.id);
        assert!(snap.assignments.is_empty());
    }
}
