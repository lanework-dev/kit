//! `find_files`: files whose path fuzzily matches a query, best first,
//! typos forgiven (`electron/main/file-finder/finder.ts`).
//!
//! - `root` is real and already in scope; `query` at most `QUERY_MAX`.
//! - The files are fff's: in a git work tree, list_files' walk with hidden
//!   files and folders too and every `.git` dropped (`files::Walk::Hidden`,
//!   as fff walks with `hidden(false)` and skips `is_git_file`); outside
//!   one, list_files' own (hidden skipped, as fff skips them there). The
//!   ignore files are list_files' either way (`.gitignore`, `.ignore`,
//!   `.rgignore`, `.git/info/exclude`, the global excludes). Through
//!   `cache`, which keeps the two walks apart.
//! - Matched and scored with `neo_frizbee`, the Smith-Waterman matcher fff
//!   uses, and ranked as fff-search 0.11.0's `FilePicker::fuzzy_search`
//!   ranks with fff-node's defaults (`score.rs`, `match_and_score_in_arena`):
//!   frizbee's path score, a bonus when the match lands in the file name
//!   (40% exact, up to 16% fuzzy, 5% for entry points like `mod.rs`), a
//!   path-alignment bonus for queries with a `/`, ties broken by newest
//!   mtime and then as fff breaks them (`rank`: its file order, and its
//!   partial sort). `score` is that total, higher is better.
//! - The query is parsed as fff's `QueryParser` parses it: whitespace-split
//!   parts, each scored and averaged; `*.ext`, globs, `dir/`, `/dir`,
//!   `!exclude` and `file:12` suffixes as fff reads them.
//! - At most `limit` hits, best first; `ranges` the matched characters as
//!   UTF-16 `[start, end)` offsets into `path`.
//!
//! - With the `git` input on, fff's git signals too (`git.rs`): 15% of
//!   frizbee's score for a file with uncommitted changes (modified, staged
//!   or untracked), one point per recent commit that touched the file, and
//!   `status:modified|untracked|staged|clean` filtering on the real status.
//!   Outside a repository, or without a `git` binary, as with `git` off.
//!
//! NOTE: With `git` off no git state is read: the boosts are 0 and every
//! file counts as clean. fff's frecency boosts (file opens, and how long
//! ago a file with uncommitted changes was written) need its frecency
//! database, which lwk does not keep: 0 either way.
//!
//! NOTE: The app's fff-node is built with fff's zlob walker, which reads
//! `.gitignore` and `.ignore` but not `.rgignore` or `.git/info/exclude`:
//! a file only those exclude is listed by the app and not here. Outside a
//! git work tree fff also skips `node_modules`, `.venv`, `target/debug`
//! and its other `IGNORED_DIRS`; lwk does not.

use super::cache;
use super::files;
use super::git::{self, Signals};
use super::types::{FileHit, FindOptions, KitError, Range};
use globset::{Glob, GlobMatcher};
use ignore::{DirEntry, ParallelVisitor, ParallelVisitorBuilder, WalkState};
use neo_frizbee::{Config, Match, Matcher, Scoring, SortStrategy};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Instant, UNIX_EPOCH};

/// Files under `root` whose path matches `query`, best first.
pub fn find_files(
    root: &Path,
    query: &str,
    options: &FindOptions,
) -> Result<Vec<FileHit>, KitError> {
    let limit = options.limit;
    if limit == 0 {
        return Ok(Vec::new());
    }
    let query = Query::parse(query);
    let paths = listed_files(root, walk_kind(root));
    let signals = if options.git { git::signals(root) } else { None };
    let signals = signals.as_ref();
    let Some(candidates) = query.filter(&paths, signals) else {
        return Ok(Vec::new());
    };
    let parts = query.fuzzy_parts();
    let max_typos = query.max_typos();
    let scored = match &parts {
        Some(parts) => score(&paths, &candidates, parts, max_typos, signals),
        // No fuzzy text (only constraints, or one character): fff's
        // frecency-only ranking with no frecency. Every file scores its
        // recent commits (0 without git), newest first.
        None => candidates
            .iter()
            .map(|&at| (at, signals.map_or(0, |git| git.recency(&paths[at as usize]))))
            .collect(),
    };
    let ranked = rank(root, &paths, scored, limit);

    let page: Vec<&str> = ranked.iter().map(|&(at, _)| paths[at as usize].as_str()).collect();
    let mut ranges = match &parts {
        Some(parts) => match_ranges(&page, parts, max_typos),
        None => vec![Vec::new(); page.len()],
    };
    Ok(ranked
        .iter()
        .zip(ranges.iter_mut())
        .map(|(&(at, total), ranges)| FileHit {
            path: paths[at as usize].clone(),
            score: f64::from(total),
            ranges: (!ranges.is_empty()).then(|| std::mem::take(ranges)),
        })
        .collect())
}

// ── The files ───────────────────────────────────────────────────────────────

/// Which walk find_files takes in `root`: hidden files too in a git work
/// tree, as fff's.
pub(crate) fn walk_kind(root: &Path) -> files::Walk {
    if git::in_work_tree(root) {
        files::Walk::Hidden
    } else {
        files::Walk::Files
    }
}

/// Every file `kind` walks, sorted as list_files sorts them, from the
/// file-list cache when it is on. A walk that runs out of
/// `files::WALK_TIMEOUT` keeps what it found.
fn listed_files(root: &Path, kind: files::Walk) -> Vec<String> {
    match cache::listing(root, kind) {
        Some(listing) => listing.files,
        None => listed_files_uncached(root, kind),
    }
}

