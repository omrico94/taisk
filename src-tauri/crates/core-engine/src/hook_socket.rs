//! Unix domain socket listener for hook events forwarded by the `hook-bridge`
//! binary (plan §3): trusted local-process IPC, no port/token bookkeeping —
//! the socket file's own permissions gate access. Separate from the
//! browser/webview-facing HTTP+WS API (§7), which needs a real TCP port.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::net::UnixListener;
use tokio::sync::mpsc;

/// A hook event forwarded from `hook-bridge`: the Claude Code hook name
/// (`session-start`, `notification`, `pre-tool-use`, `post-tool-use`, `stop`,
/// `session-end`) plus its raw JSON payload from stdin.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HookEvent {
    pub event: String,
    pub payload: serde_json::Value,
}

/// `~/Library/Application Support/SessionBoard/engine.sock` (or platform
/// equivalent via the `dirs` crate). Shared convention with `hook-bridge`,
/// which resolves the same path independently rather than depending on this
/// crate (keeping the bridge binary's own dependency footprint — and
/// therefore its startup latency — minimal; see plan §3's robustness note
/// that a broken/slow hook must never block a real Claude Code session).
pub fn socket_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("SessionBoard")
        .join("engine.sock")
}

/// Binds the UDS at the default `socket_path()`. Thin wrapper around
/// `listen_at` — see that function for the actual logic. Kept separate so
/// production call sites (the orchestrator's default config) don't need to
/// know the default path, while tests can bind an isolated per-test path via
/// `listen_at` directly (parallel test runs binding the same real path would
/// otherwise collide with each other and with a real running instance).
pub async fn listen(tx: mpsc::Sender<HookEvent>) -> std::io::Result<()> {
    listen_at(socket_path(), tx).await
}

/// Binds the UDS at `path` and forwards every accepted connection's full
/// contents (parsed as a `HookEvent`) onto `tx`. Removes a stale socket file
/// left behind by a previous, uncleanly-terminated run before binding —
/// otherwise `bind` fails with "address already in use" even though nothing
/// is actually listening.
pub async fn listen_at(path: PathBuf, tx: mpsc::Sender<HookEvent>) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if path.exists() {
        let _ = tokio::fs::remove_file(&path).await;
    }

    let listener = UnixListener::bind(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }

    loop {
        let (mut stream, _addr) = listener.accept().await?;
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            if stream.read_to_end(&mut buf).await.is_err() {
                return;
            }
            if let Ok(event) = serde_json::from_slice::<HookEvent>(&buf) {
                let _ = tx.send(event).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::net::UnixStream as StdUnixStream;

    #[tokio::test]
    async fn forwards_a_connection_as_a_hook_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine.sock");

        let listener = UnixListener::bind(&path).unwrap();
        let (tx, mut rx) = mpsc::channel(8);

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.unwrap();
            let event: HookEvent = serde_json::from_slice(&buf).unwrap();
            tx.send(event).await.unwrap();
        });

        // Simulate hook-bridge: connect, write the envelope, close (EOF).
        let envelope = serde_json::json!({
            "event": "session-start",
            "payload": {"session_id": "s1", "cwd": "/Users/omricohen/Desktop"}
        });
        let path_clone = path.clone();
        tokio::task::spawn_blocking(move || {
            let mut stream = StdUnixStream::connect(&path_clone).unwrap();
            stream.write_all(envelope.to_string().as_bytes()).unwrap();
            // drop(stream) on return -> EOF for the reader
        })
        .await
        .unwrap();

        let received = rx.recv().await.unwrap();
        assert_eq!(received.event, "session-start");
        assert_eq!(received.payload["session_id"], "s1");
    }

    #[test]
    fn socket_path_is_under_a_sessionboard_directory() {
        let p = socket_path();
        assert_eq!(p.file_name().unwrap(), "engine.sock");
        assert_eq!(p.parent().unwrap().file_name().unwrap(), "SessionBoard");
    }
}
