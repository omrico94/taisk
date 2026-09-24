//! Idempotent merge of taisk's hook entries into Claude Code's
//! `~/.claude/settings.json` (plan §3). Operates on `serde_json::Value`
//! rather than a strict struct so any fields we don't model (the user's own
//! settings, other tools' hooks, etc.) survive completely untouched.

use std::path::Path;

use serde_json::{Value, json};

/// (settings.json event key, hook-bridge argv suffix). `PreToolUse`/
/// `PostToolUse` and `Notification`/`Stop`/`SessionEnd` are the events the
/// state machine (see `state.rs`) actually reacts to; `SessionStart` is how a
/// new session is registered in the first place. `PermissionRequest` drives
/// `Waiting` the instant Claude Code is about to show an approval dialog —
/// see its handling in `orchestrator.rs` for why `Notification`'s
/// `permission_prompt` type alone isn't good enough (it's gated behind ~6s
/// of user inactivity, so most approvals never trigger it at all).
pub const HOOK_EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session-start"),
    ("Notification", "notification"),
    ("PreToolUse", "pre-tool-use"),
    ("PostToolUse", "post-tool-use"),
    ("PermissionRequest", "permission-request"),
    ("Stop", "stop"),
    ("SessionEnd", "session-end"),
];

fn command_for(hook_bridge_path: &str, arg: &str, board: Option<&str>) -> String {
    match board {
        Some(id) => format!("{hook_bridge_path} {arg} --board {id}"),
        None => format!("{hook_bridge_path} {arg}"),
    }
}

fn is_ours(entry_command: &str, hook_bridge_path: &str) -> bool {
    entry_command.contains(hook_bridge_path)
}

/// Removes hook-bridge entries whose binary no longer exists on disk (a
/// deleted worktree, an old bundle path). They can never run — each one just
/// prints "No such file or directory" into every Claude Code session — so
/// dropping them can't affect any working registration, including other
/// checkouts' live ones (those paths still exist and are left untouched).
pub fn prune_dead_hooks(settings: &mut Value) {
    let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return;
    };
    for (_event, groups_val) in hooks.iter_mut() {
        let Some(groups) = groups_val.as_array_mut() else { continue };
        for group in groups.iter_mut() {
            if let Some(hs) = group.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                hs.retain(|h| {
                    let Some(cmd) = h.get("command").and_then(|c| c.as_str()) else { return true };
                    let bin = cmd.split_whitespace().next().unwrap_or("");
                    let is_bridge = std::path::Path::new(bin).file_name().is_some_and(|n| n == "hook-bridge");
                    !(is_bridge && std::path::Path::new(bin).is_absolute() && !std::path::Path::new(bin).exists())
                });
            }
        }
        groups.retain(|g| {
            g.get("hooks").and_then(|h| h.as_array()).map(|hs| !hs.is_empty()).unwrap_or(true)
        });
    }
}

/// Adds our hook entries for every event in `HOOK_EVENTS`, skipping any event
/// that already has one of our entries (idempotent — safe to call on every
/// app start). Never touches or removes any other entry.
pub fn merge_hooks(settings: &mut Value, hook_bridge_path: &str) {
    merge_hooks_for_board(settings, hook_bridge_path, None);
}

/// Same as `merge_hooks`, but tags each command with `--board <id>` so the
/// hook-bridge can say which board's config directory the event came from.
/// `None` is the default board (bare command).
pub fn merge_hooks_for_board(settings: &mut Value, hook_bridge_path: &str, board: Option<&str>) {
    if !settings.is_object() {
        *settings = json!({});
    }
    let root = settings.as_object_mut().unwrap();
    let hooks = root.entry("hooks").or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let hooks = hooks.as_object_mut().unwrap();

    for (event_key, arg) in HOOK_EVENTS {
        let matcher_groups = hooks.entry(*event_key).or_insert_with(|| json!([]));
        if !matcher_groups.is_array() {
            *matcher_groups = json!([]);
        }
        let groups = matcher_groups.as_array_mut().unwrap();

        let already_present = groups.iter().any(|group| {
            group
                .get("hooks")
                .and_then(|h| h.as_array())
                .map(|hs| {
                    hs.iter().any(|h| {
                        h.get("command")
                            .and_then(|c| c.as_str())
                            .is_some_and(|c| is_ours(c, hook_bridge_path))
                    })
                })
                .unwrap_or(false)
        });

        if !already_present {
            groups.push(json!({
                "matcher": "",
                "hooks": [
                    { "type": "command", "command": command_for(hook_bridge_path, arg, board) }
                ]
            }));
        }
    }
}

