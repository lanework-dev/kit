//! Claude Code's transcripts: `<claude dir>/projects/<folder>/<id>.jsonl`.
//!
//! A sub-agent's own transcript (`<id>/subagents/`) is not searched: what it
//! was asked and what it answered are in its parent's transcript already.

use super::js;
use super::jsonl::{claude_project_dir, lines, read_range, tidy, Line, CHUNK};
use super::{candidate, is_uuid, Candidate, Described, Kind, Scope};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// How far into a transcript its title is looked for when the head holds
/// none: a first prompt with an image pasted in is that long.
const OPENING_MAX: u64 = 4 * 1024 * 1024;

/// Claude Code's own home: `$CLAUDE_CONFIG_DIR`, as it reads it, or
/// `~/.claude`.
pub fn dir(home: &Path) -> PathBuf {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home.join(".claude"),
    }
}

/// Whether the project folder `name` can hold sessions run in `scope`:
/// Claude names it for the folder's path, so a folder inside one is named
/// as that folder, a `-` and more.
fn in_scope(name: &str, scope: &Scope) -> bool {
    scope.claude_dirs().iter().any(|dir| {
        dir == "-" || name == dir || name.strip_prefix(dir.as_str()).is_some_and(|r| r.starts_with('-'))
    })
}

/// Whether `name` is exactly a folder of `scope`'s project folder.
pub fn is_exact(name: &str, scope: &Scope) -> bool {
    scope.claude_dirs().iter().any(|dir| dir == name)
}

/// Claude's transcripts of the project folders `scope` can reach.
pub fn candidates(home: &Path, scope: &Scope) -> Vec<Candidate> {
    let root = dir(home).join("projects");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !in_scope(&name, scope) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(root.join(&name)) else {
            continue;
        };
        for file in files.flatten() {
            let file_name = file.file_name().to_string_lossy().into_owned();
            let Some(id) = file_name.strip_suffix(".jsonl") else {
                continue;
            };
            if is_uuid(id) {
                out.extend(candidate(Kind::Claude, file.path(), None));
            }
        }
    }
    out
}

/// A transcript's id, title and folder, the way the rail names it
/// (`agent-sessions.ts`): its latest `ai-title`, else its first prompt.
pub fn described(file: &Candidate) -> Option<Described> {
    let head = lines(&read_range(&file.path, 0, CHUNK).ok()?, true);
    let tail = if file.size > CHUNK {
        Some(lines(
            &read_range(&file.path, file.size - CHUNK, CHUNK).ok()?,
            false,
        ))
    } else {
        None
    };
    let tail = tail.as_deref().unwrap_or(&head);
    // The latest: the head's last line backwards, then the tail's.
    let named = head
        .iter()
        .rev()
        .chain(tail.iter().rev())
        .find_map(|line| {
            (line.get("type").and_then(Value::as_str) == Some("ai-title"))
                .then(|| line.get("aiTitle").and_then(Value::as_str))
                .flatten()
        })
        .filter(|title| !title.is_empty());
    let cwd = head
        .iter()
        .find_map(|line| line.get("cwd").and_then(Value::as_str))
        .map(str::to_owned);
    let mut title = match named {
        Some(named) => Some(tidy(named)),
        None => head.iter().filter_map(prompt).find(|t| !t.is_empty()),
    }
    .filter(|title| !title.is_empty());
    // A first prompt longer than the head (an image or a long paste in it):
    // read further, as the rail does.
    if title.is_none() {
        let opening = lines(&read_range(&file.path, 0, OPENING_MAX).ok()?, true);
        title = opening
            .iter()
            .filter_map(prompt)
            .find(|t| !t.is_empty())
            .or_else(|| opening.iter().filter_map(image_prompt).find(|t| !t.is_empty()));
    }
    Some(Described {
        session_id: file.path.file_stem()?.to_string_lossy().into_owned(),
        title: title.unwrap_or_else(|| "Untitled conversation".to_owned()),
        cwd,
    })
}

/// Whether a line is a person's own turn: a user line that is no meta line
/// and no sub-agent's.
fn own_user_line(line: &Line) -> bool {
    line.get("type").and_then(Value::as_str) == Some("user")
        && !line.get("isMeta").is_some_and(js::truthy)
        && !line.get("isSidechain").is_some_and(js::truthy)
}

/// The prompt a Claude line holds, if it is one somebody typed.
fn prompt(line: &Line) -> Option<String> {
    if !own_user_line(line) {
        return None;
    }
    match line.get("message")?.as_object()?.get("content")? {
        Value::String(text) => super::prompt::typed(text),
        Value::Array(parts) => {
            for part in parts {
                if part.get("type").and_then(Value::as_str) == Some("text") {
                    return part
                        .get("text")
                        .and_then(Value::as_str)
                        .and_then(super::prompt::typed);
                }
            }
            None
        }
        _ => None,
    }
}

/// A Claude prompt that is only images (a screenshot pasted, nothing
/// typed), named by what it holds: the title of last resort.
fn image_prompt(line: &Line) -> Option<String> {
    if !own_user_line(line) {
        return None;
    }
    let content = line.get("message")?.as_object()?.get("content")?.as_array()?;
    let kind = |part: &Value| part.get("type").and_then(Value::as_str).map(str::to_owned);
    if content
        .iter()
        .any(|part| kind(part).as_deref() == Some("tool_result"))
    {
        return None;
    }
    let images = content
        .iter()
        .filter(|part| kind(part).as_deref() == Some("image"))
        .count();
    match images {
        0 => None,
        1 => Some("Image".to_owned()),
        n => Some(format!("{n} images")),
    }
}

/// Claude's folder name for each folder of `folders`.
pub fn project_dirs(folders: &[String]) -> Vec<String> {
    let mut dirs: Vec<String> = Vec::new();
    for folder in folders {
        let name = claude_project_dir(folder);
        if !dirs.contains(&name) {
            dirs.push(name);
        }
    }
    dirs
}
