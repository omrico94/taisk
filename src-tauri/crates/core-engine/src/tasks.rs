//! Tasks: the Kanban redesign's unit of work. A task moves through a
//! workflow of columns (`Stage` is a column id) and groups many sessions.
//! Which session belongs to which task is an explicit, user-made mapping
//! (drag or the assign menu) — never inferred, since a wrong auto-guess would
//! be confusing. A session with no mapping is "unassigned" and lives in the
//! board's tray.
//!
//! Columns are user-editable per board (`BoardColumns`): add, rename,
//! recolor, reorder, delete. A board with no stored layout uses
//! the four defaults, whose ids (`backlog`/`todo`/`inprogress`/`done`) are
//! exactly what the old fixed `Stage` enum serialized to — so `tasks.json`
//! files from before columns were editable load with no migration.
//!
//! State is durable in a flat `tasks.json` (same atomic-write pattern as
//! `EndedSessions`/`DismissedSessions`) and owned by `TaskHub`, which also
//! broadcasts a full `TasksSnapshot` on every change (tiny payload) so the
//! WS layer can keep every open frontend in sync.
//!
//! A task's stage is changed only by the user (drag or menu) — session state
//! never moves a task between columns. Columns carry display-only roles
//! (`done` dims its cards, `active` gets the strong accent, `intake` is where
//! quick-add files new tasks).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex};

use crate::engine::SessionId;
use crate::trackers::{TicketRef, TicketState};

pub type TaskId = String;

/// Id of a column on the task's board (see `Column`).
pub type Stage = String;

/// Ids of the default columns.
pub mod stage {
    pub const BACKLOG: &str = "backlog";
    pub const TODO: &str = "todo";
    pub const IN_PROGRESS: &str = "inprogress";
    pub const DONE: &str = "done";
}

/// Column ids that would shadow the API's `/boards/{id}/columns/{order,roles}`
/// routes (a column named "Order" gets `order-2`).
const RESERVED_COLUMN_IDS: [&str; 2] = ["order", "roles"];

/// Longest column name accepted, in characters.
const MAX_COLUMN_NAME: usize = 40;

/// Colors handed to new columns (first one not already used on the board).
const PALETTE: [&str; 8] = ["#A08FC4", "#6FB9C9", "#C8FF3D", "#86B98C", "#E0A458", "#E07A8B", "#7C9CE0", "#C9B26F"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Column {
    /// Stable across renames; what `Task::stage` stores.
    pub id: String,
    pub name: String,
    /// `#RRGGBB` (the frontend appends alpha bytes to it).
    pub color: String,
}

/// One board's ordered columns plus the column ids that carry a role.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardColumns {
    pub columns: Vec<Column>,
    /// Cards here read as finished (dimmed). Display only: nothing moves a
    /// task into or out of it automatically.
    #[serde(default)]
    pub done: Option<String>,
    /// Gets the strong accent (In Progress by default). Display only.
    #[serde(default)]
    pub active: Option<String>,
    /// Where quick-add (⌥⌘N) files new tasks. `None` means the first column.
    #[serde(default)]
    pub intake: Option<String>,
}

impl Default for BoardColumns {
    fn default() -> Self {
        let col = |id: &str, name: &str, color: &str| Column { id: id.into(), name: name.into(), color: color.into() };
        Self {
            columns: vec![
                col(stage::BACKLOG, "Backlog", "#A08FC4"),
                col(stage::TODO, "To Do", "#6FB9C9"),
                col(stage::IN_PROGRESS, "In Progress", "#C8FF3D"),
                col(stage::DONE, "Done", "#86B98C"),
            ],
            done: Some(stage::DONE.into()),
            active: Some(stage::IN_PROGRESS.into()),
            intake: Some(stage::TODO.into()),
        }
    }
}

impl BoardColumns {
    pub fn has(&self, id: &str) -> bool {
        self.columns.iter().any(|c| c.id == id)
    }