/// Removes only the hook entries whose `command` matches `hook_bridge_path`,
/// leaving everything else (including other matcher-groups on the same
/// event, and unrelated top-level settings) untouched. Used for uninstall
/// and for repairing a stale entry from a previous binary path/version.
pub fn remove_our_hooks(settings: &mut Value, hook_bridge_path: &str) {
    let Some(hooks) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
        return;
    };

    for (_event_key, groups_val) in hooks.iter_mut() {
        let Some(groups) = groups_val.as_array_mut() else {
            continue;
        };
        for group in groups.iter_mut() {
            if let Some(hs) = group.get_mut("hooks").and_then(|h| h.as_array_mut()) {
                hs.retain(|h| {
                    !h.get("command")
                        .and_then(|c| c.as_str())
                        .is_some_and(|c| is_ours(c, hook_bridge_path))
                });
            }
        }
        groups.retain(|group| {
            group
                .get("hooks")
                .and_then(|h| h.as_array())
                .map(|hs| !hs.is_empty())
                .unwrap_or(false)
        });
    }
}

/// Reads `path` (treating a missing file as `{}`), merges our hook entries
/// in, and writes the result back atomically (temp file + rename, so a crash
/// mid-write can't corrupt the user's real settings file).
pub fn apply_to_file(path: &Path, hook_bridge_path: &str) -> std::io::Result<()> {
    apply_to_file_for_board(path, hook_bridge_path, None)
}

pub fn apply_to_file_for_board(path: &Path, hook_bridge_path: &str, board: Option<&str>) -> std::io::Result<()> {
    let mut settings: Value = if path.exists() {
        let raw = std::fs::read_to_string(path)?;
        serde_json::from_str(&raw).unwrap_or_else(|_| json!({}))
    } else {
        json!({})
    };

    prune_dead_hooks(&mut settings);
    merge_hooks_for_board(&mut settings, hook_bridge_path, board);

    let pretty = serde_json::to_string_pretty(&settings)?;
    let tmp_path = path.with_extension("json.tmp");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&tmp_path, pretty)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Strips our hook entries from the settings file at `path`, if it exists.
