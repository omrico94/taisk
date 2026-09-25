use std::sync::Arc;

use core_engine::api::router;
use core_engine::bootstrap::{self, BootstrapOptions};
use core_engine::ollama::{HttpOllamaClient, OllamaClient};
use core_engine::terminal::TerminalManager;
use core_engine::API_PORT;

mod shortcuts;

/// Opens a new Terminal.app window running `claude` under `config_dir`, so the
/// user can log in to the Claude account that board should use. Login state
/// is per config dir, which is what keeps boards' accounts separate.
#[tauri::command]
fn login_board_terminal(config_dir: String) -> Result<(), String> {
    let dir = core_engine::boards::expand_home(&config_dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let shell_cmd = format!("CLAUDE_CONFIG_DIR={} claude", shell_single_quote(&dir.to_string_lossy()));
    run_in_terminal(&shell_cmd)
}

/// POSIX single-quote escaping, so a path or id can't break out of its quoting
/// or inject extra shell commands.
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Runs `shell_cmd` in a new Terminal.app window. The command is embedded as
/// an AppleScript double-quoted literal, escaping backslashes/quotes for
/// AppleScript's own syntax.
fn run_in_terminal(shell_cmd: &str) -> Result<(), String> {
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
    // Created up front (it's cheap and needs no runtime) so the same instance
    // reaches both the engine's `AppState` and the exit handler below.
    let terminal = TerminalManager::new();
    let terminal_for_engine = terminal.clone();

    #[allow(unused_mut)]
    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(shortcuts::plugin());
    #[cfg(target_os = "macos")]
    {
        builder = builder.plugin(tauri_nspanel::init());
    }
    builder
        .invoke_handler(tauri::generate_handler![login_board_terminal])
        .on_window_event(shortcuts::on_window_event)
        .setup(move |app| {
            shortcuts::setup(app)?;
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
    // hook-bridge sits next to this executable: in target/debug for dev
    // (`beforeDevCommand` builds it), or in Contents/MacOS for a release
    // bundle (`externalBin` in tauri.release.conf.json).
    let sibling = std::env::current_exe()
        .ok()
        .map(|p| p.with_file_name("hook-bridge"));
    // Claude Code's settings.json stores the absolute path, so a bundled
    // build registers a stable copy under the data dir. That way the hooks
    // keep working when the .app is upgraded, moved or reinstalled.
    let hook_bridge_path = sibling
        .map(|p| if cfg!(debug_assertions) { p } else { install_stable_hook_bridge(&p).unwrap_or(p) })
        .map(|p| p.to_string_lossy().to_string());

    let ollama: Arc<dyn OllamaClient> = Arc::new(HttpOllamaClient::local());
    let api_state = bootstrap::start(ollama, BootstrapOptions { hook_bridge_path, terminal }).await?;

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", API_PORT)).await?;
    tauri::async_runtime::spawn(async move {
        let _ = axum::serve(listener, router(api_state)).await;
    });

    Ok(())
}

/// Copies the bundled hook-bridge to `<data dir>/bin/hook-bridge`, replacing
/// it only when its bytes differ (so a running hook is never clobbered by an
/// identical rewrite). Returns the stable path.
fn install_stable_hook_bridge(bundled: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let dir = core_engine::first_run::app_data_dir().join("bin");
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join("hook-bridge");
    let new_bytes = std::fs::read(bundled)?;
    if std::fs::read(&dest).ok().as_deref() != Some(new_bytes.as_slice()) {
        // Write-then-rename so Claude Code never execs a half-written file.
        let tmp = dir.join("hook-bridge.tmp");
        std::fs::write(&tmp, &new_bytes)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
        std::fs::rename(&tmp, &dest)?;
    }
    Ok(dest)
}
