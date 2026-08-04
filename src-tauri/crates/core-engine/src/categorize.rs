//! The categorization pipeline: embed the initiating prompt (for durable
//! search, FR5/FR6), then ask the instruct model to pick the best-fitting
//! *existing* category — given the full list of categories already in use —
//! or mint a new one if its own confidence is below a configurable
//! threshold (default 70%, enforced in code, not just requested of the
//! model). One combined LLM call also produces the initial "current task"
//! summary line, so a new session costs exactly one `embed` + one
//! `generate` call, not more.
//!
//! Always runs after the session card has already been created as
//! "Uncategorized" — never on the card-creation critical path (see
//! `engine::SessionView::new_uncategorized` and the orchestrator, which
//! dispatches `SessionStart` before calling into this module).
//!
//! `refresh_task_summary` is the separate, ongoing counterpart: called on
//! every `Stop` hook (once per assistant turn) to keep the task line
//! tracking the most recent activity instead of freezing after the first
//! summary.

use serde::Deserialize;

use crate::engine::{EngineCommand, EngineHandle};
use crate::memory_repo::{Exemplar, Memory, MemoryKind, MemoryRepo};
use crate::ollama::{OllamaClient, OllamaError};

pub struct CategorizationConfig {
    pub embedding_model: String,
    pub instruct_model: String,
    /// Minimum LLM-reported confidence (0-100) required to join an existing
    /// category; below this, a new category is minted instead. This is the
    /// authoritative check — enforced here in code regardless of what the
    /// model itself claims via `is_new`.
    pub min_confidence_percent: u8,
}

impl Default for CategorizationConfig {
    fn default() -> Self {
        Self {
            embedding_model: "nomic-embed-text".to_string(),
            instruct_model: "qwen2.5:1.5b".to_string(),
            min_confidence_percent: 70,
        }
    }
}

#[derive(Debug, Deserialize)]
struct CategorizeResponse {
    category: String,
    confidence: u8,
    #[serde(default)]
    is_new: bool,
    #[serde(default)]
    task_summary: String,
    /// Short, stable session name (Phase 2 design change) — set once here,
    /// never refreshed by `refresh_task_summary`. `#[serde(default)]` so
    /// callers (and every existing test's `FakeOllamaClient` response) that
    /// predate this field still parse fine; an empty result falls back to
    /// the first few words of the prompt itself (see `categorize_session`).
    #[serde(default)]
    title: String,
}

/// Small local instruct models routinely wrap JSON in prose or markdown
/// code fences despite being asked not to — extract the outermost `{...}`
/// substring rather than requiring an exact-JSON response.
fn parse_llm_json(raw: &str) -> Option<CategorizeResponse> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    if end < start {
        return None;
    }
    serde_json::from_str(&raw[start..=end]).ok()
}

fn categorize_prompt(existing_categories: &[String], task: &str, min_confidence_percent: u8) -> String {
    let categories_list =
        if existing_categories.is_empty() { "(none yet — this will be the first)".to_string() } else { existing_categories.join(", ") };

    format!(
        "Existing categories: {categories_list}\n\n\
         Task: \"{task}\"\n\n\
         Pick the single best-fitting existing category for this task, write a short current-task summary \
         (under 12 words) describing what this task is about, and also write a short, stable title for this \
         session (4 words or fewer, e.g. \"Auth middleware refactor\") that will keep making sense even after \
         the task summary changes. If none of the existing categories fit well \
         (less than {min_confidence_percent}% confident), invent a new short category name (1-3 words) instead.\n\n\
         Respond with ONLY a JSON object, no other text, in exactly this shape: \
         {{\"category\": \"<name>\", \"confidence\": <0-100 integer>, \"is_new\": <true|false>, \"task_summary\": \"<short summary>\", \"title\": \"<short title>\"}}"
    )
}

