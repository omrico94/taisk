//! `ClaudeCodeCollector`: the one Phase-1 implementation of the (currently
//! implicit) collector adapter boundary (plan §2 — a new tool later means a
//! new module like this one, without touching the engine actor, UI, or
//! LanceDB schema). Owns:
//! - mapping a session's cwd to its transcript file path,
//! - append-safe tailing of that file with on-disk checkpoints, and
//! - a best-effort extractor for the initiating prompt out of transcript
//!   lines.
//!
//! Note (plan §12 open question, not resolved here): the exact JSONL schema
//! assumed by `extract_initiating_prompt` below is a best-effort guess at the
//! real Claude Code CLI's transcript format and needs verification against a
//! live `claude` session before this is trusted end-to-end (see the M11
//! manual checklist). `sanitize_cwd`, by contrast, **is** verified — it
//! matches a real transcript directory observed on this machine.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Claude Code's own sanitization of a session's cwd into its project
/// directory name: every `/` becomes `-`. Confirmed locally: cwd
/// `/Users/omricohen/Desktop` -> `-Users-omricohen-Desktop`.
pub fn sanitize_cwd(cwd: &str) -> String {
    cwd.replace('/', "-")
}

pub fn transcript_path(claude_projects_dir: &Path, cwd: &str, session_id: &str) -> PathBuf {
    claude_projects_dir
        .join(sanitize_cwd(cwd))
        .join(format!("{session_id}.jsonl"))
}

