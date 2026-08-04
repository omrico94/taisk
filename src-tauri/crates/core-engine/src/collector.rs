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

/// Reads a transcript's complete lines from the very start, independent of
/// any checkpoint. `session-start` needs this instead of `tail_new_lines`:
/// the initiating prompt lives at a *fixed* position near the top of the
/// file, not "whatever's new since I last checked" — and `tail_new_lines`'s
/// checkpoint is a single shared, path-keyed cursor. If anything else that
/// tails the same file (a `stop` hook, a retried/duplicate `session-start`)
/// advances that cursor past the prompt first, `session-start`'s own next
/// read would come back empty and the session gets silently dropped forever
/// (checkpoints never rewind) — a real, reproduced bug. Reading fresh from
/// offset 0 every time is cheap here (only used for a session's still-small
/// initial content) and can't be raced out from under.
pub fn read_lines_from_start(file_path: &Path) -> std::io::Result<Vec<String>> {
    let contents = std::fs::read_to_string(file_path)?;
    Ok(contents.lines().map(str::to_string).collect())
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

/// Claude Code stamps a per-message `"entrypoint"` field directly onto every
/// transcript line (observed real values: `"cli"`, `"claude-desktop"`,
/// `"claude-vscode"`) — a more specific signal than what the `SessionStart`
/// hook payload itself reports, which is only ever `"cli"` for anything
/// built on the plain `claude` binary (including a third-party VS Code
/// extension like DevSwarm that just spawns it programmatically; the hook
/// has no way to know it's being orchestrated by anything else). Prefer this
/// over the hook payload's `entrypoint` when present — see its call site in
/// `orchestrator.rs`'s `session-start` handler.
pub fn extract_entrypoint(lines: &[String]) -> Option<String> {
    for line in lines {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if let Some(entrypoint) = value.get("entrypoint").and_then(|e| e.as_str()) {
            return Some(entrypoint.to_string());
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

/// Real per-turn token/cost/context-window figures for a session, derived
/// from the transcript's actual `usage` objects (Phase 2 design change).
/// Confirmed directly against real transcripts on this machine: `usage` is
/// NOT a single consistent semantic —
/// - `output_tokens` and `cache_creation_input_tokens` are genuine per-turn
///   deltas, so they're summed across every turn.
/// - `cache_read_input_tokens` is a running/cumulative signal (with a sharp
///   reset at a context-compaction event), and combined with the latest
///   turn's own `cache_creation_input_tokens`/`input_tokens` is the closest
///   real proxy to "current context occupancy" — there is no direct field
///   for this, so only the *latest* turn's numbers are used for `ctx_used`.
/// - `input_tokens` is a near-constant placeholder in real data, not real
///   per-turn size — never summed on its own as a running total.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct UsageMetrics {
    /// Cumulative output_tokens + cache_creation_input_tokens across every
    /// turn — "how much this session has generated/newly processed",
    /// deliberately not including the near-constant `input_tokens` field.
    pub tokens: i64,
    /// Cumulative USD cost across every turn, priced per that turn's own
    /// `model` field (a session can span more than one model tier).
    pub cost: f64,
    /// Latest turn's cache_read + cache_creation + input tokens — the
    /// closest real signal to current context-window occupancy.
    pub ctx_used: i64,
    /// The latest turn's model's real context-window ceiling.
    pub ctx_max: i64,
}

struct ModelPricing {
    input_per_million: f64,
    output_per_million: f64,
}

/// Current (as of this implementation) per-model pricing — Claude Sonnet 5's
/// intro rate is used since that's the actually-active price through
/// 2026-08-31; unrecognized/future model strings fall back to Sonnet 5's
/// standard rate as a reasonable default rather than guessing wildly.
fn pricing_for_model(model: &str) -> ModelPricing {
    match model {
        "claude-opus-4-8" => ModelPricing { input_per_million: 5.0, output_per_million: 25.0 },
        "claude-sonnet-5" => ModelPricing { input_per_million: 2.0, output_per_million: 10.0 },
        "claude-haiku-4-5" => ModelPricing { input_per_million: 1.0, output_per_million: 5.0 },
        "claude-fable-5" | "claude-mythos-5" => ModelPricing { input_per_million: 10.0, output_per_million: 50.0 },
        _ => ModelPricing { input_per_million: 3.0, output_per_million: 15.0 },
    }
}

/// Real per-model context-window ceilings. Haiku 4.5 is the one current
/// exception at 200K; every other current/future model defaults to the 1M
/// ceiling most current Claude models actually ship with.
fn ctx_max_for_model(model: &str) -> i64 {
    if model == "claude-haiku-4-5" {
        200_000
    } else {
        1_000_000
    }
}

/// Standard cache economics: writing to the cache costs ~1.25x the base
/// input rate (5-minute TTL), reading from it costs ~0.1x.
const CACHE_WRITE_MULTIPLIER: f64 = 1.25;
const CACHE_READ_MULTIPLIER: f64 = 0.1;

/// Reads the *entire* transcript file (not just newly-tailed lines — this is
/// re-derived fresh on each call, deliberately not tracked incrementally
/// alongside `TailCheckpoints`, since real sessions are small enough locally
/// that this is cheap and it avoids yet another durable-state file) and
/// computes real cumulative usage metrics. Returns `None` if the file is
/// missing or contains no real assistant turns with a `usage` object yet
/// (e.g. a session that's only just started) — callers should simply not
/// dispatch metrics in that case rather than show a zeroed-out bar.
pub fn extract_usage_metrics(path: &Path) -> Option<UsageMetrics> {
    let contents = std::fs::read_to_string(path).ok()?;

    let mut tokens = 0i64;
    let mut cost = 0f64;
    let mut latest: Option<(i64, i64)> = None; // (ctx_used, ctx_max) from the most recent real turn

    for line in contents.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if value.get("type").and_then(|t| t.as_str()) != Some("assistant") {
            continue;
        }
        let Some(message) = value.get("message") else { continue };
        let model = message.get("model").and_then(|m| m.as_str()).unwrap_or("");
        // The synthetic stop-sequence placeholder turn (`model: "<synthetic>"`,
        // all-zero usage) isn't a real model call — skip it entirely so it
        // can't wrongly become "the latest turn" and zero out ctx_used/ctx_max.
        if model.is_empty() || model == "<synthetic>" {
            continue;
        }
        let Some(usage) = message.get("usage") else { continue };
        let input_tokens = usage.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
        let output_tokens = usage.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
        let cache_creation = usage.get("cache_creation_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
        let cache_read = usage.get("cache_read_input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);

        tokens += output_tokens + cache_creation;

        let pricing = pricing_for_model(model);
        cost += (output_tokens as f64) * pricing.output_per_million / 1_000_000.0
            + (cache_creation as f64) * pricing.input_per_million * CACHE_WRITE_MULTIPLIER / 1_000_000.0
            + (cache_read as f64) * pricing.input_per_million * CACHE_READ_MULTIPLIER / 1_000_000.0
            + (input_tokens as f64) * pricing.input_per_million / 1_000_000.0;

        latest = Some((cache_read + cache_creation + input_tokens, ctx_max_for_model(model)));
    }

    let (ctx_used, ctx_max) = latest?;
    Some(UsageMetrics { tokens, cost, ctx_used, ctx_max })
}

/// A subagent (parent → child tree, Phase 2 design change) — confirmed
/// directly on this machine: Claude Code writes a real subagent transcript
/// at `<parent-session-dir>/subagents/agent-<id>.jsonl` with a sidecar
/// `agent-<id>.meta.json` (`agentType`/`description`/`toolUseId`/`spawnDepth`).
/// There is no `SubagentStop` hook (checked `~/.claude/settings.json`
/// directly — absent), so `state` here is a heuristic, not pushed truth: see
/// `list_subagents`'s doc comment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubagentInfo {
    pub id: String,
    pub title: String,
    pub desc: String,
    /// `"Working"` or `"Done"` — deliberately not the full `SessionState`
    /// enum. Subagents don't get Waiting/Idle: there's no hook to detect
    /// either for them, so collapsing to a plain busy/finished heuristic is
    /// more honest than pretending to a precision this data doesn't have.
    pub state: String,
    pub tokens: i64,
    pub cost: f64,
    pub ctx_used: i64,
    pub ctx_max: i64,
}

/// A subagent transcript with no activity for this long is presumed
/// finished — shorter than the main session's 10-minute idle TTL, since
/// subagents are typically short-lived, focused sub-tasks rather than
/// hours-long interactive sessions.
const SUBAGENT_ACTIVE_WINDOW_SECS: u64 = 120;

/// Lists every subagent spawned by `parent_session_id`, reading directly
/// from `<claude_projects_dir>/<sanitized-cwd>/<parent_session_id>/subagents/`.
/// Returns an empty vec (not an error) when that directory doesn't exist —
/// most sessions never spawn a subagent, and that's not a failure case.
///
/// **State is a heuristic, not a guarantee** (see the module-level note on
/// `AskUserQuestion`-style gaps elsewhere in this codebase for the same
/// pattern): a subagent transcript modified within `SUBAGENT_ACTIVE_WINDOW_SECS`
/// is treated as `Working`, otherwise `Done`. A real `SubagentStop` hook, if
/// Claude Code ever adds one, should replace this outright.
pub fn list_subagents(claude_projects_dir: &Path, cwd: &str, parent_session_id: &str) -> Vec<SubagentInfo> {
    let dir = claude_projects_dir.join(sanitize_cwd(cwd)).join(parent_session_id).join("subagents");
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };

    let mut result = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        let Some(id) = stem.strip_prefix("agent-") else { continue };

        let meta_path = path.with_extension("meta.json");
        let meta: serde_json::Value = std::fs::read_to_string(&meta_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or(serde_json::Value::Null);
        let description = meta.get("description").and_then(|d| d.as_str()).unwrap_or("").trim().to_string();

        let contents = std::fs::read_to_string(&path).unwrap_or_default();
        let lines: Vec<String> = contents.lines().map(|s| s.to_string()).collect();
        let prompt = extract_initiating_prompt(&lines);
        let activity = extract_latest_activity(&lines);

        // Prefer the parent's own description of why it spawned this
        // subagent (real, human-authored context) over the subagent's own
        // first prompt line for the title; fall back sensibly either way.
        let title_source = if !description.is_empty() { Some(description.clone()) } else { prompt.clone() };
        let title = match title_source {
            Some(s) => s.split_whitespace().take(4).collect::<Vec<_>>().join(" "),
            None => "Subagent".to_string(),
        };
        let desc = activity.or(prompt).unwrap_or_else(|| description.clone());

        let is_active = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|modified| std::time::SystemTime::now().duration_since(modified).ok())
            .map(|age| age.as_secs() < SUBAGENT_ACTIVE_WINDOW_SECS)
            .unwrap_or(false);

        let metrics = extract_usage_metrics(&path).unwrap_or_default();

        result.push(SubagentInfo {
            id: id.to_string(),
            title,
            desc,
            state: if is_active { "Working" } else { "Done" }.to_string(),
            tokens: metrics.tokens,
            cost: metrics.cost,
            ctx_used: metrics.ctx_used,
            ctx_max: metrics.ctx_max,
        });
    }

    // Stable, deterministic ordering — file listing order isn't guaranteed
    // across platforms/filesystems.
    result.sort_by(|a, b| a.id.cmp(&b.id));
    result
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
    fn extracts_entrypoint_from_a_real_transcript_line() {
        let lines = vec![serde_json::json!({
            "type":"user",
            "message":{"role":"user","content":"this is newwwww"},
            "entrypoint":"claude-vscode",
            "sessionId":"8c58db7c-7f3f-41c4-a6fe-e1c6c6dec819"
        })
        .to_string()];
        assert_eq!(extract_entrypoint(&lines), Some("claude-vscode".to_string()));
    }

    #[test]
    fn extract_entrypoint_returns_none_when_no_line_carries_it() {
        let lines = vec![serde_json::json!({"type":"user","message":{"content":"hi"}}).to_string()];
        assert_eq!(extract_entrypoint(&lines), None);
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

    /// Real-shaped fixture (mirrors actual transcript `usage` objects
    /// observed on this machine): `output_tokens`/`cache_creation_input_tokens`
    /// sum across turns, `ctx_used` reflects only the latest turn.
    #[test]
    fn extract_usage_metrics_sums_deltas_and_uses_latest_turn_for_ctx() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let content = [
            serde_json::json!({"type":"assistant","message":{"model":"claude-sonnet-5","content":[],
                "usage":{"input_tokens":2,"output_tokens":83,"cache_creation_input_tokens":13474,"cache_read_input_tokens":29502}}}).to_string(),
            serde_json::json!({"type":"assistant","message":{"model":"claude-sonnet-5","content":[],
                "usage":{"input_tokens":2,"output_tokens":219,"cache_creation_input_tokens":4200,"cache_read_input_tokens":42976}}}).to_string(),
        ]
        .join("\n")
            + "\n";
        std::fs::write(&path, content).unwrap();

        let metrics = extract_usage_metrics(&path).expect("should extract metrics from real-shaped usage");
        assert_eq!(metrics.tokens, 83 + 13474 + 219 + 4200, "tokens = sum of output + cache_creation across turns");
        // ctx_used comes from the LATEST turn only: cache_read + cache_creation + input.
        assert_eq!(metrics.ctx_used, 42976 + 4200 + 2);
        assert_eq!(metrics.ctx_max, 1_000_000, "sonnet-5 gets the 1M ceiling");
        assert!(metrics.cost > 0.0);
    }

    #[test]
    fn extract_usage_metrics_ignores_the_synthetic_placeholder_turn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let content = [
            serde_json::json!({"type":"assistant","message":{"model":"claude-sonnet-5","content":[],
                "usage":{"input_tokens":2,"output_tokens":100,"cache_creation_input_tokens":500,"cache_read_input_tokens":1000}}}).to_string(),
            // A trailing synthetic turn (all-zero usage) must not become "the
            // latest turn" and wipe out the real ctx_used/ctx_max above.
            serde_json::json!({"type":"assistant","message":{"model":"<synthetic>",
                "usage":{"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}).to_string(),
        ]
        .join("\n")
            + "\n";
        std::fs::write(&path, content).unwrap();

        let metrics = extract_usage_metrics(&path).unwrap();
        assert_eq!(metrics.ctx_used, 1000 + 500 + 2, "the synthetic turn must be skipped, not treated as latest");
        assert_eq!(metrics.tokens, 100 + 500);
    }

    #[test]
    fn extract_usage_metrics_uses_haikus_smaller_context_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(
            &path,
            serde_json::json!({"type":"assistant","message":{"model":"claude-haiku-4-5",
                "usage":{"input_tokens":1,"output_tokens":10,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}).to_string()
                + "\n",
        )
        .unwrap();

        assert_eq!(extract_usage_metrics(&path).unwrap().ctx_max, 200_000);
    }

    #[test]
    fn extract_usage_metrics_returns_none_for_a_session_with_no_usage_yet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        std::fs::write(&path, serde_json::json!({"type":"user","message":{"content":"hi"}}).to_string() + "\n").unwrap();

        assert_eq!(extract_usage_metrics(&path), None);
    }

    /// Mirrors the real on-disk layout confirmed on this machine:
    /// `<projects-dir>/<sanitized-cwd>/<parent-id>/subagents/agent-<id>.jsonl`
    /// plus a sidecar `.meta.json`.
    #[test]
    fn list_subagents_reads_the_real_directory_layout_and_meta_json() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/Users/omricohen/Desktop/sessionboard";
        let subs_dir = root.path().join(sanitize_cwd(cwd)).join("parent-1").join("subagents");
        std::fs::create_dir_all(&subs_dir).unwrap();

        std::fs::write(
            subs_dir.join("agent-abc.jsonl"),
            serde_json::json!({"type":"user","message":{"role":"user","content":"Find the flaky test"}}).to_string() + "\n"
                + &(serde_json::json!({"type":"assistant","message":{"model":"claude-sonnet-5","content":[{"type":"text","text":"Investigating retries in the CI log"}],
                    "usage":{"input_tokens":2,"output_tokens":50,"cache_creation_input_tokens":100,"cache_read_input_tokens":200}}}).to_string() + "\n"),
        )
        .unwrap();
        std::fs::write(
            subs_dir.join("agent-abc.meta.json"),
            serde_json::json!({"agentType":"general-purpose","description":"Investigate flaky CI test","spawnDepth":1}).to_string(),
        )
        .unwrap();

        let subs = list_subagents(root.path(), cwd, "parent-1");
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].id, "abc");
        assert_eq!(subs[0].title, "Investigate flaky CI test");
        assert_eq!(subs[0].tokens, 50 + 100);
        assert_eq!(subs[0].ctx_max, 1_000_000);
        // Just written, so mtime is fresh — must read as still active.
        assert_eq!(subs[0].state, "Working");
    }

    #[test]
    fn list_subagents_returns_empty_when_no_subagents_directory_exists() {
        let root = tempfile::tempdir().unwrap();
        assert!(list_subagents(root.path(), "/nowhere", "parent-1").is_empty());
    }

    #[test]
    fn list_subagents_falls_back_to_meta_description_absent_uses_transcript_prompt() {
        let root = tempfile::tempdir().unwrap();
        let cwd = "/x";
        let subs_dir = root.path().join(sanitize_cwd(cwd)).join("parent-1").join("subagents");
        std::fs::create_dir_all(&subs_dir).unwrap();

        // No .meta.json at all for this one — title/desc must fall back to
        // the subagent's own transcript content instead of panicking.
        std::fs::write(
            subs_dir.join("agent-xyz.jsonl"),
            serde_json::json!({"type":"user","message":{"role":"user","content":"Refactor the auth middleware now"}}).to_string() + "\n",
        )
        .unwrap();

        let subs = list_subagents(root.path(), cwd, "parent-1");
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].id, "xyz");
        assert_eq!(subs[0].title, "Refactor the auth middleware");
        assert_eq!(subs[0].tokens, 0, "no usage lines in this fixture");
    }
}
