//! Shared production bootstrap: opens the LanceDB repo, registers Claude
//! Code hooks, starts the orchestrator, and returns an `AppState` ready to
//! serve over the local API. Used by both the Tauri app
//! (src-tauri/src/lib.rs) and the standalone dev-server example
//! (examples/dev_server.rs, used for headless E2E verification since there's
//! no tool available to drive the native Tauri window directly) — one
//! implementation, so the two hosts can't drift out of sync.

use std::sync::Arc;

use crate::api::AppState;
use crate::categorize::CategorizationConfig;
use crate::engine::EngineHandle;
use crate::memory_repo::MemoryRepo;
use crate::ollama::OllamaClient;
use crate::orchestrator::{self, OrchestratorConfig};

pub struct BootstrapOptions {
    /// Path to the `hook-bridge` binary to register in
    /// `~/.claude/settings.json`. `None` skips registration (e.g. for a
    /// throwaway dev-server run against a scratch `claude_projects_dir`
    /// where touching the user's real settings would be undesirable).
    pub hook_bridge_path: Option<String>,
}

/// Never blocks on first-run readiness (Ollama reachable, models present) —
/// that's a concern for the UI to surface, not a precondition for starting
/// the engine itself (plan §9).
pub async fn start(ollama: Arc<dyn OllamaClient>, options: BootstrapOptions) -> Result<AppState, Box<dyn std::error::Error>> {
    let app_data_dir = crate::first_run::app_data_dir();
    std::fs::create_dir_all(&app_data_dir)?;

    if let Some(path) = &options.hook_bridge_path {
        let _ = crate::first_run::register_hooks(path);
    }

    let repo = Arc::new(MemoryRepo::open(crate::first_run::lancedb_dir().to_str().unwrap()).await?);
    let engine = EngineHandle::spawn();
    let cat_config = Arc::new(CategorizationConfig::default());
    let orch_config = Arc::new(OrchestratorConfig::default());

    tokio::spawn(crate::engine::run_idle_sweeper(engine.clone(), orch_config.idle_ttl, orch_config.idle_sweep_interval));
    tokio::spawn(orchestrator::run(engine.clone(), repo.clone(), ollama.clone(), cat_config.clone(), orch_config));

    Ok(AppState {
        engine,
        repo,
        ollama,
        config: cat_config,
        claude_projects_dir: crate::first_run::claude_projects_dir(),
    })
}
