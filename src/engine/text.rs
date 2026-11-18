//! `search_text`: lines matching a literal or a regex, one entry per file,
//! with ripgrep's crates (`ignore`, `grep-searcher`, `grep-regex`)
//! (`electron/main/fs-kit/search-text.ts`, which runs `rg --json`).
//!
//! - `root` is real and already in scope; `options` checked (query length,
//!   globs count and length).
//! - The files are list_files' walk ([`files::walker`]) narrowed by the
//!   globs, as `rg --glob` narrows it; binary files skipped as rg skips
//!   them (stop at the first NUL byte).
//! - A literal unless `regex`; case ignored unless `case_sensitive ==
//!   Some(true)`. An empty query finds nothing.
//! - The search stops at `max_files` files, at [`MATCHES_MAX`] lines over
//!   all files, or at [`SEARCH_TIMEOUT`]; the last file is then marked
//!   `truncated`. A timeout before any match is `timeout`.
//! - A bad regex or glob is `invalid` ("Not a valid ...").
//! - A line longer than [`LINE_MAX`] UTF-16 units is cut to a window
//!   around its first match; a path or line that is not UTF-8 is left out.
//!
//! NOTE: rg's parallel walk hands files out in no fixed order. Here the
//! files are walked and searched as rg does, one searcher per thread, but
//! answered in the sorted walk's order, so the same search answers the
//! same files, and a cut one keeps the first in that order. Once the files
//! found settle the answer, nothing after them in that order is walked.

use super::files::{self, Cutoff, CutoffView, WalkKey};
use super::types::{FileMatch, KitError, MatchLine, Range, TextOptions};
use grep_matcher::Matcher;
use grep_regex::{RegexMatcher, RegexMatcherBuilder};
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use ignore::overrides::OverrideBuilder;
use ignore::{DirEntry, ParallelVisitor, ParallelVisitorBuilder, WalkBuilder, WalkState};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The most matching lines over all files (`maxMatches`).
const MATCHES_MAX: usize = 2_000;
/// How long a search may take (`timeoutMs`, 10 s).
const SEARCH_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest line kept, in UTF-16 units.
const LINE_MAX: usize = 400;
/// How much of a cut line is kept before its first match.
const LINE_LEAD: usize = 80;

/// Lines under `root` matching `options.query`, one entry per file.
pub fn search_text(root: &Path, options: &TextOptions) -> Result<Vec<FileMatch>, KitError> {
    if options.query.is_empty() {
        return Ok(Vec::new());
    }
    let deadline = Instant::now() + SEARCH_TIMEOUT;
    let matcher = matcher_for(options)?;
    let builder = walk(root, &options.globs)?;
    let found = search_all(root, builder, &matcher, options.max_files, deadline);
    answer(found, options.max_files)
}

// ── Inputs ──────────────────────────────────────────────────────────────────

/// rg's matcher for `options`: `--fixed-strings` unless `regex`,
/// `--ignore-case` unless `case_sensitive`, one line at a time.
fn matcher_for(options: &TextOptions) -> Result<RegexMatcher, KitError> {
    let mut builder = RegexMatcherBuilder::new();
    builder
        .multi_line(true)
        .unicode(true)
        .octal(false)
        .fixed_strings(!options.regex)
        .case_insensitive(options.case_sensitive != Some(true))
        .line_terminator(Some(b'\n'))
        .dot_matches_new_line(false);
    builder.build(&options.query).map_err(|error| {
        let text = error.to_string();
        // rg's own words, as the app picks them from its stderr.
        let reason = text
            .find("error: ")
            .map(|at| text[at + "error: ".len()..].lines().next().unwrap_or(""))
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .unwrap_or("rg could not read it");
        KitError::invalid(format!("Not a valid regular expression: {reason}."))
    })
}

/// The walk of the files to search: list_files' walk narrowed by `globs`.
fn walk(root: &Path, globs: &[String]) -> Result<WalkBuilder, KitError> {
    let mut builder = files::walker(root);
    if !globs.is_empty() {
        let mut overrides = OverrideBuilder::new(root);
        for glob in globs {
            overrides.add(glob).map_err(glob_error)?;
        }
        builder.overrides(overrides.build().map_err(glob_error)?);
    }
    Ok(builder)
}

fn glob_error(error: ignore::Error) -> KitError {
    KitError::invalid(format!("Not a valid file filter: {error}."))
}

// ── Searching ───────────────────────────────────────────────────────────────

/// One file's search: its matching lines, at most [`MATCHES_MAX`].
type Lines = Vec<MatchLine>;

