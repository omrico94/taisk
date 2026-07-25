//! First-run / installability flow (plan §9). Split from `main.rs` so it's
//! testable without a Tauri window: readiness checks, hook registration, and
//! the optional historical backfill can all run — and be asserted on — as
//! plain async functions.
//!
//! Deliberately does **not** auto-pull models on its own: per plan §9, that
//! stays an explicit, user-triggered action (a "Pull models" button calling
//! `HttpOllamaClient::pull_model`) — this module only reports what's missing.

use std::path::{Path, PathBuf};

use crate::collector::extract_initiating_prompt;
use crate::memory_repo::{Memory, MemoryKind, MemoryRepo};
use crate::ollama::{HttpOllamaClient, OllamaClient};

pub const REQUIRED_MODELS: &[&str] = &["nomic-embed-text", "qwen2.5:1.5b"];

#[derive(Debug, Clone, PartialEq)]
pub enum OllamaReadiness {
    NotReachable,
    MissingModels(Vec<String>),
    Ready,
}

/// Ollama model names from `/api/tags` often carry a `:tag` suffix
/// (`"qwen2.5:1.5b"` itself, or `"nomic-embed-text:latest"`); treat a
/// required name as present if it matches exactly or is a prefix up to `:`.
fn model_present(required: &str, installed: &[String]) -> bool {
    installed.iter().any(|name| name == required || name.starts_with(&format!("{required}:")))
}

pub async fn check_ollama_readiness(client: &HttpOllamaClient) -> OllamaReadiness {
    if !client.is_reachable().await {
        return OllamaReadiness::NotReachable;
    }
    let installed = client.list_models().await.unwrap_or_default();
    let missing: Vec<String> = REQUIRED_MODELS
        .iter()
        .filter(|m| !model_present(m, &installed))
        .map(|m| m.to_string())
        .collect();
    if missing.is_empty() { OllamaReadiness::Ready } else { OllamaReadiness::MissingModels(missing) }
}

/// `~/Library/Application Support/SessionBoard` (or platform equivalent).
pub fn app_data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("SessionBoard")
}

pub fn lancedb_dir() -> PathBuf {
    app_data_dir().join("lancedb")
}

pub fn claude_settings_path() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".claude").join("settings.json")
}

pub fn claude_projects_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".claude").join("projects")
}

/// Registers our hook entries into `~/.claude/settings.json`, idempotently
/// (plan §3). Returns the path actually written, for logging/diagnostics.
pub fn register_hooks(hook_bridge_path: &str) -> std::io::Result<PathBuf> {
    let path = claude_settings_path();
    crate::settings_merge::apply_to_file(&path, hook_bridge_path)?;
    Ok(path)
}

