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
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tower_http::cors::CorsLayer;

use crate::categorize::CategorizationConfig;
use crate::collector::{parse_transcript_for_display, transcript_path, TranscriptRow};
use crate::engine::{EngineCommand, EngineHandle, SessionView};
use crate::memory_repo::{MemoryRepo, PurgeScope};
use crate::ollama::OllamaClient;

#[derive(Clone)]
pub struct AppState {
    pub engine: EngineHandle,
    pub repo: Arc<MemoryRepo>,
    pub ollama: Arc<dyn OllamaClient>,
    pub config: Arc<CategorizationConfig>,
    pub claude_projects_dir: PathBuf,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/sessions", get(get_sessions))
        .route("/events", get(ws_events))
        .route("/sessions/{id}/approve", post(approve))
        .route("/sessions/{id}/reject", post(reject))
        .route("/sessions/{id}/reply", post(reply))
        .route("/sessions/{id}/recategorize", post(recategorize))
        .route("/sessions/{id}/transcript", get(get_transcript))
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

async fn handle_ws(mut socket: WebSocket, state: AppState) {
    let mut rx = state.engine.subscribe();
    while let Ok(diff) = rx.recv().await {
        let Ok(text) = serde_json::to_string(&diff) else { continue };
        if socket.send(Message::Text(text.into())).await.is_err() {
            break;
        }
    }
}

#[derive(Deserialize, Default)]
struct ReplyBody {
    text: Option<String>,
}

async fn approve(State(state): State<AppState>, Path(id): Path<String>) -> StatusCode {
    state
        .engine
        .dispatch(EngineCommand::ResolveWaiting { id, task: "Resumed — applying your approval".into() })
        .await;
    StatusCode::OK
}

async fn reject(State(state): State<AppState>, Path(id): Path<String>) -> StatusCode {
    state
        .engine
        .dispatch(EngineCommand::ResolveWaiting { id, task: "Continuing without that change".into() })
        .await;
    StatusCode::OK
}

async fn reply(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<ReplyBody>>,
) -> StatusCode {
    let text = body.and_then(|b| b.0.text).filter(|t| !t.trim().is_empty());
    let task = match text {
        Some(t) => format!("Working on: {t}"),
        None => "Working on your reply…".into(),
    };
    state.engine.dispatch(EngineCommand::ResolveWaiting { id, task }).await;
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
    let path = transcript_path(&state.claude_projects_dir, &session.cwd, &session.id);
    Json(parse_transcript_for_display(&path))
}

#[derive(Deserialize)]
struct RecategorizeBody {
    category: String,
}

async fn recategorize(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RecategorizeBody>,
) -> StatusCode {
    state.engine.dispatch(EngineCommand::Recategorize { id, category: body.category }).await;
    StatusCode::OK
}

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
}

#[derive(Serialize, Deserialize)]
struct SearchResult {
    text: String,
    project: String,
    tool: String,
    category: String,
    session_id: String,
    distance: f32,
    /// Distinguishes a live session's content from purely historical memory
    /// — the ⌘K overlay renders these differently (design: "● live now" vs a
    /// date) and clicking a live result opens its drawer.
    live: bool,
    created_at: i64,
}

/// Phase 2 roadmap item 5a: only surface results whose displayed relevance
/// (`1 - distance`, matching AskMemoryOverlay.tsx's own formula) is at least
/// 70% — i.e. `distance <= 0.3`. Filtered server-side so a low-confidence
/// match is never sent to the client at all, rather than trusting every
/// client to hide it.
const RELEVANCE_FLOOR_DISTANCE: f32 = 0.3;

async fn search(State(state): State<AppState>, Query(q): Query<SearchQuery>) -> Json<Vec<SearchResult>> {
    let Ok(embedding) = state.ollama.embed(&state.config.embedding_model, &q.q).await else {
        return Json(vec![]);
    };
    let live_ids: HashSet<String> = state.engine.snapshot().await.into_iter().map(|s| s.id).collect();
    let results = state.repo.search(&embedding, 10).await.unwrap_or_default();
    Json(
        results
            .into_iter()
            .filter(|r| r.distance <= RELEVANCE_FLOOR_DISTANCE)
            .map(|r| SearchResult {
                live: live_ids.contains(&r.memory.session_id),
                text: r.memory.text,
                project: r.memory.project,
                tool: r.memory.tool,
                category: r.memory.category,
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
    use crate::memory_repo::{Exemplar, Memory, MemoryKind};
    use crate::ollama::fake::FakeOllamaClient;
    use crate::state::SessionEvent;
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    async fn spawn_test_server() -> (String, AppState) {
        let lance_dir = tempfile::tempdir().unwrap();
        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        let claude_dir = tempfile::tempdir().unwrap();
        let claude_projects_dir = claude_dir.path().to_path_buf();
        // Leak the tempdirs so they aren't cleaned up while the server is
        // running for the lifetime of the test.
        std::mem::forget(lance_dir);
        std::mem::forget(claude_dir);

        let state = AppState {
            engine: EngineHandle::spawn(),
            repo: Arc::new(repo),
            ollama: Arc::new(FakeOllamaClient::new("Backend / API")),
            config: Arc::new(CategorizationConfig::default()),
            claude_projects_dir,
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
    async fn approve_reject_reply_and_recategorize_resolve_via_engine() {
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
        assert_eq!(snapshot[0].task, "Resumed — applying your approval");

        let resp = client
            .post(format!("{base}/sessions/s1/reply"))
            .json(&serde_json::json!({"text": "please also add tests"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(state.engine.snapshot().await[0].task, "Working on: please also add tests");

        let resp = client
            .post(format!("{base}/sessions/s1/recategorize"))
            .json(&serde_json::json!({"category": "Frontend"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(state.engine.snapshot().await[0].category, "Frontend");
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
                category: "Backend / API".into(),
                created_at: 0,
            })
            .await
            .unwrap();
        state
            .repo
            .upsert_exemplar(&Exemplar { category: "Backend / API".into(), exemplar_embedding: embedding, created_at: 0 })
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

    #[tokio::test]
    async fn search_excludes_results_below_the_70_percent_relevance_floor() {
        let (base, state) = spawn_test_server().await;
        let client = reqwest::Client::new();

        let query_embedding = state.ollama.embed("nomic-embed-text", "refactor auth middleware").await.unwrap();

        // High-relevance memory: identical embedding -> cosine distance ~0 ->
        // ~100% relevance. Must be kept.
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
                category: "Backend / API".into(),
                created_at: 0,
            })
            .await
            .unwrap();

        // Low-relevance memory: flip half the embedding's dimensions so cosine
        // distance lands comfortably past 0.3 (well below the 70% floor). Must
        // be excluded from the response entirely (not just hidden client-side).
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
                category: "Backend / API".into(),
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

        assert_eq!(results.len(), 1, "only the high-relevance result should clear the 70% floor");
        assert_eq!(results[0].session_id, "s-high");
        assert!(1.0 - results[0].distance >= 0.7);
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
}
