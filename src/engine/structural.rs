//! `search_structural`: code matching an ast-grep pattern, one entry per
//! file, with `ast-grep-core` and `ast-grep-language` (the grammars of
//! `STRUCTURAL_LANGS` only). `electron/kit/structural.ts` in Rust.
//!
//! - `root` is real and already in scope; `pattern` at most `QUERY_MAX`.
//! - The files are what ripgrep would search (`.gitignore` respected, hidden
//!   files skipped), kept to the languages' extensions. A symlink is never
//!   read: the folder's scope ends at it.
//! - `lang` None: each file in its own language, from its extension. With a
//!   `lang`, only that language's extensions are read.
//! - A language the pattern does not parse in is left out; when that is all
//!   of them, the search is `invalid`.
//! - Stops at `max_files` files or [`MAX_MATCHES`] lines, or at the timeout,
//!   with the last file `truncated`. A timeout before any match is `timeout`.
//! - Every offset in an answer is UTF-16, as the contract says; ast-grep's
//!   are bytes.

use super::types::{FileMatch, KitError, MatchLine, Range, StructuralLang};
use ast_grep_core::tree_sitter::StrDoc;
use ast_grep_core::{AstGrep, Language, Node, Pattern};
use ast_grep_language::SupportLang;
use ignore::WalkBuilder;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The most matching lines over all files.
const MAX_MATCHES: usize = 2_000;
/// The whole search, listing included.
const TIMEOUT: Duration = Duration::from_millis(10_000);
/// A larger file is skipped: generated or minified, and slow to parse.
const FILE_BYTES_MAX: u64 = 1024 * 1024;
/// Files read and parsed at once.
const BATCH: usize = 64;
/// The longest line kept, in UTF-16 units; a longer one is cut to a window
/// around its first match.
const LINE_MAX: usize = 400;
/// How much of a cut line is kept before its first match.
const LINE_LEAD: usize = 80;
/// Worker threads parse deep trees; give them room.
const STACK_BYTES: usize = 16 * 1024 * 1024;

/// What the engine needs to know about one `STRUCTURAL_LANGS` language.
struct LangSpec {
    /// ast-grep's language.
    support: SupportLang,
    /// What it reads in, lower case, without the dot.
    extensions: &'static [&'static str],
    /// Name for an error message.
    label: &'static str,
    /// HTML takes a pattern with a missing end tag (`<div>`) as usable.
    html: bool,
}

fn spec_of(lang: StructuralLang) -> LangSpec {
    let (support, extensions, label, html): (SupportLang, &[&str], &str, bool) = match lang {
        StructuralLang::Ts => (
            SupportLang::TypeScript,
            &["ts", "mts", "cts"],
            "TypeScript",
            false,
        ),
        StructuralLang::Tsx => (SupportLang::Tsx, &["tsx"], "TSX", false),
        StructuralLang::Js => (
            SupportLang::JavaScript,
            &["js", "mjs", "cjs"],
            "JavaScript",
            false,
        ),
        // NOTE: ast-grep's JavaScript grammar parses JSX.
        StructuralLang::Jsx => (SupportLang::JavaScript, &["jsx"], "JSX", false),
        StructuralLang::Json => (SupportLang::Json, &["json"], "JSON", false),
        StructuralLang::Css => (SupportLang::Css, &["css"], "CSS", false),
        StructuralLang::Html => (SupportLang::Html, &["html", "htm"], "HTML", true),
        StructuralLang::Python => (SupportLang::Python, &["py", "pyi"], "Python", false),
        StructuralLang::Rust => (SupportLang::Rust, &["rs"], "Rust", false),
        StructuralLang::Go => (SupportLang::Go, &["go"], "Go", false),
    };
    LangSpec {
        support,
        extensions,
        label,
        html,
    }
}

