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

/// `~/Library/Application Support/taisk` (or platform equivalent).
pub fn app_data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("taisk")
}

/// One-time migration from the pre-rename "SessionBoard" data directory to
/// this one (LanceDB index, boards/tasks/ended-sessions/waiting-sessions
/// records, `engine.sock`'s parent — everything lives under this single
/// directory, so a plain rename carries all of it). Must run before anything
/// else creates `app_data_dir()`, since that would make the new dir already
/// exist and this become a permanent no-op instead of a one-time migration.
/// A no-op on any later boot, and for a fresh install that never had the old
/// directory.
pub fn migrate_legacy_data_dir() {
    let new_dir = app_data_dir();
    if new_dir.exists() {
        return;
    }
    let Some(parent) = new_dir.parent() else { return };
    let old_dir = parent.join("SessionBoard");
    if !old_dir.exists() {
        return;
    }
    let _ = std::fs::create_dir_all(parent);
    if let Err(e) = std::fs::rename(&old_dir, &new_dir) {
        eprintln!("Could not migrate the old SessionBoard data directory to taisk: {e}");
    }
}

pub fn lancedb_dir() -> PathBuf {
    app_data_dir().join("lancedb")
}

/// The default board's Claude config directory, `~/.claude`.
pub fn claude_config_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".claude")
}

pub fn claude_settings_path() -> PathBuf {
    claude_config_dir().join("settings.json")
}

pub fn claude_projects_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".claude").join("projects")
}

/// `~/.claude/tasks` — real `TaskCreate`/`TaskUpdate` data, one JSON file per
/// task under a `<session_id>/` subdirectory (Phase 2 plan §4; confirmed
/// against this project's own build tracking — the deprecated `TodoWrite`
/// mechanism has no on-disk equivalent).
pub fn claude_tasks_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".claude").join("tasks")
}

/// Registers our hook entries into `~/.claude/settings.json`, idempotently
/// (plan §3). Returns the path actually written, for logging/diagnostics.
pub fn register_hooks(hook_bridge_path: &str) -> std::io::Result<PathBuf> {
    let path = claude_settings_path();
    crate::settings_merge::apply_to_file(&path, hook_bridge_path)?;
    Ok(path)
}

/// The file Claude Code itself writes `hasTrustDialogAccepted` into for the
/// *default* config dir: `~/.claude.json`, a **sibling** of `~/.claude`, not
/// a file inside it (confirmed against this project's own real file layout).
/// A named board's own `<config_dir>/.claude.json` lives inside its config
/// dir instead (also confirmed against a real board's on-disk layout) — see
/// `trust_config_path_for` at the call site in `api.rs`.
pub fn claude_user_config_path() -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(".claude.json")
}

/// Pre-approves Claude Code's interactive "do you trust this folder?" dialog
/// for `cwd`, so a `claude` process taisk spawns programmatically (a "start
/// new session from a task" launch, or resuming one) doesn't sit blocked on
/// a prompt the embedded terminal panel makes easy to miss. Live-reproduced:
/// the dialog defaults to focus on "No, exit", so a stray Enter (or the user
/// just typing their actual message before noticing the prompt) kills the
/// process before it ever writes a real transcript line — the session then
/// never surfaces on the board at all, let alone gets filed under its task.
/// There's nothing left for the user to decide here anyway: starting a
/// session in `cwd` via taisk's own UI *is* the trust decision.
///
/// Mirrors exactly what Claude Code itself writes when a user answers "Yes"
/// by hand: a `projects.<cwd>.hasTrustDialogAccepted: true` entry in
/// `config_path`. Merges rather than overwrites so any other fields Claude
/// Code has already recorded for this project (`allowedTools`,
/// `mcpContextUris`, history, ...) survive untouched, and is safe to call on
/// every launch (idempotent, same as `settings_merge`).
pub fn trust_project_dir(config_path: &Path, cwd: &str) -> std::io::Result<()> {
    let mut root: serde_json::Value = std::fs::read_to_string(config_path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    if !root.is_object() {
        root = serde_json::json!({});
    }
    let root_obj = root.as_object_mut().unwrap();
    let projects = root_obj.entry("projects").or_insert_with(|| serde_json::json!({}));
    if !projects.is_object() {
        *projects = serde_json::json!({});
    }
    let project_entry = projects.as_object_mut().unwrap().entry(cwd.to_string()).or_insert_with(|| serde_json::json!({}));
    if !project_entry.is_object() {
        *project_entry = serde_json::json!({});
    }
    project_entry.as_object_mut().unwrap().insert("hasTrustDialogAccepted".to_string(), serde_json::Value::Bool(true));

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = config_path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&root)?)?;
    std::fs::rename(&tmp, config_path)
}

