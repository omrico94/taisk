//! Throwaway harness for the M4 scripted smoke test (plan §11): runs the
//! real UDS listener at the real default socket path, prints "READY" once
//! bound, then prints the first hook event it receives as one JSON line and
//! exits. Driven from a shell script that pipes a fake payload through the
//! actually-compiled `hook-bridge` binary — not part of the app itself.

use tokio::io::AsyncWriteExt;

#[tokio::main]
async fn main() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(core_engine::hook_socket::listen(tx));

    // Give the listener a moment to bind before signaling readiness.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    println!("READY");
    tokio::io::stdout().flush().await.unwrap();

    if let Some(event) = rx.recv().await {
        println!("{}", serde_json::to_string(&event).unwrap());
        tokio::io::stdout().flush().await.unwrap();
    }
}
