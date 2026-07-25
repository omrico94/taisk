use std::sync::Arc;

use core_engine::api::router;
use core_engine::bootstrap::{self, BootstrapOptions};
use core_engine::ollama::{HttpOllamaClient, OllamaClient};
use core_engine::API_PORT;

#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

/// Phase 2 roadmap item 6 ("Jump to session"): reattach to a CLI-originated
/// Claude Code session by opening a new Terminal.app window running
/// `claude --resume <id>` in the session's original cwd. `claude --resume
/// <session-id>` is a documented CLI flag that reattaches to a specific
/// session by id. Only meaningful for `entrypoint == "cli"` sessions — the
/// frontend decides whether this command applies (`DetailDrawer.tsx`).
#[tauri::command]
fn jump_to_cli_session(cwd: String, session_id: String) -> Result<(), String> {
    // Two layers of escaping, deliberately not a naive string concatenation:
    // (1) build the inner shell command with standard POSIX single-quote
    // escaping so cwd/session_id can't break out of their quoting or inject
    // extra shell commands; (2) embed that whole string as an AppleScript
    // double-quoted literal, escaping backslashes/quotes for AppleScript's
    // own syntax.
    fn shell_single_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
    let shell_cmd =
        format!("cd {} && claude --resume {}", shell_single_quote(&cwd), shell_single_quote(&session_id));
    let osa_escaped = shell_cmd.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(r#"tell application "Terminal" to do script "{osa_escaped}""#);

    let status = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .status()
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err("osascript exited with a non-zero status".to_string())
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![greet, jump_to_cli_session])
        .setup(|_app| {
            tauri::async_runtime::spawn(async move {
                if let Err(e) = start_core_engine().await {
                    eprintln!("Core Engine failed to start: {e}");
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Boots the Core Engine as background tasks (plan §1/§9) and serves the
/// local HTTP/WS API the frontend (and, later, a VS Code extension) talks
/// to. The actual bootstrap logic lives in `core_engine::bootstrap` so the
/// standalone dev-server example shares it exactly.
async fn start_core_engine() -> Result<(), Box<dyn std::error::Error>> {
    // hook-bridge is built as a sibling binary in the same workspace; in dev
    // (`cargo tauri dev`) it lands next to this executable in target/debug.
    // A packaged/installed build would resolve this to a bundled resource
    // path instead — that repackaging concern is out of scope here.
    let hook_bridge_path = std::env::current_exe()
        .ok()
        .map(|p| p.with_file_name("hook-bridge").to_string_lossy().to_string());

    let ollama: Arc<dyn OllamaClient> = Arc::new(HttpOllamaClient::local());
    let api_state = bootstrap::start(ollama, BootstrapOptions { hook_bridge_path }).await?;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", API_PORT)).await?;
    tauri::async_runtime::spawn(async move {
        let _ = axum::serve(listener, router(api_state)).await;
    });

    Ok(())
}