/// Registers our hooks into `board`'s own `<config_dir>/settings.json`. The
/// default board keeps the bare command (so existing `~/.claude` entries need
/// no migration); every other board tags its events with `--board <id>`.
pub fn register_board_hooks(hook_bridge_path: &str, board: &crate::boards::Board) -> std::io::Result<PathBuf> {
    let path = board.config_dir.join("settings.json");
    let tag = (board.id != crate::boards::DEFAULT_BOARD_ID).then_some(board.id.as_str());
    crate::settings_merge::apply_to_file_for_board(&path, hook_bridge_path, tag)?;
    Ok(path)
}

/// Removes our hooks from `board`'s settings.json (board deleted). Leaves the
/// rest of the file, and the config directory itself, alone.
pub fn unregister_board_hooks(hook_bridge_path: &str, board: &crate::boards::Board) -> std::io::Result<()> {
    crate::settings_merge::remove_from_file(&board.config_dir.join("settings.json"), hook_bridge_path)
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
                    // No summary pass runs during backfill (see the
                    // `cwd` comment above — these rows only feed search, not
                    // live reconstruction), so there's no LLM title to store.
                    title: String::new(),
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

    #[test]
    fn trust_project_dir_creates_the_entry_on_a_fresh_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        trust_project_dir(&path, "/Users/omricohen/some-project").unwrap();

        let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["projects"]["/Users/omricohen/some-project"]["hasTrustDialogAccepted"], true);
    }

    #[test]
    fn trust_project_dir_merges_without_disturbing_other_fields_or_projects() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        std::fs::write(
            &path,
            serde_json::json!({
                "projects": {
                    "/Users/omricohen/some-project": {"allowedTools": ["Bash"], "hasTrustDialogAccepted": false},
                    "/Users/omricohen/other-project": {"hasTrustDialogAccepted": true},
                },
                "userID": "keep-me",
            })
            .to_string(),
        )
        .unwrap();

        trust_project_dir(&path, "/Users/omricohen/some-project").unwrap();

        let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["projects"]["/Users/omricohen/some-project"]["hasTrustDialogAccepted"], true);
        assert_eq!(value["projects"]["/Users/omricohen/some-project"]["allowedTools"], serde_json::json!(["Bash"]));
        assert_eq!(value["projects"]["/Users/omricohen/other-project"]["hasTrustDialogAccepted"], true);
        assert_eq!(value["userID"], "keep-me");
    }

    #[test]
    fn trust_project_dir_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".claude.json");
        trust_project_dir(&path, "/Users/omricohen/some-project").unwrap();
        trust_project_dir(&path, "/Users/omricohen/some-project").unwrap();

        let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["projects"].as_object().unwrap().len(), 1);
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
        let ollama = FakeOllamaClient::new("General");

        let count = backfill_existing_sessions(claude_dir.path(), &repo, &ollama, "nomic-embed-text").await;
        assert_eq!(count, 2);

        let embedding = ollama.embed("nomic-embed-text", "refactor auth middleware").await.unwrap();
        let results = repo.search(&embedding, 5).await.unwrap();
        assert!(results.iter().any(|r| r.memory.text == "refactor auth middleware"));
        assert!(results.iter().any(|r| r.memory.project == "-Users-omricohen-api-gateway"));
    }
}
