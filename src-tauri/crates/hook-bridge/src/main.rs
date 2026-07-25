//! Tiny stdin -> Unix domain socket forwarder invoked directly by Claude
//! Code's hook system (plan §3).
//!
//! Deliberately dependency-light and synchronous (no tokio, no async
//! runtime): this binary's own startup latency is on Claude Code's critical
//! path every time a hook fires, so it must connect, forward, and exit in
//! low milliseconds. It must also **fail silently and fast** if the Core
//! Engine isn't running — a broken hook must never block or error the
//! user's real Claude Code session. No retries, no stderr noise, no
//! nonzero exit codes for "engine not running" (that's an expected,
//! everyday condition, not a bridge failure).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

/// Duplicated (not shared via a dependency on core-engine) so this binary's
/// own dependency graph — and therefore its startup time — stays minimal.
/// See `core_engine::hook_socket::socket_path` for the canonical version;
/// keep the two in sync if this ever changes.
fn socket_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("SessionBoard")
        .join("engine.sock")
}

fn main() {
    let event = std::env::args().nth(1).unwrap_or_default();

    let mut stdin_raw = String::new();
    // A read failure here just means an empty payload gets forwarded (or
    // nothing, if the connect below also fails) — never worth aborting over.
    let _ = std::io::stdin().read_to_string(&mut stdin_raw);

    let payload: serde_json::Value =
        serde_json::from_str(&stdin_raw).unwrap_or(serde_json::Value::String(stdin_raw));

    let envelope = serde_json::json!({ "event": event, "payload": payload });
    let Ok(bytes) = serde_json::to_vec(&envelope) else {
        return;
    };

    // Fire-and-forget: a short connect timeout, one write, then exit — no
    // retry loop. If the Core Engine isn't running, this is expected
    // (SessionBoard not installed/open) and must be a silent no-op.
    if let Ok(mut stream) = connect_with_timeout(&socket_path(), Duration::from_millis(200)) {
        let _ = stream.write_all(&bytes);
    }
}

fn connect_with_timeout(path: &std::path::Path, _timeout: Duration) -> std::io::Result<UnixStream> {
    // UnixStream::connect has no built-in timeout; a stale/missing socket
    // file fails fast (ENOENT/ECONNREFUSED) rather than hanging, which is
    // the case that actually matters here (engine not running).
    UnixStream::connect(path)
}