/// A human-readable project name derived from cwd (last path segment),
/// falling back to the full cwd if it has no segments (e.g. `/`).
pub fn project_name_from_cwd(cwd: &str) -> String {
    cwd.rsplit('/').find(|s| !s.is_empty()).unwrap_or(cwd).to_string()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
struct FileCheckpoint {
    inode: u64,
    offset: u64,
}

/// On-disk (path -> checkpoint) map, stored as one flat JSON file rather than
/// a database (plan §4 — this is a handful of small entries, a second
/// storage engine would be overkill). Written atomically (temp file +
/// rename) so a crash mid-write can't corrupt it.
pub struct TailCheckpoints {
    path: PathBuf,
    data: HashMap<String, FileCheckpoint>,
}

impl TailCheckpoints {
    pub fn load(path: &Path) -> Self {
        let data = std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        Self { path: path.to_path_buf(), data }
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
}

/// Reads whatever new, complete lines have been appended to `file_path`
/// since the last checkpoint, and advances the checkpoint. A changed inode
/// (file replaced/rotated) resets the offset to 0 rather than trusting a
/// stale byte offset against different file content. Only bytes up to the
/// last `\n` are consumed — a trailing partial line (writer mid-flush) is
/// left for the next call, so a half-written JSON line is never parsed.
pub fn tail_new_lines(file_path: &Path, checkpoints: &mut TailCheckpoints) -> std::io::Result<Vec<String>> {
    let mut file = File::open(file_path)?;
    let meta = file.metadata()?;
    let inode = meta.ino();
    let key = file_path.to_string_lossy().to_string();

    let start_offset = match checkpoints.data.get(&key) {
        Some(cp) if cp.inode == inode => cp.offset,
        _ => 0,
    };

    file.seek(SeekFrom::Start(start_offset))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;

    let Some(last_newline) = buf.iter().rposition(|&b| b == b'\n') else {
        // No complete line yet; checkpoint unchanged.
        return Ok(Vec::new());
    };

    let consumed = &buf[..=last_newline];
    let lines: Vec<String> = String::from_utf8_lossy(consumed)
        .lines()
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
        .collect();

    checkpoints.data.insert(
        key,
        FileCheckpoint { inode, offset: start_offset + consumed.len() as u64 },
    );

    Ok(lines)
}

/// Best-effort extraction of the first user-turn's prompt text out of
/// transcript lines, tolerant of a couple of known Claude message-content
/// shapes (plain string, or an array of `{"type":"text","text":...}`
/// blocks). See the module-level note: this needs verifying against a real
/// transcript before being trusted for anything beyond the fixture harness.
pub fn extract_initiating_prompt(lines: &[String]) -> Option<String> {
    for line in lines {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if value.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }
        // A missing/malformed `message.content` on one "user" line must not
        // abort the whole search — keep looking at subsequent lines instead
        // of bailing out with `?` (that was a latent bug: harmless while
        // every real "user" line happened to have content, but not
        // guaranteed).
        let Some(content) = value.get("message").and_then(|m| m.get("content")) else { continue };
        if let Some(text) = text_from_content(content) {
            return Some(text);
        }
    }
    None
}

/// Same idea as `extract_initiating_prompt`, but returns the MOST RECENT
/// user/assistant turn's text (walking backwards) rather than the first —
/// used to refresh the "current task" line as a session keeps working
/// (plan feedback: the task line should track the latest relevant activity,
/// not freeze after the initial prompt).
pub fn extract_latest_activity(lines: &[String]) -> Option<String> {
    for line in lines.iter().rev() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let ty = value.get("type").and_then(|t| t.as_str());
        if ty != Some("user") && ty != Some("assistant") {
            continue;
        }
        let Some(content) = value.get("message").and_then(|m| m.get("content")) else { continue };
        if let Some(text) = text_from_content(content) {
            return Some(text);
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptRow {
    pub role: String,
    pub text: String,
}

fn text_from_content(content: &serde_json::Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return (!s.is_empty()).then(|| s.to_string());
    }
    if let Some(blocks) = content.as_array() {
        let text: String = blocks
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n");
        return (!text.is_empty()).then_some(text);
    }
    None
}

/// Parses a full transcript file into display rows for the detail drawer's
/// "Transcript tail" (design spec) — unlike `extract_initiating_prompt`
/// (which only needs the first user turn for categorization), this walks
/// every line and keeps user/assistant text turns in order. Tool-call/result
/// entries and anything else unparseable are skipped rather than guessed at
/// (see the module-level note on schema uncertainty).
pub fn parse_transcript_for_display(path: &Path) -> Vec<TranscriptRow> {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Vec::new();
    };

    contents
        .lines()
        .filter_map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).ok()?;
            let role = match value.get("type").and_then(|t| t.as_str()) {
                Some("user") => "You",
                Some("assistant") => "Claude",
                _ => return None,
            };
            let content = value.get("message").and_then(|m| m.get("content"))?;
            let text = text_from_content(content)?;
            Some(TranscriptRow { role: role.to_string(), text })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_cwd_matches_the_observed_real_directory_name() {
        assert_eq!(sanitize_cwd("/Users/omricohen/Desktop"), "-Users-omricohen-Desktop");
    }

    #[test]
    fn transcript_path_joins_projects_dir_sanitized_cwd_and_session_id() {
        let p = transcript_path(Path::new("/Users/omricohen/.claude/projects"), "/Users/omricohen/Desktop", "abc-123");
        assert_eq!(
            p,
            Path::new("/Users/omricohen/.claude/projects/-Users-omricohen-Desktop/abc-123.jsonl")
        );
    }

    #[test]
    fn project_name_takes_the_last_path_segment() {
        assert_eq!(project_name_from_cwd("/Users/omricohen/api-gateway"), "api-gateway");
    }

    #[test]
    fn tail_new_lines_only_reads_forward_and_leaves_partial_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        std::fs::write(&path, "{\"a\":1}\n{\"a\":2}\n").unwrap();

        let mut checkpoints = TailCheckpoints::load(&dir.path().join("checkpoints.json"));
        let lines = tail_new_lines(&path, &mut checkpoints).unwrap();
        assert_eq!(lines, vec!["{\"a\":1}", "{\"a\":2}"]);

        // Nothing new yet -> empty.
        let lines = tail_new_lines(&path, &mut checkpoints).unwrap();
        assert!(lines.is_empty());

        // Append a complete line plus a partial (unterminated) one.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"a\":3}\n{\"a\":4 partial").unwrap();
        drop(f);

        let lines = tail_new_lines(&path, &mut checkpoints).unwrap();
        assert_eq!(lines, vec!["{\"a\":3}"], "the partial trailing line must not be returned yet");

        // Complete the partial line -> now it should come through.
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"}\n").unwrap();
        drop(f);
        let lines = tail_new_lines(&path, &mut checkpoints).unwrap();
        assert_eq!(lines, vec!["{\"a\":4 partial}"]);
    }

    #[test]
    fn checkpoints_persist_and_reload_across_a_simulated_restart() {
        let dir = tempfile::tempdir().unwrap();
        let jsonl_path = dir.path().join("session.jsonl");
        let checkpoint_path = dir.path().join("checkpoints.json");
        std::fs::write(&jsonl_path, "{\"a\":1}\n").unwrap();

        {
            let mut checkpoints = TailCheckpoints::load(&checkpoint_path);
            let lines = tail_new_lines(&jsonl_path, &mut checkpoints).unwrap();
            assert_eq!(lines.len(), 1);
            checkpoints.save().unwrap();
        }

        // Simulate a restart: fresh TailCheckpoints loaded from disk must not
        // reprocess the line already consumed above.
        let mut reloaded = TailCheckpoints::load(&checkpoint_path);
        let lines = tail_new_lines(&jsonl_path, &mut reloaded).unwrap();
        assert!(lines.is_empty(), "restart must not reprocess already-seen lines");

        std::fs::OpenOptions::new()
            .append(true)
            .open(&jsonl_path)
            .unwrap()
            .write_all(b"{\"a\":2}\n")
            .map(|_| ())
            .unwrap();
        let lines = tail_new_lines(&jsonl_path, &mut reloaded).unwrap();
        assert_eq!(lines, vec!["{\"a\":2}"], "restart must not lose newly appended lines either");
    }

    use std::io::Write;

    #[test]
    fn extracts_prompt_from_string_content() {
        let lines = vec![
            serde_json::json!({"type":"user","message":{"role":"user","content":"refactor auth"}}).to_string(),
        ];
        assert_eq!(extract_initiating_prompt(&lines), Some("refactor auth".to_string()));
    }

    #[test]
    fn extracts_prompt_from_text_block_array_content() {
        let lines = vec![
            serde_json::json!({
                "type":"user",
                "message":{"role":"user","content":[{"type":"text","text":"refactor auth"}]}
            })
            .to_string(),
        ];
        assert_eq!(extract_initiating_prompt(&lines), Some("refactor auth".to_string()));
    }

    #[test]
    fn returns_none_when_no_user_line_present() {
        let lines = vec![serde_json::json!({"type":"assistant","message":{"content":"hi"}}).to_string()];
        assert_eq!(extract_initiating_prompt(&lines), None);
    }

    #[test]
    fn extract_latest_activity_returns_the_most_recent_turn_not_the_first() {
        let lines = vec![
            serde_json::json!({"type":"user","message":{"content":"refactor auth"}}).to_string(),
            serde_json::json!({"type":"assistant","message":{"content":"Converting to async/await."}}).to_string(),
            serde_json::json!({"type":"assistant","message":{"content":"Now running the test suite."}}).to_string(),
        ];
        assert_eq!(extract_latest_activity(&lines), Some("Now running the test suite.".to_string()));
    }

    #[test]
    fn extract_latest_activity_skips_unparseable_trailing_lines() {
        let lines = vec![
            serde_json::json!({"type":"assistant","message":{"content":"Converting to async/await."}}).to_string(),
            serde_json::json!({"type":"summary","message":{"content":"irrelevant"}}).to_string(),
            "not even json".to_string(),
        ];
        assert_eq!(extract_latest_activity(&lines), Some("Converting to async/await.".to_string()));
    }

    #[test]
    fn parse_transcript_for_display_keeps_user_and_assistant_turns_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let content = [
            serde_json::json!({"type":"user","message":{"content":"refactor auth"}}).to_string(),
            serde_json::json!({"type":"assistant","message":{"content":[{"type":"text","text":"On it."}]}}).to_string(),
            serde_json::json!({"type":"summary","message":{"content":"irrelevant"}}).to_string(),
            serde_json::json!({"type":"assistant","message":{"content":"Done."}}).to_string(),
        ]
        .join("\n")
            + "\n";
        std::fs::write(&path, content).unwrap();

        let rows = parse_transcript_for_display(&path);
        assert_eq!(
            rows,
            vec![
                TranscriptRow { role: "You".into(), text: "refactor auth".into() },
                TranscriptRow { role: "Claude".into(), text: "On it.".into() },
                TranscriptRow { role: "Claude".into(), text: "Done.".into() },
            ]
        );
    }

    #[test]
    fn parse_transcript_for_display_returns_empty_for_missing_file() {
        assert_eq!(parse_transcript_for_display(Path::new("/nonexistent/path.jsonl")), Vec::new());
    }
}