    /// Where quick-add files new tasks: the intake column, else the first.
    pub fn intake_or_first(&self) -> &str {
        self.intake.as_deref().unwrap_or(&self.columns[0].id)
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut Column> {
        self.columns.iter_mut().find(|c| c.id == id)
    }

    /// Slug of `name`, suffixed until it's unique on this board.
    fn fresh_id(&self, name: &str) -> String {
        let base = match crate::boards::slugify(name) {
            s if s.is_empty() => "column".to_string(),
            s => s,
        };
        let mut id = base.clone();
        let mut n = 2;
        while self.has(&id) || RESERVED_COLUMN_IDS.contains(&id.as_str()) {
            id = format!("{base}-{n}");
            n += 1;
        }
        id
    }

    fn unused_color(&self) -> String {
        let used: HashSet<String> = self.columns.iter().map(|c| c.color.to_ascii_uppercase()).collect();
        PALETTE
            .iter()
            .find(|c| !used.contains(**c))
            .unwrap_or(&PALETTE[self.columns.len() % PALETTE.len()])
            .to_string()
    }

    fn roles_mut(&mut self) -> [&mut Option<String>; 3] {
        [&mut self.done, &mut self.active, &mut self.intake]
    }
}

fn clean_name(name: &str) -> Result<String, TaskError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(TaskError::Invalid("column name can't be blank".into()));
    }
    Ok(name.chars().take(MAX_COLUMN_NAME).collect())
}

fn clean_color(color: &str) -> Result<String, TaskError> {
    let c = color.trim();
    let ok = c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|ch| ch.is_ascii_hexdigit());
    if ok {
        Ok(c.to_ascii_uppercase())
    } else {
        Err(TaskError::Invalid(format!("color must look like #RRGGBB, got {color:?}")))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskError {
    NotFound,
    Invalid(String),
}

impl std::fmt::Display for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskError::NotFound => f.write_str("not found"),
            TaskError::Invalid(msg) => f.write_str(msg),
        }
    }
}

/// Partial edit of a column.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct ColumnPatch {
    pub name: Option<String>,
    pub color: Option<String>,
}

/// Partial edit of a board's roles. `Some(None)` unsets a role; an absent
/// field leaves it alone.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct RolesPatch {
    #[serde(default, deserialize_with = "present")]
    pub done: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    pub active: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    pub intake: Option<Option<String>>,
}

/// Tells an explicit JSON `null` (→ `Some(None)`) apart from an absent field
/// (→ `None`, via `#[serde(default)]`).
fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
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
    /// The tracker ticket this task was imported from (see `trackers`).
    /// Tasks saved before trackers existed deserialize to `None`.
    #[serde(default)]
    pub ticket: Option<TicketRef>,
}