fn listed_files_uncached(root: &Path, kind: files::Walk) -> Vec<String> {
    let found = Mutex::new(Vec::new());
    let mut builder = Collector {
        root,
        deadline: Instant::now() + files::WALK_TIMEOUT,
        found: &found,
    };
    files::walker(root, kind).build_parallel().visit(&mut builder);
    let mut paths = found.into_inner().unwrap_or_else(|poison| poison.into_inner());
    files::sort_js(&mut paths);
    paths
}

struct Collector<'s> {
    root: &'s Path,
    deadline: Instant,
    found: &'s Mutex<Vec<String>>,
}

impl<'s> ParallelVisitorBuilder<'s> for Collector<'s> {
    fn build(&mut self) -> Box<dyn ParallelVisitor + 's> {
        Box::new(Visitor {
            root: self.root,
            deadline: self.deadline,
            found: self.found,
            local: Vec::new(),
        })
    }
}

/// One walker thread: its files, handed over when the thread is done.
struct Visitor<'s> {
    root: &'s Path,
    deadline: Instant,
    found: &'s Mutex<Vec<String>>,
    local: Vec<String>,
}

impl ParallelVisitor for Visitor<'_> {
    fn visit(&mut self, entry: Result<DirEntry, ignore::Error>) -> WalkState {
        if Instant::now() >= self.deadline {
            return WalkState::Quit;
        }
        if let Some(path) = files::file_of(self.root, entry) {
            self.local.push(path);
        }
        WalkState::Continue
    }
}

impl Drop for Visitor<'_> {
    fn drop(&mut self) {
        let mut found = self.found.lock().unwrap_or_else(|poison| poison.into_inner());
        found.append(&mut self.local);
    }
}

// ── The query (fff-query-parser's FileSearchConfig) ─────────────────────────

#[derive(Debug, PartialEq)]
enum Fuzzy<'q> {
    Empty,
    Text(&'q str),
    Parts(Vec<&'q str>),
}

#[derive(Debug)]
enum Constraint<'q> {
    /// `*.rs`
    Extension(&'q str),
    /// `src/**/*.ts`; None when the glob does not compile (matches nothing).
    Glob(Option<GlobMatcher>),
    /// `/src`, `src/`, `a/b/`
    PathSegment(&'q str),
    /// `!test`: only ever negated.
    Text(&'q str),
    /// `status:modified`
    GitStatus(StatusFilter),
    /// `type:rust`: fff parses it and ignores it.
    FileType,
    Not(Box<Constraint<'q>>),
}

/// fff's `GitStatusFilter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusFilter {
    Modified,
    Untracked,
    Staged,
    Clean,
}

#[derive(Debug)]
struct Query<'q> {
    raw: &'q str,
    constraints: Vec<Constraint<'q>>,
    fuzzy: Fuzzy<'q>,
}

impl<'q> Query<'q> {
    fn parse(raw: &'q str) -> Query<'q> {
        let query = raw.trim();
        let mut constraints = Vec::new();

        if !query.chars().any(char::is_whitespace) {
            match parse_token(query) {
                // A lone path segment is fuzzy text (`treat_lone_path_as_text`).
                Some(Constraint::PathSegment(_)) | None => {}
                Some(constraint) => {
                    constraints.push(constraint);
                    return Query {
                        raw,
                        constraints,
                        fuzzy: Fuzzy::Empty,
                    };
                }
            }
            let text = strip_location(query);
            let fuzzy = if text.is_empty() {
                Fuzzy::Empty
            } else {
                Fuzzy::Text(text)
            };
            return Query {
                raw,
                constraints,
                fuzzy,
            };
        }

        let mut parts: Vec<&str> = Vec::new();
        for token in query.split_whitespace() {
            match parse_token(token) {
                Some(constraint) => constraints.push(constraint),
                None => parts.push(token),
            }
        }
        if let Some(last) = parts.last_mut() {
            *last = strip_location(last);
        }
        let fuzzy = match parts.as_slice() {
            [] => Fuzzy::Empty,
            [""] => Fuzzy::Empty,
            [only] => Fuzzy::Text(only),
            all if all.iter().all(|part| part.is_empty()) => Fuzzy::Empty,
            _ => Fuzzy::Parts(parts),
        };
        Query {
            raw,
            constraints,
            fuzzy,
        }
    }

    /// The parts frizbee matches, or None when the files are ranked without
    /// a fuzzy match (no text, or a single text under 2 bytes).
    fn fuzzy_parts(&self) -> Option<Vec<&'q str>> {
        match &self.fuzzy {
            Fuzzy::Text(text) if text.len() >= 2 => Some(vec![*text]),
            Fuzzy::Parts(parts) if !parts.is_empty() => Some(parts.clone()),
            _ => None,
        }
    }

    /// fff's typo budget: a quarter of the first part's bytes, 2 to 6.
    fn max_typos(&self) -> u16 {
        let effective = match &self.fuzzy {
            Fuzzy::Text(text) => text,
            Fuzzy::Parts(parts) if !parts.is_empty() => parts[0],
            _ => self.raw.trim(),
        };
        (effective.len().min(usize::from(u16::MAX)) as u16 / 4).clamp(2, 6)
    }

    /// The candidates (indices into `paths`) that pass every constraint;
    /// None when constraints leave nothing. Without `signals` every file
    /// is clean.
    fn filter(&self, paths: &[String], signals: Option<&Signals>) -> Option<Vec<u32>> {
        let all = || (0..paths.len() as u32).collect::<Vec<u32>>();
        if self.constraints.is_empty() {
            return Some(all());
        }
        // Extensions OR together; everything else ANDs.
        let extensions: Vec<&str> = self
            .constraints
            .iter()
            .filter_map(|c| match c {
                Constraint::Extension(ext) => Some(*ext),
                _ => None,
            })
            .collect();
        let kept: Vec<u32> = paths
            .iter()
            .enumerate()
            .filter(|(_, path)| {
                let name = file_name(path);
                (extensions.is_empty() || extensions.iter().any(|ext| has_extension(name, ext)))
                    && self
                        .constraints
                        .iter()
                        .filter(|c| !matches!(c, Constraint::Extension(_)))
                        .all(|c| c.matches(path, signals))
            })
            .map(|(at, _)| at as u32)
            .collect();
        (!kept.is_empty()).then_some(kept)
    }
}

impl Constraint<'_> {
    fn matches(&self, path: &str, signals: Option<&Signals>) -> bool {
        match self {
            Constraint::Extension(ext) => has_extension(file_name(path), ext),
            Constraint::Glob(glob) => glob.as_ref().is_some_and(|glob| glob.is_match(path)),
            Constraint::PathSegment(segment) => contains_segment(path, segment),
            Constraint::Text(text) => contains_ignore_ascii_case(path, text),
            Constraint::GitStatus(filter) => {
                let status = signals.map(|git| git.status(path)).unwrap_or_default();
                match filter {
                    StatusFilter::Modified => status.is_modified(),
                    StatusFilter::Untracked => status.is_untracked(),
                    StatusFilter::Staged => status.is_staged(),
                    StatusFilter::Clean => status.is_clean(),
                }
            }
            Constraint::FileType => true,
            Constraint::Not(inner) => !inner.matches(path, signals),
        }
    }
}

fn has_wildcards(text: &str) -> bool {
    text.bytes().any(|b| matches!(b, b'*' | b'?' | b'[' | b'{'))
}

fn glob(pattern: &str) -> Constraint<'_> {
    Constraint::Glob(Glob::new(pattern).ok().map(|glob| glob.compile_matcher()))
}

