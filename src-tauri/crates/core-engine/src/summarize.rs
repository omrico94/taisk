//! The session-summary pipeline: embed the initiating prompt (for durable
//! search, FR5/FR6) and ask the instruct model, in one combined call, for a
//! short stable session title plus the initial "current task" summary line —
//! so a new session costs exactly one `embed` + one `generate` call. (There
//! are no categories any more: grouping is done by the user's Kanban tasks,
//! see `tasks.rs`.)
//!
//! Always runs after the session card has already been created as
//! "Starting…" — never on the card-creation critical path (see
//! `engine::SessionView::new_starting` and the orchestrator, which
//! dispatches `SessionStart` before calling into this module).
//!
//! `refresh_task_summary` is the separate, ongoing counterpart: called on
//! every `Stop` hook (once per assistant turn) to keep the task line
//! tracking the most recent activity instead of freezing after the first
//! summary.

use serde::Deserialize;

use crate::collector::{extract_last_prompt, extract_native_title, short_line};
use crate::engine::{EngineCommand, EngineHandle};
use crate::memory_repo::{Memory, MemoryKind, MemoryRepo, EMBEDDING_DIM};
use crate::ollama::OllamaClient;

/// Where titles, task lines and search embeddings come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferenceBackend {
    /// Local Ollama: LLM-written title/summary + `nomic-embed-text` vectors
    /// for semantic search.
    Ollama,
    /// No model at all, for users who don't want to run Ollama: titles and
    /// task lines come from what Claude Code itself writes into the
    /// transcript (`ai-title`, `custom-title`, `last-prompt` — see
    /// `collector::extract_native_title`), and search is keyword-based
    /// (`MemoryRepo::keyword_search`).
    Native,
}

impl InferenceBackend {
    pub fn as_str(&self) -> &'static str {
        match self {
            InferenceBackend::Ollama => "ollama",
            InferenceBackend::Native => "native",
        }
    }

    /// `TAISK_INFERENCE=ollama|native` forces a backend; anything else (or
    /// unset) means auto: Ollama only if it's running with both required
    /// models pulled, Claude-native otherwise.
    pub async fn detect() -> Self {
        match std::env::var("TAISK_INFERENCE").as_deref() {
            Ok("ollama") => return InferenceBackend::Ollama,
            Ok("native") => return InferenceBackend::Native,
            _ => {}
        }
        let client = crate::ollama::HttpOllamaClient::local();
        match crate::first_run::check_ollama_readiness(&client).await {
            crate::first_run::OllamaReadiness::Ready => InferenceBackend::Ollama,
            _ => InferenceBackend::Native,
        }
    }
}

pub struct SummarizeConfig {
    pub embedding_model: String,
    pub instruct_model: String,
    pub backend: InferenceBackend,
}

impl Default for SummarizeConfig {
    fn default() -> Self {
        Self {
            embedding_model: "nomic-embed-text".to_string(),
            instruct_model: "qwen2.5:1.5b".to_string(),
            backend: InferenceBackend::Ollama,
        }
    }
}

/// Max length of the card's "current task" line when it's taken verbatim
/// from a prompt rather than written by a model.
const NATIVE_DESC_CHARS: usize = 80;

#[derive(Debug, Deserialize)]
struct SummaryResponse {
    #[serde(default)]
    task_summary: String,
    /// Short, stable session name — set once here, never refreshed by
    /// `refresh_task_summary`. `#[serde(default)]` so an empty/missing result
    /// falls back to the first few words of the prompt (see
    /// `summarize_session`).
    #[serde(default)]
    title: String,
}

/// Small local instruct models routinely wrap JSON in prose or markdown
/// code fences despite being asked not to — extract the outermost `{...}`
/// substring rather than requiring an exact-JSON response.
fn parse_llm_json(raw: &str) -> Option<SummaryResponse> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end < start {
        return None;
    }
    serde_json::from_str(&raw[start..=end]).ok()
}

