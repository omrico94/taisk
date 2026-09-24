//! Shortcut kit: system-wide hotkeys that work from any app while taisk
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
//!
//! On macOS the popups are converted to `NSPanel`s with the `NonactivatingPanel`
//! style mask (see `ShortcutPanel` below and `docs` on `build_popup`) — this is
//! the standard technique Spotlight-alternative apps (Alfred, Raycast) use so a
//! global-hotkey popup can become key window and accept typing *without*
//! making taisk the active application. A plain Tauri window plus
//! `activateIgnoringOtherApps`/`makeKeyAndOrderFront`/window-level tricks was
//! tried first and reliably failed to render at all when the hotkey was
//! pressed while Chrome was the frontmost app (confirmed via a direct
//! `CGWindowListCopyWindowInfo` query: the window was correctly configured —
//! right layer, right bounds, alpha 1.0 — but `kCGWindowIsOnscreen` stayed
//! false) even though the identical code worked reliably from Finder and
//! Safari. The non-activating panel sidesteps the whole app-activation
//! question instead of fighting it.

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

#[cfg(target_os = "macos")]
tauri_nspanel::tauri_panel! {
    panel!(ShortcutPanel {
        config: {
            can_become_key_window: true,
            can_become_main_window: false,
            is_floating_panel: true,
            hides_on_deactivate: false
        }
    })
}

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
    // Initial size only — the frontend measures its own content and resizes
    // the window to fit exactly (useAutoResizeWindow.ts), rather than the
    // panel stretching to fill a fixed size and leaving empty space below
    // short content. This is just a reasonable first paint before that
    // happens (harmless either way since the window is built hidden, well
    // before a user can actually see it).
    build_popup(app, QUICK_ADD_LABEL, "index.html#/quick-add", 460.0, 130.0)?;
    build_popup(app, PEEK_LABEL, "index.html#/peek", 360.0, 520.0)?;

    // A taken combo shouldn't stop the app booting; the tray still reaches both popups.
    for keys in [QUICK_ADD_KEYS, PEEK_KEYS] {
        if let Err(e) = app.global_shortcut().register(shortcut(keys)) {
            eprintln!("Could not register global shortcut: {e}");
        }
    }

    let show = MenuItem::with_id(app, "show", "Show Board", true, None::<&str>)?;
    let add = MenuItem::with_id(app, "add", "Quick Add Task  ⌥⌘N", true, None::<&str>)?;
    let peek = MenuItem::with_id(app, "peek", "Task List  ⌥⌘L", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit taisk", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &add, &peek, &quit])?;

    let mut tray = TrayIconBuilder::new()
        .menu(&menu)
        .tooltip("taisk")
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
            // Menu-bar mode while the board is hidden: no Dock icon / ⌘Tab entry.
            #[cfg(target_os = "macos")]
            {
                use tauri::Manager;
                let _ = window.app_handle().set_activation_policy(tauri::ActivationPolicy::Accessory);
            }
        }
        (QUICK_ADD_LABEL | PEEK_LABEL, WindowEvent::Focused(false)) => {
            hide_popup(window);
        }
        _ => {}
    }
}

fn build_popup(app: &App, label: &str, url: &str, w: f64, h: f64) -> tauri::Result<()> {
    #[allow(unused_variables)]
    let window = WebviewWindowBuilder::new(app, label, WebviewUrl::App(url.into()))
        .title(label)
        .inner_size(w, h)
        .decorations(false)
        .resizable(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible_on_all_workspaces(true)
        .visible(false)
        // Without this the *window* (not just the webview content) keeps
        // AppKit's default opaque white background, which shows through as
        // a stark rectangular frame around the CSS-rounded `.panel` div in
        // the corners the border-radius doesn't cover. The CSS side
        // (`html.popup` background: transparent, Popup.module.css) only
        // controls the webview's own painting, not the native window behind
        // it — both have to be transparent for the rounded corners to
        // actually show the desktop/app-behind-it, not a white box.
        .transparent(true)
        .center()
        .build()?;

    #[cfg(target_os = "macos")]
    {
        use tauri_nspanel::{StyleMask, WebviewWindowExt};
        let panel = window.to_panel::<ShortcutPanel>()?;
        // Same level system popup menus use, so it's reliably above normal
        // app windows and menu bars regardless of what level the frontmost
        // app's own windows happen to sit at.
        panel.set_level(objc2_app_kit::NSPopUpMenuWindowLevel as i64);
        panel.set_collection_behavior(
            objc2_app_kit::NSWindowCollectionBehavior::MoveToActiveSpace
                | objc2_app_kit::NSWindowCollectionBehavior::FullScreenAuxiliary
                | objc2_app_kit::NSWindowCollectionBehavior::Stationary,
        );
        // The load-bearing line: lets this panel become key window (and
        // therefore receive typing) while a *different* app stays active —
        // see the module doc comment for why this replaced an
        // activate-the-app approach.
        let _ = panel.add_style_mask(StyleMask::empty().nonactivating_panel().into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn toggle_popup(app: &AppHandle, label: &str) {
    use tauri_nspanel::ManagerExt;
    let Some(win) = app.get_webview_window(label) else { return };
    let Ok(panel) = app.get_webview_panel(label) else { return };

    if panel.is_visible() && win.is_focused().unwrap_or(false) {
        panel.hide();
        return;
    }
    // Hide the sibling so the two popups never stack.
    let other = if label == QUICK_ADD_LABEL { PEEK_LABEL } else { QUICK_ADD_LABEL };
    if let Ok(o) = app.get_webview_panel(other) {
        o.hide();
    }
    let _ = win.center();
    panel.show_and_make_key();
    // The view resets its state / refetches on this.
    let _ = win.emit("shortcut:shown", ());
}

#[cfg(not(target_os = "macos"))]
fn toggle_popup(app: &AppHandle, label: &str) {
    let Some(win) = app.get_webview_window(label) else { return };
    if win.is_visible().unwrap_or(false) && win.is_focused().unwrap_or(false) {
        let _ = win.hide();
        return;
    }
    let other = if label == QUICK_ADD_LABEL { PEEK_LABEL } else { QUICK_ADD_LABEL };
    if let Some(o) = app.get_webview_window(other) {
        let _ = o.hide();
    }
    let _ = win.center();
    let _ = win.show();
    let _ = win.set_focus();
    let _ = win.emit("shortcut:shown", ());
}

#[cfg(target_os = "macos")]
fn hide_popup(window: &Window) {
    use tauri_nspanel::ManagerExt;
    if let Ok(panel) = window.app_handle().get_webview_panel(window.label()) {
        panel.hide();
    } else {
        let _ = window.hide();
    }
}

#[cfg(not(target_os = "macos"))]
fn hide_popup(window: &Window) {
    let _ = window.hide();
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