/// A token as a constraint, or None when it is fuzzy text.
fn parse_token(token: &str) -> Option<Constraint<'_>> {
    // `\*.rs`: literal text.
    if token.starts_with('\\') && token.len() > 1 {
        return None;
    }
    if let Some(rest) = token.strip_prefix('!') {
        if rest.is_empty() {
            return None;
        }
        if let Some(inner) = parse_plain_token(rest) {
            return Some(Constraint::Not(Box::new(inner)));
        }
        // `!=`, `!!` stay text.
        if rest.len() < 3 || !rest.chars().any(char::is_alphanumeric) {
            return None;
        }
        return Some(Constraint::Not(Box::new(Constraint::Text(rest))));
    }
    if token == "*" || token == "*." {
        return None;
    }
    parse_plain_token(token)
}

/// `parse_token` without negation.
fn parse_plain_token(token: &str) -> Option<Constraint<'_>> {
    if token.starts_with('\\') && token.len() > 1 {
        return None;
    }
    let first = *token.as_bytes().first()?;
    if first == b'*' {
        if let Some(ext) = token.strip_prefix("*.") {
            if !ext.is_empty() && !has_wildcards(ext) {
                return Some(Constraint::Extension(ext));
            }
        }
        return has_wildcards(token).then(|| glob(token));
    }
    if first == b'/' {
        let segment = token.trim_matches('/');
        return (token.len() > 1 && !segment.is_empty()).then_some(Constraint::PathSegment(segment));
    }
    if token.ends_with('/') {
        let segment = token.trim_end_matches('/');
        return (token.len() > 1 && !segment.is_empty()).then_some(Constraint::PathSegment(segment));
    }
    if has_wildcards(token) {
        return Some(glob(token));
    }
    let (key, value) = token.split_once(':')?;
    match key {
        "type" => Some(Constraint::FileType),
        "status" | "st" | "g" | "git" => {
            if value == "*" {
                return None;
            }
            // Any prefix, the first that fits: `s` is staged, `` modified.
            [
                ("modified", StatusFilter::Modified),
                ("untracked", StatusFilter::Untracked),
                ("staged", StatusFilter::Staged),
                ("clean", StatusFilter::Clean),
            ]
            .into_iter()
            .find(|(name, _)| name.starts_with(value))
            .map(|(_, filter)| Constraint::GitStatus(filter))
        }
        _ => None,
    }
}

/// `text` without a `:12`, `:12:4`, `:12-20`, `:12:4-14:2` or `(12,4)`
/// location suffix, as fff's `parse_location` strips it.
fn strip_location(text: &str) -> &str {
    let text = text.trim_end_matches([':', '-', '(']);
    let number = |s: &str| s.parse::<i32>().is_ok();
    let pair = |s: &str, split: char| {
        let mut it = s.split(split);
        matches!((it.next(), it.next(), it.next()), (Some(a), Some(b), None) if number(a) && number(b))
    };
    let mut from = 0;
    while let Some(offset) = text[from..].find(':') {
        let at = from + offset;
        let location = &text[at + 1..];
        let is_location = if let Some((start, end)) = location.split_once('-') {
            match (start.contains(':'), end.contains(':')) {
                (true, true) => pair(start, ':') && pair(end, ':'),
                (true, false) => pair(start, ':') && number(end),
                _ => pair(location, '-'),
            }
        } else if location.contains(':') {
            location
                .split_once(':')
                .is_some_and(|(line, col)| number(line) && number(col))
        } else {
            number(location)
        };
        if is_location {
            return &text[..at];
        }
        from = at + 1;
    }
    if let Some(inner) = text.strip_suffix(')') {
        if let Some((path, location)) = inner.rsplit_once('(') {
            if number(location) || pair(location, ',') {
                return path;
            }
        }
    }
    text
}

