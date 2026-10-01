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
        .join("taisk")
        .join("engine.sock")
}

/// The controlling terminal device (e.g. `/dev/ttys003`) this process is
/// attached to, if any. See the call site below for why this matters.
fn detect_tty() -> Option<String> {
    for fd in [libc::STDERR_FILENO, libc::STDOUT_FILENO] {
        let ptr = unsafe { libc::ttyname(fd) };
        if ptr.is_null() {
            continue;
        }
        if let Ok(name) = unsafe { std::ffi::CStr::from_ptr(ptr) }.to_str() {
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let event = args.get(1).cloned().unwrap_or_default();
    // `--board <id>` is written into non-default boards' settings.json by
    // taisk, so events say which Claude config directory (account)
    // they came from. Absent means the default board (`~/.claude`).
    let board = args.iter().position(|a| a == "--board").and_then(|i| args.get(i + 1)).cloned();

    let mut stdin_raw = String::new();
    // A read failure here just means an empty payload gets forwarded (or
    // nothing, if the connect below also fails) — never worth aborting over.
    let _ = std::io::stdin().read_to_string(&mut stdin_raw);

    let mut payload: serde_json::Value =
        serde_json::from_str(&stdin_raw).unwrap_or(serde_json::Value::String(stdin_raw));
    // Carried inside the payload object (not as a new envelope field) so the
    // engine's `HookEvent` shape is unchanged for older bridges/tests.
    if let (Some(board), Some(obj)) = (board, payload.as_object_mut()) {
        obj.insert("_sessionboard_board".to_string(), serde_json::Value::String(board));
    }

    // Set by taisk's "new session from a task" launcher; inherited
    // from the `claude` process that spawned this hook. Lets the engine file
    // the new session under that task deterministically.
    if let (Some(obj), Ok(task_id)) = (payload.as_object_mut(), std::env::var("SESSIONBOARD_TASK_ID")) {
        if !task_id.is_empty() {
            obj.insert("sessionboard_task_id".into(), serde_json::Value::String(task_id));
        }
    }

    // Same idea for terminals embedded in the board: the PTY manager sets
    // this on the `claude` it spawns so the engine can link that pty to the
    // real session id once session-start fires.
    if let (Some(obj), Ok(pty_id)) = (payload.as_object_mut(), std::env::var("SESSIONBOARD_PTY_ID")) {
        if !pty_id.is_empty() {
            obj.insert("sessionboard_pty_id".into(), serde_json::Value::String(pty_id));
        }
    }

    // The controlling terminal device (e.g. `/dev/ttys003`) this hook process
    // inherited, if any — the only channel that exists for delivering an
    // approve/reject/answer keypress into a session running in an ordinary
    // external terminal (one with no embedded pty for `SESSIONBOARD_PTY_ID`
    // to tag above). stdin here is the hook's JSON payload, never a tty, so
    // it's not tried. Checked in order stderr, then stdout: Claude Code may
    // capture a hook's stdout to read a synchronous decision for hooks that
    // support one, in which case fd 1 is a pipe (`ttyname` fails, ENOTTY)
    // even though the process is still attached to a real terminal via fd 2.
    if let Some(obj) = payload.as_object_mut() {
        if let Some(tty) = detect_tty() {
            obj.insert("sessionboard_tty".into(), serde_json::Value::String(tty));
        }
    }

    let envelope = serde_json::json!({ "event": event, "payload": payload });
    let Ok(bytes) = serde_json::to_vec(&envelope) else {
        return;
    };

    // Fire-and-forget: a short connect timeout, one write, then exit — no
    // retry loop. If the Core Engine isn't running, this is expected
    // (taisk not installed/open) and must be a silent no-op.
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
