//! Core Engine: session state machine, collectors, memory repo, and local API.
//! Populated milestone-by-milestone per the SessionBoard implementation plan.

pub mod api;
pub mod bootstrap;
pub mod boards;
pub mod collector;
pub mod engine;
pub mod first_run;
pub mod hook_socket;
pub mod memory_repo;
pub mod ollama;
pub mod orchestrator;
pub mod plan;
pub mod settings_merge;
pub mod state;
pub mod summarize;
pub mod tasks;
pub mod terminal;

pub(crate) fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

/// Fixed local port for the desktop app's own Core Engine instance (plan
/// §7). Shared between the Tauri app (src-tauri/src/lib.rs) and the
/// standalone dev-server example so both agree on where the frontend can
/// find it.
pub const API_PORT: u16 = 37888;
