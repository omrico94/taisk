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
//! A task's stage is changed only by the user (drag or menu) — session state
//! never moves a task between columns.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex};

use crate::engine::SessionId;

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
    /// Board (Claude account) this task lives on. Tasks saved before boards
    /// existed deserialize to the default board.
    #[serde(default = "default_board")]
    pub board: String,
    /// Directories a session started from this task works in: the first is
    /// the new `claude` process's cwd, the rest are passed as `--add-dir`.
    /// Absolute paths; empty means "fall back to the last session's cwd".
    #[serde(default)]
    pub directories: Vec<String>,
}

fn default_board() -> String {
    crate::boards::DEFAULT_BOARD_ID.to_string()
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
}

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn new_task_id() -> TaskId {
    format!("t{:x}{:x}", crate::now_ms(), NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

impl TaskStore {
    pub fn load(path: &Path) -> Self {
        let data = std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        Self { path: path.to_path_buf(), data }
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
        self.create_on_board(title, stage, crate::boards::DEFAULT_BOARD_ID)
    }

    pub fn create_on_board(&mut self, title: &str, stage: Stage, board: &str) -> Option<Task> {
        let title = title.trim();
        if title.is_empty() {
            return None;
        }
        let task = Task { id: new_task_id(), title: title.to_string(), stage, created_at_ms: crate::now_ms(), board: board.to_string(), directories: vec![] };
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

    /// Replaces the task's attached directories. Returns whether the task
    /// exists. Callers validate/normalize the paths (see `api::update_task`).
    pub fn set_directories(&mut self, id: &str, directories: Vec<String>) -> bool {
        let Some(task) = self.data.tasks.iter_mut().find(|t| t.id == id) else {
            return false;
        };
        task.directories = directories;
        true
    }

    /// Its sessions simply become unassigned (assignments are dropped).
    pub fn delete(&mut self, id: &str) -> bool {
        let before = self.data.tasks.len();
        self.data.tasks.retain(|t| t.id != id);
        self.data.assignments.retain(|_, task_id| task_id != id);
        self.data.tasks.len() != before
    }

    /// Drops every task on `board` and the assignments pointing at them
    /// (board removed). Returns whether anything changed.
    pub fn delete_board(&mut self, board: &str) -> bool {
        let gone: Vec<TaskId> = self.data.tasks.iter().filter(|t| t.board == board).map(|t| t.id.clone()).collect();
        for id in &gone {
            self.delete(id);
        }
        !gone.is_empty()
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

    pub async fn create_on_board(&self, title: &str, stage: Stage, board: &str) -> Option<Task> {
        self.mutate(|s| {
            let t = s.create_on_board(title, stage, board);
            let changed = t.is_some();
            (t, changed)
        })
        .await
    }

    pub async fn delete_board(&self, board: &str) -> bool {
        self.mutate(|s| {
            let ok = s.delete_board(board);
            (ok, ok)
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

    pub async fn set_directories(&self, id: &str, directories: Vec<String>) -> bool {
        self.mutate(|s| {
            let ok = s.set_directories(id, directories);
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

    /// Assigns/unassigns. Never changes the task's stage.
    pub async fn assign(&self, session_id: &str, task_id: Option<&str>) -> bool {
        self.mutate(|s| {
            let ok = s.assign(session_id, task_id);
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn directories_persist_and_old_tasks_load_without_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        std::fs::write(&path, r#"{"tasks":[{"id":"t1","title":"A","stage":"todo","created_at_ms":0}],"assignments":{}}"#).unwrap();
        let mut s = TaskStore::load(&path);
        assert!(s.snapshot().tasks[0].directories.is_empty());
        assert!(s.set_directories("t1", vec!["/a".into(), "/b".into()]));
        assert!(!s.set_directories("missing", vec![]));
        s.save().unwrap();
        assert_eq!(TaskStore::load(&path).snapshot().tasks[0].directories, vec!["/a", "/b"]);
    }

    #[test]
    fn assigning_sessions_never_changes_a_tasks_stage() {
        let (mut s, _d) = store();
        let t = s.create("A", Stage::Todo).unwrap();
        assert!(s.assign("s1", Some(&t.id)));
        assert!(s.assign("s2", Some(&t.id)));
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
