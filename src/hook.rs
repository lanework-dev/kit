//! `lwk hook on|off|status`: Claude Code's PreToolUse hook for Bash
//! (`lwk rewrite`, `rewrite.rs`) in `~/.claude/settings.json` (or
//! `$CLAUDE_CONFIG_DIR/settings.json`). Off until the user turns it on.
//!
//! - On: one `PreToolUse` group, matcher `Bash`, running
//!   `<app home>/bin/lwk rewrite` (else this binary), appended after the
//!   user's own groups. Already there: the file is left alone.
//! - Off: every handler whose command is an `lwk rewrite` is removed, and a
//!   group or event left empty by that goes with it. Nothing else changes;
//!   every key keeps its place.
//! - A settings file that is not a JSON object is refused, never repaired.
//! - The write is atomic (temp file in the same folder, then rename), keeps
//!   the file's mode, follows a symlink to the file it names, and never goes
//!   over a change made since the read.

use crate::paths;
use serde_json::{Map, Value};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// How long Claude Code waits for the hook, in seconds.
const HOOK_TIMEOUT: u64 = 5;

const USAGE: &str = "Usage: lwk hook on|off|status";

/// Where Claude Code's user settings live.
fn settings_path() -> PathBuf {
    let dir = match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => paths::home_dir().unwrap_or_default().join(".claude"),
    };
    dir.join("settings.json")
}

/// Whether a hook handler is ours: its command runs `lwk rewrite`
/// (`(^|/)lwk(-[a-z0-9]+)?'? rewrite$`).
fn is_rewrite_hook(entry: &Value) -> bool {
    let Some(command) = entry.get("command").and_then(Value::as_str) else {
        return false;
    };
    let Some(rest) = command.strip_suffix(" rewrite") else {
        return false;
    };
    let rest = rest.strip_suffix('\'').unwrap_or(rest);
    let name = rest.rsplit('/').next().unwrap_or(rest);
    // Without a slash, the command must start with the name.
    let Some(suffix) = name.strip_prefix("lwk") else {
        return false;
    };
    suffix.is_empty()
        || suffix.strip_prefix('-').is_some_and(|arch| {
            !arch.is_empty()
                && arch
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}

/// Whether `settings` run our hook on Bash's PreToolUse.
fn has_rewrite_hook(settings: &Map<String, Value>) -> bool {
    settings
        .get("hooks")
        .and_then(|hooks| hooks.get("PreToolUse"))
        .and_then(Value::as_array)
        .is_some_and(|groups| {
            groups.iter().any(|group| {
                group.get("matcher").and_then(Value::as_str) == Some("Bash")
                    && group
                        .get("hooks")
                        .and_then(Value::as_array)
                        .is_some_and(|hooks| hooks.iter().any(is_rewrite_hook))
            })
        })
}

/// `settings` without any `lwk rewrite` handler, empty shells dropped.
fn without_rewrite_hook(settings: &Map<String, Value>) -> Map<String, Value> {
    let Some(hooks) = settings.get("hooks").and_then(Value::as_object) else {
        return settings.clone();
    };
    let Some(groups) = hooks.get("PreToolUse").and_then(Value::as_array) else {
        return settings.clone();
    };
    let mut kept: Vec<Value> = Vec::new();
    for group in groups {
        let Some(entries) = group.get("hooks").and_then(Value::as_array) else {
            kept.push(group.clone());
            continue;
        };
        if !entries.iter().any(is_rewrite_hook) {
            kept.push(group.clone());
            continue;
        }
        let entries: Vec<Value> = entries
            .iter()
            .filter(|entry| !is_rewrite_hook(entry))
            .cloned()
            .collect();
        if !entries.is_empty() {
            let mut group = group.as_object().cloned().unwrap_or_default();
            group.insert("hooks".into(), Value::Array(entries));
            kept.push(Value::Object(group));
        }
    }
    // Every key keeps its place; an emptied PreToolUse, then hooks, goes.
    let mut new_hooks = Map::new();
    for (event, value) in hooks {
        if event != "PreToolUse" {
            new_hooks.insert(event.clone(), value.clone());
        } else if !kept.is_empty() {
            new_hooks.insert(event.clone(), Value::Array(kept.clone()));
        }
    }
    let mut next = Map::new();
    for (key, value) in settings {
        if key != "hooks" {
            next.insert(key.clone(), value.clone());
        } else if !new_hooks.is_empty() {
            next.insert(key.clone(), Value::Object(new_hooks.clone()));
        }
    }
    next
}

/// `settings` with our hook last in Bash's PreToolUse.
fn with_rewrite_hook(settings: &Map<String, Value>, command: &str) -> Map<String, Value> {
    let clean = without_rewrite_hook(settings);
    let mut hooks = match clean.get("hooks") {
        Some(Value::Object(hooks)) => hooks.clone(),
        _ => Map::new(),
    };
    let mut groups = match hooks.get("PreToolUse") {
        Some(Value::Array(groups)) => groups.clone(),
        _ => Vec::new(),
    };
    let mut handler = Map::new();
    handler.insert("type".into(), "command".into());
    handler.insert("command".into(), command.into());
    handler.insert("timeout".into(), HOOK_TIMEOUT.into());
    let mut group = Map::new();
    group.insert("matcher".into(), "Bash".into());
    group.insert("hooks".into(), Value::Array(vec![Value::Object(handler)]));
    groups.push(Value::Object(group));
    // insert keeps the key's place when it exists, else appends.
    hooks.insert("PreToolUse".into(), Value::Array(groups));
    let mut next = clean;
    next.insert("hooks".into(), Value::Object(hooks));
    next
}

/// `value` for `sh`, single-quoted.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The command the hook runs: the installed lwk, else this binary.
fn hook_command() -> Result<String, String> {
    let installed = paths::app_home().join("bin").join("lwk");
    let lwk = if installed.exists() {
        installed
    } else {
        paths::self_path().ok_or("cannot tell where lwk is installed")?
    };
    Ok(format!("{} rewrite", shell_quote(&lwk.to_string_lossy())))
}

/// The settings file's text, or None when there is no file.
fn read_text(path: &Path) -> Result<Option<String>, String> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// `text` as a settings object; a missing or blank file is `{}`.
fn parse_settings(text: Option<&str>, path: &Path) -> Result<Map<String, Value>, String> {
    let refused = || format!("{} is not a JSON object; fix it first. Nothing was changed.", path.display());
    match text {
        None => Ok(Map::new()),
        Some(text) if text.trim().is_empty() => Ok(Map::new()),
        Some(text) => match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(map)) => Ok(map),
            _ => Err(refused()),
        },
    }
}

