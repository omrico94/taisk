//! Shortcut kit: system-wide hotkeys that work from any app while SessionBoard
//! is running (it lives in the menu bar, so closing the main window doesn't
//! stop them).
//!
//! - `QUICK_ADD_KEYS`: small input window to file a task without switching apps.
//! - `PEEK_KEYS`: small floating list of every task and its stage.
//!
//! Both popups are the normal frontend on a hash route (`#/quick-add`,
//! `#/peek`, see `src/main.tsx`), created hidden at startup so showing them is
//! instant. They talk to the Core Engine over its HTTP API like the main
//! window, so no Rust-side task logic lives here.

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{App, AppHandle, Emitter, Manager, Window, WindowEvent, WebviewUrl, WebviewWindowBuilder, Wry};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

const QUICK_ADD_LABEL: &str = "quick-add";
const PEEK_LABEL: &str = "peek";

/// ⌥⌘N — add a task from anywhere.
const QUICK_ADD_KEYS: (Modifiers, Code) = (Modifiers::ALT.union(Modifiers::SUPER), Code::KeyN);
/// ⌥⌘L — peek at all tasks and their stages.
const PEEK_KEYS: (Modifiers, Code) = (Modifiers::ALT.union(Modifiers::SUPER), Code::KeyL);

fn shortcut((mods, code): (Modifiers, Code)) -> Shortcut {
    Shortcut::new(Some(mods), code)
}

pub fn plugin() -> tauri::plugin::TauriPlugin<Wry> {
    tauri_plugin_global_shortcut::Builder::new()
        .with_handler(|app, sc, event| {
            if event.state() != ShortcutState::Pressed {
                return;
            }
            if *sc == shortcut(QUICK_ADD_KEYS) {
                toggle_popup(app, QUICK_ADD_LABEL);
            } else if *sc == shortcut(PEEK_KEYS) {
                toggle_popup(app, PEEK_LABEL);
            }
        })
        .build()
}

pub fn setup(app: &mut App) -> Result<(), Box<dyn std::error::Error>> {
    build_popup(app, QUICK_ADD_LABEL, "index.html#/quick-add", 540.0, 220.0)?;
    build_popup(app, PEEK_LABEL, "index.html#/peek", 400.0, 520.0)?;

    // A taken combo shouldn't stop the app booting; the tray still reaches both popups.
    for keys in [QUICK_ADD_KEYS, PEEK_KEYS] {
        if let Err(e) = app.global_shortcut().register(shortcut(keys)) {
            eprintln!("Could not register global shortcut: {e}");
        }
    }

    let show = MenuItem::with_id(app, "show", "Show Board", true, None::<&str>)?;
    let add = MenuItem::with_id(app, "add", "Quick Add Task  ⌥⌘N", true, None::<&str>)?;
    let peek = MenuItem::with_id(app, "peek", "Task List  ⌥⌘L", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit SessionBoard", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &add, &peek, &quit])?;

    let mut tray = TrayIconBuilder::new()
        .menu(&menu)
        .tooltip("SessionBoard")
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main(app),
            "add" => toggle_popup(app, QUICK_ADD_LABEL),
            "peek" => toggle_popup(app, PEEK_LABEL),
            "quit" => app.exit(0),
            _ => {}
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    Ok(())
}

/// Main window: closing hides it (so the hotkeys keep working); quit is via the
/// tray or ⌘Q. Popups: hide when they lose focus (click-away dismiss).
pub fn on_window_event(window: &Window, event: &WindowEvent) {
    match (window.label(), event) {
        ("main", WindowEvent::CloseRequested { api, .. }) => {
            api.prevent_close();
            let _ = window.hide();
            // Menu-bar mode while the board is hidden: no Dock icon / ⌘Tab entry,
            // which is also what lets popups appear over full-screen apps.
            #[cfg(target_os = "macos")]
            {
                use tauri::Manager;
                let _ = window.app_handle().set_activation_policy(tauri::ActivationPolicy::Accessory);
            }
        }
        (QUICK_ADD_LABEL | PEEK_LABEL, WindowEvent::Focused(false)) => {
            let _ = window.hide();
        }
        _ => {}
    }
}

fn build_popup(app: &App, label: &str, url: &str, w: f64, h: f64) -> tauri::Result<()> {
    WebviewWindowBuilder::new(app, label, WebviewUrl::App(url.into()))
        .title(label)
        .inner_size(w, h)
        .decorations(false)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible_on_all_workspaces(true)
        .visible(false)
        .center()
        .build()?;
    Ok(())
}

fn toggle_popup(app: &AppHandle, label: &str) {
    let Some(win) = app.get_webview_window(label) else { return };
    if win.is_visible().unwrap_or(false) && win.is_focused().unwrap_or(false) {
        let _ = win.hide();
        return;
    }
    // Hide the sibling so the two popups never stack.
    let other = if label == QUICK_ADD_LABEL { PEEK_LABEL } else { QUICK_ADD_LABEL };
    if let Some(o) = app.get_webview_window(other) {
        let _ = o.hide();
    }
    let _ = win.center();
    #[cfg(target_os = "macos")]
    float_over_all_spaces(&win);
    let _ = win.show();
    let _ = win.set_focus();
    // The view resets its state / refetches on this.
    let _ = win.emit("shortcut:shown", ());
}

fn show_main(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

/// Makes the window join every Space and sit over full-screen apps, so it
/// shows up where the user is instead of pulling them to the app's own Space.
#[cfg(target_os = "macos")]
fn float_over_all_spaces(win: &tauri::WebviewWindow) {
    use objc2_app_kit::{NSWindow, NSWindowCollectionBehavior};
    let Ok(ptr) = win.ns_window() else { return };
    // SAFETY: `ptr` is this live window's NSWindow, and we're only setting a property on it.
    let ns_window: &NSWindow = unsafe { &*(ptr as *const NSWindow) };
    ns_window.setCollectionBehavior(
        NSWindowCollectionBehavior::CanJoinAllSpaces
            | NSWindowCollectionBehavior::FullScreenAuxiliary
            | NSWindowCollectionBehavior::Stationary,
    );
    // Above normal floating windows (NSStatusWindowLevel = 25).
    ns_window.setLevel(25);
}
