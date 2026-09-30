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
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex};
use tower_http::cors::CorsLayer;

use crate::boards::{Board, BoardStore, DEFAULT_BOARD_ID};
use crate::summarize::{InferenceBackend, SummarizeConfig};
use crate::collector::{parse_transcript_for_display, transcript_path, TranscriptRow};
use crate::engine::{EngineCommand, EngineHandle, SessionView};
use crate::memory_repo::{Memory, MemoryRepo, PurgeScope, ScoredMemory};
use crate::ollama::OllamaClient;
use crate::orchestrator::{DismissedSessions, WaitingSessions};
use crate::tasks::{Column, ColumnPatch, RolesPatch, Stage, Task, TaskError, TaskHub, TasksSnapshot};
use crate::terminal::{PtyInfo, PtyOutput, SpawnSpec, TerminalManager};

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
    /// Embedded PTY-backed terminals (see `terminal.rs`). Shared with
    /// `orchestrator::run`, which links a pending pty to its real session id.
    pub terminal: TerminalManager,
    /// Program spawned for a new/resumed session. `claude` in production;
    /// tests substitute a harmless fixture.
    pub claude_bin: String,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/sessions", get(get_sessions))
        .route("/events", get(ws_events))
        .route("/boards", get(list_boards).post(create_board))
        .route("/boards/{id}", patch(update_board).delete(delete_board))
        .route("/boards/{id}/columns", post(add_column))
        .route("/boards/{id}/columns/order", put(reorder_columns))
        .route("/boards/{id}/columns/roles", put(set_column_roles))
        .route("/boards/{id}/columns/{col}", patch(update_column).delete(delete_column))
        .route("/tasks", get(get_tasks).post(create_task))
        .route("/tasks/{id}", patch(update_task).delete(delete_task))
        .route("/sessions/{id}/task", put(assign_session))
        .route("/sessions/{id}/approve", post(approve))
        .route("/sessions/{id}/reject", post(reject))
        .route("/sessions/{id}/reply", post(reply))
        .route("/sessions/{id}/transcript", get(get_transcript))
        .route("/sessions/{id}", delete(delete_session))
        .route("/terminals/sessions/{session_id}", post(open_session_terminal))
        .route("/terminals/tasks/{task_id}", post(open_task_terminal))
        .route("/terminals/{pty_id}", get(get_terminal_info))
        .route("/terminals/{pty_id}/ws", get(ws_terminal))
        .route("/search", get(search))
        .route("/inference", get(get_inference))
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

#[derive(Serialize)]
struct OpenedTerminal {
    pty_id: String,
    reused: bool,
}