/// Write `text` over `path` atomically: a temp file beside it, the mode
/// kept, then a rename. A symlink is written through. Refuses when the file
/// is no longer `expected` (something else changed it since the read).
fn write_atomic(path: &Path, text: &str, expected: Option<&str>) -> Result<(), String> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let tmp = dir.join(format!(
        ".settings.json.lwk-{}.tmp",
        std::process::id()
    ));
    let result = (|| -> std::io::Result<()> {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        if let Ok(meta) = fs::metadata(&target) {
            fs::set_permissions(&tmp, meta.permissions())?;
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(format!("cannot write {}: {e}", target.display()));
    }
    // Last look before the swap: still what we read?
    let now = read_text(&target).unwrap_or(None);
    if now.as_deref() != expected {
        let _ = fs::remove_file(&tmp);
        return Err(format!(
            "{} changed while lwk was working on it; run the command again. Nothing was changed.",
            target.display()
        ));
    }
    fs::rename(&tmp, &target).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("cannot replace {}: {e}", target.display())
    })
}

/// Change the settings file; None from `change` leaves it alone. Returns
/// whether it wrote.
fn update(
    path: &Path,
    change: impl Fn(&Map<String, Value>) -> Result<Option<Map<String, Value>>, String>,
) -> Result<bool, String> {
    let text = read_text(path)?;
    let settings = parse_settings(text.as_deref(), path)?;
    let Some(next) = change(&settings)? else {
        return Ok(false);
    };
    let mut out = serde_json::to_string_pretty(&Value::Object(next))
        .map_err(|e| format!("cannot serialize settings: {e}"))?;
    out.push('\n');
    write_atomic(path, &out, text.as_deref())?;
    Ok(true)
}

fn execute(args: &[String]) -> Result<(), String> {
    if args.len() > 1 {
        return Err(USAGE.into());
    }
    let path = settings_path();
    match args.first().map(String::as_str) {
        None | Some("status") => {
            let text = read_text(&path)?;
            let settings = parse_settings(text.as_deref(), &path)
                .map_err(|_| format!("{} is not a JSON object.", path.display()))?;
            let state = if has_rewrite_hook(&settings) { "on" } else { "off" };
            crate::out(&format!("{state}  {}\n", path.display()));
            Ok(())
        }
        Some("on") => {
            let command = hook_command()?;
            let wrote = update(&path, |settings| {
                Ok(if has_rewrite_hook(settings) {
                    None
                } else {
                    Some(with_rewrite_hook(settings, &command))
                })
            })?;
            crate::out(&format!(
                "{}: plain recursive grep runs as lwk grep in Claude Code ({}).\n",
                if wrote { "Turned on" } else { "Already on" },
                path.display()
            ));
            Ok(())
        }
        Some("off") => {
            let wrote = update(&path, |settings| {
                let next = without_rewrite_hook(settings);
                Ok(if &next == settings { None } else { Some(next) })
            })?;
            crate::out(&format!(
                "{} ({}).\n",
                if wrote { "Turned off" } else { "Already off" },
                path.display()
            ));
            Ok(())
        }
        Some(_) => Err(USAGE.into()),
    }
}

/// Run `lwk hook ARGS`. Returns the exit code: 0 done, 2 a usage error or an
/// unreadable settings file.
pub fn run(args: &[String]) -> i32 {
    match execute(args) {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("lwk hook: {message}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn recognises_our_command() {
        for yes in ["'/a/b/lwk' rewrite", "/x/lwk rewrite", "lwk rewrite", "/x/lwk-arm64 rewrite"] {
            assert!(is_rewrite_hook(&json!({ "command": yes })), "{yes}");
        }
        for no in ["/x/mylwk rewrite", "lwk rewrite --x", "other rewrite", "/x/lwk-A rewrite"] {
            assert!(!is_rewrite_hook(&json!({ "command": no })), "{no}");
        }
    }

    #[test]
    fn on_off_round_trip_keeps_the_rest() {
        let start = obj(json!({
            "zeta": 1,
            "hooks": {
                "Stop": [{"hooks": [{"type": "command", "command": "x"}]}],
                "PreToolUse": [{"matcher": "Bash", "hooks": [{"type": "command", "command": "mine"}]}]
            },
            "alpha": 2
        }));
        let on = with_rewrite_hook(&start, "'/h/lwk' rewrite");
        assert!(has_rewrite_hook(&on));
        assert_eq!(without_rewrite_hook(&on), start);
        let keys: Vec<_> = on.keys().cloned().collect();
        assert_eq!(keys, ["zeta", "hooks", "alpha"]);
    }

    #[test]
    fn off_drops_empty_shells() {
        let on = with_rewrite_hook(&Map::new(), "'/h/lwk' rewrite");
        assert_eq!(without_rewrite_hook(&on), Map::new());
    }
}