/// Code under `root` matching `pattern`, one entry per file.
pub fn search_structural(
    root: &Path,
    pattern: &str,
    lang: Option<StructuralLang>,
    max_files: usize,
) -> Result<Vec<FileMatch>, KitError> {
    if pattern.trim().is_empty() {
        return Ok(Vec::new());
    }
    let started = Instant::now();
    let max_files = max_files.min(super::types::SEARCH_STRUCTURAL_LIMIT.max);

    let langs: Vec<StructuralLang> = match lang {
        Some(lang) => vec![lang],
        None => StructuralLang::ALL.to_vec(),
    };
    let specs: Vec<LangSpec> = langs.iter().map(|lang| spec_of(*lang)).collect();

    let mut files = list_files(root, &specs, started)?;
    files.sort_by(|a, b| a.path.cmp(&b.path));

    // Only the languages there are files for are checked, and kept when the
    // pattern parses in them.
    let mut patterns: Vec<Option<Pattern>> = Vec::with_capacity(specs.len());
    let mut errors: Vec<String> = Vec::new();
    for (at, spec) in specs.iter().enumerate() {
        if !files.iter().any(|file| file.spec == at) {
            patterns.push(None);
            continue;
        }
        match check_pattern(spec, pattern) {
            Ok(compiled) => patterns.push(Some(compiled)),
            Err(error) => {
                errors.push(error.lines().next().unwrap_or(&error).to_owned());
                patterns.push(None);
            }
        }
    }
    if !files.is_empty() && patterns.iter().all(Option::is_none) {
        return Err(KitError::invalid(if errors.len() == 1 {
            errors.remove(0)
        } else {
            "That pattern does not parse as code in any of this folder's languages. Check its brackets and quotes."
                .to_owned()
        }));
    }
    files.retain(|file| patterns[file.spec].is_some());

    let word = required_word(pattern);
    let mut found: Vec<FileMatch> = Vec::new();
    let mut matches = 0usize;
    let mut timed_out = false;
    'outer: for batch in files.chunks(BATCH) {
        if started.elapsed() >= TIMEOUT {
            timed_out = true;
            break;
        }
        let results = match_batch(root, batch, &specs, &patterns, word.as_deref());
        for mut result in results.into_iter().flatten() {
            if found.len() >= max_files {
                if let Some(last) = found.last_mut() {
                    last.truncated = true;
                }
                break 'outer;
            }
            let room = MAX_MATCHES - matches;
            if result.lines.len() > room {
                result.lines.truncate(room);
                result.truncated = true;
            }
            matches += result.lines.len();
            let full = matches >= MAX_MATCHES;
            if full {
                result.truncated = true;
            }
            found.push(result);
            if full {
                break 'outer;
            }
        }
    }
    if timed_out || started.elapsed() >= TIMEOUT {
        match found.last_mut() {
            Some(last) => last.truncated = true,
            None => return Err(KitError::timeout()),
        }
    }
    Ok(found)
}

/// A file to search: its path relative to the folder, and its language's
/// index in the specs.
struct Candidate {
    path: String,
    spec: usize,
}

/// The files under `root` that ripgrep would search, kept to the specs'
/// extensions; symlinks and files over 1 MB left out.
fn list_files(root: &Path, specs: &[LangSpec], started: Instant) -> Result<Vec<Candidate>, KitError> {
    let walk = WalkBuilder::new(root).follow_links(false).build();
    let mut files = Vec::new();
    for entry in walk {
        if started.elapsed() >= TIMEOUT {
            return Err(KitError::timeout());
        }
        let Ok(entry) = entry else { continue };
        // A symlink is not a file here: `file_type` is the link's own.
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Some(extension) = entry
            .path()
            .extension()
            .and_then(|extension| extension.to_str())
        else {
            continue;
        };
        let extension = extension.to_ascii_lowercase();
        let Some(spec) = specs
            .iter()
            .position(|spec| spec.extensions.contains(&extension.as_str()))
        else {
            continue;
        };
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        let path = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        files.push(Candidate { path, spec });
    }
    Ok(files)
}

