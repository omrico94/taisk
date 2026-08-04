//! Reads a session's linked execution plan from the real
//! `~/.claude/tasks/<session_id>/*.json` files (Phase 2 design change).
//! This Claude Code version has no on-disk trace of the deprecated
//! `TodoWrite` tool at all — `TaskCreate`/`TaskUpdate` is the real, current
//! mechanism, and this project's own build (see `~/.claude/tasks/` on this
//! machine) has been tracked with it the whole time.

use std::path::Path;

use crate::engine::{PlanStep, PlanView};

/// Absence of a `<session_id>/` directory is the normal case — most sessions
/// never call `TaskCreate` — and returns `None`, not an error.
pub fn read_session_plan(tasks_dir: &Path, session_id: &str, category: &str) -> Option<PlanView> {
    let dir = tasks_dir.join(session_id);
    let entries = std::fs::read_dir(&dir).ok()?;

    let mut steps: Vec<(Option<i64>, PlanStep)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&path) else { continue };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents) else { continue };
        let Some(id) = value.get("id").and_then(|v| v.as_str()) else { continue };
        let subject = value.get("subject").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let status = value.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let done = status == "completed";
        let numeric_id = id.parse::<i64>().ok();
        steps.push((numeric_id, PlanStep { id: id.to_string(), subject, done }));
    }

    if steps.is_empty() {
        return None;
    }

    // Numeric ids sort naturally; anything unparseable falls back to
    // whatever order read_dir happened to yield it in (there's no other
    // ordering signal in the real data).
    steps.sort_by_key(|(numeric_id, _)| numeric_id.unwrap_or(i64::MAX));

    Some(PlanView { title: format!("{category} plan"), steps: steps.into_iter().map(|(_, step)| step).collect() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_task(dir: &Path, id: &str, subject: &str, status: &str) {
        std::fs::write(
            dir.join(format!("{id}.json")),
            serde_json::json!({"id": id, "subject": subject, "status": status, "blocks": [], "blockedBy": []}).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn returns_none_when_no_tasks_directory_exists_for_this_session() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(read_session_plan(root.path(), "no-such-session", "Backend"), None);
    }

    #[test]
    fn reads_and_orders_steps_numerically_and_counts_done() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("s1");
        std::fs::create_dir_all(&dir).unwrap();
        write_task(&dir, "2", "Second step", "pending");
        write_task(&dir, "10", "Tenth step", "pending");
        write_task(&dir, "1", "First step", "completed");

        let plan = read_session_plan(root.path(), "s1", "Backend / API").expect("plan should be found");
        assert_eq!(plan.title, "Backend / API plan");
        // Numeric ordering (1, 2, 10), not lexical (1, 10, 2).
        assert_eq!(plan.steps.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), vec!["1", "2", "10"]);
        assert!(plan.steps[0].done);
        assert!(!plan.steps[1].done);
        assert!(!plan.steps[2].done);
    }

    #[test]
    fn ignores_non_json_files_in_the_session_directory() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("s1");
        std::fs::create_dir_all(&dir).unwrap();
        write_task(&dir, "1", "Only real task", "pending");
        std::fs::write(dir.join(".DS_Store"), b"junk").unwrap();

        let plan = read_session_plan(root.path(), "s1", "General").unwrap();
        assert_eq!(plan.steps.len(), 1);
    }
}
