//! Standalone Core Engine runner for headless E2E verification (plan §10/
//! §11's "scripted DOM check"). There's no tool available to drive the
//! native Tauri window directly, so this runs the exact same production
//! bootstrap (`core_engine::bootstrap::start`) as the real app, minus the
//! window — letting the real frontend (via `npm run dev`) be driven by a
//! real browser against a fully real, non-mocked backend. Not part of the
//! shipped app.
//!
//! Auto-detects the inference backend like the real app (Ollama if ready,
//! Claude-native otherwise); `SESSIONBOARD_FAKE_OLLAMA=1` forces a
//! deterministic `FakeOllamaClient` for stable E2E titles.

use std::sync::Arc;

use core_engine::api::router;
use core_engine::bootstrap::{self, BootstrapOptions};
use core_engine::ollama::{HttpOllamaClient, OllamaClient};
use core_engine::summarize::InferenceBackend;
use core_engine::API_PORT;

#[tokio::main]
async fn main() {
    let http_ollama = HttpOllamaClient::local();
    // SESSIONBOARD_FAKE_OLLAMA=1 forces the deterministic fake even when a
    // real Ollama is running (E2E runs want stable titles: first 4 prompt words).
    let force_fake = std::env::var("SESSIONBOARD_FAKE_OLLAMA").is_ok();
    // Without the fake, the backend is auto-detected like the real app
    // (Claude-native when Ollama isn't running; TAISK_INFERENCE overrides).
    let (ollama, backend): (Arc<dyn OllamaClient>, Option<InferenceBackend>) = if force_fake {
        println!("dev_server: using FakeOllamaClient (SESSIONBOARD_FAKE_OLLAMA)");
        (Arc::new(core_engine::ollama::fake::FakeOllamaClient::new("")), Some(InferenceBackend::Ollama))
    } else {
        (Arc::new(http_ollama), None)
    };

    // Deliberately None: this is a throwaway test harness, not an install —
    // it must never register hooks into the user's real ~/.claude/settings.json.
    // (An earlier version of this example computed a path via current_exe(),
    // which resolves incorrectly for an example binary and wrote a broken
    // entry into the real file — drive hook events directly at the UDS in
    // tests/manual verification instead, as the orchestrator test does.)
    let api_state = bootstrap::start(ollama, BootstrapOptions { hook_bridge_path: None, terminal: core_engine::terminal::TerminalManager::new(), backend })
        .await
        .expect("Core Engine bootstrap failed");

    // SESSIONBOARD_API_PORT lets an E2E run coexist with a real app instance on the default port.
    let port: u16 = std::env::var("SESSIONBOARD_API_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(API_PORT);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.expect("failed to bind API port");
    println!("dev_server: serving on http://127.0.0.1:{port}");
    axum::serve(listener, router(api_state)).await.expect("axum server failed");
}