/// `pattern` compiled for `spec`, or ast-grep's complaint if it does not
/// parse as that language's code.
fn check_pattern(spec: &LangSpec, pattern: &str) -> Result<Pattern, String> {
    let lang = spec.support;
    // ast-grep refuses some patterns outright (no node, several nodes).
    let compiled = Pattern::try_new(pattern, lang).map_err(|error| error.to_string())?;
    // A pattern with a syntax error matches nothing, silently.
    let source = lang.pre_process_pattern(pattern);
    let doc = StrDoc::try_new(&source, lang).map_err(|error| error.to_string())?;
    let tree = AstGrep::doc(doc);
    let top = tree.root();
    // NOTE: HTML leaves a missing end tag out of a `<div>` pattern, which is
    // still a usable one; only its ERROR nodes count.
    if has_syntax_error(&top, !spec.html) {
        return Err(format!("That pattern is not valid {}.", spec.label));
    }
    // HTML takes any text as a text node; a pattern there starts with a tag.
    if spec.html && top.children().all(|child| child.kind() == "text") {
        return Err("An HTML pattern starts with a tag.".to_owned());
    }
    Ok(compiled)
}

/// An ERROR node, or (with `missing`) a node the parser made up.
fn has_syntax_error<D: ast_grep_core::Doc>(node: &Node<'_, D>, missing: bool) -> bool {
    if node.is_error() || node.kind() == "ERROR" {
        return true;
    }
    if missing && node.is_leaf() {
        let range = node.range();
        if range.start == range.end && node.text().is_empty() {
            return true;
        }
    }
    node.children().any(|child| has_syntax_error(&child, missing))
}

/// The longest plain word of `pattern` (not a metavariable): a file without
/// it cannot match, so it is never parsed. None when there is none.
fn required_word(pattern: &str) -> Option<String> {
    let mut best: Option<&str> = None;
    let bytes = pattern.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        let start = at;
        if !(bytes[at].is_ascii_alphabetic() || bytes[at] == b'_') {
            at += 1;
            continue;
        }
        while at < bytes.len() && (bytes[at].is_ascii_alphanumeric() || bytes[at] == b'_') {
            at += 1;
        }
        // Not a word when a `$` or word character comes right before it, as
        // in `$A` or `$$$ARGS`, or when a non-ASCII letter touches it.
        let before = pattern[..start].chars().next_back();
        let after = pattern[at..].chars().next();
        let touches = |c: Option<char>| c.is_some_and(|c| c == '$' || c.is_alphanumeric() || c == '_');
        if touches(before) || after.is_some_and(|c| !c.is_ascii() && c.is_alphanumeric()) {
            continue;
        }
        let word = &pattern[start..at];
        if word.len() >= 2 && word.len() > best.map_or(0, str::len) {
            best = Some(word);
        }
    }
    best.map(str::to_owned)
}

/// Each file's matches, in the batch's order, found on several threads.
fn match_batch(
    root: &Path,
    batch: &[Candidate],
    specs: &[LangSpec],
    patterns: &[Option<Pattern>],
    word: Option<&str>,
) -> Vec<Option<FileMatch>> {
    let slots: Vec<Mutex<Option<FileMatch>>> = batch.iter().map(|_| Mutex::new(None)).collect();
    let next = AtomicUsize::new(0);
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(batch.len());
    let work = || loop {
        let at = next.fetch_add(1, Ordering::Relaxed);
        let Some(file) = batch.get(at) else { break };
        let Some(pattern) = patterns[file.spec].as_ref() else {
            continue;
        };
        let found = match_file(root, file, specs[file.spec].support, pattern, word);
        if let Ok(mut slot) = slots[at].lock() {
            *slot = found;
        }
    };
    std::thread::scope(|scope| {
        for _ in 1..workers {
            let spawned = std::thread::Builder::new()
                .stack_size(STACK_BYTES)
                .spawn_scoped(scope, work);
            if spawned.is_err() {
                break;
            }
        }
        work();
    });
    slots
        .into_iter()
        .map(|slot| slot.into_inner().ok().flatten())
        .collect()
}

/// A line's start byte, and each match's start and end byte on it.
type LineSpans = (usize, Vec<(usize, Option<usize>)>);

