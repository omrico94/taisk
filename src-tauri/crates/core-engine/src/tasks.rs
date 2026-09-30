//! Tasks: the Kanban redesign's unit of work. A task moves through a
//! workflow of columns (`Stage` is a column id) and groups many sessions.
//! Which session belongs to which task is an explicit, user-made mapping
//! (drag or the assign menu) — never inferred, since a wrong auto-guess would
//! be confusing. A session with no mapping is "unassigned" and lives in the
//! board's tray.
//!
//! Columns are user-editable per board (`BoardColumns`): add, rename,
//! recolor, WIP limit, reorder, delete. A board with no stored layout uses
//! the four defaults, whose ids (`backlog`/`todo`/`inprogress`/`done`) are
//! exactly what the old fixed `Stage` enum serialized to — so `tasks.json`
//! files from before columns were editable load with no migration.
//!
//! State is durable in a flat `tasks.json` (same atomic-write pattern as
//! `EndedSessions`/`DismissedSessions`) and owned by `TaskHub`, which also
//! broadcasts a full `TasksSnapshot` on every change (tiny payload) so the
//! WS layer can keep every open frontend in sync.
//!
//! Rollup ("the task follows its sessions") is backend-owned and
//! edge-triggered: a task moves to the board's `done` column at the moment
//! its sessions *become* all settled, and back to its `active` column at the
//! moment a settled task gets a live session again. Edge-triggering (rather
//! than re-asserting "all done ⇒ Done" on every diff) keeps a manual drag out
//! of Done from bouncing back. Either role can be unset, which turns that
//! half of the rollup off for the board.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex};

use crate::engine::{EngineHandle, SessionId, SessionView};
use crate::state::SessionState;

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
    /// Shown as a WIP badge that turns red once exceeded. Never enforced.
    #[serde(default)]
    pub wip_limit: Option<u32>,
}

/// One board's ordered columns plus the column ids that carry a role.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoardColumns {
    pub columns: Vec<Column>,
    /// Where the rollup moves a task once all its sessions settle. `None`
    /// turns auto-complete off for the board.
    #[serde(default)]
    pub done: Option<String>,
    /// Where the rollup pulls a `done` task back to when one of its sessions
    /// wakes up. `None` leaves such tasks where they are.
    #[serde(default)]
    pub active: Option<String>,
    /// Where quick-add (⌥⌘N) files new tasks. `None` means the first column.
    #[serde(default)]
    pub intake: Option<String>,
}