fn file_name(path: &str) -> &str {
    &path[filename_offset(path)..]
}

fn filename_offset(path: &str) -> usize {
    path.rfind('/').map_or(0, |at| at + 1)
}

/// `name` ends in `.ext`, ASCII case ignored.
fn has_extension(name: &str, ext: &str) -> bool {
    let (name, ext) = (name.as_bytes(), ext.as_bytes());
    name.len() > ext.len() + 1
        && name[name.len() - ext.len() - 1] == b'.'
        && name[name.len() - ext.len()..].eq_ignore_ascii_case(ext)
}

/// `path` has `segment` (one or more whole folders) before its file name.
fn contains_segment(path: &str, segment: &str) -> bool {
    let (path, segment) = (path.as_bytes(), segment.as_bytes());
    let len = segment.len();
    (0..path.len())
        .filter(|&start| start == 0 || path[start - 1] == b'/')
        .any(|start| {
            path.len() > start + len
                && path[start + len] == b'/'
                && path[start..start + len].eq_ignore_ascii_case(segment)
        })
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let (haystack, needle) = (haystack.as_bytes(), needle.as_bytes());
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
}

// ── Scoring (fff-search's score.rs) ─────────────────────────────────────────

/// frizbee's config for these parts, as fff sets it.
fn config(parts: &[&str], max_typos: u16) -> Config {
    let has_uppercase = parts.iter().any(|part| part.chars().any(char::is_uppercase));
    Config {
        max_typos: Some(max_typos),
        sort: SortStrategy::Unsorted,
        scoring: Scoring {
            capitalization_bonus: if has_uppercase { 8 } else { 0 },
            matching_case_bonus: if has_uppercase { 4 } else { 0 },
            ..Scoring::default()
        },
        ..Config::default()
    }
}

fn threads() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

/// Each candidate that matches every part (of 2 bytes or more), with its
/// total score: `(index into paths, total)`. `signals` adds the git boosts.
fn score(
    paths: &[String],
    candidates: &[u32],
    parts: &[&str],
    max_typos: u16,
    signals: Option<&Signals>,
) -> Vec<(u32, i32)> {
    let valid: Vec<&str> = parts.iter().copied().filter(|part| part.len() >= 2).collect();
    let Some(first) = valid.first() else {
        return Vec::new();
    };
    let options = config(parts, max_typos);
    let threads = threads();
    let haystacks: Vec<&str> = candidates.iter().map(|&at| paths[at as usize].as_str()).collect();

    // The first part over every candidate, then each next part over the
    // survivors; a survivor's score is (score so far + this part's) / parts,
    // its end column the first part's.
    let mut matches: Vec<Match> = Matcher::new(*first, &options).match_list_parallel(&haystacks, threads);
    let total_parts = valid.len() as u32;
    for part in &valid[1..] {
        let mut part_options = options;
        part_options.max_typos = options.max_typos.map(|typos| typos.min(part.len() as u16));
        let survivors: Vec<&str> = matches.iter().map(|m| haystacks[m.index as usize]).collect();
        let found = Matcher::new(*part, &part_options).match_list_parallel(&survivors, threads);
        if found.is_empty() {
            return Vec::new();
        }
        matches = found
            .into_iter()
            .map(|sub| {
                let prev = &matches[sub.index as usize];
                let average = (u32::from(prev.score) + u32::from(sub.score)) / total_parts;
                Match {
                    index: prev.index,
                    score: average.min(u32::from(u16::MAX)) as u16,
                    end_col: prev.end_col,
                    exact: prev.exact && sub.exact,
                }
            })
            .collect();
    }

    let main_needle = parts[0].as_bytes();
    let main_len = main_needle.len().min(usize::from(u16::MAX)) as u16;
    let match_start = |m: &Match| m.end_col.saturating_sub(main_len.saturating_sub(1));
    let has_separator = parts.iter().any(|part| part.contains('/'));

    // Matches that ended in a folder: the first part against the file name
    // alone, to see whether the name matches too.
    let mut name_matches: Vec<Option<Match>> = vec![None; matches.len()];
    if !has_separator && matches.len() <= 15_000 {
        let fallback: Vec<usize> = (0..matches.len())
            .filter(|&at| {
                let path = haystacks[matches[at].index as usize];
                usize::from(match_start(&matches[at])) < filename_offset(path)
            })
            .collect();
        if !fallback.is_empty() {
            let names: Vec<&str> = fallback
                .iter()
                .map(|&at| file_name(haystacks[matches[at].index as usize]))
                .collect();
            let threads = if matches.len() > 4096 { threads.div_ceil(2048) } else { 1 };
            for found in Matcher::new(parts[0], &options).match_list_parallel(&names, threads) {
                name_matches[fallback[found.index as usize]] = Some(found);
            }
        }
    }

    matches
        .iter()
        .zip(name_matches)
        .map(|(m, name_match)| {
            let path = haystacks[m.index as usize];
            let base = i32::from(m.score);
            let name_start = filename_offset(path);
            let name_len = path.len() - name_start;

            let in_name = usize::from(match_start(m)) >= name_start;
            let is_name_match = in_name || name_match.is_some();
            let is_exact_name = name_match.is_some_and(|n| n.exact)
                || (in_name
                    && main_needle.len() == name_len
                    && main_needle.eq_ignore_ascii_case(&path.as_bytes()[name_start..]));

            let filename_bonus = if is_exact_name {
                base / 5 * 2
            } else if is_name_match {
                // Up to 16%; a name matched on its own scales with how well
                // it matched ("rename" in rename.ts, not r-e-n-a-m-e).
                let max_bonus = (base / 6).min(30);
                match name_match {
                    Some(n) => {
                        let max_possible = i32::from(main_len) * 16;
                        max_bonus * i32::from(n.score).min(max_possible) / max_possible.max(1)
                    }
                    None => max_bonus,
                }
            } else if (5..=11).contains(&name_len) && is_entry_point(&path[name_start..]) {
                base * 5 / 100
            } else {
                0
            };

            // A query that looks like a path: reward the paths that end as
            // it does.
            let path_alignment_bonus = if has_separator {
                let common = main_needle
                    .iter()
                    .rev()
                    .zip(path.as_bytes().iter().rev())
                    .take_while(|(a, b)| a.eq_ignore_ascii_case(b))
                    .count();
                let coverage = common * 100 / main_needle.len().max(1);
                if common > 10 && coverage >= 30 {
                    base * coverage as i32 / 100
                } else {
                    0
                }
            } else {
                0
            };

            // Uncommitted changes: 15%. Recent commits: a point each.
            let (git_status_boost, git_recency_boost) = match signals {
                Some(git) => (
                    if git.status(path).is_modified() { base * 15 / 100 } else { 0 },
                    git.recency(path),
                ),
                None => (0, 0),
            };

            let total = base
                .saturating_add(git_status_boost)
                .saturating_add(git_recency_boost)
                .saturating_add(filename_bonus)
                .saturating_add(path_alignment_bonus);
            (candidates[m.index as usize], total)
        })
        .collect()
}