/// `file`'s matching lines, or None when it has none, is too large, or
/// cannot be read or parsed.
fn match_file(
    root: &Path,
    file: &Candidate,
    lang: SupportLang,
    pattern: &Pattern,
    word: Option<&str>,
) -> Option<FileMatch> {
    let full = root.join(&file.path);
    // symlink_metadata: a symlink may lead out of the folder, and is not
    // followed.
    let info = std::fs::symlink_metadata(&full).ok()?;
    if !info.is_file() || info.len() > FILE_BYTES_MAX {
        return None;
    }
    // NOTE: Lossy where node's `utf8` decoding is too: a bad byte is U+FFFD.
    let source = String::from_utf8_lossy(&std::fs::read(&full).ok()?).into_owned();
    if word.is_some_and(|word| !source.contains(word)) {
        return None;
    }
    let doc = StrDoc::try_new(&source, lang).ok()?;
    let tree = AstGrep::doc(doc);
    let top = tree.root();

    // Byte offsets in `source`, per line: where it starts, and each match's
    // start and end on it (the end is None when the match runs on to a later
    // line: marked to its first line's end).
    let mut by_line: BTreeMap<usize, LineSpans> = BTreeMap::new();
    for found in top.find_all(pattern) {
        let range = found.range();
        let (line, column) = found.start_pos().byte_point();
        let (end_line, end_column) = found.end_pos().byte_point();
        let entry = by_line
            .entry(line)
            .or_insert_with(|| (range.start - column, Vec::new()));
        entry
            .1
            .push((column, (end_line == line).then_some(end_column)));
    }
    if by_line.is_empty() {
        return None;
    }
    let mut lines: Vec<MatchLine> = Vec::with_capacity(by_line.len());
    for (line, (start, spans)) in by_line {
        let rest = source.get(start..)?;
        let text = rest.find('\n').map_or(rest, |newline| &rest[..newline]);
        let text = text.strip_suffix('\r').unwrap_or(text);
        let ranges: Vec<(usize, usize)> = spans
            .into_iter()
            .map(|(from, to)| {
                let to = to.unwrap_or(text.len());
                (utf16_at(text, from), utf16_at(text, to))
            })
            .collect();
        lines.push(to_line(text, line as u64 + 1, merge_ranges(ranges)));
    }
    Some(FileMatch {
        path: file.path.clone(),
        lines,
        truncated: false,
    })
}

/// The UTF-16 offset of byte `at` in `text`, clamped to its end.
fn utf16_at(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    text[..at].encode_utf16().count()
}

/// Sorted, and nested or overlapping ones joined.
fn merge_ranges(mut ranges: Vec<(usize, usize)>) -> Vec<Range> {
    ranges.sort_unstable();
    let mut merged: Vec<Range> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        match merged.last_mut() {
            Some(last) if start < last[1] => last[1] = last[1].max(end),
            _ => merged.push([start, end]),
        }
    }
    merged
}

/// One match line, a long one cut to a window around its first match.
///
/// NOTE: The same cut as `search_text` makes, so both modes show a minified
/// line alike.
fn to_line(text: &str, line: u64, ranges: Vec<Range>) -> MatchLine {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= LINE_MAX {
        return MatchLine {
            line,
            text: text.to_owned(),
            ranges,
        };
    }
    let mut from = ranges.first().map_or(0, |range| range[0]).saturating_sub(LINE_LEAD);
    let mut to = units.len().min(from + LINE_MAX);
    // Never cut a surrogate pair in half.
    if is_low_surrogate(units[from]) {
        from -= 1;
    }
    if to < units.len() && is_low_surrogate(units[to]) {
        to -= 1;
    }
    MatchLine {
        line,
        text: String::from_utf16_lossy(&units[from..to]),
        ranges: ranges
            .into_iter()
            .filter(|range| range[0] >= from && range[0] < to)
            .map(|range| [range[0] - from, range[1].min(to) - from])
            .collect(),
    }
}

fn is_low_surrogate(unit: u16) -> bool {
    (0xdc00..=0xdfff).contains(&unit)
}
