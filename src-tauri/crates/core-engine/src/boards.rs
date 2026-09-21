//! Named boards, each tied to one Claude config directory (one Claude
//! account, via `CLAUDE_CONFIG_DIR`). Persisted as a single flat JSON file in
//! the app data dir, same atomic-write pattern as `EndedSessions`.
//!
//! The `default` board is always present and always points at `~/.claude`, so
//! everything recorded before boards existed belongs to it without any
//! migration: its hooks carry no `--board` argument, its tasks default to it,
//! and a session with no recorded board resolves to it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

pub const DEFAULT_BOARD_ID: &str = "default";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Board {
    pub id: String,
    pub name: String,
    pub config_dir: PathBuf,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    boards: Vec<Board>,
    /// Sessions that belong to a non-default board. Absence means `default`.
    #[serde(default)]
    session_boards: HashMap<String, String>,
}

pub struct BoardStore {
    path: Option<PathBuf>,
    inner: RwLock<Persisted>,
}

/// `~` / `~/x` -> absolute, so a user can type `~/.claude-work`.
pub fn expand_home(raw: &str) -> PathBuf {
    let raw = raw.trim();
    if raw == "~" {
        return dirs::home_dir().unwrap_or_default();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return dirs::home_dir().unwrap_or_default().join(rest);
    }
    PathBuf::from(raw)
}

