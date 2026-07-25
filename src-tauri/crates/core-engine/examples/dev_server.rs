//! Standalone Core Engine runner for headless E2E verification (plan §10/
//! §11's "scripted DOM check"). There's no tool available to drive the
//! native Tauri window directly, so this runs the exact same production
//! bootstrap (`core_engine::bootstrap::start`) as the real app, minus the
//! window — letting the real frontend (via `npm run dev`) be driven by a
//! real browser against a fully real, non-mocked backend. Not part of the
//! shipped app.
//!
//! Uses a real `HttpOllamaClient` if reachable, otherwise falls back to
//! `FakeOllamaClient` so this still runs on a machine without Ollama
//! installed (e.g. this dev machine, per plan §11).

use std::sync::Arc;

use core_engine::api::router;
use core_engine::bootstrap::{self, BootstrapOptions};
use core_engine::ollama::{HttpOllamaClient, OllamaClient};
use core_engine::API_PORT;

#[tokio::main]
async fn main() {
    let http_ollama = HttpOllamaClient::local();
    let ollama: Arc<dyn OllamaClient> = if http_ollama.is_reachable().await {
        println!("dev_server: using real Ollama at 127.0.0.1:11434");
        Arc::new(http_ollama)
    } else {
        println!("dev_server: Ollama not reachable, using FakeOllamaClient (see plan §11)");
        Arc::new(core_engine::ollama::fake::FakeOllamaClient::new("Uncategorized"))
    };

    // Deliberately None: this is a throwaway test harness, not an install —
    // it must never register hooks into the user's real ~/.claude/settings.json.
    // (An earlier version of this example computed a path via current_exe(),
    // which resolves incorrectly for an example binary and wrote a broken
    // entry into the real file — drive hook events directly at the UDS in
    // tests/manual verification instead, as the orchestrator test does.)
    let api_state = bootstrap::start(ollama, BootstrapOptions { hook_bridge_path: None })
        .await
        .expect("Core Engine bootstrap failed");

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", API_PORT)).await.expect("failed to bind API port");
    println!("dev_server: serving on http://127.0.0.1:{API_PORT}");
    axum::serve(listener, router(api_state)).await.expect("axum server failed");
}