/// What the search found: the files with a matching line, in walk order,
/// and whether it ran out of time.
struct Found {
    files: Vec<(WalkKey, Lines)>,
    timed_out: bool,
}

/// What the walking threads share.
struct Shared<'a> {
    root: &'a Path,
    matcher: &'a RegexMatcher,
    max_files: usize,
    deadline: Instant,
    /// Set when `lwk serve`'s caller cancelled the search.
    cancel: files::Cancel,
    timed_out: AtomicBool,
    /// The files found so far up to the cutoff, by walk key, with their
    /// number of lines.
    settled: Mutex<BTreeMap<WalkKey, usize>>,
    cutoff: Cutoff,
    found: Mutex<Vec<(WalkKey, Lines)>>,
}

impl Shared<'_> {
    /// Count a file found: once the files found up to some file hold more
    /// than `max_files` files or [`MATCHES_MAX`] lines, any file found
    /// later can only come before it, so nothing after it in walk order
    /// can be in the answer, and that file becomes the cutoff.
    fn settle(&self, key: &WalkKey, lines: usize) {
        let Ok(mut settled) = self.settled.lock() else { return };
        settled.insert(key.clone(), lines);
        let mut files = 0usize;
        let mut matches = 0usize;
        let mut cut: Option<WalkKey> = None;
        for (key, lines) in settled.iter() {
            files += 1;
            matches += lines;
            if files > self.max_files || matches >= MATCHES_MAX {
                cut = Some(key.clone());
                break;
            }
        }
        if let Some(cut) = cut {
            settled.retain(|key, _| *key <= cut);
            self.cutoff.lower(cut);
        }
    }
}

/// Walk and search `root` on every core, as rg does, stopping at
/// `deadline` or once what was found settles the answer.
fn search_all(
    root: &Path,
    builder: WalkBuilder,
    matcher: &RegexMatcher,
    max_files: usize,
    deadline: Instant,
) -> Found {
    let shared = Shared {
        root,
        matcher,
        max_files,
        deadline,
        cancel: files::Cancel::current(),
        timed_out: AtomicBool::new(false),
        settled: Mutex::new(BTreeMap::new()),
        cutoff: Cutoff::default(),
        found: Mutex::new(Vec::new()),
    };
    builder.build_parallel().visit(&mut SearchBuilder(&shared));
    let mut files = shared.found.into_inner().unwrap_or_else(|e| e.into_inner());
    files.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    Found {
        files,
        timed_out: shared.timed_out.into_inner(),
    }
}

struct SearchBuilder<'a>(&'a Shared<'a>);

impl<'s> ParallelVisitorBuilder<'s> for SearchBuilder<'s> {
    fn build(&mut self) -> Box<dyn ParallelVisitor + 's> {
        Box::new(Worker {
            shared: self.0,
            matcher: self.0.matcher.clone(),
            searcher: searcher(),
            cutoff: CutoffView::default(),
            found: Vec::new(),
        })
    }
}

/// One walking thread: its own matcher and searcher, and what it found.
struct Worker<'a> {
    shared: &'a Shared<'a>,
    matcher: RegexMatcher,
    searcher: Searcher,
    cutoff: CutoffView,
    found: Vec<(WalkKey, Lines)>,
}

impl ParallelVisitor for Worker<'_> {
    fn visit(&mut self, entry: Result<DirEntry, ignore::Error>) -> WalkState {
        let shared = self.shared;
        if Instant::now() >= shared.deadline || shared.cancel.is_set() {
            shared.timed_out.store(true, Ordering::Relaxed);
            return WalkState::Quit;
        }
        let Ok(entry) = entry else {
            return WalkState::Continue;
        };
        let Some(kind) = entry.file_type() else {
            return WalkState::Continue;
        };
        if kind.is_dir() {
            if !self.cutoff.active(&shared.cutoff) {
                return WalkState::Continue;
            }
            return match files::walk_key(shared.root, entry.path()) {
                Some(key) if self.cutoff.after(&shared.cutoff, &key) => WalkState::Skip,
                _ => WalkState::Continue,
            };
        }
        // Left out, as a path rg can only give as bytes.
        if !kind.is_file() || entry.path().to_str().is_none() {
            return WalkState::Continue;
        }
        let Some(key) = files::walk_key(shared.root, entry.path()) else {
            return WalkState::Continue;
        };
        if self.cutoff.after(&shared.cutoff, &key) {
            return WalkState::Continue;
        }
        let mut sink = LineSink {
            matcher: &self.matcher,
            lines: Vec::new(),
            deadline: shared.deadline,
            timed_out: false,
        };
        // A file that cannot be read is skipped, as rg skips it.
        let _ = self.searcher.search_path(&self.matcher, entry.path(), &mut sink);
        let timed_out = sink.timed_out;
        if !sink.lines.is_empty() {
            let key: WalkKey = Arc::from(key);
            shared.settle(&key, sink.lines.len());
            self.found.push((key, sink.lines));
        }
        if timed_out {
            // Cut mid-file: what it found is kept, the rest is not known,
            // so the search stops here.
            shared.timed_out.store(true, Ordering::Relaxed);
            return WalkState::Quit;
        }
        WalkState::Continue
    }
}