fn summarize_prompt(task: &str) -> String {
    format!(
        "Task: \"{task}\"\n\n\
         Write a short current-task summary (under 12 words) describing what this specific task is about, and \
         a short, stable title for this session (4 words or fewer, e.g. \"Auth middleware refactor\") that will keep \
         making sense even after the task summary changes.\n\n\
         Respond with ONLY a JSON object, no other text, in exactly this shape: \
         {{\"task_summary\": \"<short summary>\", \"title\": \"<short title>\"}}"
    )
}

/// Asks Ollama for the embedding + title/summary. `None` if either call
/// fails, so the caller can fall back to the Claude-native path.
async fn summarize_with_ollama(ollama: &dyn OllamaClient, config: &SummarizeConfig, prompt: &str) -> Option<(Vec<f32>, String, String)> {
    let embedding = ollama.embed(&config.embedding_model, prompt).await.ok()?;
    let raw = ollama.generate(&config.instruct_model, &summarize_prompt(prompt)).await.ok()?;
    // Model didn't return parseable JSON — degrade gracefully rather than
    // fail the whole pipeline over a formatting slip.
    let (task_summary, title) = match parse_llm_json(&raw) {
        Some(resp) => (resp.task_summary, resp.title),
        None => (String::new(), String::new()),
    };
    Some((embedding, title, task_summary))
}

/// Runs the pipeline for one session's initiating prompt and dispatches the
/// resulting title + initial task summary to the engine. Also persists the
/// prompt into durable memory (FR5) — always, even with no Ollama at all:
/// that row is what `reconstruct_live_sessions` rebuilds the board from, so
/// skipping it would make the session vanish on the next restart.
///
/// `transcript_lines` are only consulted for a Claude-native title, which
/// Claude Code usually hasn't written yet this early — the `stop` hook's
/// `refresh_native_title` picks it up once it has.
pub async fn summarize_session(
    engine: &EngineHandle,
    repo: &MemoryRepo,
    ollama: &dyn OllamaClient,
    config: &SummarizeConfig,
    session_id: &str,
    project: &str,
    cwd: &str,
    tool: &str,
    prompt: &str,
    transcript_lines: &[String],
) {
    let from_ollama = match config.backend {
        InferenceBackend::Ollama => summarize_with_ollama(ollama, config, prompt).await,
        InferenceBackend::Native => None,
    };
    // No usable vector: store zeros so the row still exists for
    // reconstruction and keyword search (the fixed-size vector column can't
    // be left empty). `api::search` knows to treat these as unembedded.
    let (embedding, title, task_summary) = from_ollama.unwrap_or_else(|| (vec![0.0; EMBEDDING_DIM as usize], String::new(), String::new()));

    let task_summary = if task_summary.trim().is_empty() { short_line(prompt, NATIVE_DESC_CHARS) } else { task_summary.trim().to_string() };
    // The model's own title, else Claude Code's (if it's written one yet),
    // else a deterministic fallback: the prompt's first few words. Not as
    // good as a real summarizing title, but never blank.
    let title = if !title.trim().is_empty() {
        title.trim().to_string()
    } else if let Some(native) = extract_native_title(transcript_lines) {
        native
    } else {
        prompt.split_whitespace().take(4).collect::<Vec<_>>().join(" ")
    };

    let _ = repo
        .upsert_memory(&Memory {
            id: format!("{session_id}-prompt"),
            session_id: session_id.to_string(),
            kind: MemoryKind::Prompt,
            text: prompt.to_string(),
            embedding,
            project: project.to_string(),
            cwd: cwd.to_string(),
            tool: tool.to_string(),
            title: title.clone(),
            created_at: crate::now_ms(),
        })
        .await;

    engine.dispatch(EngineCommand::SetTitle { id: session_id.to_string(), title }).await;
    engine.dispatch(EngineCommand::SetDesc { id: session_id.to_string(), desc: task_summary }).await;
}