/// "Jump into this session": reuses the running pty for it if there is one,
/// otherwise spawns `claude --resume <id>` in the session's own cwd.
async fn open_session_terminal(
    State(state): State<AppState>,
    Path(session_id): Path<String>,
) -> Result<Json<OpenedTerminal>, StatusCode> {
    if let Some(pty_id) = state.terminal.find_by_session_id(&session_id) {
        return Ok(Json(OpenedTerminal { pty_id, reused: true }));
    }
    let session = state
        .engine
        .snapshot()
        .await
        .into_iter()
        .find(|s| s.id == session_id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let env = board_env(&state, &session.board);
    let cwd = usable_cwd(Some(session.cwd));
    trust_project_dir(&state, &session.board, &cwd);
    let pty_id = state
        .terminal
        .spawn(SpawnSpec {
            cwd,
            program: state.claude_bin.clone(),
            args: vec!["--resume".into(), session_id.clone()],
            env,
            session_id: Some(session_id),
            task_id: None,
        })
        .map_err(|e| {
            eprintln!("failed to spawn terminal: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(Json(OpenedTerminal { pty_id, reused: false }))
}

#[derive(Deserialize, Default)]
struct OpenTaskTerminalBody {
    cwd: Option<String>,
}

/// "Start a new session from a task": always a fresh process. The task id
/// rides along as `SESSIONBOARD_TASK_ID` so the engine files the new session
/// under the task once its session-start hook fires (see hook-bridge).
async fn open_task_terminal(
    State(state): State<AppState>,
    Path(task_id): Path<String>,
    body: Option<Json<OpenTaskTerminalBody>>,
) -> Result<Json<OpenedTerminal>, StatusCode> {
    let task = state
        .tasks
        .snapshot()
        .await
        .tasks
        .into_iter()
        .find(|t| t.id == task_id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let (cwd, add_dirs) = task_launch_dirs(&task.directories, body.and_then(|Json(b)| b.cwd));
    let mut env = board_env(&state, &task.board);
    env.push(("SESSIONBOARD_TASK_ID".to_string(), task_id.clone()));
    trust_project_dir(&state, &task.board, &cwd);
    let mut args = Vec::new();
    for dir in add_dirs {
        trust_project_dir(&state, &task.board, &dir);
        args.push("--add-dir".to_string());
        args.push(dir);
    }
    let pty_id = state
        .terminal
        .spawn(SpawnSpec {
            cwd,
            program: state.claude_bin.clone(),
            args,
            env,
            session_id: None,
            task_id: Some(task_id),
        })
        .map_err(|e| {
            eprintln!("failed to spawn terminal: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    Ok(Json(OpenedTerminal { pty_id, reused: false }))
}

/// Where a session started from a task runs: the task's first attached
/// directory that still exists is the cwd and every other existing one is
/// handed to `claude --add-dir`. With nothing attached (or nothing left on
/// disk) it falls back to `fallback_cwd` (the frontend sends the task's most
/// recent session's cwd), then the home dir, same as before directories.
fn task_launch_dirs(directories: &[String], fallback_cwd: Option<String>) -> (String, Vec<String>) {
    let mut existing = directories.iter().filter(|d| std::path::Path::new(d).is_dir()).cloned();
    match existing.next() {
        Some(cwd) => (cwd, existing.collect()),
        None => (usable_cwd(fallback_cwd), vec![]),
    }
}

/// Validates directories typed into a task: `~` expanded, trailing slashes
/// dropped, duplicates removed; each must be an existing absolute directory.
fn normalize_task_directories(raw: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for entry in raw.iter().map(|d| d.trim()).filter(|d| !d.is_empty()) {
        let path = crate::boards::expand_home(entry);
        if !path.is_absolute() {
            return Err(format!("{entry} must be an absolute path (or start with ~/)"));
        }
        if !path.is_dir() {
            return Err(format!("{entry} is not an existing directory"));
        }
        let mut s = path.to_string_lossy().to_string();
        while s.len() > 1 && s.ends_with('/') {
            s.pop();
        }
        if !out.contains(&s) {
            out.push(s);
        }
    }
    Ok(out)
}

/// A session on a non-default board lives under that board's own Claude config
/// dir (its own login, transcripts, hooks); `claude` only finds or resumes it
/// when run with the same `CLAUDE_CONFIG_DIR`.
fn board_env(state: &AppState, board: &str) -> Vec<(String, String)> {
    match state.boards.get(board) {
        Some(b) if b.id != DEFAULT_BOARD_ID => {
            vec![("CLAUDE_CONFIG_DIR".to_string(), b.config_dir.to_string_lossy().to_string())]
        }
        _ => vec![],
    }
}

/// Pre-approves Claude Code's "do you trust this folder?" dialog for `cwd`
/// before taisk spawns a `claude` process into it, in whichever board's
/// config dir that process will actually read from (`board_env` above picks
/// the same one via `CLAUDE_CONFIG_DIR`). A named board's own config dir
/// keeps its trust store *inside* itself (`<config_dir>/.claude.json`,
/// confirmed against a real board's on-disk layout); the default board's
/// lives *outside* `~/.claude`, as the sibling `~/.claude.json` (also
/// confirmed against this project's own layout) — see
/// `first_run::claude_user_config_path`'s doc comment. Best-effort: a
/// failure here just means the user sees the normal interactive prompt
/// instead of a silently-abandoned session, so it's logged and swallowed
/// rather than failing the terminal launch over it.
fn trust_project_dir(state: &AppState, board: &str, cwd: &str) {
    let path = match state.boards.get(board) {
        Some(b) if b.id != DEFAULT_BOARD_ID => b.config_dir.join(".claude.json"),
        _ => crate::first_run::claude_user_config_path(),
    };
    if let Err(e) = crate::first_run::trust_project_dir(&path, cwd) {
        eprintln!("failed to pre-trust {cwd} in {path:?}: {e}");
    }
}

/// A directory that exists, else the user's home directory.
fn usable_cwd(cwd: Option<String>) -> String {
    cwd.filter(|c| std::path::Path::new(c).is_dir())
        .or_else(|| dirs::home_dir().map(|h| h.to_string_lossy().to_string()))
        .unwrap_or_else(|| "/".to_string())
}

async fn get_terminal_info(State(state): State<AppState>, Path(pty_id): Path<String>) -> Result<Json<PtyInfo>, StatusCode> {
    state.terminal.info(&pty_id).map(Json).ok_or(StatusCode::NOT_FOUND)
}

async fn ws_terminal(ws: WebSocketUpgrade, Path(pty_id): Path<String>, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_terminal_ws(socket, pty_id, state))
}

async fn send_json(socket: &mut WebSocket, value: serde_json::Value) -> bool {
    socket.send(Message::Text(value.to_string().into())).await.is_ok()
}

/// Bidirectional (unlike `handle_ws`, which only sends). Server → client:
/// `scrollback` (replay of recent output, once, first), `data`, `linked`,
/// `exited`. Client → server: `input` and `resize`. Payloads are base64
/// since PTY output isn't guaranteed to be valid UTF-8.
async fn handle_terminal_ws(mut socket: WebSocket, pty_id: String, state: AppState) {
    let Some(sub) = state.terminal.subscribe(&pty_id) else {
        let _ = socket.send(Message::Close(None)).await;
        return;
    };
    let mut rx = sub.rx;
    if !send_json(&mut socket, serde_json::json!({"type": "scrollback", "data": B64.encode(&sub.scrollback)})).await {
        return;
    }
    if let Some(sid) = &sub.session_id {
        if !send_json(&mut socket, serde_json::json!({"type": "linked", "session_id": sid})).await {
            return;
        }
    }
    if let Some(code) = sub.exited {
        let _ = send_json(&mut socket, serde_json::json!({"type": "exited", "code": code})).await;
        return;
    }
    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Ok(PtyOutput::Data(bytes)) => {
                    if !send_json(&mut socket, serde_json::json!({"type": "data", "data": B64.encode(&bytes)})).await { break; }
                }
                Ok(PtyOutput::Linked(sid)) => {
                    if !send_json(&mut socket, serde_json::json!({"type": "linked", "session_id": sid})).await { break; }
                }
                Ok(PtyOutput::Exited(code)) => {
                    let _ = send_json(&mut socket, serde_json::json!({"type": "exited", "code": code})).await;
                    break;
                }
                // A slow viewer dropped some output; keep going with what's next.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(text))) => handle_terminal_client_frame(&state.terminal, &pty_id, &text).await,
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                _ => {}
            },
        }
    }
}

async fn handle_terminal_client_frame(terminal: &TerminalManager, pty_id: &str, text: &str) {
    let Ok(frame) = serde_json::from_str::<serde_json::Value>(text) else { return };
    match frame.get("type").and_then(|v| v.as_str()) {
        Some("input") => {
            if let Some(bytes) = frame.get("data").and_then(|v| v.as_str()).and_then(|d| B64.decode(d).ok()) {
                let _ = terminal.write(pty_id, &bytes).await;
            }
        }
        Some("resize") => {
            let dim = |k: &str| frame.get(k).and_then(|v| v.as_u64()).filter(|n| (1..=u16::MAX as u64).contains(n));
            if let (Some(cols), Some(rows)) = (dim("cols"), dim("rows")) {
                let _ = terminal.resize(pty_id, cols as u16, rows as u16).await;
            }
        }
        _ => {}
    }
}

async fn get_tasks(State(state): State<AppState>) -> Json<TasksSnapshot> {
    Json(state.tasks.snapshot().await)
}

#[derive(Deserialize)]
struct CreateTaskBody {
    title: String,
    /// Omitted means the board's intake column (quick-add).
    stage: Option<Stage>,
    /// Board the task belongs to; omitted means the default board.
    board: Option<String>,
}

async fn create_task(State(state): State<AppState>, Json(body): Json<CreateTaskBody>) -> Result<Json<Task>, StatusCode> {
    let board = body.board.as_deref().unwrap_or(DEFAULT_BOARD_ID);
    if state.boards.get(board).is_none() {
        return Err(StatusCode::BAD_REQUEST);
    }
    state.tasks.create_on_board(&body.title, body.stage.as_deref(), board).await.map(Json).ok_or(StatusCode::BAD_REQUEST)
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

/// Removes a board: takes its live cards and its tasks off taisk and
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
    /// Replaces the task's attached directories (see `Task::directories`).
    directories: Option<Vec<String>>,
}

async fn update_task(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateTaskBody>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    // Validate before touching anything so a bad path never half-applies a patch.
    let directories = body
        .directories
        .as_deref()
        .map(normalize_task_directories)
        .transpose()
        .map_err(|e| api_error(StatusCode::BAD_REQUEST, e))?;
    state.tasks.update(&id, body.title.as_deref(), body.stage.as_deref()).await.map_err(task_error)?;
    if let Some(dirs) = directories {
        state.tasks.set_directories(&id, dirs).await;
    }
    Ok(StatusCode::OK)
}

fn task_error(e: TaskError) -> (StatusCode, Json<ApiError>) {
    let status = match e {
        TaskError::NotFound => StatusCode::NOT_FOUND,
        TaskError::Invalid(_) => StatusCode::BAD_REQUEST,
    };
    api_error(status, e.to_string())
}

/// Column edits are scoped to a board that exists; an unknown one is a 404.
fn require_board(state: &AppState, board: &str) -> Result<(), (StatusCode, Json<ApiError>)> {
    match state.boards.get(board) {
        Some(_) => Ok(()),
        None => Err(api_error(StatusCode::NOT_FOUND, format!("no board {board:?}"))),
    }
}

#[derive(Deserialize)]
struct AddColumnBody {
    name: String,
    color: Option<String>,
}

async fn add_column(
    State(state): State<AppState>,
    Path(board): Path<String>,
    Json(body): Json<AddColumnBody>,
) -> Result<Json<Column>, (StatusCode, Json<ApiError>)> {
    require_board(&state, &board)?;
    state.tasks.add_column(&board, &body.name, body.color.as_deref()).await.map(Json).map_err(task_error)
}

async fn update_column(
    State(state): State<AppState>,
    Path((board, col)): Path<(String, String)>,
    Json(patch): Json<ColumnPatch>,
) -> Result<Json<Column>, (StatusCode, Json<ApiError>)> {
    require_board(&state, &board)?;
    state.tasks.update_column(&board, &col, &patch).await.map(Json).map_err(task_error)
}

#[derive(Deserialize)]
struct ReorderColumnsBody {
    ids: Vec<String>,
}

async fn reorder_columns(
    State(state): State<AppState>,
    Path(board): Path<String>,
    Json(body): Json<ReorderColumnsBody>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    require_board(&state, &board)?;
    state.tasks.reorder_columns(&board, &body.ids).await.map_err(task_error)?;
    Ok(StatusCode::OK)
}

async fn set_column_roles(
    State(state): State<AppState>,
    Path(board): Path<String>,
    Json(patch): Json<RolesPatch>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    require_board(&state, &board)?;
    state.tasks.set_roles(&board, &patch).await.map_err(task_error)?;
    Ok(StatusCode::OK)
}

#[derive(Deserialize)]
struct DeleteColumnQuery {
    move_tasks_to: String,
}

/// Deletes a column; its tasks move to `?move_tasks_to=<column id>`.
async fn delete_column(
    State(state): State<AppState>,
    Path((board, col)): Path<(String, String)>,
    Query(q): Query<DeleteColumnQuery>,
) -> Result<StatusCode, (StatusCode, Json<ApiError>)> {
    require_board(&state, &board)?;
    state.tasks.delete_column(&board, &col, &q.move_tasks_to).await.map_err(task_error)?;
    Ok(StatusCode::OK)
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
    if state.tasks.assign(&id, body.task_id.as_deref()).await {
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
///
/// Claude-native mode (no Ollama) — or Ollama failing to embed the query —
/// uses `MemoryRepo::keyword_search` instead. Rows written without Ollama
/// carry an all-zero vector that vector search can't rank, so in Ollama mode
/// their keyword matches are merged in behind the vector results.
async fn search(State(state): State<AppState>, Query(q): Query<SearchQuery>) -> Json<Vec<SearchResult>> {
    let live_ids: HashSet<String> = state.engine.snapshot().await.into_iter().map(|s| s.id).collect();
    // Over-fetch when filtering by board so a board with few matches isn't
    // starved by other boards' rows in the global top-N.
    let fetch = if q.board.is_some() { 50 } else { 10 };
    let embedding = match state.config.backend {
        InferenceBackend::Ollama => state.ollama.embed(&state.config.embedding_model, &q.q).await.ok(),
        InferenceBackend::Native => None,
    };
    let keyword = state.repo.keyword_search(&q.q, fetch).await.unwrap_or_default();
    let results = match embedding {
        None => keyword,
        Some(embedding) => {
            let is_unembedded = |m: &Memory| m.embedding.iter().all(|v| *v == 0.0);
            let mut results: Vec<ScoredMemory> = state
                .repo
                .search(&embedding, fetch)
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|r| !is_unembedded(&r.memory) && r.distance.is_finite())
                .collect();
            results.extend(keyword.into_iter().filter(|r| is_unembedded(&r.memory)));
            results
        }
    };
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

/// Which backend produces titles/summaries/search — lets the UI say
/// "Ollama" vs "Claude-native" instead of assuming Ollama is installed.
async fn get_inference(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "backend": state.config.backend.as_str() }))
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
        spawn_test_server_with(Arc::new(FakeOllamaClient::new("Backend / API")), SummarizeConfig::default()).await
    }

    async fn spawn_test_server_with(ollama: Arc<dyn OllamaClient>, config: SummarizeConfig) -> (String, AppState) {
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
            ollama,
            config: Arc::new(config),
            claude_projects_dir,
            boards: Arc::new(BoardStore::in_memory()),
            hook_bridge_path: None,
            waiting_sessions: Arc::new(Mutex::new(WaitingSessions::load(&waiting_sessions_path))),
            dismissed_sessions: Arc::new(Mutex::new(DismissedSessions::load(&dismissed_sessions_path))),
            tasks: TaskHub::load(&app_dir_path.join("tasks.json")),
            terminal: TerminalManager::new(),
            // `cat` echoes input back, which is all the terminal API tests need.
            claude_bin: "/bin/cat".into(),
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
        assert_eq!(task.stage, "todo");
        // A stage that isn't a column on the board is rejected.
        let r = client.post(format!("{base}/tasks")).json(&serde_json::json!({"title": "X", "stage": "nope"})).send().await.unwrap();
        assert_eq!(r.status(), 400);
        let r = client.patch(format!("{base}/tasks/{}", task.id)).json(&serde_json::json!({"stage": "nope"})).send().await.unwrap();
        assert_eq!(r.status(), 400);

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
        assert_eq!(snap.tasks[0].stage, "inprogress");
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
    async fn columns_can_be_added_edited_reordered_and_deleted() {
        let (base, state) = spawn_test_server().await;
        let client = reqwest::Client::new();
        let cols = |snap: &TasksSnapshot| -> Vec<String> {
            snap.columns.get("default").unwrap_or(&snap.default_columns).columns.iter().map(|c| c.id.clone()).collect()
        };

        // Untouched board: defaults are served, nothing stored yet.
        let snap: TasksSnapshot = client.get(format!("{base}/tasks")).send().await.unwrap().json().await.unwrap();
        assert!(snap.columns.is_empty());
        assert_eq!(cols(&snap), ["backlog", "todo", "inprogress", "done"]);

        // No stage: filed in the board's intake column (quick-add).
        let quick: Task = client.post(format!("{base}/tasks")).json(&serde_json::json!({"title": "Q"})).send().await.unwrap().json().await.unwrap();
        assert_eq!(quick.stage, "todo");
        state.tasks.delete(&quick.id).await;

        let col: Column = client
            .post(format!("{base}/boards/default/columns"))
            .json(&serde_json::json!({"name": "In Review", "color": "#e07a8b"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!((col.id.as_str(), col.color.as_str()), ("in-review", "#E07A8B"));
        let r = client.post(format!("{base}/boards/nope/columns")).json(&serde_json::json!({"name": "X"})).send().await.unwrap();
        assert_eq!(r.status(), 404);
        let r = client.post(format!("{base}/boards/default/columns")).json(&serde_json::json!({"name": " "})).send().await.unwrap();
        assert_eq!(r.status(), 400);

        let r = client
            .patch(format!("{base}/boards/default/columns/in-review"))
            .json(&serde_json::json!({"name": "Review", "color": "#7c9ce0"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let r = client
            .patch(format!("{base}/boards/default/columns/inprogress"))
            .json(&serde_json::json!({"color": "red"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);

        let order = ["todo", "inprogress", "in-review", "done", "backlog"];
        let r = client.put(format!("{base}/boards/default/columns/order")).json(&serde_json::json!({"ids": order})).send().await.unwrap();
        assert_eq!(r.status(), 200);
        let r = client.put(format!("{base}/boards/default/columns/order")).json(&serde_json::json!({"ids": ["todo"]})).send().await.unwrap();
        assert_eq!(r.status(), 400);

        let r = client.put(format!("{base}/boards/default/columns/roles")).json(&serde_json::json!({"done": "in-review"})).send().await.unwrap();
        assert_eq!(r.status(), 200);
        let r = client.put(format!("{base}/boards/default/columns/roles")).json(&serde_json::json!({"active": "nope"})).send().await.unwrap();
        assert_eq!(r.status(), 400);

        let task = state.tasks.create("A", "in-review").await.unwrap();
        let r = client.delete(format!("{base}/boards/default/columns/in-review?move_tasks_to=in-review")).send().await.unwrap();
        assert_eq!(r.status(), 400);
        let r = client.delete(format!("{base}/boards/default/columns/in-review?move_tasks_to=done")).send().await.unwrap();
        assert_eq!(r.status(), 200);

        let snap = state.tasks.snapshot().await;
        assert_eq!(cols(&snap), ["todo", "inprogress", "done", "backlog"]);
        let layout = &snap.columns["default"];
        assert_eq!(layout.done, None, "the deleted column's role is unset, not guessed");
        assert_eq!(snap.tasks.iter().find(|t| t.id == task.id).unwrap().stage, "done");
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

    async fn next_terminal_frame(ws: &mut (impl futures::Stream<Item = Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin)) -> serde_json::Value {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next()).await.unwrap().unwrap().unwrap();
        let WsMessage::Text(text) = msg else { panic!("expected text") };
        serde_json::from_str(&text).unwrap()
    }

    #[tokio::test]
    async fn task_terminal_spawns_and_its_ws_replays_scrollback_then_echoes_input() {
        use futures::SinkExt;
        let (base, state) = spawn_test_server().await;
        let client = reqwest::Client::new();
        let task = state.tasks.create("Billing", "todo").await.unwrap();

        // Unknown task -> 404, no process spawned.
        let resp = client.post(format!("{base}/terminals/tasks/nope")).send().await.unwrap();
        assert_eq!(resp.status(), 404);

        let opened: serde_json::Value = client
            .post(format!("{base}/terminals/tasks/{}", task.id))
            .json(&serde_json::json!({"cwd": "/definitely/not/a/dir"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let pty_id = opened["pty_id"].as_str().unwrap().to_string();
        assert_eq!(opened["reused"], false);

        let info: serde_json::Value =
            client.get(format!("{base}/terminals/{pty_id}")).send().await.unwrap().json().await.unwrap();
        assert_eq!(info["task_id"], task.id);
        assert_eq!(info["alive"], true);

        let ws_url = base.replace("http://", "ws://") + &format!("/terminals/{pty_id}/ws");
        let (mut ws, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
        // Scrollback always comes first, even when empty.
        assert_eq!(next_terminal_frame(&mut ws).await["type"], "scrollback");

        let input = B64.encode(b"ping\n");
        ws.send(WsMessage::Text(serde_json::json!({"type": "input", "data": input}).to_string().into()))
            .await
            .unwrap();
        ws.send(WsMessage::Text(serde_json::json!({"type": "resize", "cols": 90, "rows": 20}).to_string().into()))
            .await
            .unwrap();
        let mut seen = String::new();
        while !seen.contains("ping") {
            let frame = next_terminal_frame(&mut ws).await;
            if frame["type"] == "data" {
                seen.push_str(&String::from_utf8_lossy(&B64.decode(frame["data"].as_str().unwrap()).unwrap()));
            }
        }

        // A second viewer (e.g. after switching away and back) replays what it missed.
        let ws_url = base.replace("http://", "ws://") + &format!("/terminals/{pty_id}/ws");
        let (mut ws2, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
        let replay = next_terminal_frame(&mut ws2).await;
        assert_eq!(replay["type"], "scrollback");
        let bytes = B64.decode(replay["data"].as_str().unwrap()).unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains("ping"));
        state.terminal.kill_all();
    }

    #[tokio::test]
    async fn task_directories_are_validated_and_drive_the_new_sessions_cwd_and_add_dirs() {
        let (base, state) = spawn_test_server().await;
        let client = reqwest::Client::new();
        let task = state.tasks.create("Multi-repo", "todo").await.unwrap();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let a_path = a.path().to_string_lossy().to_string();
        let b_path = b.path().to_string_lossy().to_string();

        for bad in [serde_json::json!(["relative/dir"]), serde_json::json!(["/definitely/not/a/dir"])] {
            let r = client.patch(format!("{base}/tasks/{}", task.id)).json(&serde_json::json!({"directories": bad})).send().await.unwrap();
            assert_eq!(r.status(), 400);
        }
        assert!(state.tasks.snapshot().await.tasks[0].directories.is_empty());

        let r = client
            .patch(format!("{base}/tasks/{}", task.id))
            .json(&serde_json::json!({"directories": [format!("{a_path}/"), "  ", b_path, a_path]}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        assert_eq!(state.tasks.snapshot().await.tasks[0].directories, vec![a_path.clone(), b_path.clone()]);

        // The first attached dir wins over the frontend's fallback cwd.
        let opened: serde_json::Value = client
            .post(format!("{base}/terminals/tasks/{}", task.id))
            .json(&serde_json::json!({"cwd": "/tmp"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let info: serde_json::Value = client
            .get(format!("{base}/terminals/{}", opened["pty_id"].as_str().unwrap()))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(info["cwd"], a_path);
        state.terminal.kill_all();
    }

    #[test]
    fn task_launch_dirs_skips_vanished_dirs_and_falls_back() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let a_path = a.path().to_string_lossy().to_string();
        let b_path = b.path().to_string_lossy().to_string();
        let (cwd, add) = task_launch_dirs(&["/gone".into(), a_path.clone(), b_path.clone()], None);
        assert_eq!((cwd, add), (a_path.clone(), vec![b_path]));
        let (cwd, add) = task_launch_dirs(&["/gone".into()], Some(a_path.clone()));
        assert_eq!((cwd, add), (a_path, vec![]));
    }

    #[tokio::test]
    async fn session_terminal_404s_for_an_unknown_session_and_reuses_a_linked_pty() {
        let (base, state) = spawn_test_server().await;
        let client = reqwest::Client::new();
        let resp = client.post(format!("{base}/terminals/sessions/ghost")).send().await.unwrap();
        assert_eq!(resp.status(), 404);

        // A live pty already linked to a session is reused, not respawned.
        let pty_id = state
            .terminal
            .spawn(SpawnSpec {
                cwd: "/tmp".into(),
                program: "/bin/cat".into(),
                args: vec![],
                env: vec![],
                session_id: Some("s1".into()),
                task_id: None,
            })
            .unwrap();
        let opened: serde_json::Value = client
            .post(format!("{base}/terminals/sessions/s1"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(opened["pty_id"], pty_id);
        assert_eq!(opened["reused"], true);
        state.terminal.kill_all();
    }

    #[tokio::test]
    async fn ws_for_an_unknown_terminal_is_closed() {
        let (base, _state) = spawn_test_server().await;
        let ws_url = base.replace("http://", "ws://") + "/terminals/nope/ws";
        let (mut ws, _) = tokio_tungstenite::connect_async(ws_url).await.unwrap();
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), ws.next()).await.unwrap();
        assert!(matches!(msg, Some(Ok(WsMessage::Close(_))) | None | Some(Err(_))));
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

    fn unembedded_memory(session_id: &str, text: &str, title: &str) -> Memory {
        Memory {
            id: format!("{session_id}-prompt"),
            session_id: session_id.into(),
            kind: MemoryKind::Prompt,
            text: text.into(),
            embedding: vec![0.0; crate::memory_repo::EMBEDDING_DIM as usize],
            project: "api-gateway".into(),
            cwd: "/x".into(),
            tool: "Claude Code".into(),
            title: title.into(),
            created_at: 0,
        }
    }

    async fn search_for(base: &str, q: &str) -> Vec<SearchResult> {
        reqwest::Client::new().get(format!("{base}/search?q={q}")).send().await.unwrap().json().await.unwrap()
    }

    #[tokio::test]
    async fn native_backend_searches_by_keyword() {
        let config = SummarizeConfig { backend: InferenceBackend::Native, ..SummarizeConfig::default() };
        let (base, state) = spawn_test_server_with(Arc::new(crate::ollama::fake::FailingOllamaClient), config).await;
        state.repo.upsert_memory(&unembedded_memory("s1", "tune the LR schedule", "LR schedule tuning")).await.unwrap();
        state.repo.upsert_memory(&unembedded_memory("s2", "refactor auth middleware to async", "Auth refactor")).await.unwrap();
        state.repo.upsert_memory(&unembedded_memory("s3", "fix the auth login bug", "Login fix")).await.unwrap();

        let results = search_for(&base, "auth middleware").await;
        assert_eq!(results.iter().map(|r| r.session_id.as_str()).collect::<Vec<_>>(), vec!["s2", "s3"]);
        assert!(results[0].distance < results[1].distance);
        assert!((0.0..=1.0).contains(&results[1].distance));

        let inference: serde_json::Value = reqwest::get(format!("{base}/inference")).await.unwrap().json().await.unwrap();
        assert_eq!(inference["backend"], "native");
    }

    /// Ollama configured but not answering: search used to return `[]`.
    #[tokio::test]
    async fn ollama_backend_falls_back_to_keyword_search_when_embed_fails() {
        let (base, state) = spawn_test_server_with(Arc::new(crate::ollama::fake::FailingOllamaClient), SummarizeConfig::default()).await;
        state.repo.upsert_memory(&unembedded_memory("s1", "tune the LR schedule", "LR schedule tuning")).await.unwrap();
        let results = search_for(&base, "schedule").await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].session_id, "s1");
    }

    /// Rows written while running without Ollama have zero vectors; once
    /// Ollama is available they must still be findable, via keywords.
    #[tokio::test]
    async fn ollama_backend_still_finds_rows_written_without_ollama() {
        let (base, state) = spawn_test_server().await;
        let embedding = state.ollama.embed("nomic-embed-text", "refactor auth middleware").await.unwrap();
        let mut embedded = unembedded_memory("s1", "refactor auth middleware", "Auth refactor");
        embedded.embedding = embedding;
        state.repo.upsert_memory(&embedded).await.unwrap();
        state.repo.upsert_memory(&unembedded_memory("s2", "auth middleware tests", "Auth tests")).await.unwrap();

        let results = search_for(&base, "auth middleware").await;
        assert_eq!(results.iter().map(|r| r.session_id.as_str()).collect::<Vec<_>>(), vec!["s1", "s2"]);
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