impl Drop for Worker<'_> {
    fn drop(&mut self) {
        if let Ok(mut found) = self.shared.found.lock() {
            found.append(&mut self.found);
        }
    }
}

/// rg's searcher for files found by a walk: line numbers, a binary file
/// given up at its first NUL, a BOM-marked UTF-16 file read as text.
fn searcher() -> Searcher {
    SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .bom_sniffing(true)
        .build()
}

/// Collects one file's matching lines.
struct LineSink<'m> {
    matcher: &'m RegexMatcher,
    lines: Lines,
    deadline: Instant,
    timed_out: bool,
}

impl Sink for LineSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _: &Searcher, found: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if let Some(line) = to_line(self.matcher, found)? {
            self.lines.push(line);
        }
        if self.lines.len() >= MATCHES_MAX {
            return Ok(false);
        }
        if Instant::now() >= self.deadline {
            self.timed_out = true;
            return Ok(false);
        }
        Ok(true)
    }
}

/// One match line as rg's JSON has it, then as the app keeps it; None for
/// a line that is not UTF-8.
fn to_line(matcher: &RegexMatcher, found: &SinkMatch<'_>) -> Result<Option<MatchLine>, std::io::Error> {
    let bytes = found.bytes();
    let Ok(raw) = std::str::from_utf8(bytes) else {
        return Ok(None);
    };
    // rg's submatches: every match in the line, its newline included, but
    // an empty one at its very end.
    let mut submatches: Vec<(usize, usize)> = Vec::new();
    matcher
        .find_iter(bytes, |m| {
            submatches.push((m.start(), m.end()));
            true
        })
        .map_err(std::io::Error::other)?;
    if submatches
        .last()
        .is_some_and(|(start, end)| start == end && *start >= bytes.len())
    {
        submatches.pop();
    }
    let line = found.line_number().unwrap_or(0);
    Ok(Some(match_line(raw, line, &submatches)))
}

/// `toLine`: rg's UTF-8 byte offsets as UTF-16 offsets into the line
/// without its newline, and a long line cut to a window around its first
/// match.
fn match_line(raw: &str, line: u64, submatches: &[(usize, usize)]) -> MatchLine {
    let text = raw
        .strip_suffix('\n')
        .map(|rest| rest.strip_suffix('\r').unwrap_or(rest))
        .unwrap_or(raw);
    let bytes = text.as_bytes();
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut ranges: Vec<Range> = if bytes.len() == units.len() {
        // ASCII: byte offsets are UTF-16 offsets (unclamped, as the app).
        submatches.iter().map(|&(start, end)| [start, end]).collect()
    } else {
        let mut byte_at = 0usize;
        let mut char_at = 0usize;
        let mut offset = |byte: usize| {
            let to = byte.max(byte_at).min(bytes.len());
            char_at += String::from_utf8_lossy(&bytes[byte_at..to]).encode_utf16().count();
            byte_at = to;
            char_at
        };
        submatches
            .iter()
            .map(|&(start, end)| [offset(start), offset(end)])
            .collect()
    };
    if units.len() <= LINE_MAX {
        return MatchLine {
            line,
            text: text.to_owned(),
            ranges,
        };
    }
    let mut from = ranges.first().map_or(0, |r| r[0]).saturating_sub(LINE_LEAD);
    let mut to = units.len().min(from + LINE_MAX);
    // Never cut a surrogate pair in half.
    if units.get(from).is_some_and(|u| is_low_surrogate(*u)) {
        from -= 1;
    }
    if units.get(to).is_some_and(|u| is_low_surrogate(*u)) {
        to -= 1;
    }
    ranges = ranges
        .into_iter()
        .filter(|r| r[0] >= from && r[0] < to)
        .map(|r| [r[0] - from, r[1].min(to) - from])
        .collect();
    MatchLine {
        line,
        text: String::from_utf16_lossy(&units[from..to]),
        ranges,
    }
}

