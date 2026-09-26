//! Embedded terminals: real PTY-backed `claude` processes the board can
//! attach a single visible terminal pane to.
//!
//! A `TerminalManager` owns every spawned PTY for the lifetime of the app,
//! keyed by a generated `pty_id`. Processes deliberately outlive whatever is
//! attached to them: switching the board's terminal pane to another session
//! (or collapsing it) only drops that pane's WebSocket subscriber, so
//! switching back replays the recent output from a bounded scrollback buffer
//! and the conversation was never interrupted.
//!
//! portable-pty's I/O is blocking, so — unlike `EngineHandle`'s single-task
//! actor — each PTY gets a dedicated OS reader thread; writes go through
//! `spawn_blocking`. Shape otherwise follows `tasks::TaskHub`: a cheap-clone
//! handle over `Arc<Mutex<HashMap<..>>>`.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use tokio::sync::broadcast;

pub type PtyId = String;

/// Bytes of recent output kept per PTY for instant replay on (re)attach.
/// Session-scoped variables of whatever Claude Code session (or taisk task
/// launch) started this app. Inherited by a pty `claude` they do real harm:
/// `CLAUDE_CODE_CHILD_SESSION` turns transcript saving off, so the session
/// never surfaces on the board and is never filed under its task; a stale
/// `SESSIONBOARD_TASK_ID` would file it under the wrong one. User config
/// (`CLAUDE_CONFIG_DIR`, provider keys, ...) is deliberately kept.
const INHERITED_SESSION_ENV: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "SESSIONBOARD_TASK_ID",
    "SESSIONBOARD_PTY_ID",
];

const SCROLLBACK_CAP: usize = 256 * 1024;

const DEFAULT_SIZE: PtySize = PtySize { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 };

static NEXT_PTY_ID: AtomicU64 = AtomicU64::new(0);

fn new_pty_id() -> PtyId {
    format!("p{:x}{:x}", crate::now_ms(), NEXT_PTY_ID.fetch_add(1, Ordering::Relaxed))
}

#[derive(Debug, Clone, PartialEq)]
pub enum PtyOutput {
    Data(Vec<u8>),
    /// Broadcast once, when the orchestrator learns this pty's real session id.
    Linked(String),
    Exited(Option<i32>),
}

/// What to spawn. Deliberately command-agnostic — callers decide whether it
/// is `claude` or `claude --resume <id>`.
pub struct SpawnSpec {
    pub cwd: String,
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Known up front for "jump into an existing session".
    pub session_id: Option<String>,
    /// Known up front for "start a new session from a task".
    pub task_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PtyInfo {
    pub pty_id: PtyId,
    pub session_id: Option<String>,
    pub task_id: Option<String>,
    pub cwd: String,
    pub alive: bool,
    pub pid: Option<u32>,
}

/// Everything a new viewer needs to attach seamlessly.
pub struct Subscription {
    pub scrollback: Vec<u8>,
    pub rx: broadcast::Receiver<PtyOutput>,
    pub session_id: Option<String>,
    /// `Some(code)` once the process has exited (`code` itself may be unknown).
    pub exited: Option<Option<i32>>,
}

struct PtyState {
    scrollback: VecDeque<u8>,
    session_id: Option<String>,
    exited: Option<Option<i32>>,
}

struct PtyEntry {
    writer: Mutex<Box<dyn Write + Send>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    output_tx: broadcast::Sender<PtyOutput>,
    /// Scrollback, link and exit state share one lock with every broadcast
    /// send, so `subscribe` (snapshot + `tx.subscribe()` under the same lock)
    /// can never miss or duplicate a chunk.
    state: Mutex<PtyState>,
    task_id: Option<String>,
    cwd: String,
    pid: Option<u32>,
}

#[derive(Clone, Default)]
pub struct TerminalManager {
    inner: Arc<Mutex<HashMap<PtyId, Arc<PtyEntry>>>>,
}

/// GUI-launched apps inherit a minimal PATH (no shell profile), so `claude`
/// installed under the user's home or Homebrew wouldn't be found.
fn augmented_path(existing: Option<String>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        for rel in [".local/bin", ".claude/local", ".npm-global/bin", ".bun/bin"] {
            let p = home.join(rel);
            if p.is_dir() {
                parts.push(p.to_string_lossy().to_string());
            }
        }
    }
    for abs in ["/opt/homebrew/bin", "/usr/local/bin"] {
        if std::path::Path::new(abs).is_dir() {
            parts.push(abs.to_string());
        }
    }
    if let Some(existing) = existing {
        parts.push(existing);
    }
    parts.join(":")
}