/// Runs the pipeline for one session's initiating prompt and dispatches the
/// resulting category + initial task summary to the engine. Also persists
/// the prompt into durable memory (FR5) regardless of which branch (join
/// vs. new-category) is taken.
pub async fn categorize_session(
    engine: &EngineHandle,
    repo: &MemoryRepo,
    ollama: &dyn OllamaClient,
    config: &CategorizationConfig,
    session_id: &str,
    project: &str,
    cwd: &str,
    tool: &str,
    prompt: &str,
) -> Result<String, OllamaError> {
    let embedding = ollama.embed(&config.embedding_model, prompt).await?;

    let existing_categories: Vec<String> =
        repo.list_exemplars().await.unwrap_or_default().into_iter().map(|e| e.category).collect();

    let raw = ollama
        .generate(&config.instruct_model, &categorize_prompt(&existing_categories, prompt, config.min_confidence_percent))
        .await?;

    let (category, task_summary, title) = match parse_llm_json(&raw) {
        // Model claims a confident match against a category that genuinely
        // exists in our list — join it. A claimed match against a name that
        // *isn't* actually in the list (small models sometimes rephrase) is
        // treated as a new category instead, not trusted blindly.
        Some(resp)
            if !resp.is_new
                && resp.confidence >= config.min_confidence_percent
                && existing_categories.iter().any(|c| c == &resp.category) =>
        {
            (resp.category, resp.task_summary, resp.title)
        }
        Some(resp) => {
            let label = if resp.category.trim().is_empty() { "General".to_string() } else { resp.category.trim().to_string() };
            let _ = repo
                .upsert_exemplar(&Exemplar { category: label.clone(), exemplar_embedding: embedding.clone(), created_at: crate::now_ms() })
                .await;
            (label, resp.task_summary, resp.title)
        }
        // Model didn't return parseable JSON — degrade gracefully rather
        // than fail the whole pipeline over a formatting slip.
        None => ("General".to_string(), String::new(), String::new()),
    };

    let task_summary = if task_summary.trim().is_empty() { prompt.chars().take(80).collect() } else { task_summary.trim().to_string() };
    // The model's own title, or — if it didn't provide one (including every
    // pre-title-field `FakeOllamaClient` response in the existing test
    // suite) — a deterministic fallback: the prompt's first few words. Not
    // as good as a real summarizing title, but never blank.
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
            category: category.clone(),
            created_at: crate::now_ms(),
        })
        .await;

    engine.dispatch(EngineCommand::SetCategory { id: session_id.to_string(), category: category.clone() }).await;
    engine.dispatch(EngineCommand::SetTitle { id: session_id.to_string(), title }).await;
    engine.dispatch(EngineCommand::SetDesc { id: session_id.to_string(), desc: task_summary }).await;

    Ok(category)
}

