use std::sync::Arc;

use core_engine::api::router;
use core_engine::bootstrap::{self, BootstrapOptions};
use core_engine::ollama::{HttpOllamaClient, OllamaClient};
use core_engine::terminal::TerminalManager;
use core_engine::API_PORT;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Created up front (it's cheap and needs no runtime) so the same instance
    // reaches both the engine's `AppState` and the exit handler below.
    let terminal = TerminalManager::new();
    let terminal_for_engine = terminal.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(move |_app| {
            let terminal = terminal_for_engine.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = start_core_engine(terminal).await {
                    eprintln!("Core Engine failed to start: {e}");
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(move |_app, event| {
            // Embedded terminals are real child processes; without this they
            // would outlive the app.
            if let tauri::RunEvent::Exit = event {
                terminal.kill_all();
            }
        });
}

/// Boots the Core Engine as background tasks (plan §1/§9) and serves the
/// local HTTP/WS API the frontend (and, later, a VS Code extension) talks
/// to. The actual bootstrap logic lives in `core_engine::bootstrap` so the
/// standalone dev-server example shares it exactly.
async fn start_core_engine(terminal: TerminalManager) -> Result<(), Box<dyn std::error::Error>> {
    // hook-bridge is built as a sibling binary in the same workspace, so it
    // lands next to this executable in target/debug — but `cargo tauri dev`'s
    // own DevCommand (`cargo run` for just the `sessionboard` package) never
    // builds it; nothing else in this crate depends on it. tauri.conf.json's
    // `beforeDevCommand` builds it explicitly for that reason (a real, once-
    // shipped bug: a fresh worktree ran fine but every hook silently failed
    // to reach this engine — Claude Code fell back to some *other* checkout's
    // stale hook-bridge binary already registered in ~/.claude/settings.json,
    // since the registered path here simply didn't exist yet). A
    // packaged/installed build resolves this to a bundled resource path
    // instead — that repackaging concern is out of scope here.
    let hook_bridge_path = std::env::current_exe()
        .ok()
        .map(|p| p.with_file_name("hook-bridge").to_string_lossy().to_string());

    let ollama: Arc<dyn OllamaClient> = Arc::new(HttpOllamaClient::local());
    let api_state = bootstrap::start(ollama, BootstrapOptions { hook_bridge_path, terminal }).await?;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", API_PORT)).await?;
    tauri::async_runtime::spawn(async move {
        let _ = axum::serve(listener, router(api_state)).await;
    });

    Ok(())
}