fn default_board() -> String {
    crate::boards::DEFAULT_BOARD_ID.to_string()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TasksSnapshot {
    pub tasks: Vec<Task>,
    /// session id → task id. Absent = unassigned.
    pub assignments: HashMap<SessionId, TaskId>,
    /// board id → its column layout. A board absent here uses
    /// `default_columns`; it gets its own entry on its first column edit.
    #[serde(default)]
    pub columns: HashMap<String, BoardColumns>,
    /// Layout of any board absent from `columns`, sent so the frontend never
    /// keeps its own copy. Never read back from disk.
    #[serde(default, skip_deserializing)]
    pub default_columns: BoardColumns,
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
        let mut store = Self { path: path.to_path_buf(), data };
        store.normalize();
        store
    }

    /// Repairs a hand-edited or partially written file: a task whose column
    /// no longer exists moves to its board's first column (it would
    /// otherwise be invisible), and a role naming a missing column is unset.
    fn normalize(&mut self) {
        let TasksSnapshot { tasks, columns, default_columns, .. } = &mut self.data;
        for layout in columns.values_mut() {
            if layout.columns.is_empty() {
                *layout = BoardColumns::default();
            }
            let ids: HashSet<String> = layout.columns.iter().map(|c| c.id.clone()).collect();
            for role in layout.roles_mut() {
                if role.as_ref().is_some_and(|r| !ids.contains(r)) {
                    *role = None;
                }
            }
        }
        for task in tasks.iter_mut() {
            let layout = columns.get(&task.board).unwrap_or(default_columns);
            if !layout.has(&task.stage) {
                task.stage = layout.columns[0].id.clone();
            }
        }
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

    /// `board`'s layout (the defaults if it has never been edited).
    pub fn columns_of(&self, board: &str) -> &BoardColumns {
        self.data.columns.get(board).unwrap_or(&self.data.default_columns)
    }

    /// `board`'s layout, materialized from the defaults on first edit.
    fn columns_of_mut(&mut self, board: &str) -> &mut BoardColumns {
        self.data.columns.entry(board.to_string()).or_default()
    }

    pub fn create(&mut self, title: &str, stage: &str) -> Option<Task> {
        self.create_on_board(title, stage, crate::boards::DEFAULT_BOARD_ID)
    }

    /// `None` for a blank title or a column that isn't on `board`.
    pub fn create_on_board(&mut self, title: &str, stage: &str, board: &str) -> Option<Task> {
        let title = title.trim();
        if title.is_empty() || !self.columns_of(board).has(stage) {
            return None;
        }
        let task = Task {
            id: new_task_id(),
            title: title.to_string(),
            stage: stage.to_string(),
            created_at_ms: crate::now_ms(),
            board: board.to_string(),
            directories: vec![],
            ticket: None,
        };
        self.data.tasks.push(task.clone());
        Some(task)
    }

    /// Idempotent per `(board, provider, key)`: importing a ticket that
    /// already has a task on this board returns that task (`false` = no
    /// change) instead of making a duplicate. `stage: None` files it in the
    /// board's intake column; a column that isn't on `board` is `None`.
    pub fn create_from_ticket(&mut self, ticket: TicketRef, board: &str, stage: Option<&str>) -> Option<(Task, bool)> {
        let existing = self.data.tasks.iter().find(|t| {
            t.board == board && t.ticket.as_ref().is_some_and(|r| r.provider == ticket.provider && r.key == ticket.key)
        });
        if let Some(t) = existing {
            return Some((t.clone(), false));
        }
        let columns = self.columns_of(board);
        let stage = stage.unwrap_or(columns.intake_or_first()).to_string();
        if !columns.has(&stage) {
            return None;
        }
        let title = match ticket.title.trim() {
            "" => ticket.key.clone(),
            t => t.to_string(),
        };
        let task = Task {
            id: new_task_id(),
            title,
            stage,
            created_at_ms: crate::now_ms(),
            board: board.to_string(),
            directories: vec![],
            ticket: Some(ticket),
        };
        self.data.tasks.push(task.clone());
        Some((task, true))
    }

    /// Updates the state on every task linked to `(provider, key)` (it may
    /// be imported on several boards). Returns whether anything changed.
    pub fn set_ticket_state(&mut self, provider: &str, key: &str, state: TicketState) -> bool {
        let mut changed = false;
        for r in self.data.tasks.iter_mut().filter_map(|t| t.ticket.as_mut()) {
            if r.provider == provider && r.key == key && r.state != state {
                r.state = state;
                changed = true;
            }
        }
        changed
    }

    /// `None` fields are left unchanged; a blank title is ignored rather than
    /// erasing the task's name. A stage must be a column on the task's board.
    pub fn update(&mut self, id: &str, title: Option<&str>, stage: Option<&str>) -> Result<(), TaskError> {
        let board = self.data.tasks.iter().find(|t| t.id == id).ok_or(TaskError::NotFound)?.board.clone();
        if let Some(s) = stage {
            if !self.columns_of(&board).has(s) {
                return Err(TaskError::Invalid(format!("no column {s:?} on board {board:?}")));
            }
        }
        let task = self.data.tasks.iter_mut().find(|t| t.id == id).ok_or(TaskError::NotFound)?;
        if let Some(t) = title.map(str::trim).filter(|t| !t.is_empty()) {
            task.title = t.to_string();
        }
        if let Some(s) = stage {
            task.stage = s.to_string();
        }
        Ok(())
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

    /// Drops every task on `board`, the assignments pointing at them, and its
    /// column layout (board removed). Returns whether anything changed.
    pub fn delete_board(&mut self, board: &str) -> bool {
        let gone: Vec<TaskId> = self.data.tasks.iter().filter(|t| t.board == board).map(|t| t.id.clone()).collect();
        for id in &gone {
            self.delete(id);
        }
        let had_layout = self.data.columns.remove(board).is_some();
        !gone.is_empty() || had_layout
    }

    /// Appends a column to `board`. Color defaults to an unused palette one.
    pub fn add_column(&mut self, board: &str, name: &str, color: Option<&str>) -> Result<Column, TaskError> {
        let name = clean_name(name)?;
        let color = color.map(clean_color).transpose()?;
        let layout = self.columns_of_mut(board);
        let column = Column {
            id: layout.fresh_id(&name),
            color: color.unwrap_or_else(|| layout.unused_color()),
            name,
        };
        layout.columns.push(column.clone());
        Ok(column)
    }

    pub fn update_column(&mut self, board: &str, id: &str, patch: &ColumnPatch) -> Result<Column, TaskError> {
        if !self.columns_of(board).has(id) {
            return Err(TaskError::NotFound);
        }
        let name = patch.name.as_deref().map(clean_name).transpose()?;
        let color = patch.color.as_deref().map(clean_color).transpose()?;
        let column = self.columns_of_mut(board).get_mut(id).ok_or(TaskError::NotFound)?;
        if let Some(n) = name {
            column.name = n;
        }
        if let Some(c) = color {
            column.color = c;
        }
        Ok(column.clone())
    }

    /// `ids` must be exactly the board's column ids, in their new order.
    pub fn reorder_columns(&mut self, board: &str, ids: &[String]) -> Result<(), TaskError> {
        let layout = self.columns_of(board);
        let current: HashSet<&str> = layout.columns.iter().map(|c| c.id.as_str()).collect();
        let wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
        if ids.len() != layout.columns.len() || current != wanted {
            return Err(TaskError::Invalid("order must list every column exactly once".into()));
        }
        let layout = self.columns_of_mut(board);
        layout.columns.sort_by_key(|c| ids.iter().position(|i| *i == c.id));
        Ok(())
    }

    pub fn set_roles(&mut self, board: &str, patch: &RolesPatch) -> Result<(), TaskError> {
        let layout = self.columns_of(board);
        for role in [&patch.done, &patch.active, &patch.intake] {
            if let Some(Some(id)) = role {
                if !layout.has(id) {
                    return Err(TaskError::Invalid(format!("no column {id:?} on board {board:?}")));
                }
            }
        }
        let layout = self.columns_of_mut(board);
        for (role, change) in layout.roles_mut().into_iter().zip([&patch.done, &patch.active, &patch.intake]) {
            if let Some(value) = change {
                *role = value.clone();
            }
        }
        Ok(())
    }

    /// Deletes a column, moving its tasks to `move_to` (another column on the
    /// same board). The last column can't be deleted. Any role the column
    /// carried is unset rather than guessed. Returns how many tasks moved.
    pub fn delete_column(&mut self, board: &str, id: &str, move_to: &str) -> Result<usize, TaskError> {
        let layout = self.columns_of(board);
        if !layout.has(id) {
            return Err(TaskError::NotFound);
        }
        if layout.columns.len() == 1 {
            return Err(TaskError::Invalid("a board needs at least one column".into()));
        }
        if move_to == id || !layout.has(move_to) {
            return Err(TaskError::Invalid(format!("can't move tasks to {move_to:?}")));
        }
        let layout = self.columns_of_mut(board);
        layout.columns.retain(|c| c.id != id);
        for role in layout.roles_mut() {
            if role.as_deref() == Some(id) {
                *role = None;
            }
        }
        let mut moved = 0;
        for task in self.data.tasks.iter_mut().filter(|t| t.board == board && t.stage == id) {
            task.stage = move_to.to_string();
            moved += 1;
        }
        Ok(moved)
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

    /// `mutate` for fallible edits: changed iff `f` succeeded.
    async fn try_mutate<T>(&self, f: impl FnOnce(&mut TaskStore) -> Result<T, TaskError>) -> Result<T, TaskError> {
        self.mutate(|s| {
            let r = f(s);
            let ok = r.is_ok();
            (r, ok)
        })
        .await
    }

    pub async fn create(&self, title: &str, stage: &str) -> Option<Task> {
        self.mutate(|s| {
            let t = s.create(title, stage);
            let changed = t.is_some();
            (t, changed)
        })
        .await
    }

    /// `stage: None` files the task in the board's intake column.
    pub async fn create_on_board(&self, title: &str, stage: Option<&str>, board: &str) -> Option<Task> {
        self.mutate(|s| {
            let stage = stage.map(str::to_string).unwrap_or_else(|| s.columns_of(board).intake_or_first().to_string());
            let t = s.create_on_board(title, &stage, board);
            let changed = t.is_some();
            (t, changed)
        })
        .await
    }

    /// See `TaskStore::create_from_ticket`; `None` for a column not on `board`.
    pub async fn create_from_ticket(&self, ticket: TicketRef, board: &str, stage: Option<&str>) -> Option<Task> {
        self.mutate(|s| match s.create_from_ticket(ticket, board, stage) {
            Some((task, created)) => (Some(task), created),
            None => (None, false),
        })
        .await
    }

    /// `(provider, key, state)` triples, applied as one change/broadcast.
    pub async fn set_ticket_states(&self, updates: &[(String, String, TicketState)]) {
        self.mutate(|s| {
            let mut changed = false;
            for (provider, key, state) in updates {
                changed |= s.set_ticket_state(provider, key, *state);
            }
            ((), changed)
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

    pub async fn update(&self, id: &str, title: Option<&str>, stage: Option<&str>) -> Result<(), TaskError> {
        self.try_mutate(|s| s.update(id, title, stage)).await
    }

    pub async fn add_column(&self, board: &str, name: &str, color: Option<&str>) -> Result<Column, TaskError> {
        self.try_mutate(|s| s.add_column(board, name, color)).await
    }

    pub async fn update_column(&self, board: &str, id: &str, patch: &ColumnPatch) -> Result<Column, TaskError> {
        self.try_mutate(|s| s.update_column(board, id, patch)).await
    }

    pub async fn reorder_columns(&self, board: &str, ids: &[String]) -> Result<(), TaskError> {
        self.try_mutate(|s| s.reorder_columns(board, ids)).await
    }

    pub async fn set_roles(&self, board: &str, patch: &RolesPatch) -> Result<(), TaskError> {
        self.try_mutate(|s| s.set_roles(board, patch)).await
    }

    pub async fn delete_column(&self, board: &str, id: &str, move_to: &str) -> Result<usize, TaskError> {
        self.try_mutate(|s| s.delete_column(board, id, move_to)).await
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
    use super::stage::{BACKLOG, DONE, IN_PROGRESS, TODO};

    fn store() -> (TaskStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (TaskStore::load(&dir.path().join("tasks.json")), dir)
    }

    #[test]
    fn create_trims_and_rejects_blank_titles() {
        let (mut s, _d) = store();
        assert!(s.create("   ", TODO).is_none());
        let t = s.create("  Ship auth  ", TODO).unwrap();
        assert_eq!(t.title, "Ship auth");
        assert_eq!(s.snapshot().tasks.len(), 1);
    }

    #[test]
    fn persists_and_reloads_tasks_and_assignments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        let mut s = TaskStore::load(&path);
        let t = s.create("A", IN_PROGRESS).unwrap();
        assert!(s.assign("s1", Some(&t.id)));
        s.save().unwrap();

        let reloaded = TaskStore::load(&path).snapshot();
        assert_eq!(reloaded.tasks[0].title, "A");
        assert_eq!(reloaded.tasks[0].stage, IN_PROGRESS);
        assert_eq!(reloaded.assignments.get("s1"), Some(&t.id));
    }

    #[test]
    fn assign_to_unknown_task_is_rejected_and_none_unassigns() {
        let (mut s, _d) = store();
        assert!(!s.assign("s1", Some("nope")));
        let t = s.create("A", TODO).unwrap();
        assert!(s.assign("s1", Some(&t.id)));
        assert!(s.assign("s1", None));
        assert!(s.snapshot().assignments.is_empty());
    }

    #[test]
    fn deleting_a_task_orphans_its_sessions() {
        let (mut s, _d) = store();
        let a = s.create("A", TODO).unwrap();
        let b = s.create("B", TODO).unwrap();
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
        let t = s.create("A", TODO).unwrap();
        assert_eq!(s.update(&t.id, Some("  "), Some(DONE)), Ok(()));
        let snap = s.snapshot();
        assert_eq!(snap.tasks[0].title, "A");
        assert_eq!(snap.tasks[0].stage, DONE);
        assert_eq!(s.update("missing", None, Some(DONE)), Err(TaskError::NotFound));
        assert!(matches!(s.update(&t.id, None, Some("nope")), Err(TaskError::Invalid(_))));
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
        let t = s.create("A", TODO).unwrap();
        assert!(s.assign("s1", Some(&t.id)));
        assert!(s.assign("s2", Some(&t.id)));
        assert_eq!(s.snapshot().tasks[0].stage, TODO);
    }

    fn ticket(key: &str) -> TicketRef {
        TicketRef { provider: "github".into(), key: key.into(), title: "Fix login".into(), url: "u".into(), state: TicketState::Open }
    }

    #[test]
    fn tasks_saved_before_trackers_existed_still_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        std::fs::write(&path, r#"{"tasks":[{"id":"t1","title":"A","stage":"todo","created_at_ms":0}],"assignments":{}}"#).unwrap();
        let snap = TaskStore::load(&path).snapshot();
        assert_eq!(snap.tasks[0].ticket, None);
        assert_eq!(snap.tasks[0].board, crate::boards::DEFAULT_BOARD_ID);
    }

    #[test]
    fn importing_a_ticket_is_idempotent_per_board() {
        let (mut s, _d) = store();
        let (a, created) = s.create_from_ticket(ticket("o/r#1"), "default", Some(stage::TODO)).unwrap();
        assert!(created);
        assert_eq!((a.title.as_str(), a.stage.as_str()), ("Fix login", stage::TODO));
        let (again, created) = s.create_from_ticket(ticket("o/r#1"), "default", Some(stage::BACKLOG)).unwrap();
        assert!(!created);
        assert_eq!(again.id, a.id);
        // No stage → the board's intake column; an unknown column is refused.
        let (other_board, created) = s.create_from_ticket(ticket("o/r#1"), "work", None).unwrap();
        assert!(created && other_board.id != a.id);
        assert_eq!(other_board.stage, s.columns_of("work").intake_or_first());
        assert!(s.create_from_ticket(ticket("o/r#2"), "default", Some("nope")).is_none());

        // State follows the ticket on every board it was imported to, once.
        assert!(s.set_ticket_state("github", "o/r#1", TicketState::Closed));
        assert!(!s.set_ticket_state("github", "o/r#1", TicketState::Closed));
        assert!(s.snapshot().tasks.iter().filter_map(|t| t.ticket.as_ref()).all(|t| t.state == TicketState::Closed));
    }

    #[tokio::test]
    async fn hub_broadcasts_a_snapshot_on_change_only() {
        let dir = tempfile::tempdir().unwrap();
        let hub = TaskHub::load(&dir.path().join("tasks.json"));
        let mut rx = hub.subscribe();
        assert!(hub.create("", TODO).await.is_none());
        assert!(rx.try_recv().is_err());
        let t = hub.create("A", TODO).await.unwrap();
        assert_eq!(rx.recv().await.unwrap().tasks[0].id, t.id);
    }

    fn ids(s: &TaskStore, board: &str) -> Vec<String> {
        s.columns_of(board).columns.iter().map(|c| c.id.clone()).collect()
    }

    #[test]
    fn a_pre_columns_tasks_json_loads_unchanged_onto_the_default_columns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        std::fs::write(
            &path,
            r#"{"tasks":[{"id":"t1","title":"A","stage":"inprogress","created_at_ms":0}],"assignments":{"s1":"t1"}}"#,
        )
        .unwrap();
        let s = TaskStore::load(&path);
        let snap = s.snapshot();
        assert_eq!(snap.tasks[0].stage, IN_PROGRESS);
        assert_eq!(snap.assignments.get("s1").map(String::as_str), Some("t1"));
        assert!(snap.columns.is_empty(), "defaults aren't written until a board is edited");
        assert_eq!(ids(&s, "default"), [BACKLOG, TODO, IN_PROGRESS, DONE]);
    }

    #[test]
    fn a_task_in_a_missing_column_is_moved_to_the_first_one_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        std::fs::write(&path, r#"{"tasks":[{"id":"t1","title":"A","stage":"gone","created_at_ms":0}],"assignments":{}}"#).unwrap();
        assert_eq!(TaskStore::load(&path).snapshot().tasks[0].stage, BACKLOG);
    }

    #[test]
    fn column_edits_are_per_board_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.json");
        let mut s = TaskStore::load(&path);
        let a = s.add_column("work", "QA", None).unwrap();
        let b = s.add_column("work", "QA", None).unwrap();
        assert_eq!((a.id.as_str(), b.id.as_str()), ("qa", "qa-2"), "ids stay unique");
        assert_ne!(a.color, b.color, "new columns get an unused palette color");
        assert_eq!(s.add_column("work", "Order", None).unwrap().id, "order-2", "route names are reserved");
        s.delete_column("work", "order-2", "qa").unwrap();
        assert!(s.add_column("work", "  ", None).is_err());
        assert!(s.add_column("work", "X", Some("red")).is_err());
        assert_eq!(ids(&s, "default").len(), 4, "other boards are untouched");

        let renamed = s.update_column("work", "qa", &ColumnPatch { name: Some("Testing".into()), ..Default::default() }).unwrap();
        assert_eq!((renamed.id.as_str(), renamed.name.as_str()), ("qa", "Testing"), "rename keeps the id");
        assert_eq!(s.update_column("work", "nope", &ColumnPatch::default()), Err(TaskError::NotFound));
        s.save().unwrap();

        let reloaded = TaskStore::load(&path);
        assert_eq!(ids(&reloaded, "work"), [BACKLOG, TODO, IN_PROGRESS, DONE, "qa", "qa-2"]);
        assert_eq!(reloaded.columns_of("work").columns[4].name, "Testing");
    }

    #[test]
    fn reorder_requires_a_permutation() {
        let (mut s, _d) = store();
        let order: Vec<String> = [DONE, IN_PROGRESS, TODO, BACKLOG].map(String::from).into();
        assert!(s.reorder_columns("default", &order[..3]).is_err());
        assert!(s.reorder_columns("default", &[DONE, DONE, TODO, BACKLOG].map(String::from)).is_err());
        s.reorder_columns("default", &order).unwrap();
        assert_eq!(ids(&s, "default"), order);
    }

    #[test]
    fn deleting_a_column_moves_its_tasks_and_unsets_its_roles() {
        let (mut s, _d) = store();
        let t = s.create("A", DONE).unwrap();
        let other = s.create_on_board("B", DONE, "work").unwrap();
        assert!(s.delete_column("default", DONE, DONE).is_err());
        assert!(s.delete_column("default", DONE, "nope").is_err());
        assert_eq!(s.delete_column("default", DONE, TODO), Ok(1));
        let snap = s.snapshot();
        assert_eq!(snap.tasks.iter().find(|x| x.id == t.id).unwrap().stage, TODO);
        assert_eq!(snap.tasks.iter().find(|x| x.id == other.id).unwrap().stage, DONE, "other board untouched");
        assert_eq!(s.columns_of("default").done, None);
        assert!(s.create("C", DONE).is_none(), "can't create into a deleted column");

        for id in [BACKLOG, TODO] {
            s.delete_column("default", id, IN_PROGRESS).unwrap();
        }
        assert_eq!(s.delete_column("default", TODO, IN_PROGRESS), Err(TaskError::NotFound));
        assert!(matches!(s.delete_column("default", IN_PROGRESS, TODO), Err(TaskError::Invalid(_))), "last column stays");
    }

    #[test]
    fn roles_must_name_existing_columns_and_null_unsets() {
        let (mut s, _d) = store();
        let bad = RolesPatch { done: Some(Some("nope".into())), ..Default::default() };
        assert!(s.set_roles("default", &bad).is_err());
        s.set_roles("default", &RolesPatch { active: Some(None), intake: Some(Some(BACKLOG.into())), ..Default::default() }).unwrap();
        let l = s.columns_of("default");
        assert_eq!((l.done.as_deref(), l.active.as_deref(), l.intake.as_deref()), (Some(DONE), None, Some(BACKLOG)));
    }
}
