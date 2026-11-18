//! `search_sessions`: the Claude Code, Codex and OpenCode transcripts under
//! `options.home` that said `query`, newest first (`electron/main/transcripts/search.ts`
//! in Rust; the app's search and this one rank and answer alike).
//!
//! - Claude Code: `<claude dir>/projects/<folder>/<id>.jsonl` ([`claude`]).
//! - Codex: `<codex home>/sessions/YYYY/MM/DD/rollout-….jsonl` ([`codex`]).
//! - OpenCode: its `opencode.db`, both schemas ([`opencode`]).
//!
//! Only what was said is matched: prompts, answers, tool calls and their
//! output. A line that matched only on its metadata (a path, an id, the
//! system prompt) is not a match ([`snippet`]).
//!
//! The query is a literal phrase, matched without regard to case. Only
//! sessions run in one of `options.folders` or in a folder inside one are
//! searched: where the app takes the folders as they are, the contract
//! takes "the folder or inside it".
//!
//! NOTE: Newest transcripts first, in batches that double (128 MB, 256 MB,
//! …); the search stops after the batch that brought `limit` sessions. So a
//! common word answers fast, and a rare one costs a read of the whole
//! history. Read-only: the CLIs' own files are only ever opened to read.

mod claude;
mod codex;
mod js;
mod jsonl;
mod opencode;
mod prompt;
mod scan;
mod snippet;
#[cfg(test)]
mod tests;
mod util;

use super::types::{KitError, SessionHarness, SessionMatch, SessionOptions, SEARCH_SESSIONS_LIMIT};
use js::Finder;
use std::cell::OnceCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The first batch's size in bytes; each next batch is twice the last.
///
/// NOTE: The transcripts are gigabytes. Searched newest first in growing
/// batches, a query with a recent hit answers after the first batch instead
/// of after the whole history.
const FIRST_BATCH_BYTES: u64 = 128 * 1024 * 1024;
/// The whole search, as the app gives it.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Which of the two JSONL stores a transcript is of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Claude,
    Codex,
}

impl Kind {
    fn harness(self) -> SessionHarness {
        match self {
            Kind::Claude => SessionHarness::Claude,
            Kind::Codex => SessionHarness::Codex,
        }
    }
}

/// One transcript file that may be searched.
struct Candidate {
    kind: Kind,
    path: PathBuf,
    mtime_ns: u128,
    size: u64,
    /// A Codex rollout's `session_meta` payload, read when it was picked.
    meta: Option<jsonl::Line>,
}

/// `path` as a candidate; `None` when it is not a readable file.
fn candidate(kind: Kind, path: PathBuf, meta: Option<jsonl::Line>) -> Option<Candidate> {
    let info = std::fs::metadata(&path).ok().filter(|info| info.is_file())?;
    let mtime_ns = info
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos());
    Some(Candidate {
        kind,
        path,
        mtime_ns,
        size: info.len(),
        meta,
    })
}

/// A transcript's session, title and folder.
struct Described {
    session_id: String,
    title: String,
    cwd: Option<String>,
}