impl TerminalManager {
    pub fn new() -> Self {
        Self::default()
    }

    fn get(&self, pty_id: &str) -> Option<Arc<PtyEntry>> {
        self.inner.lock().unwrap().get(pty_id).cloned()
    }

    /// Opens a pty pair and spawns `spec.program` in it. `SESSIONBOARD_PTY_ID`
    /// is injected here (not by callers) so every pty-spawned `claude` is
    /// uniformly taggable by hook-bridge / the orchestrator.
    pub fn spawn(&self, spec: SpawnSpec) -> Result<PtyId, String> {
        let pty_id = new_pty_id();

        let pair = native_pty_system().openpty(DEFAULT_SIZE).map_err(|e| e.to_string())?;

        let mut cmd = CommandBuilder::new(&spec.program);
        cmd.args(&spec.args);
        cmd.cwd(&spec.cwd);
        for k in INHERITED_SESSION_ENV {
            cmd.env_remove(k);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("PATH", augmented_path(std::env::var("PATH").ok()));
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        cmd.env("SESSIONBOARD_PTY_ID", &pty_id);

        let mut child = pair.slave.spawn_command(cmd).map_err(|e| e.to_string())?;
        // The parent must not hold the slave end, or the reader never sees EOF.
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let writer = pair.master.take_writer().map_err(|e| e.to_string())?;
        let pid = child.process_id();

        let (output_tx, _) = broadcast::channel(1024);
        let entry = Arc::new(PtyEntry {
            writer: Mutex::new(writer),
            master: Mutex::new(pair.master),
            output_tx,
            state: Mutex::new(PtyState {
                scrollback: VecDeque::new(),
                session_id: spec.session_id.clone(),
                exited: None,
            }),
            task_id: spec.task_id.clone(),
            cwd: spec.cwd.clone(),
            pid,
        });

        self.inner.lock().unwrap().insert(pty_id.clone(), entry.clone());

        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = buf[..n].to_vec();
                        let mut st = entry.state.lock().unwrap();
                        st.scrollback.extend(chunk.iter().copied());
                        let overflow = st.scrollback.len().saturating_sub(SCROLLBACK_CAP);
                        if overflow > 0 {
                            st.scrollback.drain(..overflow);
                        }
                        let _ = entry.output_tx.send(PtyOutput::Data(chunk));
                    }
                }
            }
            let code = child.wait().ok().map(|s| s.exit_code() as i32);
            let mut st = entry.state.lock().unwrap();
            st.exited = Some(code);
            let _ = entry.output_tx.send(PtyOutput::Exited(code));
        });

        Ok(pty_id)
    }

    /// A *live* pty already attached to `session_id`, so repeat "jump" clicks
    /// reuse the running process instead of spawning a second
    /// `claude --resume <id>` beside it.
    pub fn find_by_session_id(&self, session_id: &str) -> Option<PtyId> {
        let map = self.inner.lock().unwrap();
        map.iter()
            .find(|(_, e)| {
                let st = e.state.lock().unwrap();
                st.exited.is_none() && st.session_id.as_deref() == Some(session_id)
            })
            .map(|(id, _)| id.clone())
    }

    /// Records this pty's real session id and tells any attached viewer.
    pub fn link_session(&self, pty_id: &str, session_id: &str) -> bool {
        let Some(entry) = self.get(pty_id) else { return false };
        let mut st = entry.state.lock().unwrap();
        st.session_id = Some(session_id.to_string());
        let _ = entry.output_tx.send(PtyOutput::Linked(session_id.to_string()));
        true
    }

    pub async fn write(&self, pty_id: &str, data: &[u8]) -> Result<(), String> {
        let entry = self.get(pty_id).ok_or_else(|| format!("unknown pty {pty_id}"))?;
        let data = data.to_vec();
        tokio::task::spawn_blocking(move || {
            let mut w = entry.writer.lock().unwrap();
            w.write_all(&data).and_then(|_| w.flush()).map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?
    }

    pub async fn resize(&self, pty_id: &str, cols: u16, rows: u16) -> Result<(), String> {
        let entry = self.get(pty_id).ok_or_else(|| format!("unknown pty {pty_id}"))?;
        let master = entry.master.lock().unwrap();
        master
            .resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| e.to_string())
    }

    pub fn subscribe(&self, pty_id: &str) -> Option<Subscription> {
        let entry = self.get(pty_id)?;
        let st = entry.state.lock().unwrap();
        let rx = entry.output_tx.subscribe();
        Some(Subscription {
            scrollback: st.scrollback.iter().copied().collect(),
            rx,
            session_id: st.session_id.clone(),
            exited: st.exited,
        })
    }

    pub fn info(&self, pty_id: &str) -> Option<PtyInfo> {
        let entry = self.get(pty_id)?;
        let st = entry.state.lock().unwrap();
        Some(PtyInfo {
            pty_id: pty_id.to_string(),
            session_id: st.session_id.clone(),
            task_id: entry.task_id.clone(),
            cwd: entry.cwd.clone(),
            alive: st.exited.is_none(),
            pid: entry.pid,
        })
    }

    /// App-quit cleanup. Each child is its own session/process-group leader
    /// (portable-pty `setsid`s it), and `claude` spawns subprocesses of its
    /// own, so signal the whole group rather than just the direct child.
    pub fn kill_all(&self) {
        let live: Vec<(Arc<PtyEntry>, i32)> = self
            .inner
            .lock()
            .unwrap()
            .values()
            .filter_map(|e| {
                let alive = e.state.lock().unwrap().exited.is_none();
                e.pid.filter(|_| alive).map(|pid| (e.clone(), pid as i32))
            })
            .collect();
        for (_, pid) in &live {
            unsafe { libc::killpg(*pid, libc::SIGTERM) };
        }
        for _ in 0..10 {
            if live.iter().all(|(e, _)| e.state.lock().unwrap().exited.is_some()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        for (e, pid) in &live {
            if e.state.lock().unwrap().exited.is_none() {
                unsafe { libc::killpg(*pid, libc::SIGKILL) };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> SpawnSpec {
        SpawnSpec {
            cwd: "/tmp".into(),
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            env: vec![],
            session_id: None,
            task_id: None,
        }
    }

    async fn collect_until(
        rx: &mut broadcast::Receiver<PtyOutput>,
        mut acc: Vec<u8>,
        pred: impl Fn(&[u8]) -> bool,
    ) -> Vec<u8> {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !pred(&acc) {
                match rx.recv().await {
                    Ok(PtyOutput::Data(d)) => acc.extend(d),
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            acc
        })
        .await
        .expect("timed out waiting for pty output")
    }

    fn contains(hay: &[u8], needle: &str) -> bool {
        String::from_utf8_lossy(hay).contains(needle)
    }

    #[tokio::test]
    async fn write_reaches_the_process_and_its_output_is_broadcast() {
        let tm = TerminalManager::new();
        let id = tm.spawn(sh(r#"read x; printf "got:%s" "$x""#)).unwrap();
        let sub = tm.subscribe(&id).unwrap();
        let mut rx = sub.rx;
        tm.write(&id, b"hello\n").await.unwrap();
        let out = collect_until(&mut rx, sub.scrollback, |b| contains(b, "got:hello")).await;
        assert!(contains(&out, "got:hello"));
        tm.kill_all();
    }

    #[tokio::test]
    async fn spawn_injects_the_pty_id_and_extra_env_into_the_child() {
        let tm = TerminalManager::new();
        let mut spec = sh(r#"printf "pid=%s task=%s" "$SESSIONBOARD_PTY_ID" "$SESSIONBOARD_TASK_ID""#);
        spec.env = vec![("SESSIONBOARD_TASK_ID".into(), "t42".into())];
        let id = tm.spawn(spec).unwrap();
        let sub = tm.subscribe(&id).unwrap();
        let mut rx = sub.rx;
        let out = collect_until(&mut rx, sub.scrollback, |b| contains(b, "task=t42")).await;
        assert!(contains(&out, &format!("pid={id}")));
    }

    #[tokio::test]
    async fn spawn_scrubs_session_env_inherited_from_a_parent_claude_session() {
        std::env::set_var("CLAUDE_CODE_CHILD_SESSION", "1");
        std::env::set_var("SESSIONBOARD_TASK_ID", "stale");
        let tm = TerminalManager::new();
        let id = tm.spawn(sh(r#"printf "child=[%s] task=[%s] end" "$CLAUDE_CODE_CHILD_SESSION" "$SESSIONBOARD_TASK_ID""#)).unwrap();
        let sub = tm.subscribe(&id).unwrap();
        let mut rx = sub.rx;
        let out = collect_until(&mut rx, sub.scrollback, |b| contains(b, " end")).await;
        assert!(contains(&out, "child=[] task=[] end"), "{}", String::from_utf8_lossy(&out));
    }

    #[tokio::test]
    async fn resize_succeeds_on_a_live_pty_and_errors_on_an_unknown_id() {
        let tm = TerminalManager::new();
        let id = tm.spawn(sh("sleep 5")).unwrap();
        assert!(tm.resize(&id, 120, 40).await.is_ok());
        assert!(tm.resize("nope", 120, 40).await.is_err());
        assert!(tm.write("nope", b"x").await.is_err());
        tm.kill_all();
    }

    #[tokio::test]
    async fn child_exit_is_broadcast_and_marks_the_pty_dead() {
        let tm = TerminalManager::new();
        let id = tm.spawn(sh("exit 3")).unwrap();
        let mut rx = tm.subscribe(&id).unwrap().rx;
        let code = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(PtyOutput::Exited(c)) = rx.recv().await {
                    break c;
                }
            }
        })
        .await
        .expect("no Exited broadcast");
        assert_eq!(code, Some(3));
        let info = tm.info(&id).unwrap();
        assert!(!info.alive);
        // A viewer attaching after the exit still learns about it.
        assert_eq!(tm.subscribe(&id).unwrap().exited, Some(Some(3)));
    }

    #[tokio::test]
    async fn scrollback_is_bounded_and_keeps_the_most_recent_bytes() {
        let tm = TerminalManager::new();
        let id = tm.spawn(sh(r#"head -c 600000 /dev/zero | tr '\0' a; echo THE_END"#)).unwrap();
        let mut rx = tm.subscribe(&id).unwrap().rx;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(PtyOutput::Exited(_)) = rx.recv().await {
                    break;
                }
            }
        })
        .await
        .expect("no Exited broadcast");
        let sb = tm.subscribe(&id).unwrap().scrollback;
        assert!(sb.len() <= SCROLLBACK_CAP, "scrollback grew to {}", sb.len());
        assert!(contains(&sb, "THE_END"), "newest bytes must be retained");
    }

    #[tokio::test]
    async fn link_session_broadcasts_linked_and_enables_lookup_by_session_id() {
        let tm = TerminalManager::new();
        let id = tm.spawn(sh("sleep 5")).unwrap();
        assert_eq!(tm.find_by_session_id("s1"), None);
        let mut rx = tm.subscribe(&id).unwrap().rx;
        assert!(tm.link_session(&id, "s1"));
        assert_eq!(rx.recv().await.unwrap(), PtyOutput::Linked("s1".into()));
        assert_eq!(tm.info(&id).unwrap().session_id.as_deref(), Some("s1"));
        assert_eq!(tm.find_by_session_id("s1"), Some(id));
        assert!(!tm.link_session("nope", "s1"));
        tm.kill_all();
    }

    #[tokio::test]
    async fn a_dead_pty_is_not_reused_for_its_session() {
        let tm = TerminalManager::new();
        let mut spec = sh("exit 0");
        spec.session_id = Some("s9".into());
        let id = tm.spawn(spec).unwrap();
        let mut rx = tm.subscribe(&id).unwrap().rx;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(PtyOutput::Exited(_)) = rx.recv().await {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(tm.find_by_session_id("s9"), None);
    }

    /// `kill_all` signals the whole process group, which is only correct if
    /// portable-pty really makes the child its own group leader.
    #[tokio::test]
    async fn the_child_is_its_own_process_group_leader_and_kill_all_ends_it() {
        let tm = TerminalManager::new();
        let id = tm.spawn(sh("sleep 30")).unwrap();
        let pid = tm.info(&id).unwrap().pid.expect("pid") as i32;
        assert_eq!(unsafe { libc::getpgid(pid) }, pid, "child should lead its own process group");
        let mut rx = tm.subscribe(&id).unwrap().rx;
        tm.kill_all();
        let code = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(PtyOutput::Exited(c)) = rx.recv().await {
                    break c;
                }
            }
        })
        .await
        .expect("process should have been killed");
        assert_ne!(code, Some(0));
    }
}