fn is_entry_point(name: &str) -> bool {
    matches!(
        name,
        "mod.rs"
            | "lib.rs"
            | "main.rs"
            | "index.js"
            | "index.jsx"
            | "index.ts"
            | "index.tsx"
            | "index.mjs"
            | "index.cjs"
            | "index.vue"
            | "__init__.py"
            | "__main__.py"
            | "main.go"
            | "main.c"
            | "index.php"
            | "main.rb"
            | "index.rb"
    )
}

/// What fff knows of a file from its walk and ranks by: whether it can
/// index the file, and its mtime in whole seconds.
#[derive(Debug, Clone, Copy, Default)]
struct Meta {
    indexable: bool,
    mtime: u64,
}

/// fff's `MAX_INDEXABLE_FILE_SIZE`.
const MAX_INDEXABLE: u64 = 2 * 1024 * 1024;

/// `path`'s [`Meta`]: indexable when fff's walk (zlob, as the app's
/// fff-node is built) does not take it for binary by its extension and it
/// holds 1 byte to 2 MiB. A file that cannot be read is neither.
fn meta(path: &Path, name: &str) -> Meta {
    let Ok(stat) = std::fs::metadata(path) else {
        return Meta::default();
    };
    let mtime = stat
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_secs());
    let size = stat.len();
    Meta {
        indexable: !is_binary_name(name) && size > 0 && size <= MAX_INDEXABLE,
        mtime,
    }
}