/// Ongoing counterpart to the initial summary in `categorize_session` —
/// called on each `Stop` hook (once per assistant turn) with whatever new
/// transcript activity has appeared since the last check, so the task line
/// tracks what the session is *currently* doing rather than freezing at the
/// first prompt.
pub async fn refresh_task_summary(
    engine: &EngineHandle,
    ollama: &dyn OllamaClient,
    config: &CategorizationConfig,
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
    //! The M5 fixture harness (plan §10/§11): a fake `SessionStart` + a
    //! scratch JSONL transcript driven end-to-end through the real
    //! collector, engine, and categorization pipeline (with a fake Ollama
    //! client for determinism) — no live Claude Code session or live Ollama
    //! required.

    use super::*;
    use crate::collector::{extract_initiating_prompt, project_name_from_cwd, tail_new_lines, transcript_path, TailCheckpoints};
    use crate::engine::SessionDiff;
    use crate::ollama::fake::FakeOllamaClient;
    use crate::state::SessionEvent;

    #[tokio::test]
    async fn fake_session_start_and_scratch_transcript_drive_end_to_end_categorization() {
        let claude_dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();
        let checkpoint_dir = tempfile::tempdir().unwrap();

        let cwd = "/Users/omricohen/api-gateway";
        let session_id = "fixture-session-1";
        let prompt_text = "Refactor auth middleware to async/await";

        // Write the scratch transcript at exactly the path the collector
        // computes from (cwd, session_id) — mirrors what Claude Code itself
        // would have written by the time SessionStart's file-existence poll
        // succeeds.
        let path = transcript_path(claude_dir.path(), cwd, session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({"type": "user", "message": {"role": "user", "content": prompt_text}})
                .to_string()
                + "\n",
        )
        .unwrap();

        // --- Simulate the hook-bridge -> engine "session-start" handling ---
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

        // First diff: the card appears immediately as Uncategorized — must
        // never block on categorization (plan §5/§10).
        let SessionDiff::Upserted(initial) = diffs.recv().await.unwrap() else { panic!() };
        assert_eq!(initial.category, "Uncategorized");
        assert_eq!(initial.project, "api-gateway");

        // --- Tail the transcript and run the real categorization pipeline ---
        let mut checkpoints = TailCheckpoints::load(&checkpoint_dir.path().join("checkpoints.json"));
        let lines = tail_new_lines(&path, &mut checkpoints).unwrap();
        let prompt = extract_initiating_prompt(&lines).expect("prompt should be extracted from the scratch transcript");
        assert_eq!(prompt, prompt_text);

        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        // No existing categories yet, so the fake model's "new category"
        // canned response is exercised (see FakeOllamaClient's default JSON).
        let ollama = FakeOllamaClient::new_categorizing("Backend / API", 90, true, "Refactoring auth middleware");
        let config = CategorizationConfig::default();

        let resolved_category = categorize_session(
            &engine,
            &repo,
            &ollama,
            &config,
            session_id,
            "api-gateway",
            cwd,
            "Claude Code",
            &prompt,
        )
        .await
        .unwrap();
        assert_eq!(resolved_category, "Backend / API");

        // Second diff: the category update broadcast to subscribers.
        let SessionDiff::Upserted(categorized) = diffs.recv().await.unwrap() else { panic!() };
        assert_eq!(categorized.id, session_id);
        assert_eq!(categorized.category, "Backend / API");

        // Third diff: the title (set once, from the prompt's first few words
        // here since `new_categorizing`'s canned response predates the
        // title field).
        let SessionDiff::Upserted(with_title) = diffs.recv().await.unwrap() else { panic!() };
        assert_eq!(with_title.title, "Refactor auth middleware to");

        // Fourth diff: the desc (current-task summary) update.
        let SessionDiff::Upserted(with_desc) = diffs.recv().await.unwrap() else { panic!() };
        assert_eq!(with_desc.desc, "Refactoring auth middleware");

        // And the final engine state (independent of the diff stream) agrees.
        let snapshot = engine.snapshot().await;
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].category, "Backend / API");
        assert_eq!(snapshot[0].desc, "Refactoring auth middleware");

        // The category exemplar was persisted (new-category branch), so a
        // second, similar session would be able to join it.
        let categories = repo.list_exemplars().await.unwrap();
        assert_eq!(categories.len(), 1);
        assert_eq!(categories[0].category, "Backend / API");

        // The prompt landed in durable memory too (FR5), searchable later.
        let embedding = ollama.embed("nomic-embed-text", &prompt).await.unwrap();
        let results = repo.search(&embedding, 5).await.unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].memory.text, prompt_text);
    }

    #[tokio::test]
    async fn joins_an_existing_category_when_confidence_meets_the_threshold() {
        let lance_dir = tempfile::tempdir().unwrap();
        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        let engine = EngineHandle::spawn();
        let ollama = FakeOllamaClient::new_categorizing("Backend / API", 85, false, "Adding request validation");
        let config = CategorizationConfig::default();

        // Seed an existing category so the "join" branch has something to match.
        repo.upsert_exemplar(&Exemplar {
            category: "Backend / API".into(),
            exemplar_embedding: vec![1.0; 768],
            created_at: 0,
        })
        .await
        .unwrap();

        let category =
            categorize_session(
                &engine,
                &repo,
                &ollama,
                &config,
                "s1",
                "api-gateway",
                "/Users/omricohen/api-gateway",
                "Claude Code",
                "add request validation",
            )
                .await
                .unwrap();
        assert_eq!(category, "Backend / API");

        // No new exemplar should have been created — still exactly one.
        assert_eq!(repo.list_exemplars().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn low_confidence_mints_a_new_category_even_if_model_says_not_new() {
        let lance_dir = tempfile::tempdir().unwrap();
        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        let engine = EngineHandle::spawn();
        // is_new: false, but confidence (40) is below the 70% threshold, and
        // the model names a category ("ML Training") different from the
        // seeded one — the threshold must win over the model's own is_new
        // claim, minting a genuinely new exemplar rather than joining.
        let ollama = FakeOllamaClient::new_categorizing("ML Training", 40, false, "Tuning a training loop");
        let config = CategorizationConfig::default();

        repo.upsert_exemplar(&Exemplar {
            category: "Backend / API".into(),
            exemplar_embedding: vec![1.0; 768],
            created_at: 0,
        })
        .await
        .unwrap();

        let category =
            categorize_session(
                &engine,
                &repo,
                &ollama,
                &config,
                "s1",
                "ml-experiments",
                "/Users/omricohen/ml-experiments",
                "Aider",
                "tune the LR schedule",
            )
                .await
                .unwrap();
        assert_eq!(category, "ML Training");

        // A second, distinct exemplar row now exists alongside the seeded
        // one — proof the mint-new branch ran, not the join branch (which
        // would have left the exemplar table unchanged at 1 row).
        let categories = repo.list_exemplars().await.unwrap();
        assert_eq!(categories.len(), 2);
        assert!(categories.iter().any(|c| c.category == "Backend / API"));
        assert!(categories.iter().any(|c| c.category == "ML Training"));
    }
}