/// Whether `text` is a UUID: the only shape of a Claude or Codex session id.
fn is_uuid(text: &str) -> bool {
    let groups: Vec<&str> = text.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(group, len)| group.len() == len && group.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The folders whose sessions are searched: each, and what is inside it.
struct Scope {
    folders: Vec<String>,
}

impl Scope {
    fn new(folders: &[String]) -> Scope {
        let mut out: Vec<String> = Vec::new();
        for folder in folders {
            let folder = match folder.trim_end_matches('/') {
                "" if folder.starts_with('/') => "/",
                trimmed => trimmed,
            };
            if !folder.is_empty() && !out.iter().any(|seen| seen == folder) {
                out.push(folder.to_owned());
            }
        }
        Scope { folders: out }
    }

    /// Whether a session run in `cwd` is in scope: `cwd` is one of the
    /// folders or inside one.
    fn contains(&self, cwd: &str) -> bool {
        self.folders.iter().any(|folder| {
            folder == "/" && cwd.starts_with('/')
                || cwd == folder
                || cwd
                    .strip_prefix(folder.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
        })
    }

    /// Claude's project folder name for each folder.
    fn claude_dirs(&self) -> Vec<String> {
        claude::project_dirs(&self.folders)
    }
}

/// The sessions that said `query`, newest first.
pub fn search_sessions(
    query: &str,
    options: &SessionOptions,
) -> Result<Vec<SessionMatch>, KitError> {
    if js::trim(query).is_empty() {
        return Ok(Vec::new());
    }
    let limit = options.limit.clamp(1, SEARCH_SESSIONS_LIMIT.max);
    let scope = Scope::new(&options.folders);
    if scope.folders.is_empty() {
        return Ok(Vec::new());
    }
    let finder = Finder::new(query);
    let home = options.home.as_path();
    let deadline = Instant::now() + TIMEOUT;

    let mut found = Found::default();
    let mut timed_out = false;
    std::thread::scope(|threads| {
        // OpenCode's search is a database's, and runs while the transcripts
        // are read.
        let opencode = threads
            .spawn(|| opencode::search(home, query, &finder, &scope, limit, deadline));
        let codex = threads.spawn(|| codex::candidates(home, &scope));
        let mut candidates = claude::candidates(home, &scope);
        candidates.extend(codex.join().unwrap_or_default());
        // Newest first; a stable sort, so equal times keep Claude first.
        candidates.sort_by(|a, b| b.mtime_ns.cmp(&a.mtime_ns));

        let names: OnceCell<HashMap<String, String>> = OnceCell::new();
        let mut budget = FIRST_BATCH_BYTES;
        let mut at = 0;
        while at < candidates.len() && found.len() < limit {
            let mut end = at;
            let mut bytes = 0u64;
            while end < candidates.len() && (end == at || bytes < budget) {
                bytes += candidates[end].size;
                end += 1;
            }
            let batch = &candidates[at..end];
            at = end;
            budget = budget.saturating_mul(2);
            if Instant::now() >= deadline {
                timed_out = true;
                break;
            }
            let scanned = scan::search_files(batch, &finder, deadline);
            timed_out = scanned.timed_out;
            for (file, matched) in batch.iter().zip(scanned.lines) {
                if matched.is_empty() {
                    continue;
                }
                if let Some(session) = describe(file, matched, home, &scope, &names) {
                    found.keep(session);
                }
            }
            if timed_out {
                break;
            }
        }
        for session in opencode.join().unwrap_or_default() {
            found.keep(session);
        }
    });

    let mut out = found.sessions;
    // ISO 8601 strings of one shape sort as the times do; the sort is stable.
    out.sort_by(|a, b| b.at.cmp(&a.at));
    out.truncate(limit);
    if timed_out && out.is_empty() {
        return Err(KitError::timeout());
    }
    Ok(out)
}

/// The sessions found so far, one per harness and id.
#[derive(Default)]
struct Found {
    sessions: Vec<SessionMatch>,
    /// By harness (its `Kind` as a byte: `SessionHarness` is not `Hash`) and id.
    index: HashMap<(u8, String), usize>,
}

impl Found {
    fn len(&self) -> usize {
        self.sessions.len()
    }

    fn keep(&mut self, session: SessionMatch) {
        let key = (harness_key(session.harness), session.session_id.clone());
        let Some(&at) = self.index.get(&key) else {
            self.index.insert(key, self.sessions.len());
            self.sessions.push(session);
            return;
        };
        // Two files of one session (a Codex thread and its conversation).
        let had = &mut self.sessions[at];
        for line in session.lines {
            if had.lines.len() < scan::LINES_PER_SESSION {
                had.lines.push(line);
            }
        }
        if session.at > had.at {
            had.at = session.at;
        }
    }
}

/// A key for a harness, which is `Eq` but not `Hash`.
fn harness_key(harness: SessionHarness) -> u8 {
    match harness {
        SessionHarness::Claude => 0,
        SessionHarness::Codex => 1,
        SessionHarness::Opencode => 2,
    }
}

/// A transcript file with matches, as a session; `None` if it is not one,
/// or not run in scope.
fn describe(
    file: &Candidate,
    matched: Vec<super::types::MatchLine>,
    home: &Path,
    scope: &Scope,
    names: &OnceCell<HashMap<String, String>>,
) -> Option<SessionMatch> {
    let (value, root) = match file.kind {
        Kind::Claude => (claude::described(file)?, claude::dir(home)),
        Kind::Codex => (
            codex::described(file, names.get_or_init(|| codex::index(home)))?,
            codex::dir(home),
        ),
    };
    // NOTE: Claude's folder name is lossy (`a_b` and `a-b` both become
    // `a-b`), so the transcript's own `cwd` has the last word. A transcript
    // that names none is in scope only as the folder itself.
    let in_scope = match &value.cwd {
        Some(cwd) => scope.contains(cwd),
        None => {
            file.kind == Kind::Codex
                || file
                    .path
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|dir| claude::is_exact(&dir.to_string_lossy(), scope))
        }
    };
    if !in_scope {
        return None;
    }
    let relative = file.path.strip_prefix(&root).ok()?;
    Some(SessionMatch {
        harness: file.kind.harness(),
        session_id: value.session_id,
        title: value.title,
        at: util::iso8601((file.mtime_ns / 1_000_000) as u64),
        file: relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
        lines: matched,
    })
}