/// Each scored file's [`Meta`], in parallel when there are many.
fn metas(root: &Path, paths: &[String], scored: &[(u32, i32)]) -> Vec<Meta> {
    let one = |&(at, _): &(u32, i32)| {
        let path = &paths[at as usize];
        meta(&root.join(path), file_name(path))
    };
    if scored.len() < 512 {
        return scored.iter().map(one).collect();
    }
    let chunk = scored.len().div_ceil(threads().min(8));
    std::thread::scope(|scope| {
        let handles: Vec<_> = scored
            .chunks(chunk)
            .map(|part| scope.spawn(move || part.iter().map(one).collect::<Vec<Meta>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap_or_default())
            .collect()
    })
}

/// fff's file order, which its matches come in (`walk_filesystem`):
/// indexable files first, then by folder, then by file name, byte by byte.
fn fff_order(a: (&str, Meta), b: (&str, Meta)) -> std::cmp::Ordering {
    let (a_dir, a_name) = a.0.split_at(filename_offset(a.0));
    let (b_dir, b_name) = b.0.split_at(filename_offset(b.0));
    b.1.indexable
        .cmp(&a.1.indexable)
        .then_with(|| a_dir.cmp(b_dir))
        .then_with(|| a_name.cmp(b_name))
}

/// fff's `is_known_binary_extension_basename`: the extension, as
/// `Path::extension` reads it (a dotfile with no other dot has none),
/// is one fff knows for binary. Case counts.
fn is_binary_name(name: &str) -> bool {
    let ext = match name.rfind('.') {
        Some(at) if at > 0 && at < name.len() - 1 => &name[at + 1..],
        _ => return false,
    };
    matches!(
        ext,
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "ico" | "webp" | "tiff" | "tif" | "avif"
            | "heic" | "heif" | "jxl" | "jp2" | "j2k" | "psd" | "icns" | "cur" | "cr2"
            | "nef" | "dng" | "tga"
            | "rgbe" | "hdr" | "exr" | "dds" | "ktx" | "ktx2" | "pvr" | "astc"
            | "ai" | "webarchive" | "mhtml"
            | "mp4" | "avi" | "mov" | "wmv" | "mkv" | "mp3" | "wav" | "flac" | "ogg" | "m4a"
            | "aac" | "webm" | "flv" | "mpg" | "mpeg" | "wma" | "opus" | "pcm" | "reapeaks"
            | "zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar" | "zst" | "lz4" | "lzma"
            | "cab" | "cpio" | "jsonlz4"
            | "deb" | "rpm" | "apk" | "dmg" | "msi" | "iso" | "nupkg" | "whl" | "egg"
            | "appimage" | "flatpak" | "crx" | "pak"
            | "exe" | "dll" | "so" | "dylib" | "o" | "a" | "lib" | "bin" | "elf"
            | "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx"
            | "db" | "sqlite" | "sqlite3" | "mdb"
            | "sqlite-wal" | "sqlite-shm" | "sqlite3-wal" | "sqlite3-shm"
            | "db-wal" | "db-shm" | "ldb"
            | "ttf" | "otf" | "woff" | "woff2" | "eot"
            | "class" | "pyc" | "pyo" | "wasm" | "dex" | "jar" | "war"
            | "cmi" | "cmt" | "cmti" | "cmx" | "nib"
            | "swiftdeps" | "swiftdeps~" | "swiftdoc" | "swiftmodule" | "swiftsourceinfo"
            | "npy" | "npz" | "h5" | "hdf5" | "pt" | "onnx"
            | "safetensors" | "tfrecord" | "tflite" | "gguf" | "ggml" | "joblib"
            | "glb" | "blend" | "blp"
            | "dia" | "bcmap"
            | "pb"
            | "parquet" | "arrow"
            | "suo"
    )
}

/// Up to this many matches [`rank`] reproduces fff's order of equal files
/// too; above it, where fff's own order of them varies, it only keeps them
/// in fff's file order.
const EXACT_TIES_MAX: usize = 4096;

/// The best `limit` of `scored`, as fff-search 0.11.0's
/// `sort_and_paginate` picks them: total descending, then newest mtime.
///
/// - The matches are put in fff's file order first ([`fff_order`]), the
///   order fff's matches come in.
/// - When fewer than half the matches are wanted and there are more than
///   100, fff first moves the best `limit` to the front with
///   `select_nth_unstable_by` and drops the rest; the stable sort that
///   follows keeps the order that left among files equal in both. Up to
///   [`EXACT_TIES_MAX`] matches the same calls run here, on the same
///   order, and leave the same order: every match is stat'ed for it.
/// - Above that, only the files whose total can make the page are stat'ed
///   and sorted, equal ones left in fff's file order.
///
/// NOTE: fff matches with frizbee's threads, one per 2,000 files searched,
/// and joins their matches thread by thread in the order the threads took
/// their chunks. With more than one thread that order varies from call to
/// call, and so does fff's order of equal files (on a 32,000-file tree,
/// every call): no order can match it there. Here equal files always come
/// in the order fff gives with one thread.
fn rank(root: &Path, paths: &[String], mut scored: Vec<(u32, i32)>, limit: usize) -> Vec<(u32, i32)> {
    let wanted = limit.min(scored.len());
    if wanted == 0 {
        return Vec::new();
    }
    let matched = scored.len();
    let exact = matched <= EXACT_TIES_MAX;
    if !exact {
        // Only the files that can make the page.
        scored.sort_unstable_by_key(|&(_, total)| std::cmp::Reverse(total));
        let cut = scored[wanted - 1].1;
        let end = scored.partition_point(|&(_, total)| total >= cut);
        scored.truncate(end);
    }
    let metas = metas(root, paths, &scored);
    let mut items: Vec<(u32, i32, Meta)> = scored
        .into_iter()
        .zip(metas)
        .map(|((at, total), meta)| (at, total, meta))
        .collect();
    items.sort_unstable_by(|a, b| fff_order((&paths[a.0 as usize], a.2), (&paths[b.0 as usize], b.2)));
    let best_first =
        |a: &(u32, i32, Meta), b: &(u32, i32, Meta)| b.1.cmp(&a.1).then_with(|| b.2.mtime.cmp(&a.2.mtime));
    if exact && wanted < matched / 2 && matched > 100 {
        items.select_nth_unstable_by(wanted - 1, best_first);
        items.truncate(wanted);
    }
    items.sort_by(best_first);
    items.truncate(wanted);
    items.into_iter().map(|(at, total, _)| (at, total)).collect()
}

// ── Ranges ──────────────────────────────────────────────────────────────────

/// The matched characters of each path, as UTF-16 ranges: every part's
/// match, merged.
fn match_ranges(page: &[&str], parts: &[&str], max_typos: u16) -> Vec<Vec<Range>> {
    let mut ranges: Vec<Vec<Range>> = vec![Vec::new(); page.len()];
    let options = config(parts, max_typos);
    let valid = parts.iter().copied().filter(|part| part.len() >= 2);
    for (nth, part) in valid.enumerate() {
        let mut part_options = options;
        if nth > 0 {
            part_options.max_typos = options.max_typos.map(|typos| typos.min(part.len() as u16));
        }
        for found in Matcher::new(part, &part_options).match_list_indices(page) {
            let at = found.index as usize;
            if let Some(path) = page.get(at) {
                ranges[at].extend(utf16_ranges(path, &found.indices));
            }
        }
    }
    ranges.into_iter().map(merge).collect()
}

/// frizbee's matched byte offsets into `path` as UTF-16 `[start, end)`
/// ranges, one per character.
fn utf16_ranges(path: &str, offsets: &[u32]) -> Vec<Range> {
    let mut wanted: Vec<usize> = offsets.iter().map(|&at| at as usize).collect();
    wanted.sort_unstable();
    wanted.dedup();
    let mut out = Vec::with_capacity(wanted.len());
    let mut next = wanted.iter().peekable();
    let mut unit = 0;
    for (byte, ch) in path.char_indices() {
        let end = unit + ch.len_utf16();
        // A byte offset inside a character counts as that character.
        while next.peek().is_some_and(|&&at| at < byte + ch.len_utf8()) {
            next.next();
            if out.last() != Some(&[unit, end]) {
                out.push([unit, end]);
            }
        }
        unit = end;
    }
    out
}

/// Sorted, overlapping or touching ranges joined.
fn merge(mut ranges: Vec<Range>) -> Vec<Range> {
    ranges.sort_unstable();
    let mut merged: Vec<Range> = Vec::with_capacity(ranges.len());
    for [start, end] in ranges {
        match merged.last_mut() {
            Some(last) if start <= last[1] => last[1] = last[1].max(end),
            _ if end > start => merged.push([start, end]),
            _ => {}
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::git::tests::Temp;

    fn ranked(paths: &[&str], query: &str) -> Vec<String> {
        let paths: Vec<String> = paths.iter().map(|p| (*p).to_owned()).collect();
        let query = Query::parse(query);
        let candidates = query.filter(&paths, None).unwrap_or_default();
        let parts = query.fuzzy_parts().unwrap();
        let mut scored = score(&paths, &candidates, &parts, query.max_typos(), None);
        scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        scored.into_iter().map(|(at, _)| paths[at as usize].clone()).collect()
    }

    #[test]
    fn file_name_beats_folder() {
        let paths = ["search/other.rs", "src/search.rs", "lib/searching/x.rs"];
        assert_eq!(ranked(&paths, "search")[0], "src/search.rs");
    }

    #[test]
    fn typos_are_forgiven() {
        let paths = ["src/transcript.rs", "src/other.rs"];
        assert_eq!(ranked(&paths, "transcrpt"), vec!["src/transcript.rs"]);
    }

    #[test]
    fn every_part_must_match() {
        let paths = ["mcp/server.ts", "mcp/client.ts", "web/server.ts"];
        assert_eq!(ranked(&paths, "mcp server"), vec!["mcp/server.ts"]);
    }

    #[test]
    fn parses_constraints() {
        let query = Query::parse("foo *.rs !test src/ bar:12");
        assert_eq!(query.fuzzy, Fuzzy::Parts(vec!["foo", "bar"]));
        let paths: Vec<String> = ["src/a.rs", "src/test/a.rs", "lib/a.rs", "src/a.ts"]
            .iter()
            .map(|p| (*p).to_owned())
            .collect();
        assert_eq!(query.filter(&paths, None), Some(vec![0]));
        // A lone folder is text; a lone extension is a filter.
        assert_eq!(Query::parse("src/").fuzzy, Fuzzy::Text("src/"));
        assert_eq!(Query::parse("*.rs").fuzzy, Fuzzy::Empty);
        assert_eq!(Query::parse("main.rs:12:4").fuzzy, Fuzzy::Text("main.rs"));
        assert_eq!(Query::parse("main.rs(12,4)").fuzzy, Fuzzy::Text("main.rs"));
    }

    #[test]
    fn binary_names_as_fff_reads_them() {
        assert!(is_binary_name("icon.png"));
        assert!(is_binary_name("a.b.sqlite-wal"));
        assert!(!is_binary_name("icon.PNG"));
        assert!(!is_binary_name(".png"));
        assert!(!is_binary_name("png."));
        assert!(!is_binary_name("main.rs"));
    }

    #[test]
    fn fff_order_is_indexable_then_folder_then_name() {
        let text = Meta { indexable: true, mtime: 0 };
        let binary = Meta { indexable: false, mtime: 0 };
        let mut paths = [
            ("a/b/c.ts", text),
            ("a/z.ts", text),
            ("a-x/f.ts", text),
            ("a/icon.png", binary),
            ("z.ts", text),
        ];
        paths.sort_by(|a, b| fff_order(*a, *b));
        let order: Vec<&str> = paths.iter().map(|(path, _)| *path).collect();
        // The root's files first, a folder's files before its subfolders'.
        assert_eq!(order, vec!["z.ts", "a-x/f.ts", "a/z.ts", "a/b/c.ts", "a/icon.png"]);
    }

    #[test]
    fn equal_files_rank_in_fff_order() {
        let temp = Temp::new("find-ties");
        let names = ["b/widget.ts", "a/b/widget.ts", "a/widget.ts", "a/widget.png", "a/empty/widget.ts"];
        for name in names {
            temp.write(name, if name.contains("empty") { "" } else { "x" });
        }
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        for name in names {
            let file = std::fs::File::options().write(true).open(temp.0.join(name)).unwrap();
            file.set_modified(when).unwrap();
        }
        let paths: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
        let scored: Vec<(u32, i32)> = (0..paths.len() as u32).map(|at| (at, 10)).collect();
        let ranked: Vec<&str> = rank(&temp.0, &paths, scored, 10)
            .into_iter()
            .map(|(at, _)| paths[at as usize].as_str())
            .collect();
        // An empty file and a binary one are not indexable: last, in the
        // same folder-then-name order.
        assert_eq!(
            ranked,
            vec!["a/widget.ts", "a/b/widget.ts", "b/widget.ts", "a/widget.png", "a/empty/widget.ts"]
        );
    }

    #[test]
    fn hidden_files_only_in_a_work_tree() {
        let temp = Temp::new("find-hidden");
        for name in [".hidden/note.txt", ".github/ci.yml", ".secret/note.txt", "src/note.txt", ".env"] {
            temp.write(name, "x");
        }
        temp.write(".gitignore", ".secret/\n");
        let all = |root: &Path| {
            let mut paths = names(&found(root, "", false)).iter().map(|p| (*p).to_owned()).collect::<Vec<_>>();
            paths.sort();
            paths
        };
        // Outside a work tree, hidden files are skipped, as fff skips them.
        assert_eq!(all(&temp.0), vec!["src/note.txt"]);
        temp.git(&["init", "-q", "-b", "main"]);
        for _ in 0..2 {
            assert_eq!(
                all(&temp.0),
                vec![".env", ".github/ci.yml", ".gitignore", ".hidden/note.txt", "src/note.txt"]
            );
        }
        // list_files is unchanged.
        let listed = files::list_files(&temp.0, 100).unwrap().files;
        assert_eq!(listed, vec!["src/note.txt"]);
    }

    fn found(root: &Path, query: &str, git: bool) -> Vec<(String, i32)> {
        let options = FindOptions { limit: 50, git };
        find_files(root, query, &options)
            .unwrap()
            .into_iter()
            .map(|hit| (hit.path, hit.score as i32))
            .collect()
    }

    fn names(hits: &[(String, i32)]) -> Vec<&str> {
        hits.iter().map(|(path, _)| path.as_str()).collect()
    }

    #[test]
    fn git_boosts_changed_and_recent_files() {
        let files = ["a/widget.rs", "b/widget.rs", "c/widget.rs", "d/widget.rs", "e/widget.rs"];
        let temp = Temp::repo("find-boost", &files);
        // c: committed twice more. b: modified. d: staged. An untracked f.
        for round in 0..2 {
            temp.write("c/widget.rs", &format!("round {round}\n"));
            temp.git(&["commit", "-q", "-am", "c"]);
        }
        temp.write("b/widget.rs", "two\n");
        temp.write("d/widget.rs", "two\n");
        temp.git(&["add", "d/widget.rs"]);
        temp.write("f/widget.rs", "one\n");

        let plain = found(&temp.0, "widget", false);
        let base = plain[0].1;
        assert!(plain.iter().all(|(_, score)| *score == base), "{plain:?}");
        assert_eq!(plain.len(), 6);

        let boosted: std::collections::HashMap<String, i32> = found(&temp.0, "widget", true).into_iter().collect();
        // Every file is in the seed commit: one point. frizbee's score is
        // what the 15% is taken of, the file-name bonus is not.
        let frizbee = {
            let mut matcher = Matcher::new("widget", &config(&["widget"], 2));
            i32::from(matcher.match_list_parallel(&["a/widget.rs"], 1)[0].score)
        };
        let status = frizbee * 15 / 100;
        assert!(status > 3);
        assert_eq!(boosted["a/widget.rs"], base + 1);
        assert_eq!(boosted["e/widget.rs"], base + 1);
        assert_eq!(boosted["c/widget.rs"], base + 3);
        assert_eq!(boosted["b/widget.rs"], base + 1 + status);
        assert_eq!(boosted["d/widget.rs"], base + 1 + status);
        assert_eq!(boosted["f/widget.rs"], base + status);
        let order = found(&temp.0, "widget", true);
        assert_eq!(names(&order)[2..4], ["f/widget.rs", "c/widget.rs"]);

        // No fuzzy text: recent commits alone order the files.
        let recent = found(&temp.0, "*.rs", true);
        assert_eq!(recent[0], ("c/widget.rs".to_owned(), 3));
        assert_eq!(recent.last().unwrap(), &("f/widget.rs".to_owned(), 0));
        assert!(found(&temp.0, "*.rs", false).iter().all(|(_, score)| *score == 0));
    }

    #[test]
    fn status_filters_read_git() {
        let files = ["clean.rs", "modified.rs", "staged.rs", "gone.rs"];
        let temp = Temp::repo("find-status", &files);
        temp.write("modified.rs", "two\n");
        temp.write("staged.rs", "two\n");
        temp.git(&["add", "staged.rs"]);
        temp.write("untracked.rs", "one\n");

        let sorted = |query: &str, git: bool| {
            let hits = found(&temp.0, query, git);
            let mut paths: Vec<String> = hits.into_iter().map(|(path, _)| path).collect();
            paths.sort();
            paths
        };
        assert_eq!(sorted("status:modified", true), ["modified.rs", "staged.rs", "untracked.rs"]);
        assert_eq!(sorted("git:m", true), ["modified.rs", "staged.rs", "untracked.rs"]);
        assert_eq!(sorted("status:untracked", true), ["untracked.rs"]);
        assert_eq!(sorted("st:s", true), ["staged.rs"]);
        assert_eq!(sorted("status:clean", true), ["clean.rs", "gone.rs"]);
        assert_eq!(sorted("!status:clean", true), ["modified.rs", "staged.rs", "untracked.rs"]);
        assert_eq!(sorted("status:modified rs", true), ["modified.rs", "staged.rs", "untracked.rs"]);
        assert_eq!(sorted("modified status:clean", true), Vec::<String>::new());
        // Without git every file is clean, as before.
        assert_eq!(sorted("status:modified", false), Vec::<String>::new());
        assert_eq!(sorted("status:clean", false).len(), 5);
    }

    #[test]
    fn git_outside_a_repository_changes_nothing() {
        let temp = Temp::new("find-norepo");
        for file in ["src/widget.rs", "lib/widgets/mod.rs", "widget/readme.md"] {
            temp.write(file, "one\n");
        }
        for query in ["widget", "*.rs", "status:clean", "wid mod"] {
            assert_eq!(found(&temp.0, query, true), found(&temp.0, query, false), "{query}");
        }
        assert!(found(&temp.0, "status:modified", true).is_empty());
    }

    #[test]
    fn ranges_are_utf16() {
        let ranges = match_ranges(&["é/ab"], &["ab"], 2);
        assert_eq!(ranges, vec![vec![[2, 4]]]);
        assert_eq!(utf16_ranges("é/ab", &[3, 4]), vec![[2, 3], [3, 4]]);
        assert_eq!(merge(vec![[3, 4], [2, 3]]), vec![[2, 4]]);
    }
}