fn slugify(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

impl BoardStore {
    /// A store that never touches disk (tests, and `OrchestratorConfig`'s
    /// default). Contains only the default board.
    pub fn in_memory() -> Self {
        Self { path: None, inner: RwLock::new(Persisted::default()) }
    }

    pub fn load(path: &Path) -> Self {
        let inner = std::fs::read_to_string(path).ok().and_then(|raw| serde_json::from_str(&raw).ok()).unwrap_or_default();
        Self { path: Some(path.to_path_buf()), inner: RwLock::new(inner) }
    }

    fn default_board(name: Option<&str>) -> Board {
        Board {
            id: DEFAULT_BOARD_ID.to_string(),
            name: name.unwrap_or("Default").to_string(),
            config_dir: crate::first_run::claude_config_dir(),
        }
    }

    /// Default board first, then user boards in creation order.
    pub fn list(&self) -> Vec<Board> {
        let inner = self.inner.read().unwrap();
        let default_name = inner.boards.iter().find(|b| b.id == DEFAULT_BOARD_ID).map(|b| b.name.clone());
        let mut out = vec![Self::default_board(default_name.as_deref())];
        out.extend(inner.boards.iter().filter(|b| b.id != DEFAULT_BOARD_ID).cloned());
        out
    }

    pub fn get(&self, id: &str) -> Option<Board> {
        self.list().into_iter().find(|b| b.id == id)
    }

    pub fn add(&self, name: &str, config_dir: &str) -> Result<Board, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("board name is required".into());
        }
        if config_dir.trim().is_empty() {
            return Err("config directory is required".into());
        }
        let dir = expand_home(config_dir);
        if !dir.is_absolute() {
            return Err("config directory must be an absolute path (or start with ~/)".into());
        }
        let existing = self.list();
        if existing.iter().any(|b| b.config_dir == dir) {
            return Err("another board already uses that config directory".into());
        }
        if existing.iter().any(|b| b.name.eq_ignore_ascii_case(name)) {
            return Err("a board with that name already exists".into());
        }

        let base = match slugify(name) {
            s if s.is_empty() || s == DEFAULT_BOARD_ID => "board".to_string(),
            s => s,
        };
        let mut id = base.clone();
        let mut n = 2;
        while existing.iter().any(|b| b.id == id) {
            id = format!("{base}-{n}");
            n += 1;
        }

        let board = Board { id, name: name.to_string(), config_dir: dir };
        self.inner.write().unwrap().boards.push(board.clone());
        self.save().map_err(|e| e.to_string())?;
        Ok(board)
    }

    pub fn rename(&self, id: &str, name: &str) -> Result<Board, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("board name is required".into());
        }
        if self.list().iter().any(|b| b.id != id && b.name.eq_ignore_ascii_case(name)) {
            return Err("a board with that name already exists".into());
        }
        {
            let mut inner = self.inner.write().unwrap();
            match inner.boards.iter_mut().find(|b| b.id == id) {
                Some(b) => b.name = name.to_string(),
                None if id == DEFAULT_BOARD_ID => inner.boards.push(Self::default_board(Some(name))),
                None => return Err("no such board".into()),
            }
        }
        self.save().map_err(|e| e.to_string())?;
        self.get(id).ok_or_else(|| "no such board".to_string())
    }

    /// Removes a board and forgets which sessions were assigned to it.
    /// The default board can't be removed.
    pub fn remove(&self, id: &str) -> Result<Board, String> {
        if id == DEFAULT_BOARD_ID {
            return Err("the default board can't be removed".into());
        }
        let removed = {
            let mut inner = self.inner.write().unwrap();
            let Some(pos) = inner.boards.iter().position(|b| b.id == id) else {
                return Err("no such board".into());
            };
            let removed = inner.boards.remove(pos);
            inner.session_boards.retain(|_, b| b != id);
            removed
        };
        self.save().map_err(|e| e.to_string())?;
        Ok(removed)
    }

    pub fn board_of(&self, session_id: &str) -> String {
        self.inner.read().unwrap().session_boards.get(session_id).cloned().unwrap_or_else(|| DEFAULT_BOARD_ID.to_string())
    }

    /// Records which board `session_id` belongs to. Assigning `default`
    /// clears any record, since absence already means default.
    pub fn assign(&self, session_id: &str, board: &str) {
        {
            let mut inner = self.inner.write().unwrap();
            if board == DEFAULT_BOARD_ID {
                if inner.session_boards.remove(session_id).is_none() {
                    return;
                }
            } else if inner.session_boards.get(session_id).map(String::as_str) == Some(board) {
                return;
            } else {
                inner.session_boards.insert(session_id.to_string(), board.to_string());
            }
        }
        let _ = self.save();
    }

    fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else { return Ok(()) };
        let json = serde_json::to_string_pretty(&*self.inner.read().unwrap())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_store_lists_only_the_default_board() {
        let store = BoardStore::in_memory();
        let boards = store.list();
        assert_eq!(boards.len(), 1);
        assert_eq!(boards[0].id, DEFAULT_BOARD_ID);
        assert_eq!(boards[0].config_dir, crate::first_run::claude_config_dir());
    }

    #[test]
    fn add_assigns_slug_ids_and_rejects_duplicates() {
        let store = BoardStore::in_memory();
        let work = store.add("Work Stuff", "/tmp/claude-work").unwrap();
        assert_eq!(work.id, "work-stuff");
        assert!(store.add("work stuff", "/tmp/other").is_err(), "duplicate name");
        assert!(store.add("Home", "/tmp/claude-work").is_err(), "duplicate dir");
        assert!(store.add("", "/tmp/x").is_err());
        assert!(store.add("Rel", "relative/dir").is_err());
        let default_clash = store.add("Default", "/tmp/d").unwrap_err();
        assert!(default_clash.contains("already exists"));
        assert_eq!(store.add("!!!", "/tmp/y").unwrap().id, "board");
    }

    #[test]
    fn persists_boards_and_session_assignments_across_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("boards.json");
        let store = BoardStore::load(&path);
        let work = store.add("Work", "/tmp/claude-work").unwrap();
        store.assign("s1", &work.id);
        store.rename(DEFAULT_BOARD_ID, "Personal").unwrap();

        let reloaded = BoardStore::load(&path);
        assert_eq!(reloaded.list().len(), 2);
        assert_eq!(reloaded.get(DEFAULT_BOARD_ID).unwrap().name, "Personal");
        assert_eq!(reloaded.board_of("s1"), "work");
        assert_eq!(reloaded.board_of("unknown"), DEFAULT_BOARD_ID);
    }

    #[test]
    fn remove_forgets_assignments_and_protects_default() {
        let store = BoardStore::in_memory();
        let work = store.add("Work", "/tmp/claude-work").unwrap();
        store.assign("s1", &work.id);
        assert!(store.remove(DEFAULT_BOARD_ID).is_err());
        store.remove(&work.id).unwrap();
        assert_eq!(store.board_of("s1"), DEFAULT_BOARD_ID);
        assert!(store.remove(&work.id).is_err());
    }
}
