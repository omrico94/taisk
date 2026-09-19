//! Tasks: the Kanban redesign's unit of work. A task moves through a
//! workflow (`Stage`) and groups many sessions. Which session belongs to
//! which task is an explicit, user-made mapping (drag or the assign menu) —
//! never inferred, since a wrong auto-guess would be confusing. A session
//! with no mapping is "unassigned" and lives in the board's tray.
//!
//! State is durable in a flat `tasks.json` (same atomic-write pattern as
//! `EndedSessions`/`DismissedSessions`) and owned by `TaskHub`, which also
//! broadcasts a full `TasksSnapshot` on every change (tiny payload) so the
//! WS layer can keep every open frontend in sync.
//!
//! Rollup ("the task follows its sessions") is backend-owned and
//! edge-triggered: a task moves to `Done` at the moment its sessions *become*
//! all settled, and back to `InProgress` at the moment a settled task gets a
//! live session again. Edge-triggering (rather than re-asserting "all done ⇒
//! Done" on every diff) keeps a manual drag out of `Done` from bouncing back.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex};

use crate::engine::{EngineHandle, SessionId, SessionView};
use crate::state::SessionState;

pub type TaskId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    Backlog,
    Todo,
    InProgress,
    Done,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub title: String,
    pub stage: Stage,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TasksSnapshot {
    pub tasks: Vec<Task>,
    /// session id → task id. Absent = unassigned.
    pub assignments: HashMap<SessionId, TaskId>,
}

