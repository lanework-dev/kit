//! A prompt somebody typed, as a title (`agent-sessions.ts`' `typed`).

use super::jsonl::tidy;
use super::js;

/// A prompt somebody typed, tidied for a title.
///
/// What a CLI puts in the user's turn for itself (command caveats, hook
/// output, injected context) is all wrapped in a tag, and is not one.
///
/// `None` for anything but a typed prompt.
pub fn typed(text: &str) -> Option<String> {
    let clean = js::trim(text);
    if clean.is_empty() {
        return None;
    }
    // `/orchestrate` and a message to a conductor: the person's request,
    // inside the prompt the app wrapped it in.
    let orchestrate = clean.starts_with("<lanework_orchestrate");
    if orchestrate || clean.starts_with("<lanework_conductor") {
        let start = clean.rfind("<user_request>\n")?;
        let end = clean.rfind("\n</user_request>")?;
        if end < start {
            return None;
        }
        let request = js::trim(clean.get(start + 15..end).unwrap_or(""));
        if request.is_empty() {
            return None;
        }
        return Some(tidy(&if orchestrate {
            format!("/orchestrate {request}")
        } else {
            mentions_as_names(request)
        }));
    }
    if clean.starts_with('<') {
        return None;
    }
    Some(tidy(&mentions_as_names(clean)))
}

/// `text` with each path mention as the file's name: `@"/Users/me/a b/c.csv"`
/// reads `c.csv` (`mentionsAsNames`; `MENTION` is
/// `/(?<=^|\s)@(?:"((?:[^"\\]|\\.)*)"|(\S+))/g`). A mention with no `/` or
/// `.` in it stays as written.
fn mentions_as_names(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while at < chars.len() {
        if chars[at] == '@' && (at == 0 || js::is_space(chars[at - 1])) {
            if let Some((path, end)) = mention(&chars, at + 1) {
                let whole: String = chars[at..end].iter().collect();
                out.push_str(&mention_name(path, whole));
                at = end;
                continue;
            }
        }
        out.push(chars[at]);
        at += 1;
    }
    out
}

/// The mention after an `@` at `from - 1`: its path (a quoted one with its
/// escapes undone) and where it ends.
fn mention(chars: &[char], from: usize) -> Option<(String, usize)> {
    if chars.get(from) == Some(&'"') {
        let mut path = String::new();
        let mut at = from + 1;
        let mut closed = None;
        while let Some(&c) = chars.get(at) {
            match c {
                '"' => {
                    closed = Some(at + 1);
                    break;
                }
                '\\' => match chars.get(at + 1) {
                    Some(&next) if !matches!(next, '\n' | '\r' | '\u{2028}' | '\u{2029}') => {
                        path.push(next);
                        at += 2;
                    }
                    _ => break,
                },
                _ => {
                    path.push(c);
                    at += 1;
                }
            }
        }
        if let Some(end) = closed {
            return Some((path, end));
        }
    }
    let end = chars[from..]
        .iter()
        .position(|&c| js::is_space(c))
        .map_or(chars.len(), |p| from + p);
    (end > from).then(|| (chars[from..end].iter().collect(), end))
}

fn mention_name(path: String, whole: String) -> String {
    if path.is_empty() || !path.contains(['.', '/']) {
        return whole;
    }
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit('/').next() {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => path,
    }
}