/// Ongoing counterpart to the initial summary in `summarize_session` —
/// called on each `Stop` hook (once per assistant turn) with whatever new
/// transcript activity has appeared since the last check, so the task line
/// tracks what the session is *currently* doing rather than freezing at the
/// first prompt.
///
/// Claude-native (or Ollama failing): the latest prompt as Claude Code
/// recorded it, else the latest turn's text, squeezed into one line.
pub async fn refresh_task_summary(
    engine: &EngineHandle,
    ollama: &dyn OllamaClient,
    config: &SummarizeConfig,
    session_id: &str,
    recent_activity: Option<&str>,
    transcript_lines: &[String],
) {
    if config.backend == InferenceBackend::Ollama {
        // Nothing new since the last turn: keep the model's current line.
        let Some(activity) = recent_activity else { return };
        let prompt = format!(
            "In one short sentence (under 12 words), describe what is currently being worked on, based on this recent activity: \"{activity}\""
        );
        if let Ok(raw) = ollama.generate(&config.instruct_model, &prompt).await {
            let summary = raw.trim();
            if !summary.is_empty() {
                engine.dispatch(EngineCommand::SetDesc { id: session_id.to_string(), desc: summary.to_string() }).await;
                return;
            }
        }
    }
    let native = extract_last_prompt(transcript_lines).or_else(|| recent_activity.map(str::to_string));
    if let Some(text) = native {
        let desc = short_line(&text, NATIVE_DESC_CHARS);
        if !desc.is_empty() {
            engine.dispatch(EngineCommand::SetDesc { id: session_id.to_string(), desc }).await;
        }
    }
}

/// Adopts Claude Code's own title for the session once it appears in the
/// transcript (on any backend: a `/rename` is the user's explicit choice,
/// and in Claude-native mode `ai-title` is the only real title there is).
/// In Ollama mode only an explicit rename overrides the model's title.
/// Updates the durable row too, so the title survives a restart.
pub async fn refresh_native_title(
    engine: &EngineHandle,
    repo: &MemoryRepo,
    config: &SummarizeConfig,
    session_id: &str,
    current_title: &str,
    transcript_lines: &[String],
) {
    let title = match config.backend {
        InferenceBackend::Native => extract_native_title(transcript_lines),
        InferenceBackend::Ollama => crate::collector::extract_custom_title(transcript_lines),
    };
    let Some(title) = title else { return };
    if title == current_title {
        return;
    }
    let _ = repo.set_title(session_id, &title).await;
    engine.dispatch(EngineCommand::SetTitle { id: session_id.to_string(), title }).await;
}

#[cfg(test)]
mod fixture_harness {
    //! A fake `SessionStart` + a scratch JSONL transcript driven end-to-end
    //! through the real collector, engine, and summary pipeline (with a fake
    //! Ollama client for determinism) — no live Claude Code session or live
    //! Ollama required.

    use super::*;
    use crate::collector::{extract_initiating_prompt, project_name_from_cwd, tail_new_lines, transcript_path, TailCheckpoints};
    use crate::engine::SessionDiff;
    use crate::ollama::fake::FakeOllamaClient;
    use crate::state::SessionEvent;