pub fn remove_from_file(path: &Path, hook_bridge_path: &str) -> std::io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let mut settings: Value = serde_json::from_str(&std::fs::read_to_string(path)?).unwrap_or_else(|_| json!({}));
    remove_our_hooks(&mut settings, hook_bridge_path);
    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, serde_json::to_string_pretty(&settings)?)?;
    std::fs::rename(&tmp_path, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BRIDGE: &str = "/Applications/taisk.app/Contents/Resources/hook-bridge";

    #[test]
    fn empty_settings_gets_all_events_added() {
        let mut settings = json!({});
        merge_hooks(&mut settings, BRIDGE);

        for (event_key, arg) in HOOK_EVENTS {
            let groups = settings["hooks"][event_key].as_array().unwrap();
            assert_eq!(groups.len(), 1, "expected exactly one matcher-group for {event_key}");
            let cmd = groups[0]["hooks"][0]["command"].as_str().unwrap();
            assert_eq!(cmd, format!("{BRIDGE} {arg}"));
        }
    }

    #[test]
    fn unrelated_existing_hooks_are_preserved() {
        let mut settings = json!({
            "hooks": {
                "SessionStart": [
                    { "matcher": "", "hooks": [ { "type": "command", "command": "some-other-tool session-start" } ] }
                ]
            },
            "someOtherTopLevelSetting": { "foo": 42 }
        });
        merge_hooks(&mut settings, BRIDGE);

        let groups = settings["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(groups.len(), 2, "our entry should be added alongside the existing unrelated one");
        assert_eq!(groups[0]["hooks"][0]["command"], "some-other-tool session-start");
        assert_eq!(settings["someOtherTopLevelSetting"]["foo"], 42);
    }

    #[test]
    fn rerunning_merge_is_a_no_op() {
        let mut settings = json!({});
        merge_hooks(&mut settings, BRIDGE);
        let after_first = settings.clone();
        merge_hooks(&mut settings, BRIDGE);
        assert_eq!(settings, after_first, "merging twice must be idempotent");
    }

    #[test]
    fn existing_entry_at_the_same_path_is_recognized_and_not_duplicated() {
        // A previous install at the exact same path is recognized, not duplicated.
        let stale_bridge = BRIDGE;
        let mut settings = json!({
            "hooks": {
                "SessionStart": [
                    { "matcher": "", "hooks": [ { "type": "command", "command": format!("{stale_bridge} session-start") } ] }
                ]
            }
        });
        merge_hooks(&mut settings, BRIDGE);
        let groups = settings["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "the existing entry should be recognized as ours, not duplicated");
    }

    #[test]
    fn remove_our_hooks_leaves_unrelated_entries_and_settings_untouched() {
        let mut settings = json!({
            "hooks": {
                "SessionStart": [
                    { "matcher": "", "hooks": [ { "type": "command", "command": "some-other-tool session-start" } ] }
                ]
            }
        });
        merge_hooks(&mut settings, BRIDGE);
        remove_our_hooks(&mut settings, BRIDGE);

        let groups = settings["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(groups.len(), 1, "only our entry should be removed");
        assert_eq!(groups[0]["hooks"][0]["command"], "some-other-tool session-start");

        // Events with only our entries end up with an empty (but present) array.
        assert_eq!(settings["hooks"]["Notification"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn apply_to_file_creates_missing_file_and_is_atomic_on_rerun() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert!(!path.exists());

        apply_to_file(&path, BRIDGE).unwrap();
        assert!(path.exists());
        let first: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(first["hooks"]["SessionStart"].as_array().unwrap().len(), 1);

        // Re-running against the now-existing file must not duplicate entries.
        apply_to_file(&path, BRIDGE).unwrap();
        let second: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(second["hooks"]["SessionStart"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn board_tagged_merge_adds_the_board_arg_and_stays_idempotent() {
        let mut settings = json!({});
        merge_hooks_for_board(&mut settings, BRIDGE, Some("work"));
        for (event_key, arg) in HOOK_EVENTS {
            let cmd = settings["hooks"][event_key][0]["hooks"][0]["command"].as_str().unwrap();
            assert_eq!(cmd, format!("{BRIDGE} {arg} --board work"));
        }
        let after_first = settings.clone();
        merge_hooks_for_board(&mut settings, BRIDGE, Some("work"));
        assert_eq!(settings, after_first);
    }

    #[test]
    fn remove_from_file_strips_only_our_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        apply_to_file_for_board(&path, BRIDGE, Some("work")).unwrap();
        remove_from_file(&path, BRIDGE).unwrap();
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after["hooks"]["SessionStart"].as_array().unwrap().len(), 0);
        // A missing file is a no-op, not an error.
        remove_from_file(&dir.path().join("nope.json"), BRIDGE).unwrap();
    }

    #[test]
    fn prune_removes_only_entries_whose_binary_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("hook-bridge");
        std::fs::write(&live, "").unwrap();
        let live = live.to_string_lossy().to_string();
        let dead = "/nonexistent/worktree/hook-bridge";
        let mut settings = json!({"hooks": {"Stop": [
            {"matcher": "", "hooks": [{"type": "command", "command": format!("{live} stop")}]},
            {"matcher": "", "hooks": [{"type": "command", "command": format!("{dead} stop")}]},
            {"matcher": "", "hooks": [{"type": "command", "command": "some-other-tool stop"}]}
        ]}});
        prune_dead_hooks(&mut settings);
        let groups = settings["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert!(groups[0]["hooks"][0]["command"].as_str().unwrap().starts_with(&live));
        assert_eq!(groups[1]["hooks"][0]["command"], "some-other-tool stop");
    }
}
