//! Codex's rollouts: `<codex home>/sessions/YYYY/MM/DD/rollout-….jsonl`.
//!
//! A thread a conversation spawned counts as that conversation; Codex's own
//! helpers (approval reviews) are skipped.

use super::js;
use super::jsonl::{lines, parse_line, read_range, tidy, Line, META_CHUNK};
use super::prompt::typed;
use super::util::parallel_map;
use super::{candidate, is_uuid, Candidate, Described, Kind, Scope};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The most of `session_index.jsonl` read; a bigger one is not one.
const INDEX_MAX: u64 = 16 * 1024 * 1024;

/// Codex's home: `$CODEX_HOME`, as Codex reads it, or `~/.codex`.
pub fn dir(home: &Path) -> PathBuf {
    match std::env::var_os("CODEX_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => home.join(".codex"),
    }
}

fn names_in(path: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(path)
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default()
}

/// Every rollout on the machine: the tree is three folders deep.
fn rollouts(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for year in names_in(root) {
        for month in names_in(&year) {
            for day in names_in(&month) {
                for file in names_in(&day) {
                    let name = file
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                        out.push(file);
                    }
                }
            }
        }
    }
    out
}

/// Codex's rollouts whose `session_meta` names a folder of `scope`: Codex
/// keeps every folder's in one tree.
pub fn candidates(home: &Path, scope: &Scope) -> Vec<Candidate> {
    let files: Vec<Candidate> = rollouts(&dir(home).join("sessions"))
        .into_iter()
        .filter_map(|path| candidate(Kind::Codex, path, None))
        .collect();
    let metas = parallel_map(&files, |file| meta(&file.path));
    files
        .into_iter()
        .zip(metas)
        .filter_map(|(mut file, meta)| {
            let inside = meta
                .as_ref()
                .and_then(|meta| meta.get("cwd"))
                .and_then(Value::as_str)
                .is_some_and(|cwd| scope.contains(cwd));
            file.meta = meta;
            inside.then_some(file)
        })
        .collect()
}

/// A rollout's `session_meta` payload: in the first `META_CHUNK` bytes, on
/// a whole line.
fn meta(path: &Path) -> Option<Line> {
    let mut handle = File::open(path).ok()?;
    let limit = META_CHUNK as usize;
    let mut data: Vec<u8> = Vec::new();
    let mut piece = [0u8; 32 * 1024];
    let mut scanned = 0;
    while data.len() < limit {
        let want = (limit - data.len()).min(piece.len());
        let read = handle.read(&mut piece[..want]).ok()?;
        if read == 0 {
            break;
        }
        data.extend_from_slice(&piece[..read]);
        while let Some(end) = data[scanned..].iter().position(|&b| b == b'\n') {
            let line = &data[scanned..scanned + end];
            scanned += end + 1;
            if !super::js::contains_bytes(line, b"session_meta") {
                continue;
            }
            let Some(line) = parse_line(line) else {
                continue;
            };
            if line.get("type").and_then(Value::as_str) == Some("session_meta") {
                return match line.get("payload") {
                    Some(Value::Object(payload)) => Some(payload.clone()),
                    _ => None,
                };
            }
        }
    }
    None
}

/// What a Codex rollout is, by its `session_meta`.
enum Role {
    Conversation,
    /// A thread a conversation spawned, with the conversation at its root.
    Spawned(String),
    /// Codex's own helper (the `guardian` that assesses approvals, a
    /// review): neither, and never listed.
    Helper,
}

/// What a rollout is, from its `session_meta` payload.
///
/// The root is `session_id`, which a spawned thread's own spawns name too,
/// so nesting flattens to the conversation.
fn role(meta: &Line) -> Role {
    let Some(source) = meta.get("source").and_then(Value::as_object) else {
        return Role::Conversation;
    };
    let Some(subagent) = source.get("subagent") else {
        return Role::Conversation;
    };
    let spawn = subagent.as_object().and_then(|s| s.get("thread_spawn"));
    let Some(spawn) = spawn.filter(|spawn| js::truthy(spawn)) else {
        return Role::Helper;
    };
    let named = |value: Option<&Value>| match value {
        Some(Value::String(id)) if is_uuid(id) => Some(id.clone()),
        _ => None,
    };
    let own = meta.get("id");
    [
        named(meta.get("session_id")),
        named(spawn.as_object().and_then(|s| s.get("parent_thread_id"))),
        named(meta.get("parent_thread_id")),
    ]
    .into_iter()
    .flatten()
    .find(|id| own.and_then(Value::as_str) != Some(id.as_str()))
    .map_or(Role::Helper, Role::Spawned)
}

/// Codex's thread names, by id: `session_index.jsonl`, the last one wins.
pub fn index(home: &Path) -> HashMap<String, String> {
    let mut names = HashMap::new();
    let path = dir(home).join("session_index.jsonl");
    let Ok(info) = std::fs::metadata(&path) else {
        return names;
    };
    if !info.is_file() || info.len() > INDEX_MAX {
        return names;
    }
    let Ok(mut bytes) = std::fs::read(&path) else {
        return names;
    };
    bytes.push(b'\n');
    for line in lines(&bytes, true) {
        if let (Some(Value::String(id)), Some(Value::String(name))) =
            (line.get("id"), line.get("thread_name"))
        {
            names.insert(id.clone(), name.clone());
        }
    }
    names
}

/// A rollout's conversation, title and folder; `None` for a helper's.
pub fn described(file: &Candidate, names: &HashMap<String, String>) -> Option<Described> {
    let meta = file.meta.as_ref()?;
    let id = meta.get("id")?.as_str().filter(|id| is_uuid(id))?;
    let role = role(meta);
    let session_id = match &role {
        Role::Helper => return None,
        Role::Spawned(root) => root.clone(),
        Role::Conversation => id.to_owned(),
    };
    let mut title = names
        .get(&session_id)
        .filter(|name| !name.is_empty())
        .map(|name| tidy(name));
    if title.is_none() && matches!(role, Role::Conversation) {
        let head = lines(&read_range(&file.path, 0, META_CHUNK).ok()?, true);
        title = head.iter().filter_map(prompt).find(|t| !t.is_empty());
    }
    Some(Described {
        session_id,
        title: title.unwrap_or_else(|| "Codex conversation".to_owned()),
        cwd: meta.get("cwd").and_then(Value::as_str).map(str::to_owned),
    })
}

/// The prompt a rollout line holds, if somebody typed it.
///
/// Older CLIs log it as an `event_msg`; newer ones only as the user's
/// `response_item`, whose first turn is the AGENTS.md Codex injected. That
/// turn is skipped like any tagged context.
fn prompt(line: &Line) -> Option<String> {
    let payload = line.get("payload")?.as_object();
    let kind = line.get("type").and_then(Value::as_str);
    let payload_type = payload.and_then(|p| p.get("type")).and_then(Value::as_str);
    if kind == Some("event_msg") && payload_type == Some("user_message") {
        return payload?.get("message")?.as_str().and_then(typed);
    }
    let payload = payload?;
    if kind != Some("response_item")
        || payload_type != Some("message")
        || payload.get("role").and_then(Value::as_str) != Some("user")
    {
        return None;
    }
    for part in payload.get("content")?.as_array()? {
        if part.get("type").and_then(Value::as_str) != Some("input_text") {
            continue;
        }
        let Some(text) = part.get("text").and_then(Value::as_str) else {
            continue;
        };
        if text.starts_with("# AGENTS.md") {
            return None;
        }
        if let Some(found) = typed(text) {
            return Some(found);
        }
    }
    None
}