impl Default for BoardColumns {
    fn default() -> Self {
        let col = |id: &str, name: &str, color: &str, wip_limit| Column {
            id: id.into(),
            name: name.into(),
            color: color.into(),
            wip_limit,
        };
        Self {
            columns: vec![
                col(stage::BACKLOG, "Backlog", "#A08FC4", None),
                col(stage::TODO, "To Do", "#6FB9C9", None),
                col(stage::IN_PROGRESS, "In Progress", "#C8FF3D", Some(5)),
                col(stage::DONE, "Done", "#86B98C", None),
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

/// Partial edit of a column. `wip_limit: Some(None)` clears the limit.
#[derive(Debug, Default, Clone, Deserialize)]
pub struct ColumnPatch {
    pub name: Option<String>,
    pub color: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub wip_limit: Option<Option<u32>>,
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
    /// Last observed "all assigned sessions settled" per task, for the
    /// edge-triggered rollup. Deliberately not persisted (see `rollup`).
    settled: HashMap<TaskId, bool>,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn new_task_id() -> TaskId {
    format!("t{:x}{:x}", crate::now_ms(), NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

/// A session counts as "settled" (no more work expected from it right now)
/// when it is Done or Idle. Working and Waiting are both *unsettled*:
/// Waiting means blocked on the user, so the task is not finished.
fn is_settled(state: SessionState) -> bool {
    matches!(state, SessionState::Done | SessionState::Idle)
}

impl TaskStore {
    pub fn load(path: &Path) -> Self {
        let data = std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        let mut store = Self { path: path.to_path_buf(), data, settled: HashMap::new() };
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
        };
        self.data.tasks.push(task.clone());
        Some(task)
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

    /// Its sessions simply become unassigned (assignments are dropped).
    pub fn delete(&mut self, id: &str) -> bool {
        let before = self.data.tasks.len();
        self.data.tasks.retain(|t| t.id != id);
        self.data.assignments.retain(|_, task_id| task_id != id);
        self.settled.remove(id);
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
    pub fn add_column(&mut self, board: &str, name: &str, color: Option<&str>, wip_limit: Option<u32>) -> Result<Column, TaskError> {
        let name = clean_name(name)?;
        let color = color.map(clean_color).transpose()?;
        let layout = self.columns_of_mut(board);
        let column = Column {
            id: layout.fresh_id(&name),
            color: color.unwrap_or_else(|| layout.unused_color()),
            name,
            wip_limit: wip_limit.filter(|n| *n > 0),
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
        if let Some(limit) = patch.wip_limit {
            column.wip_limit = limit.filter(|n| *n > 0);
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

    /// Applies the rollup rule against the current live sessions. Returns
    /// whether any task's stage changed.
    ///
    /// A task with no live member sessions has no rollup signal (its cached
    /// state is forgotten), so after a restart — when reconstruction
    /// re-adds sessions gradually — the first observation of a task only
    /// *records* its state and never moves it, except that a task in its
    /// board's `done` column which turns out to have a live session is pulled
    /// back to the `active` column.
    pub fn rollup(&mut self, sessions: &[SessionView]) -> bool {
        // Index live sessions by id so member lookup below is O(1).
        let by_id: HashMap<&str, &SessionView> = sessions.iter().map(|s| (s.id.as_str(), s)).collect();
        let TasksSnapshot { tasks, assignments, columns, default_columns } = &mut self.data;
        let mut changed = false;
        for task in tasks.iter_mut() {
            // Collect this task's member sessions: every assignment pointing
            // at the task whose session is currently live on the board.
            let members: Vec<&SessionView> = assignments
                .iter()
                .filter(|(_, tid)| **tid == task.id)
                .filter_map(|(sid, _)| by_id.get(sid.as_str()).copied())
                .collect();
            // No live members -> no signal. Forget the cached state so the
            // next observation is treated as a first look, and leave the
            // task's stage exactly where the user (or last rollup) put it.
            if members.is_empty() {
                self.settled.remove(&task.id);
                continue;
            }
            let layout = columns.get(&task.board).unwrap_or(default_columns);
            let in_done = layout.done.as_deref() == Some(task.stage.as_str());
            // The task is "settled" only when *every* member session is.
            // One still-working (or waiting) session keeps it unsettled.
            let settled = members.iter().all(|s| is_settled(s.state));
            // Remember this observation and get back the previous one; the
            // stage only moves on a *change* (edge), never on a steady state.
            // That way a user who manually drags a task to another column
            // isn't overridden on every diff while sessions stay unchanged.
            let prev = self.settled.insert(task.id.clone(), settled);
            let target = match prev {
                // Edge: the settled status flipped since the last look.
                // Working -> all settled: auto-move to the done column; a
                // session woke back up: pull it back to the active column.
                Some(p) if p != settled => {
                    if settled {
                        layout.done.clone()
                    } else if in_done {
                        layout.active.clone()
                    } else {
                        None
                    }
                }
                // First observation (e.g. right after a restart): never
                // auto-complete, since sessions are re-added gradually and
                // "all settled" may just mean "not all loaded yet". Only
                // correct the one clearly wrong case: a done task with a
                // live, unsettled session.
                None if !settled && in_done => layout.active.clone(),
                // Steady state: leave the stage alone.
                _ => None,
            };
            if let Some(stage) = target.filter(|s| *s != task.stage) {
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

    pub async fn add_column(&self, board: &str, name: &str, color: Option<&str>, wip_limit: Option<u32>) -> Result<Column, TaskError> {
        self.try_mutate(|s| s.add_column(board, name, color, wip_limit)).await
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
            // The diff's contents are ignored on purpose: we re-read a full
            // snapshot instead. A lagged receiver (missed diffs) is handled
            // the same way, since the snapshot is the source of truth.
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
    use super::stage::{BACKLOG, DONE, IN_PROGRESS, TODO};

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
    fn rollup_moves_task_to_done_when_all_sessions_settle_and_back_when_revived() {
        let (mut s, _d) = store();
        let t = s.create("A", IN_PROGRESS).unwrap();
        s.assign("s1", Some(&t.id));
        s.assign("s2", Some(&t.id));

        // First observation only records.
        assert!(!s.rollup(&[session("s1", SessionState::Working), session("s2", SessionState::Done)]));
        assert_eq!(s.snapshot().tasks[0].stage, IN_PROGRESS);

        // Working -> Done edge: task advances.
        assert!(s.rollup(&[session("s1", SessionState::Done), session("s2", SessionState::Done)]));
        assert_eq!(s.snapshot().tasks[0].stage, DONE);

        // Idle counts as settled: no change.
        assert!(!s.rollup(&[session("s1", SessionState::Idle), session("s2", SessionState::Done)]));

        // A settled Done task gets a live session again: pulled back.
        assert!(s.rollup(&[session("s1", SessionState::Working), session("s2", SessionState::Done)]));
        assert_eq!(s.snapshot().tasks[0].stage, IN_PROGRESS);
    }

    #[test]
    fn rollup_waiting_counts_as_not_settled() {
        let (mut s, _d) = store();
        let t = s.create("A", DONE).unwrap();
        s.assign("s1", Some(&t.id));
        s.rollup(&[session("s1", SessionState::Done)]);
        assert!(s.rollup(&[session("s1", SessionState::Waiting)]));
        assert_eq!(s.snapshot().tasks[0].stage, IN_PROGRESS);
    }

    #[test]
    fn a_manual_drag_out_of_done_is_not_bounced_back() {
        let (mut s, _d) = store();
        let t = s.create("A", IN_PROGRESS).unwrap();
        s.assign("s1", Some(&t.id));
        s.rollup(&[session("s1", SessionState::Working)]);
        s.rollup(&[session("s1", SessionState::Done)]);
        assert_eq!(s.snapshot().tasks[0].stage, DONE);

        s.update(&t.id, None, Some(TODO)).unwrap();
        // Unrelated diffs with the session still Done must not re-advance.
        assert!(!s.rollup(&[session("s1", SessionState::Done)]));
        assert_eq!(s.snapshot().tasks[0].stage, TODO);
    }

    #[test]
    fn done_task_found_with_a_live_session_on_first_observation_is_pulled_back() {
        let (mut s, _d) = store();
        let t = s.create("A", DONE).unwrap();
        s.assign("s1", Some(&t.id));
        assert!(s.rollup(&[session("s1", SessionState::Working)]));
        assert_eq!(s.snapshot().tasks[0].stage, IN_PROGRESS);
    }

    #[test]
    fn task_without_live_sessions_is_left_alone() {
        let (mut s, _d) = store();
        let t = s.create("A", TODO).unwrap();
        s.assign("gone", Some(&t.id));
        assert!(!s.rollup(&[]));
        assert_eq!(s.snapshot().tasks[0].stage, TODO);
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
        let a = s.add_column("work", "QA", None, Some(2)).unwrap();
        let b = s.add_column("work", "QA", None, None).unwrap();
        assert_eq!((a.id.as_str(), b.id.as_str()), ("qa", "qa-2"), "ids stay unique");
        assert_ne!(a.color, b.color, "new columns get an unused palette color");
        assert_eq!(s.add_column("work", "Order", None, None).unwrap().id, "order-2", "route names are reserved");
        s.delete_column("work", "order-2", "qa").unwrap();
        assert!(s.add_column("work", "  ", None, None).is_err());
        assert!(s.add_column("work", "X", Some("red"), None).is_err());
        assert_eq!(ids(&s, "default").len(), 4, "other boards are untouched");

        let renamed = s.update_column("work", "qa", &ColumnPatch { name: Some("Testing".into()), ..Default::default() }).unwrap();
        assert_eq!((renamed.id.as_str(), renamed.name.as_str()), ("qa", "Testing"), "rename keeps the id");
        let cleared = s.update_column("work", "qa", &ColumnPatch { wip_limit: Some(None), ..Default::default() }).unwrap();
        assert_eq!(cleared.wip_limit, None);
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

    #[test]
    fn rollup_follows_the_boards_roles() {
        let (mut s, _d) = store();
        let shipped = s.add_column("default", "Shipped", None, None).unwrap().id;
        s.set_roles("default", &RolesPatch { done: Some(Some(shipped.clone())), active: Some(Some(TODO.into())), ..Default::default() })
            .unwrap();
        let t = s.create("A", IN_PROGRESS).unwrap();
        s.assign("s1", Some(&t.id));
        s.rollup(&[session("s1", SessionState::Working)]);
        assert!(s.rollup(&[session("s1", SessionState::Done)]));
        assert_eq!(s.snapshot().tasks[0].stage, shipped);
        assert!(s.rollup(&[session("s1", SessionState::Working)]));
        assert_eq!(s.snapshot().tasks[0].stage, TODO);

        // No done role: auto-complete is off.
        s.set_roles("default", &RolesPatch { done: Some(None), ..Default::default() }).unwrap();
        assert!(!s.rollup(&[session("s1", SessionState::Done)]));
        assert_eq!(s.snapshot().tasks[0].stage, TODO);
    }
}
