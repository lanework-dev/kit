//! What was said in a transcript line, and the snippet that shows it
//! (`search.ts`'s "What was said, and the snippet").
//!
//! Only what was said is matched: prompts, answers, tool calls and their
//! output. A line that matched only on its metadata (a path, an id, the
//! system prompt) is not a match.

use super::js::{self, Finder};
use super::jsonl::Line;
use super::Kind;
use crate::engine::types::MatchLine;
use serde_json::Value;

/// A snippet's length around its match, in characters.
const SNIPPET_MAX: usize = 200;
/// How much of a snippet comes before its match.
const SNIPPET_LEAD: usize = 60;
/// `…`, as a UTF-16 unit.
const ELLIPSIS: u16 = 0x2026;

/// Keys whose values are ids, types, signatures or base64, never words.
const NOT_SAID: [&str; 13] = [
    "id",
    "type",
    "uuid",
    "parentUuid",
    "tool_use_id",
    "call_id",
    "signature",
    "data",
    "encrypted_content",
    "media_type",
    "model",
    "cwd",
    "providerID",
];

/// Context a CLI put in a turn for itself (`<system-reminder>`, Codex's
/// `<environment_context>`, AGENTS.md), not what was said: `/^(<[a-z][\w-]*[\s>]|# AGENTS\.md)/`.
fn injected(text: &str) -> bool {
    let text = js::trim_start(text);
    if text.starts_with("# AGENTS.md") {
        return true;
    }
    let Some(rest) = text.strip_prefix('<') else {
        return false;
    };
    let mut chars = rest.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_lowercase()) {
        return false;
    }
    chars
        .find(|c| !(c.is_ascii_alphanumeric() || *c == '_' || *c == '-'))
        .is_some_and(|c| c == '>' || js::is_space(c))
}

/// Every string under `value`, but ids, types and base64.
pub fn said<'a>(value: &'a Value, out: &mut Vec<&'a str>, depth: usize) {
    if depth > 8 || out.len() > 200 {
        return;
    }
    match value {
        Value::String(text) => {
            if !injected(text) {
                out.push(text);
            }
        }
        Value::Array(items) => {
            for item in items {
                said(item, out, depth + 1);
            }
        }
        Value::Object(map) => {
            // JavaScript lists an object's index-like keys first.
            if map.keys().any(|key| js::index_key(key).is_some()) {
                let mut entries: Vec<(&String, &Value)> = map.iter().collect();
                entries.sort_by_key(|(key, _)| js::index_key(key).map_or((1, 0), |i| (0, i)));
                for (key, item) in entries {
                    if !NOT_SAID.contains(&key.as_str()) {
                        said(item, out, depth + 1);
                    }
                }
            } else {
                for (key, item) in map {
                    if !NOT_SAID.contains(&key.as_str()) {
                        said(item, out, depth + 1);
                    }
                }
            }
        }
        _ => {}
    }
}

/// The part of a transcript line that holds what was said, if any.
fn spoken(kind: Kind, line: &Line) -> Option<&Value> {
    match kind {
        Kind::Claude => {
            if !matches!(line.get("type")?.as_str()?, "user" | "assistant") {
                return None;
            }
            if line.get("isMeta").is_some_and(js::truthy) {
                return None;
            }
            line.get("message")?.as_object()?.get("content")
        }
        // `session_meta` (the system prompt) and `turn_context` are not.
        Kind::Codex => {
            if !matches!(line.get("type")?.as_str()?, "response_item" | "event_msg") {
                return None;
            }
            line.get("payload")
        }
    }
}

/// A matching transcript line as a snippet: the text around the first match
/// in what was said, whitespace collapsed, with its matches marked.
///
/// `None` when the line matched only outside what was said.
pub fn snippet_of(kind: Kind, raw: &[u8], line: u64, finder: &Finder) -> Option<MatchLine> {
    let parsed = super::jsonl::parse_line(raw)?;
    let mut texts = Vec::new();
    if let Some(value) = spoken(kind, &parsed) {
        said(value, &mut texts, 0);
    }
    snippet_from(&texts, line, finder)
}

/// [`snippet_of`] for strings already picked out.
pub fn snippet_from(texts: &[&str], line: u64, finder: &Finder) -> Option<MatchLine> {
    for text in texts {
        if !finder.prefilter(text.as_bytes()) {
            continue;
        }
        let units = js::utf16(text);
        let Some(start) = finder.find(&units, 0) else {
            continue;
        };
        let end = start + finder.len();
        // A window of the raw text around the match, then tidied: collapsing
        // whitespace in a megabyte of tool output only to cut it is waste.
        let from = start.saturating_sub(SNIPPET_LEAD * 2);
        let to = (end + SNIPPET_MAX * 2).min(units.len());
        let window = js::collapse_space(&units[from..to]);
        let Some(lead) = finder.find(&window, 0) else {
            continue;
        };
        let cut = lead.saturating_sub(SNIPPET_LEAD);
        let window = &window[cut..(cut + SNIPPET_MAX).min(window.len())];
        let before = from > 0 || cut > 0;
        let after = to < units.len() || window.len() >= SNIPPET_MAX;
        let mut body: Vec<u16> = Vec::with_capacity(window.len() + 2);
        if before {
            body.push(ELLIPSIS);
        }
        body.extend_from_slice(js::trim_units(window));
        if after {
            body.push(ELLIPSIS);
        }
        let mut ranges = Vec::new();
        let mut at = 0;
        while let Some(found) = finder.find(&body, at) {
            ranges.push([found, found + finder.len()]);
            at = found + finder.len();
        }
        if ranges.is_empty() {
            continue;
        }
        return Some(MatchLine {
            line,
            text: js::from_utf16(&body),
            ranges,
        });
    }
    None
}