fn is_low_surrogate(unit: u16) -> bool {
    (0xdc00..=0xdfff).contains(&unit)
}

// ── The answer ──────────────────────────────────────────────────────────────

/// The files found, in walk order, cut as the app cuts its stream: a file
/// past `max_files`, the [`MATCHES_MAX`]th line, or the time running out
/// mark the last file kept `truncated`.
fn answer(found: Found, max_files: usize) -> Result<Vec<FileMatch>, KitError> {
    let mut out: Vec<FileMatch> = Vec::new();
    let mut matches = 0usize;
    for (key, lines) in found.files {
        if out.len() >= max_files {
            if let Some(last) = out.last_mut() {
                last.truncated = true;
            }
            return Ok(out);
        }
        out.push(FileMatch {
            path: files::key_path(&key),
            lines: Vec::with_capacity(lines.len()),
            truncated: false,
        });
        let current = out.last_mut().expect("just pushed");
        for line in lines {
            current.lines.push(line);
            matches += 1;
            if matches >= MATCHES_MAX {
                current.truncated = true;
                return Ok(out);
            }
        }
    }
    // The search ran out of time: there may be more.
    if found.timed_out {
        match out.last_mut() {
            None => return Err(KitError::timeout()),
            Some(last) => last.truncated = true,
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The search as the sorted walk, searched file by file, answers it.
    fn searched_in_order(root: &Path, options: &TextOptions) -> Vec<FileMatch> {
        let matcher = matcher_for(options).unwrap();
        let mut searcher = searcher();
        let mut files: Vec<(WalkKey, Lines)> = Vec::new();
        for entry in walk(root, &options.globs).unwrap().build() {
            let entry = entry.unwrap();
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let mut sink = LineSink {
                matcher: &matcher,
                lines: Vec::new(),
                deadline: Instant::now() + SEARCH_TIMEOUT,
                timed_out: false,
            };
            searcher.search_path(&matcher, entry.path(), &mut sink).unwrap();
            if !sink.lines.is_empty() {
                let key = files::walk_key(root, entry.path()).unwrap();
                files.push((Arc::from(key), sink.lines));
            }
        }
        answer(Found { files, timed_out: false }, options.max_files).unwrap()
    }

    fn same(a: &[FileMatch], b: &[FileMatch]) -> bool {
        a.len() == b.len()
            && a.iter().zip(b).all(|(a, b)| {
                a.path == b.path
                    && a.truncated == b.truncated
                    && a.lines.len() == b.lines.len()
                    && a.lines.iter().zip(&b.lines).all(|(a, b)| {
                        a.line == b.line && a.text == b.text && a.ranges == b.ranges
                    })
            })
    }

    #[test]
    fn answers_the_first_files_of_the_sorted_walk() {
        let root = std::env::temp_dir().join(format!("lwk-test-text-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for path in ["a/x.txt", "a/y/z.txt", "a-b", "a.c", "b/\u{1F600}.txt", "b/\u{FF5E}.txt", "c/d/e/f.txt", "c/d.txt", "Z.txt"] {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, "Needle one\nnone\nneedle two\n").unwrap();
        }
        for globs in [vec![], vec!["!*.txt".to_owned()], vec!["*.txt".to_owned()]] {
            for max_files in 0..11 {
                let options = TextOptions {
                    query: "needle".to_owned(),
                    regex: false,
                    case_sensitive: None,
                    globs: globs.clone(),
                    max_files,
                };
                let want = searched_in_order(&root, &options);
                for _ in 0..5 {
                    let got = search_text(&root, &options).unwrap();
                    assert!(same(&got, &want), "limit {max_files}, globs {globs:?}");
                }
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn converts_offsets_to_utf16() {
        // "é" is 2 bytes, 1 unit; "😀" is 4 bytes, 2 units.
        let line = match_line("é😀ab\n", 3, &[(6, 8)]);
        assert_eq!(line.text, "é😀ab");
        assert_eq!(line.ranges, vec![[3, 5]]);
        assert_eq!(line.line, 3);
    }

    #[test]
    fn cuts_a_long_line_around_its_first_match() {
        let raw = format!("{}needle{}\r\n", "x".repeat(500), "y".repeat(500));
        let line = match_line(&raw, 1, &[(500, 506)]);
        assert_eq!(line.text.len(), LINE_MAX);
        assert_eq!(line.ranges, vec![[LINE_LEAD, LINE_LEAD + 6]]);
        assert!(line.text.starts_with('x'));
    }
}
