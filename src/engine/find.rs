//! `find_files`: files whose path fuzzily matches a query, best first,
//! typos forgiven (`electron/main/file-finder/finder.ts`).
//!
//! - `root` is real and already in scope; `query` at most `QUERY_MAX`.
//! - The files are list_files' (`files::walker`, through `cache`): .gitignore respected,
//!   hidden files skipped.
//! - Matched and scored with `neo_frizbee`, the Smith-Waterman matcher fff
//!   uses, and ranked as fff-search 0.11.0's `FilePicker::fuzzy_search`
//!   ranks with fff-node's defaults (`score.rs`, `match_and_score_in_arena`):
//!   frizbee's path score, a bonus when the match lands in the file name
//!   (40% exact, up to 16% fuzzy, 5% for entry points like `mod.rs`), a
//!   path-alignment bonus for queries with a `/`, ties broken by newest
//!   mtime. `score` is that total, higher is better.
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
    let paths = listed_files(root);
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

/// Every file list_files would give, sorted as it sorts them, from the
/// file-list cache when it is on. A walk that runs out of
/// `files::WALK_TIMEOUT` keeps what it found.
fn listed_files(root: &Path) -> Vec<String> {
    match cache::listing(root) {
        Some(listing) => listing.files,
        None => listed_files_uncached(root),
    }
}

fn listed_files_uncached(root: &Path) -> Vec<String> {
    let found = Mutex::new(Vec::new());
    let mut builder = Collector {
        root,
        deadline: Instant::now() + files::WALK_TIMEOUT,
        found: &found,
    };
    files::walker(root).build_parallel().visit(&mut builder);
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

/// The best `limit`: total descending, then newest mtime (whole seconds,
/// as fff keeps it), then path. Only the files that can make the page are
/// stat'ed.
fn rank(root: &Path, paths: &[String], mut scored: Vec<(u32, i32)>, limit: usize) -> Vec<(u32, i32)> {
    scored.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    if scored.len() > limit {
        let cut = scored[limit - 1].1;
        let end = scored.partition_point(|&(_, total)| total >= cut);
        scored.truncate(end);
    }
    let times = modified(root, paths, &scored);
    let mut order: Vec<usize> = (0..scored.len()).collect();
    order.sort_unstable_by(|&a, &b| {
        scored[b].1
            .cmp(&scored[a].1)
            .then(times[b].cmp(&times[a]))
            .then(scored[a].0.cmp(&scored[b].0))
    });
    order.truncate(limit);
    order.into_iter().map(|at| scored[at]).collect()
}

/// Each file's mtime in seconds, 0 when it cannot be read.
fn modified(root: &Path, paths: &[String], files: &[(u32, i32)]) -> Vec<u64> {
    let stat = |&(at, _): &(u32, i32)| -> u64 {
        std::fs::metadata(root.join(&paths[at as usize]))
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |since| since.as_secs())
    };
    if files.len() < 512 {
        return files.iter().map(stat).collect();
    }
    let chunk = files.len().div_ceil(threads().min(8));
    std::thread::scope(|scope| {
        let handles: Vec<_> = files
            .chunks(chunk)
            .map(|part| scope.spawn(move || part.iter().map(stat).collect::<Vec<u64>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap_or_default())
            .collect()
    })
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