/// One-time, non-blocking scan of existing `~/.claude/projects/**/*.jsonl`
/// transcripts so semantic search has real content from the very first
/// launch (plan §9 step 5) instead of starting empty. Stores each session's
/// initiating prompt as `Uncategorized` — backfilled sessions don't need to
/// go through the categorization pipeline; that would just spend model calls
/// without serving FR6 (search), which is the only reason backfill exists.
/// Returns the number of sessions backfilled.
pub async fn backfill_existing_sessions(
    claude_projects_dir: &Path,
    repo: &MemoryRepo,
    ollama: &dyn OllamaClient,
    embedding_model: &str,
) -> usize {
    let mut count = 0;
    let Ok(project_dirs) = std::fs::read_dir(claude_projects_dir) else {
        return 0;
    };

    for project_entry in project_dirs.flatten() {
        let project_path = project_entry.path();
        if !project_path.is_dir() {
            continue;
        }
        let project_name = project_entry.file_name().to_string_lossy().to_string();

        let Ok(session_files) = std::fs::read_dir(&project_path) else { continue };
        for session_entry in session_files.flatten() {
            let path = session_entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(&path) else { continue };
            let lines: Vec<String> = contents.lines().map(|s| s.to_string()).collect();
            let Some(prompt) = extract_initiating_prompt(&lines) else { continue };
            let Ok(embedding) = ollama.embed(embedding_model, &prompt).await else { continue };

            let session_id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown").to_string();
            let created_at = session_entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);

            let result = repo
                .upsert_memory(&Memory {
                    id: format!("{session_id}-prompt"),
                    session_id,
                    kind: MemoryKind::Prompt,
                    text: prompt,
                    embedding,
                    project: project_name.clone(),
                    // The real cwd isn't recoverable from the sanitized
                    // directory name alone (sanitize_cwd is lossy). Left
                    // empty — backfilled historical entries aren't expected
                    // to feed live-session reconstruction, only search.
                    cwd: String::new(),
                    tool: "Claude Code".to_string(),
                    category: "Uncategorized".to_string(),
                    created_at,
                })
                .await;
            if result.is_ok() {
                count += 1;
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ollama::fake::FakeOllamaClient;

    #[test]
    fn model_present_handles_tagged_and_untagged_names() {
        let installed = vec!["nomic-embed-text:latest".to_string(), "qwen2.5:1.5b".to_string()];
        assert!(model_present("nomic-embed-text", &installed));
        assert!(model_present("qwen2.5:1.5b", &installed));
        assert!(!model_present("llama3.2:3b", &installed));
    }

    #[tokio::test]
    async fn readiness_reports_not_reachable_when_ollama_is_down() {
        // Nothing is listening on this port — this machine had no Ollama
        // installed as of writing this test (see plan §11), so this also
        // doubles as documentation of that real, exercised branch.
        let client = HttpOllamaClient::new("http://127.0.0.1:1");
        assert_eq!(check_ollama_readiness(&client).await, OllamaReadiness::NotReachable);
    }

    #[test]
    fn register_hooks_writes_a_real_idempotent_settings_file() {
        let dir = tempfile::tempdir().unwrap();
        // Can't use claude_settings_path() directly in a test (it's the
        // user's real file) — exercise apply_to_file the same way
        // register_hooks does, against a scratch path instead.
        let path = dir.path().join(".claude").join("settings.json");
        crate::settings_merge::apply_to_file(&path, "/fake/hook-bridge").unwrap();
        crate::settings_merge::apply_to_file(&path, "/fake/hook-bridge").unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&contents).unwrap();
        assert_eq!(value["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn backfill_reads_fixture_transcripts_into_memories() {
        let claude_dir = tempfile::tempdir().unwrap();
        let lance_dir = tempfile::tempdir().unwrap();

        // Two fixture "projects", each with one transcript containing a
        // user prompt line, mirroring what a real ~/.claude/projects looks
        // like (plan §4's directory-naming convention doesn't matter for
        // backfill itself — it just walks whatever directories exist).
        for (project, session_id, prompt) in [
            ("-Users-omricohen-api-gateway", "s1", "refactor auth middleware"),
            ("-Users-omricohen-web-dashboard", "s2", "fix dark mode contrast"),
        ] {
            let dir = claude_dir.path().join(project);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(format!("{session_id}.jsonl")),
                serde_json::json!({"type":"user","message":{"role":"user","content":prompt}}).to_string() + "\n",
            )
            .unwrap();
        }
        // A non-jsonl file in a project dir must be ignored, not error.
        std::fs::write(claude_dir.path().join("-Users-omricohen-api-gateway").join("notes.txt"), "ignore me").unwrap();

        let repo = MemoryRepo::open(lance_dir.path().to_str().unwrap()).await.unwrap();
        let ollama = FakeOllamaClient::new("Uncategorized");

        let count = backfill_existing_sessions(claude_dir.path(), &repo, &ollama, "nomic-embed-text").await;
        assert_eq!(count, 2);

        let embedding = ollama.embed("nomic-embed-text", "refactor auth middleware").await.unwrap();
        let results = repo.search(&embedding, 5).await.unwrap();
        assert!(results.iter().any(|r| r.memory.text == "refactor auth middleware"));
        assert!(results.iter().any(|r| r.memory.project == "-Users-omricohen-api-gateway"));
    }
}