pub struct TaskStore {
    path: PathBuf,
    data: TasksSnapshot,
    /// Last observed "all assigned sessions settled" per task, for the
    /// edge-triggered rollup. Deliberately not persisted (see `rollup`).
    settled: HashMap<TaskId, bool>,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn new_task_id() -> TaskId {
    format!("t{:x}{:x}", crate::now_ms(), NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

fn is_settled(state: SessionState) -> bool {
    matches!(state, SessionState::Done | SessionState::Idle)
}

impl TaskStore {
    pub fn load(path: &Path) -> Self {
        let data = std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        Self { path: path.to_path_buf(), data, settled: HashMap::new() }
    }

    pub fn snapshot(&self) -> TasksSnapshot {
        self.data.clone()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let pretty = serde_json::to_string_pretty(&self.data)?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, pretty)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn create(&mut self, title: &str, stage: Stage) -> Option<Task> {
        let title = title.trim();
        if title.is_empty() {
            return None;
        }
        let task = Task { id: new_task_id(), title: title.to_string(), stage, created_at_ms: crate::now_ms() };
        self.data.tasks.push(task.clone());
        Some(task)
    }

    /// Returns whether the task exists. `None` fields are left unchanged; a
    /// blank title is ignored rather than erasing the task's name.
    pub fn update(&mut self, id: &str, title: Option<&str>, stage: Option<Stage>) -> bool {
        let Some(task) = self.data.tasks.iter_mut().find(|t| t.id == id) else {
            return false;
        };
        if let Some(t) = title.map(str::trim).filter(|t| !t.is_empty()) {
            task.title = t.to_string();
        }
        if let Some(s) = stage {
            task.stage = s;
        }
        true
    }

    /// Its sessions simply become unassigned (assignments are dropped).
    pub fn delete(&mut self, id: &str) -> bool {
        let before = self.data.tasks.len();
        self.data.tasks.retain(|t| t.id != id);
        self.data.assignments.retain(|_, task_id| task_id != id);
        self.settled.remove(id);
        self.data.tasks.len() != before
    }

    /// `task_id: None` unassigns. Returns false for an unknown task id.
    pub fn assign(&mut self, session_id: &str, task_id: Option<&str>) -> bool {
        match task_id {
            None => {
                self.data.assignments.remove(session_id);
                true
            }
            Some(tid) => {
                if !self.data.tasks.iter().any(|t| t.id == tid) {
                    return false;
                }
                self.data.assignments.insert(session_id.to_string(), tid.to_string());
                true
            }
        }
    }

    pub fn unassign_session(&mut self, session_id: &str) -> bool {
        self.data.assignments.remove(session_id).is_some()
    }

    /// Applies the rollup rule against the current live sessions. Returns
    /// whether any task's stage changed.
    ///
    /// A task with no live member sessions has no rollup signal (its cached
    /// state is forgotten), so after a restart — when reconstruction
    /// re-adds sessions gradually — the first observation of a task only
    /// *records* its state and never moves it, except that a `Done` task
    /// which turns out to have a live session is pulled back to `InProgress`.
    pub fn rollup(&mut self, sessions: &[SessionView]) -> bool {
        let by_id: HashMap<&str, &SessionView> = sessions.iter().map(|s| (s.id.as_str(), s)).collect();
        let mut changed = false;
        for task in &mut self.data.tasks {
            let members: Vec<&SessionView> = self
                .data
                .assignments
                .iter()
                .filter(|(_, tid)| **tid == task.id)
                .filter_map(|(sid, _)| by_id.get(sid.as_str()).copied())
                .collect();
            if members.is_empty() {
                self.settled.remove(&task.id);
                continue;
            }
            let settled = members.iter().all(|s| is_settled(s.state));
            let prev = self.settled.insert(task.id.clone(), settled);
            let target = match prev {
                Some(p) if p != settled => {
                    if settled && task.stage != Stage::Done {
                        Some(Stage::Done)
                    } else if !settled && task.stage == Stage::Done {
                        Some(Stage::InProgress)
                    } else {
                        None
                    }
                }
                None if !settled && task.stage == Stage::Done => Some(Stage::InProgress),
                _ => None,
            };
            if let Some(stage) = target {
                task.stage = stage;
                changed = true;
            }
        }
        changed
    }
}

/// Shared handle: the store plus its change broadcast. Cheap to clone.
#[derive(Clone)]
pub struct TaskHub {
    store: Arc<Mutex<TaskStore>>,
    tx: broadcast::Sender<TasksSnapshot>,
}

impl TaskHub {
    pub fn new(store: TaskStore) -> Self {
        let (tx, _) = broadcast::channel(64);
        Self { store: Arc::new(Mutex::new(store)), tx }
    }

    pub fn load(path: &Path) -> Self {
        Self::new(TaskStore::load(path))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TasksSnapshot> {
        self.tx.subscribe()
    }

    pub async fn snapshot(&self) -> TasksSnapshot {
        self.store.lock().await.snapshot()
    }

    /// Runs `f` on the store; if it reports a change, persists and
    /// broadcasts the new snapshot. Returns `f`'s result.
    async fn mutate<R>(&self, f: impl FnOnce(&mut TaskStore) -> (R, bool)) -> R {
        let mut store = self.store.lock().await;
        let (result, changed) = f(&mut store);
        if changed {
            let _ = store.save();
            let _ = self.tx.send(store.snapshot());
        }
        result
    }

    pub async fn create(&self, title: &str, stage: Stage) -> Option<Task> {
        self.mutate(|s| {
            let t = s.create(title, stage);
            let changed = t.is_some();
            (t, changed)
        })
        .await
    }

    pub async fn update(&self, id: &str, title: Option<&str>, stage: Option<Stage>) -> bool {
        self.mutate(|s| {
            let ok = s.update(id, title, stage);
            (ok, ok)
        })
        .await
    }

    pub async fn delete(&self, id: &str) -> bool {
        self.mutate(|s| {
            let ok = s.delete(id);
            (ok, ok)
        })
        .await
    }

    /// Assigns/unassigns, then re-runs the rollup against `sessions` so
    /// (for example) dropping a live session onto a Done task pulls it back
    /// to In Progress in the same broadcast.
    pub async fn assign(&self, session_id: &str, task_id: Option<&str>, sessions: &[SessionView]) -> bool {
        self.mutate(|s| {
            let ok = s.assign(session_id, task_id);
            if ok {
                s.rollup(sessions);
            }
            (ok, ok)
        })
        .await
    }

    /// Drops any assignment for a deleted session.
    pub async fn forget_session(&self, session_id: &str) {
        self.mutate(|s| {
            let changed = s.unassign_session(session_id);
            ((), changed)
        })
        .await
    }

    pub async fn rollup(&self, sessions: &[SessionView]) {
        self.mutate(|s| {
            let changed = s.rollup(sessions);
            ((), changed)
        })
        .await
    }
}

/// Keeps task stages following their sessions: on every engine diff, re-runs
/// the rollup against a fresh snapshot. Spawned once from `bootstrap::start`.
pub async fn run_rollup(hub: TaskHub, engine: EngineHandle) {
    let mut rx = engine.subscribe();
    loop {
        match rx.recv().await {
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {
                let sessions = engine.snapshot().await;
                hub.rollup(&sessions).await;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, state: SessionState) -> SessionView {
        let mut s = SessionView::new_starting(id.into(), "proj".into(), "/tmp".into(), "cli".into(), 0);
        s.state = state;
        s
    }

    fn store() -> (TaskStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (TaskStore::load(&dir.path().join("tasks.json")), dir)
    }

    #[test]
    fn create_trims_and_rejects_blank_titles() {
        let (mut s, _d) = store();
        assert!(s.create("   ", Stage::Todo).is_none());
        let t = s.create("  Ship auth  ", Stage::Todo).unwrap();
        assert_eq!(t.title, "Ship auth");
        assert_eq!(s.snapshot().tasks.len(), 1);
    }

    #[test]
    fn persists_and_reloads_tasks_and_assignments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        let mut s = TaskStore::load(&path);
        let t = s.create("A", Stage::InProgress).unwrap();
        assert!(s.assign("s1", Some(&t.id)));
        s.save().unwrap();

        let reloaded = TaskStore::load(&path).snapshot();
        assert_eq!(reloaded.tasks[0].title, "A");
        assert_eq!(reloaded.tasks[0].stage, Stage::InProgress);
        assert_eq!(reloaded.assignments.get("s1"), Some(&t.id));
    }

    #[test]
    fn assign_to_unknown_task_is_rejected_and_none_unassigns() {
        let (mut s, _d) = store();
        assert!(!s.assign("s1", Some("nope")));
        let t = s.create("A", Stage::Todo).unwrap();
        assert!(s.assign("s1", Some(&t.id)));
        assert!(s.assign("s1", None));
        assert!(s.snapshot().assignments.is_empty());
    }

    #[test]
    fn deleting_a_task_orphans_its_sessions() {
        let (mut s, _d) = store();
        let a = s.create("A", Stage::Todo).unwrap();
        let b = s.create("B", Stage::Todo).unwrap();
        s.assign("s1", Some(&a.id));
        s.assign("s2", Some(&b.id));
        assert!(s.delete(&a.id));
        let snap = s.snapshot();
        assert_eq!(snap.tasks.len(), 1);
        assert!(!snap.assignments.contains_key("s1"));
        assert!(snap.assignments.contains_key("s2"));
    }

    #[test]
    fn update_changes_stage_and_ignores_blank_title() {
        let (mut s, _d) = store();
        let t = s.create("A", Stage::Todo).unwrap();
        assert!(s.update(&t.id, Some("  "), Some(Stage::Done)));
        let snap = s.snapshot();
        assert_eq!(snap.tasks[0].title, "A");
        assert_eq!(snap.tasks[0].stage, Stage::Done);
        assert!(!s.update("missing", None, Some(Stage::Done)));
    }

    #[test]
    fn rollup_moves_task_to_done_when_all_sessions_settle_and_back_when_revived() {
        let (mut s, _d) = store();
        let t = s.create("A", Stage::InProgress).unwrap();
        s.assign("s1", Some(&t.id));
        s.assign("s2", Some(&t.id));

        // First observation only records.
        assert!(!s.rollup(&[session("s1", SessionState::Working), session("s2", SessionState::Done)]));
        assert_eq!(s.snapshot().tasks[0].stage, Stage::InProgress);

        // Working -> Done edge: task advances.
        assert!(s.rollup(&[session("s1", SessionState::Done), session("s2", SessionState::Done)]));
        assert_eq!(s.snapshot().tasks[0].stage, Stage::Done);

        // Idle counts as settled: no change.
        assert!(!s.rollup(&[session("s1", SessionState::Idle), session("s2", SessionState::Done)]));

        // A settled Done task gets a live session again: pulled back.
        assert!(s.rollup(&[session("s1", SessionState::Working), session("s2", SessionState::Done)]));
        assert_eq!(s.snapshot().tasks[0].stage, Stage::InProgress);
    }

    #[test]
    fn rollup_waiting_counts_as_not_settled() {
        let (mut s, _d) = store();
        let t = s.create("A", Stage::Done).unwrap();
        s.assign("s1", Some(&t.id));
        s.rollup(&[session("s1", SessionState::Done)]);
        assert!(s.rollup(&[session("s1", SessionState::Waiting)]));
        assert_eq!(s.snapshot().tasks[0].stage, Stage::InProgress);
    }

    #[test]
    fn a_manual_drag_out_of_done_is_not_bounced_back() {
        let (mut s, _d) = store();
        let t = s.create("A", Stage::InProgress).unwrap();
        s.assign("s1", Some(&t.id));
        s.rollup(&[session("s1", SessionState::Working)]);
        s.rollup(&[session("s1", SessionState::Done)]);
        assert_eq!(s.snapshot().tasks[0].stage, Stage::Done);

        s.update(&t.id, None, Some(Stage::Todo));
        // Unrelated diffs with the session still Done must not re-advance.
        assert!(!s.rollup(&[session("s1", SessionState::Done)]));
        assert_eq!(s.snapshot().tasks[0].stage, Stage::Todo);
    }

    #[test]
    fn done_task_found_with_a_live_session_on_first_observation_is_pulled_back() {
        let (mut s, _d) = store();
        let t = s.create("A", Stage::Done).unwrap();
        s.assign("s1", Some(&t.id));
        assert!(s.rollup(&[session("s1", SessionState::Working)]));
        assert_eq!(s.snapshot().tasks[0].stage, Stage::InProgress);
    }

    #[test]
    fn task_without_live_sessions_is_left_alone() {
        let (mut s, _d) = store();
        let t = s.create("A", Stage::Todo).unwrap();
        s.assign("gone", Some(&t.id));
        assert!(!s.rollup(&[]));
        assert_eq!(s.snapshot().tasks[0].stage, Stage::Todo);
    }

    #[tokio::test]
    async fn hub_broadcasts_a_snapshot_on_change_only() {
        let dir = tempfile::tempdir().unwrap();
        let hub = TaskHub::load(&dir.path().join("tasks.json"));
        let mut rx = hub.subscribe();
        assert!(hub.create("", Stage::Todo).await.is_none());
        assert!(rx.try_recv().is_err());
        let t = hub.create("A", Stage::Todo).await.unwrap();
        assert_eq!(rx.recv().await.unwrap().tasks[0].id, t.id);
    }
}
