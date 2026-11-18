//! Reading the agents' JSONL transcripts (`transcripts/jsonl.ts`): a byte
//! range, its whole lines, a title-sized line, and where Claude keeps a
//! folder's conversations. Read-only: a transcript is only ever opened to
//! read.

use super::js;
use serde_json::{Map, Value};
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// How much of a transcript's start and end is read; never the whole file.
pub const CHUNK: u64 = 64 * 1024;
/// A Codex `session_meta` line carries the whole system prompt.
pub const META_CHUNK: u64 = 256 * 1024;
/// The longest title a rail row shows, in characters.
const TITLE_MAX: usize = 120;

/// One parsed line of a transcript.
pub type Line = Map<String, Value>;

/// Up to `length` bytes of `file` from byte `start`.
pub fn read_range(file: &Path, start: u64, length: u64) -> io::Result<Vec<u8>> {
    let mut handle = File::open(file)?;
    handle.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    handle.take(length).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// The line `part` holds, if it is a JSON object.
pub fn parse_line(part: &[u8]) -> Option<Line> {
    match serde_json::from_str(&String::from_utf8_lossy(part)) {
        Ok(Value::Object(line)) => Some(line),
        _ => None,
    }
}

/// The whole lines of a chunk, parsed.
///
/// A chunk cut mid-line loses that line: the first one of a tail, the last
/// one of a head. A line that is not JSON is skipped.
///
/// `head`: the chunk is a file's start, not its end.
pub fn lines(bytes: &[u8], head: bool) -> Vec<Line> {
    let mut parts: Vec<&[u8]> = bytes.split(|&b| b == b'\n').collect();
    if head {
        parts.pop();
    } else if !parts.is_empty() {
        parts.remove(0);
    }
    parts.into_iter().filter_map(parse_line).collect()
}

/// One line, whitespace collapsed, short enough for a rail row.
pub fn tidy(text: &str) -> String {
    tidy_max(text, TITLE_MAX)
}

/// [`tidy`] with another length, in UTF-16 units.
pub fn tidy_max(text: &str, max: usize) -> String {
    let units = js::collapse_space(&js::utf16(text));
    let flat = js::trim_units(&units);
    if flat.len() > max {
        let mut cut = js::from_utf16(&flat[..max - 1]);
        cut.push('…');
        cut
    } else {
        js::from_utf16(flat)
    }
}

/// Claude's folder name for `cwd`: every character but [A-Za-z0-9] is `-`
/// (one for each UTF-16 unit, as JavaScript's `replace` sees it).
pub fn claude_project_dir(cwd: &str) -> String {
    let mut out = String::with_capacity(cwd.len());
    for c in cwd.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else {
            for _ in 0..c.len_utf16() {
                out.push('-');
            }
        }
    }
    out
}