    #[tokio::test]
    async fn fake_session_start_and_scratch_transcript_drive_end_to_end_summary() {
        let claude_dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();
        let checkpoint_dir = tempfile::tempdir().unwrap();

        let cwd = "/Users/omricohen/api-gateway";
        let session_id = "fixture-session-1";
        let prompt_text = "Refactor auth middleware to async/await";

        let path = transcript_path(claude_dir.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::json!({"type": "user", "message": {"role": "user", "content": prompt_text}}).to_string() + "\n").unwrap();

        let engine = EngineHandle::spawn();
        let mut diffs = engine.subscribe();
        engine
            .dispatch(EngineCommand::SessionEvent {
                id: session_id.to_string(),
                event: SessionEvent::SessionStart,
                project: Some(project_name_from_cwd(cwd)),
                cwd: Some(cwd.to_string()),
                entrypoint: None,
                started_at_ms: Some(0),
                waiting_question: None,
            })
            .await;

        // First diff: the card appears immediately as "Starting…" — must
        // never block on summarization.
        let SessionDiff::Upserted(initial) = diffs.recv().await.unwrap() else { panic!() };
        assert_eq!(initial.title, "Starting…");
        assert_eq!(initial.project, "api-gateway");

        let mut checkpoints = TailCheckpoints::load(&checkpoint_dir.path().join("checkpoints.json"));
        let lines = tail_new_lines(&path, &mut checkpoints).unwrap();
        let prompt = extract_initiating_prompt(&lines).expect("prompt should be extracted from the scratch transcript");
        assert_eq!(prompt, prompt_text);

        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        let ollama = FakeOllamaClient::new_summarizing("Refactoring auth middleware");
        let config = SummarizeConfig::default();
        summarize_session(&engine, &repo, &ollama, &config, session_id, "api-gateway", cwd, "Claude Code", &prompt, &lines).await;

        // Title (from the prompt's first few words, since the canned response
        // carries no title), then the desc (current-task summary).
        let SessionDiff::Upserted(with_title) = diffs.recv().await.unwrap() else { panic!() };
        assert_eq!(with_title.title, "Refactor auth middleware to");
        let SessionDiff::Upserted(with_desc) = diffs.recv().await.unwrap() else { panic!() };
        assert_eq!(with_desc.desc, "Refactoring auth middleware");

        let snapshot = engine.snapshot().await;
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].desc, "Refactoring auth middleware");

        // The prompt landed in durable memory too (FR5), searchable later.
        let embedding = ollama.embed("nomic-embed-text", &prompt).await.unwrap();
        let results = repo.search(&embedding, 5).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].memory.text, prompt_text);
    }

    #[tokio::test]
    async fn unparseable_model_output_falls_back_to_prompt_derived_title_and_summary() {
        let lance_dir = tempfile::tempdir().unwrap();
        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        let engine = EngineHandle::spawn();
        let ollama = FakeOllamaClient::new("not json at all");
        summarize_session(&engine, &repo, &ollama, &SummarizeConfig::default(), "s1", "p", "/p", "Claude Code", "tune the LR schedule for the run", &[])
            .await;
        // The engine only knows sessions that had a SessionStart, so check
        // the durable row instead.
        let rows = repo.list_session_memories().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "tune the LR schedule");
    }

    fn native_config() -> SummarizeConfig {
        SummarizeConfig { backend: InferenceBackend::Native, ..SummarizeConfig::default() }
    }

    async fn started_engine(session_id: &str) -> (EngineHandle, tokio::sync::broadcast::Receiver<SessionDiff>) {
        let engine = EngineHandle::spawn();
        let mut diffs = engine.subscribe();
        engine
            .dispatch(EngineCommand::SessionEvent {
                id: session_id.to_string(),
                event: SessionEvent::SessionStart,
                project: Some("p".into()),
                cwd: Some("/p".into()),
                entrypoint: None,
                started_at_ms: Some(0),
                waiting_question: None,
            })
            .await;
        diffs.recv().await.unwrap();
        (engine, diffs)
    }

    async fn session(engine: &EngineHandle, id: &str) -> crate::engine::SessionView {
        engine.snapshot().await.into_iter().find(|s| s.id == id).unwrap()
    }

    /// Regression: with Ollama unreachable, `summarize_session` used to bail
    /// out on the failed embed — the card sat on "Starting…" forever and no
    /// durable row was written, so the session vanished on restart.
    #[tokio::test]
    async fn failing_ollama_still_titles_the_card_and_persists_the_row() {
        let lance_dir = tempfile::tempdir().unwrap();
        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        let (engine, _diffs) = started_engine("s1").await;
        let ollama = crate::ollama::fake::FailingOllamaClient;
        summarize_session(&engine, &repo, &ollama, &SummarizeConfig::default(), "s1", "p", "/p", "Claude Code", "tune the LR schedule for the run", &[])
            .await;

        let s = session(&engine, "s1").await;
        assert_eq!(s.title, "tune the LR schedule");
        assert_eq!(s.desc, "tune the LR schedule for the run");
        let rows = repo.list_session_memories().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].embedding.iter().all(|v| *v == 0.0));
    }

    #[tokio::test]
    async fn native_backend_never_calls_ollama_and_uses_claudes_own_title() {
        let lance_dir = tempfile::tempdir().unwrap();
        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        let (engine, _diffs) = started_engine("s1").await;
        // Would panic-free "succeed" with a canned title if it were called.
        let ollama = FakeOllamaClient::new(r#"{"title":"FROM OLLAMA","task_summary":"FROM OLLAMA"}"#);
        let lines = vec![serde_json::json!({"type":"ai-title","aiTitle":"LR schedule tuning","sessionId":"s1"}).to_string()];
        summarize_session(&engine, &repo, &ollama, &native_config(), "s1", "p", "/p", "Claude Code", "tune the LR schedule for the run", &lines)
            .await;

        let s = session(&engine, "s1").await;
        assert_eq!(s.title, "LR schedule tuning");
        assert_eq!(s.desc, "tune the LR schedule for the run");
        assert_eq!(repo.list_session_memories().await.unwrap()[0].title, "LR schedule tuning");
    }

    #[tokio::test]
    async fn native_refresh_adopts_a_late_ai_title_and_the_latest_prompt() {
        let lance_dir = tempfile::tempdir().unwrap();
        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        let (engine, _diffs) = started_engine("s1").await;
        let ollama = FakeOllamaClient::new("unused");
        let config = native_config();
        summarize_session(&engine, &repo, &ollama, &config, "s1", "p", "/p", "Claude Code", "tune the LR schedule for the run", &[]).await;
        assert_eq!(session(&engine, "s1").await.title, "tune the LR schedule");

        // Claude Code writes its title after the first turn.
        let lines = vec![
            serde_json::json!({"type":"ai-title","aiTitle":"LR schedule tuning","sessionId":"s1"}).to_string(),
            serde_json::json!({"type":"last-prompt","lastPrompt":"now try cosine decay\nwith warmup","sessionId":"s1"}).to_string(),
        ];
        refresh_task_summary(&engine, &ollama, &config, "s1", Some("assistant text"), &lines).await;
        let current = session(&engine, "s1").await.title;
        refresh_native_title(&engine, &repo, &config, "s1", &current, &lines).await;

        let s = session(&engine, "s1").await;
        assert_eq!(s.title, "LR schedule tuning");
        assert_eq!(s.desc, "now try cosine decay");
        assert_eq!(repo.list_session_memories().await.unwrap()[0].title, "LR schedule tuning", "title must survive a restart");
    }

    #[tokio::test]
    async fn ollama_backend_only_lets_an_explicit_rename_override_its_title() {
        let lance_dir = tempfile::tempdir().unwrap();
        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        let (engine, _diffs) = started_engine("s1").await;
        let config = SummarizeConfig::default();
        let ai = vec![serde_json::json!({"type":"ai-title","aiTitle":"Claude title","sessionId":"s1"}).to_string()];
        refresh_native_title(&engine, &repo, &config, "s1", "Model title", &ai).await;
        assert_eq!(session(&engine, "s1").await.title, "Starting…");

        let renamed = vec![serde_json::json!({"type":"custom-title","customTitle":"Mine","sessionId":"s1"}).to_string()];
        refresh_native_title(&engine, &repo, &config, "s1", "Model title", &renamed).await;
        assert_eq!(session(&engine, "s1").await.title, "Mine");
    }
}
