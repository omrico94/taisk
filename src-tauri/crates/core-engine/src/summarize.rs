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

use crate::engine::{EngineCommand, EngineHandle};
use crate::memory_repo::{Memory, MemoryKind, MemoryRepo};
use crate::ollama::{OllamaClient, OllamaError};

pub struct SummarizeConfig {
    pub embedding_model: String,
    pub instruct_model: String,
}

impl Default for SummarizeConfig {
    fn default() -> Self {
        Self { embedding_model: "nomic-embed-text".to_string(), instruct_model: "qwen2.5:1.5b".to_string() }
    }
}

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

/// Runs the pipeline for one session's initiating prompt and dispatches the
/// resulting title + initial task summary to the engine. Also persists the
/// prompt into durable memory (FR5).
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
) -> Result<(), OllamaError> {
    let embedding = ollama.embed(&config.embedding_model, prompt).await?;

    let raw = ollama.generate(&config.instruct_model, &summarize_prompt(prompt)).await?;

    // Model didn't return parseable JSON — degrade gracefully rather than
    // fail the whole pipeline over a formatting slip.
    let (task_summary, title) = match parse_llm_json(&raw) {
        Some(resp) => (resp.task_summary, resp.title),
        None => (String::new(), String::new()),
    };

    let task_summary = if task_summary.trim().is_empty() { prompt.chars().take(80).collect() } else { task_summary.trim().to_string() };
    // The model's own title, or a deterministic fallback: the prompt's first
    // few words. Not as good as a real summarizing title, but never blank.
    let title = if title.trim().is_empty() {
        prompt.split_whitespace().take(4).collect::<Vec<_>>().join(" ")
    } else {
        title.trim().to_string()
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

    Ok(())
}

/// Ongoing counterpart to the initial summary in `summarize_session` —
/// called on each `Stop` hook (once per assistant turn) with whatever new
/// transcript activity has appeared since the last check, so the task line
/// tracks what the session is *currently* doing rather than freezing at the
/// first prompt.
pub async fn refresh_task_summary(
    engine: &EngineHandle,
    ollama: &dyn OllamaClient,
    config: &SummarizeConfig,
    session_id: &str,
    recent_activity: &str,
) {
    let prompt = format!(
        "In one short sentence (under 12 words), describe what is currently being worked on, based on this recent activity: \"{recent_activity}\""
    );
    if let Ok(raw) = ollama.generate(&config.instruct_model, &prompt).await {
        let summary = raw.trim();
        if !summary.is_empty() {
            engine.dispatch(EngineCommand::SetDesc { id: session_id.to_string(), desc: summary.to_string() }).await;
        }
    }
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
        summarize_session(&engine, &repo, &ollama, &config, session_id, "api-gateway", cwd, "Claude Code", &prompt).await.unwrap();

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
        summarize_session(&engine, &repo, &ollama, &SummarizeConfig::default(), "s1", "p", "/p", "Claude Code", "tune the LR schedule for the run")
            .await
            .unwrap();
        // The engine only knows sessions that had a SessionStart, so check
        // the durable row instead.
        let rows = repo.list_session_memories().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "tune the LR schedule");
    }
}
