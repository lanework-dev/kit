//! The git signals `find_files` ranks with when its `git` input is on:
//! each file's status and how many recent commits touched it, as
//! fff-search 0.11.0 reads them (`git.rs`, `git_recency.rs`), but from the
//! `git` binary instead of libgit2.
//!
//! - The repository is the nearest `.git` at or above the folder searched:
//!   a folder, or a worktree's `gitdir: …` file. None found, no `git`
//!   binary, or `git` failing: no signals, never an error.
//! - Status is `git status --porcelain=v2 -z --untracked-files=all`, mapped
//!   to the flags libgit2 gives fff. fff asks libgit2 for no rename
//!   detection, so a staged rename is a new file here too (and its old
//!   path a staged deletion).
//! - Recency is fff's `compute_git_recency`: walk at most 64 commits from
//!   HEAD (`git log --name-only -z -n 64`, each commit marked and with its
//!   parents), skip merges and commits that touched more than 50 files
//!   under the folder searched, and stop after 10 that count. A file's
//!   score is how many of those touched it. On a branch other than the
//!   base branch (`origin/HEAD`'s, else `init.defaultBranch`, else `main`,
//!   else `master`; `origin/<it>` before the local one) only the commits
//!   since the merge base with it are walked. `diff.renames` is off for
//!   the call, as libgit2's tree diff detects no renames.
//! - git runs with optional locks off: it never rewrites the index for us.
//! - The status is kept [`STATUS_TTL`] at most, and only while the git
//!   folder's `index` and `HEAD` keep their stat (mtime ns, size, inode):
//!   in memory, and in `<cache folder>/<hash of the work tree>.status`
//!   for the next process that asks within that time. An edit to a
//!   tracked file touches neither file, so the age is what bounds it.
//! - The commits walked are kept, in memory and in `<hash>.git`, for as
//!   long as HEAD and the refs keep their stat: `HEAD`, the branch it
//!   names, `packed-refs`, `reftable/tables.list`, `origin/HEAD` and the
//!   branch it names, and `main` and `master`, local and `origin`'s.
//! - Both files sit beside the file-list cache, are written to a temporary
//!   file and renamed, and are a miss when they fail to parse or their
//!   checksum is wrong. `LWK_NO_CACHE=1` turns them off, as it does the
//!   file-list cache.
//!
//! NOTE: What can still differ from fff's recency: a change of
//! `init.defaultBranch`, or of a base branch that is neither `main`,
//! `master` nor `origin/HEAD`'s, is not seen until HEAD or another ref
//! above changes; a base branch that is a symbolic ref counts here and
//! not for fff; with several merge bases git and libgit2 may pick
//! different ones.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LOG_MAGIC: &[u8; 8] = b"LWKGITLG";
const STATUS_MAGIC: &[u8; 8] = b"LWKGITST";
const VERSION: u32 = 2;

/// How long a status answer is used again.
const STATUS_TTL: Duration = Duration::from_secs(2);

/// fff's `GitRecencyConfig` defaults and its walk budget for them.
const MAX_COMMITS: usize = 10;
const MAX_FILES_PER_COMMIT: usize = 50;
const WALK_BUDGET: usize = 64;

/// A file's status, in libgit2's `git_status_t` bits. 0 is clean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Status(u16);

impl Status {
    const INDEX_NEW: u16 = 1 << 0;
    const INDEX_MODIFIED: u16 = 1 << 1;
    const INDEX_DELETED: u16 = 1 << 2;
    const INDEX_RENAMED: u16 = 1 << 3;
    const INDEX_TYPECHANGE: u16 = 1 << 4;
    const WT_NEW: u16 = 1 << 7;
    const WT_MODIFIED: u16 = 1 << 8;
    const WT_DELETED: u16 = 1 << 9;
    const WT_TYPECHANGE: u16 = 1 << 10;
    const WT_RENAMED: u16 = 1 << 11;
    const CONFLICTED: u16 = 1 << 15;

    /// fff's `is_modified_status`: what earns the status bonus and passes
    /// `status:modified`.
    pub(crate) fn is_modified(self) -> bool {
        self.0
            & (Status::WT_MODIFIED
                | Status::INDEX_MODIFIED
                | Status::WT_NEW
                | Status::INDEX_NEW
                | Status::WT_RENAMED)
            != 0
    }

    pub(crate) fn is_untracked(self) -> bool {
        self.0 & Status::WT_NEW != 0
    }

    pub(crate) fn is_staged(self) -> bool {
        self.0
            & (Status::INDEX_NEW
                | Status::INDEX_MODIFIED
                | Status::INDEX_DELETED
                | Status::INDEX_RENAMED
                | Status::INDEX_TYPECHANGE)
            != 0
    }

    pub(crate) fn is_clean(self) -> bool {
        self.0 == 0
    }
}

/// The signals of the files under one folder, by the path `find_files`
/// lists them with (relative to the folder, `/`-separated).
#[derive(Debug, Default)]
pub(crate) struct Signals {
    status: HashMap<String, Status>,
    recency: HashMap<String, i16>,
}

impl Signals {
    pub(crate) fn status(&self, path: &str) -> Status {
        self.status.get(path).copied().unwrap_or_default()
    }

    /// How many of the recent commits touched `path`.
    pub(crate) fn recency(&self, path: &str) -> i32 {
        self.recency.get(path).copied().map_or(0, i32::from)
    }
}

/// `root`'s signals; None outside a repository, without a `git` binary or
/// when `git` fails. `root` is real (canonical).
pub(crate) fn signals(root: &Path) -> Option<Signals> {
    let cache = cache_dir();
    let env = Env {
        cache: cache.as_deref(),
        program: OsStr::new("git"),
        ttl: STATUS_TTL,
    };
    signals_with(root, &env).map(|(signals, _)| signals)
}

#[cfg(not(test))]
fn cache_dir() -> Option<PathBuf> {
    super::cache::Settings::from_env(|name| std::env::var_os(name)).map(|settings| settings.dir)
}

/// Tests never touch the user's cache.
#[cfg(test)]
fn cache_dir() -> Option<PathBuf> {
    Some(std::env::temp_dir().join("lwk-test-cache"))
}

/// What a call works with.
struct Env<'e> {
    /// The cache folder; None when the cache files are off.
    cache: Option<&'e Path>,
    /// The `git` to run.
    program: &'e OsStr,
    /// How long a status answer is used again.
    ttl: Duration,
}

/// Where an answer came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Memory,
    Disk,
    Git,
}

/// Where the status and the commits came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct From {
    status: Source,
    log: Source,
}

fn signals_with(root: &Path, env: &Env) -> Option<(Signals, From)> {
    let place = discover(root)?;
    let (status, log, from) = repo_signals(&place, env)?;
    Some((under(&status, &log, &place.workdir, root), from))
}

// ── The repository ──────────────────────────────────────────────────────────

/// A work tree, the git folder that holds its `index` and `HEAD`, and the
/// one that holds the refs (the same, but for a linked worktree).
#[derive(Debug, PartialEq, Eq)]
struct Place {
    workdir: PathBuf,
    gitdir: PathBuf,
    common: PathBuf,
}

/// Whether `root` is in a git work tree: fff's `is_git_repo`, which makes
/// find_files list hidden files.
pub(crate) fn in_work_tree(root: &Path) -> bool {
    discover(root).is_some()
}

/// The nearest `.git` at or above `start`: a folder with a `HEAD`, or a
/// file naming a worktree's git folder.
fn discover(start: &Path) -> Option<Place> {
    start.ancestors().find_map(|dir| {
        let dot_git = dir.join(".git");
        let meta = std::fs::metadata(&dot_git).ok()?;
        let gitdir = if meta.is_dir() {
            dot_git
        } else {
            let text = std::fs::read_to_string(&dot_git).ok()?;
            let named = text.lines().next()?.strip_prefix("gitdir:")?.trim();
            dir.join(named)
        };
        if !gitdir.join("HEAD").is_file() {
            return None;
        }
        let common = std::fs::read_to_string(gitdir.join("commondir"))
            .ok()
            .and_then(|text| text.lines().next().map(|line| gitdir.join(line.trim())))
            .unwrap_or_else(|| gitdir.clone());
        Some(Place {
            workdir: dir.to_path_buf(),
            gitdir,
            common,
        })
    })
}

/// A file's stat: mtime ns, size, inode; None when it is missing.
type Stamp = Option<(i64, u64, u64)>;
/// What the status is good for: `index` and `HEAD`.
type StatusKey = [Stamp; 2];
/// What the commits walked are good for: HEAD and the refs.
type RefsKey = [Stamp; 10];

#[cfg(unix)]
fn stamp(file: &Path) -> Stamp {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(file).ok()?;
    let mtime = meta.mtime().saturating_mul(1_000_000_000).saturating_add(meta.mtime_nsec());
    Some((mtime, meta.len(), meta.ino()))
}

#[cfg(not(unix))]
fn stamp(_: &Path) -> Stamp {
    None
}

/// None where there are no inodes to key by: nothing is kept, `git` runs
/// every call.
fn status_key(place: &Place) -> Option<StatusKey> {
    cfg!(unix).then(|| [stamp(&place.gitdir.join("index")), stamp(&place.gitdir.join("HEAD"))])
}

fn refs_key(place: &Place) -> Option<RefsKey> {
    if !cfg!(unix) {
        return None;
    }
    // The ref a `ref: refs/…` file names.
    let named = |file: &Path| -> Option<PathBuf> {
        let text = std::fs::read_to_string(file).ok()?;
        Some(place.common.join(text.lines().next()?.strip_prefix("ref:")?.trim()))
    };
    let head = place.gitdir.join("HEAD");
    let origin_head = place.common.join("refs/remotes/origin/HEAD");
    let of = |file: Option<PathBuf>| file.and_then(|file| stamp(&file));
    Some([
        stamp(&head),
        of(named(&head)),
        stamp(&place.common.join("packed-refs")),
        stamp(&place.common.join("reftable/tables.list")),
        stamp(&origin_head),
        of(named(&origin_head)),
        stamp(&place.common.join("refs/remotes/origin/main")),
        stamp(&place.common.join("refs/remotes/origin/master")),
        stamp(&place.common.join("refs/heads/main")),
        stamp(&place.common.join("refs/heads/master")),
    ])
}

/// Every path that is not clean, relative to the work tree, sorted.
type StatusList = Vec<(String, Status)>;

/// A commit of the walk: whether it is a merge, and the paths it touched
/// (relative to the work tree; none kept for a merge).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Commit {
    merge: bool,
    files: Vec<String>,
}

/// The signals of the files under `root`, a folder at or under `workdir`,
/// keyed relative to `root`.
fn under(status: &StatusList, log: &[Commit], workdir: &Path, root: &Path) -> Signals {
    let mut prefix = root
        .strip_prefix(workdir)
        .ok()
        .map(|sub| sub.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/"))
        .unwrap_or_default();
    if !prefix.is_empty() {
        prefix.push('/');
    }
    let fold = |path: &str| path.strip_prefix(prefix.as_str()).map(str::to_owned);

    // fff diffs each commit with the folder as its pathspec: only the
    // files under it count toward the 50, and a commit that touched none
    // still counts toward the 10.
    let mut recency: HashMap<String, i16> = HashMap::new();
    let mut counted = 0;
    for commit in log {
        if counted >= MAX_COMMITS {
            break;
        }
        if commit.merge {
            continue;
        }
        let inside = || commit.files.iter().filter_map(|path| fold(path));
        if inside().count() > MAX_FILES_PER_COMMIT {
            continue;
        }
        for path in inside() {
            let count = recency.entry(path).or_default();
            *count = count.saturating_add(1);
        }
        counted += 1;
    }
    Signals {
        status: status
            .iter()
            .filter_map(|(path, status)| Some((fold(path)?, *status)))
            .collect(),
        recency,
    }
}

/// What this process knows of a work tree.
#[derive(Default)]
struct Held {
    /// The status, the key it had and when git was asked.
    status: Option<(StatusKey, Instant, Arc<StatusList>)>,
    log: Option<(RefsKey, Arc<Vec<Commit>>)>,
}

type Memory = Mutex<HashMap<PathBuf, Held>>;

fn memory() -> &'static Memory {
    static MEMORY: OnceLock<Memory> = OnceLock::new();
    MEMORY.get_or_init(Default::default)
}

fn remember(workdir: &Path, keep: impl FnOnce(&mut Held)) {
    let mut held = memory().lock().unwrap_or_else(|poison| poison.into_inner());
    keep(held.entry(workdir.to_path_buf()).or_default());
}

fn nanos_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_nanos()).unwrap_or(i64::MAX))
}

/// The repository's status and commits walked: each from memory, else its
/// cache file, else `git`.
fn repo_signals(place: &Place, env: &Env) -> Option<(Arc<StatusList>, Arc<Vec<Commit>>, From)> {
    let workdir = place.workdir.as_path();
    let (status_key, refs_key) = (status_key(place), refs_key(place));
    let file = |ext: &str| env.cache.map(|dir| dir.join(file_name(workdir, ext)));
    let (status_file, log_file) = (file("status"), file("git"));
    let read = |file: &Option<PathBuf>| file.as_deref().and_then(|file| std::fs::read(file).ok());

    let (mut status, mut log) = (None, None);
    {
        let held = memory().lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some(held) = held.get(workdir) {
            status = held
                .status
                .as_ref()
                .filter(|(key, asked, _)| Some(*key) == status_key && asked.elapsed() < env.ttl)
                .map(|(_, _, status)| (Arc::clone(status), Source::Memory));
            log = held
                .log
                .as_ref()
                .filter(|(key, _)| Some(*key) == refs_key)
                .map(|(_, log)| (Arc::clone(log), Source::Memory));
        }
    }
    if let (None, Some(key)) = (&status, status_key) {
        let stored = read(&status_file).and_then(|bytes| decode_status(&bytes, workdir, &key));
        let young = stored.and_then(|(asked, list)| {
            let age = u64::try_from(nanos_now().checked_sub(asked)?).ok().map(Duration::from_nanos)?;
            (age < env.ttl).then_some((age, list))
        });
        if let Some((age, list)) = young {
            let list = Arc::new(list);
            if let Some(asked) = Instant::now().checked_sub(age) {
                remember(workdir, |held| held.status = Some((key, asked, Arc::clone(&list))));
            }
            status = Some((list, Source::Disk));
        }
    }
    if let (None, Some(key)) = (&log, refs_key) {
        if let Some(stored) = read(&log_file).and_then(|bytes| decode_log(&bytes, workdir, &key)) {
            let stored = Arc::new(stored);
            remember(workdir, |held| held.log = Some((key, Arc::clone(&stored))));
            log = Some((stored, Source::Disk));
        }
    }

    // git status runs while the commits are asked for.
    let asked = (Instant::now(), nanos_now());
    let child = match &status {
        Some(_) => None,
        None => Some(spawn(env.program, workdir, STATUS_ARGS).ok()?),
    };
    let log = match log {
        Some((log, from)) => Some((log, from)),
        None => ask_log(workdir, env.program).map(|commits| {
            let commits = Arc::new(commits);
            // Kept only when the refs are what they were before git ran:
            // otherwise the answer may be older than the key.
            if let Some(key) = refs_key.filter(|key| self::refs_key(place) == Some(*key)) {
                remember(workdir, |held| held.log = Some((key, Arc::clone(&commits))));
                if let Some(file) = &log_file {
                    write(file, &encode_log(&commits, workdir, &key));
                }
            }
            (commits, Source::Git)
        }),
    };
    let status = match child {
        None => status?,
        Some(child) => {
            let out = child.wait_with_output().ok().filter(|out| out.status.success())?;
            let list = Arc::new(parse_status(&out.stdout));
            if let Some(key) = status_key.filter(|key| self::status_key(place) == Some(*key)) {
                remember(workdir, |held| held.status = Some((key, asked.0, Arc::clone(&list))));
                if let Some(file) = &status_file {
                    write(file, &encode_status(&list, workdir, &key, asked.1));
                }
            }
            (list, Source::Git)
        }
    };
    // No `git` to ask for the commits (the status came from a cache):
    // no recency this call.
    let (log, log_from) = log.unwrap_or_else(|| (Arc::default(), Source::Git));
    let from = From {
        status: status.1,
        log: log_from,
    };
    Some((status.0, log, from))
}

// ── Asking git ──────────────────────────────────────────────────────────────

const STATUS_ARGS: &[&str] = &["status", "--porcelain=v2", "-z", "--untracked-files=all"];

fn spawn(program: &OsStr, workdir: &Path, args: &[&str]) -> std::io::Result<Child> {
    Command::new(program)
        .arg("-C")
        .arg(workdir)
        .args(args)
        // The repository is the one `discover` found, whatever git hook
        // or wrapper lwk was started from.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
}

/// A finished child's output as a line of text, when it succeeded.
fn line(out: std::io::Result<Output>) -> Option<String> {
    let out = out.ok().filter(|out| out.status.success())?;
    let text = String::from_utf8(out.stdout).ok()?;
    Some(text.trim().to_owned()).filter(|text| !text.is_empty())
}

/// The commits fff's recency walks, newest first. None when `git` cannot
/// be run; empty when there is no commit yet.
fn ask_log(workdir: &Path, program: &OsStr) -> Option<Vec<Commit>> {
    let run = |args: &[&str]| spawn(program, workdir, args).and_then(Child::wait_with_output);
    // HEAD's commit and short name (`HEAD` when detached), the branch
    // `origin/HEAD` names and the configured default, side by side.
    let head = spawn(program, workdir, &["rev-parse", "HEAD", "--abbrev-ref", "HEAD"]).ok()?;
    let origin_head = spawn(program, workdir, &["symbolic-ref", "-q", "refs/remotes/origin/HEAD"]);
    let configured = spawn(program, workdir, &["config", "--get", "init.defaultBranch"]);
    let Some(head) = line(head.wait_with_output()) else {
        return Some(Vec::new());
    };
    let (head, head_branch) = head.split_once('\n').unwrap_or((&head, ""));
    let origin_head = line(origin_head.and_then(Child::wait_with_output))
        .and_then(|name| name.strip_prefix("refs/remotes/origin/").map(str::to_owned));
    let configured = line(configured.and_then(Child::wait_with_output));

    // The base branch is the first of these that exists, `origin`'s
    // before the local one. Off it, the walk stops at the merge base.
    let candidates = origin_head.iter().chain(&configured).map(String::as_str).chain(["main", "master"]);
    let mut hidden = None;
    'base: for branch in candidates {
        // HEAD's own branch exists.
        if branch == head_branch {
            break;
        }
        for name in [format!("origin/{branch}"), branch.to_owned()] {
            let out = run(&["merge-base", "HEAD", &name]).ok()?;
            match out.status.code() {
                // 0 with the merge base; 1 when the two share no commit.
                Some(0) | Some(1) => {
                    hidden = line(Ok(out)).filter(|base| base != head);
                    break 'base;
                }
                // No such branch.
                _ => {}
            }
        }
    }

    let budget = WALK_BUDGET.to_string();
    let mut args = vec![
        "-c",
        "diff.renames=false",
        "log",
        "--name-only",
        "-z",
        "-n",
        &budget,
        "--format=%x01%P%x02",
        "HEAD",
    ];
    if let Some(base) = &hidden {
        args.extend(["--not", base]);
    }
    args.push("--");
    let out = run(&args).ok()?;
    Some(if out.status.success() { parse_log(&out.stdout) } else { Vec::new() })
}

/// `git status --porcelain=v2 -z` as the flags libgit2 gives each path
/// (without rename detection, which fff leaves off).
fn parse_status(out: &[u8]) -> Vec<(String, Status)> {
    let index = |x: u8| match x {
        b'M' => Status::INDEX_MODIFIED,
        b'A' | b'R' | b'C' => Status::INDEX_NEW,
        b'D' => Status::INDEX_DELETED,
        b'T' => Status::INDEX_TYPECHANGE,
        _ => 0,
    };
    let worktree = |y: u8| match y {
        b'M' => Status::WT_MODIFIED,
        // `A` is `git add -N`: in the index in name only.
        b'A' | b'R' | b'C' => Status::WT_NEW,
        b'D' => Status::WT_DELETED,
        b'T' => Status::WT_TYPECHANGE,
        _ => 0,
    };
    // The path after `fields` space-separated fields.
    let path_after = |record: &[u8], fields: usize| -> Option<String> {
        let path = record.splitn(fields + 1, |&b| b == b' ').nth(fields)?;
        String::from_utf8(path.to_vec()).ok()
    };

    let mut flags: HashMap<String, u16> = HashMap::new();
    let mut add = |path: Option<String>, bits: u16| {
        if let Some(path) = path.filter(|_| bits != 0) {
            *flags.entry(path).or_default() |= bits;
        }
    };
    let mut records = out.split(|&b| b == 0);
    while let Some(record) = records.next() {
        let xy = (record.get(2).copied().unwrap_or(b'.'), record.get(3).copied().unwrap_or(b'.'));
        match record.first() {
            Some(b'1') => add(path_after(record, 8), index(xy.0) | worktree(xy.1)),
            Some(b'2') => {
                add(path_after(record, 9), index(xy.0) | worktree(xy.1));
                // The path it was renamed (not copied) from is gone.
                let from = records.next().and_then(|path| String::from_utf8(path.to_vec()).ok());
                let gone = if xy.0 == b'R' { Status::INDEX_DELETED } else { 0 }
                    | if xy.1 == b'R' { Status::WT_DELETED } else { 0 };
                add(from, gone);
            }
            Some(b'u') => add(path_after(record, 10), Status::CONFLICTED),
            Some(b'?') => add(path_after(record, 1), Status::WT_NEW),
            _ => {}
        }
    }
    let mut status: Vec<(String, Status)> = flags.into_iter().map(|(path, bits)| (path, Status(bits))).collect();
    status.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    status
}

/// `git log --name-only -z --format=%x01%P%x02`: per commit `\x01`, its
/// parents, `\x02`, then each path it touched, NUL-ended.
fn parse_log(out: &[u8]) -> Vec<Commit> {
    out.split(|&b| b == 1)
        .skip(1)
        .map(|record| {
            let end = record.iter().position(|&b| b == 2).unwrap_or(record.len());
            let merge = record[..end].split(|&b| b == b' ').filter(|parent| !parent.is_empty()).count() > 1;
            let files = record[(end + 1).min(record.len())..]
                .split(|&b| b == 0)
                .filter_map(|path| std::str::from_utf8(path.strip_prefix(b"\n").unwrap_or(path)).ok())
                .filter(|path| !path.is_empty() && !merge)
                .map(str::to_owned)
                .collect();
            Commit { merge, files }
        })
        .collect()
}

// ── The files ───────────────────────────────────────────────────────────────
//
// Little-endian; a path is a u32 length and its bytes; a stat is a u8
// (0 missing | 1 there), mtime i64, size u64, inode u64. Both end in a
// checksum u64 (FNV-1a) over everything before it.
//
// <hash>.git:    "LWKGITLG", version u32, work tree, 10 stats (the refs),
//                commits u32 × (merge u8, files u32 × path)
// <hash>.status: "LWKGITST", version u32, work tree, 2 stats (index,
//                HEAD), asked i64 (ns since the epoch),
//                files u32 × (path, flags u16)

fn file_name(workdir: &Path, ext: &str) -> String {
    format!("{:016x}.{ext}", fnv1a(workdir.as_os_str().as_encoded_bytes()))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash: u64, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

struct Writer(Vec<u8>);

impl Writer {
    fn new(magic: &[u8; 8], workdir: &Path, key: &[Stamp]) -> Writer {
        let mut writer = Writer(Vec::new());
        writer.0.extend_from_slice(magic);
        writer.0.extend_from_slice(&VERSION.to_le_bytes());
        writer.bytes(workdir.as_os_str().as_encoded_bytes());
        for stamp in key {
            let (mtime, size, ino) = stamp.unwrap_or_default();
            writer.0.push(u8::from(stamp.is_some()));
            writer.0.extend_from_slice(&mtime.to_le_bytes());
            writer.0.extend_from_slice(&size.to_le_bytes());
            writer.0.extend_from_slice(&ino.to_le_bytes());
        }
        writer
    }

    fn len(&mut self, len: usize) {
        self.0.extend_from_slice(&(len as u32).to_le_bytes());
    }

    fn bytes(&mut self, bytes: &[u8]) {
        self.len(bytes.len());
        self.0.extend_from_slice(bytes);
    }

    fn finish(mut self) -> Vec<u8> {
        let sum = fnv1a(&self.0);
        self.0.extend_from_slice(&sum.to_le_bytes());
        self.0
    }
}

fn encode_log(commits: &[Commit], workdir: &Path, key: &RefsKey) -> Vec<u8> {
    let mut writer = Writer::new(LOG_MAGIC, workdir, key);
    writer.len(commits.len());
    for commit in commits {
        writer.0.push(u8::from(commit.merge));
        writer.len(commit.files.len());
        for path in &commit.files {
            writer.bytes(path.as_bytes());
        }
    }
    writer.finish()
}

fn encode_status(status: &StatusList, workdir: &Path, key: &StatusKey, asked: i64) -> Vec<u8> {
    let mut writer = Writer::new(STATUS_MAGIC, workdir, key);
    writer.0.extend_from_slice(&asked.to_le_bytes());
    writer.len(status.len());
    for (path, status) in status {
        writer.bytes(path.as_bytes());
        writer.0.extend_from_slice(&status.0.to_le_bytes());
    }
    writer.finish()
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    /// What follows the header, when the file is whole and is `workdir`'s
    /// at `key`.
    fn open(bytes: &'a [u8], magic: &[u8; 8], workdir: &Path, key: &[Stamp]) -> Option<Reader<'a>> {
        let (body, sum) = bytes.split_at_checked(bytes.len().checked_sub(8)?)?;
        if fnv1a(body).to_le_bytes() != sum {
            return None;
        }
        let mut reader = Reader(body);
        if reader.take(magic.len())? != magic || reader.u32()? != VERSION {
            return None;
        }
        if reader.bytes()? != workdir.as_os_str().as_encoded_bytes() {
            return None;
        }
        for stamp in key {
            let there = reader.take(1)?[0] == 1;
            let stored = (reader.array().map(i64::from_le_bytes)?, reader.u64()?, reader.u64()?);
            if there.then_some(stored) != *stamp {
                return None;
            }
        }
        Some(reader)
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let (head, rest) = self.0.split_at_checked(len)?;
        self.0 = rest;
        Some(head)
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    fn u32(&mut self) -> Option<u32> {
        self.array().map(u32::from_le_bytes)
    }

    fn u64(&mut self) -> Option<u64> {
        self.array().map(u64::from_le_bytes)
    }

    fn bytes(&mut self) -> Option<&'a [u8]> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    fn path(&mut self) -> Option<String> {
        String::from_utf8(self.bytes()?.to_vec()).ok()
    }
}

fn decode_log(bytes: &[u8], workdir: &Path, key: &RefsKey) -> Option<Vec<Commit>> {
    let mut reader = Reader::open(bytes, LOG_MAGIC, workdir, key)?;
    let mut commits = Vec::new();
    for _ in 0..reader.u32()? {
        let merge = reader.take(1)?[0] == 1;
        let mut files = Vec::new();
        for _ in 0..reader.u32()? {
            files.push(reader.path()?);
        }
        commits.push(Commit { merge, files });
    }
    reader.0.is_empty().then_some(commits)
}

/// The stored status and when git was asked for it (ns since the epoch).
fn decode_status(bytes: &[u8], workdir: &Path, key: &StatusKey) -> Option<(i64, StatusList)> {
    let mut reader = Reader::open(bytes, STATUS_MAGIC, workdir, key)?;
    let asked = reader.array().map(i64::from_le_bytes)?;
    let mut status = Vec::new();
    for _ in 0..reader.u32()? {
        let path = reader.path()?;
        status.push((path, Status(reader.array().map(u16::from_le_bytes)?)));
    }
    reader.0.is_empty().then_some((asked, status))
}

/// Written beside and renamed over, so no reader sees half a file.
fn write(file: &Path, bytes: &[u8]) {
    let Some(dir) = file.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let name = file.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let unique = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let temp = dir.join(format!(".{name}.{}.{unique}.tmp", std::process::id()));
    if std::fs::write(&temp, bytes).is_err() || std::fs::rename(&temp, file).is_err() {
        let _ = std::fs::remove_file(&temp);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::fs;

    /// A fresh folder under the temp folder, real path, gone on drop.
    pub(crate) struct Temp(pub PathBuf);

    impl Temp {
        pub(crate) fn new(name: &str) -> Temp {
            let dir = std::env::temp_dir().join(format!("lwk-git-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Temp(fs::canonicalize(&dir).unwrap())
        }

        pub(crate) fn write(&self, path: &str, text: &str) {
            let file = self.0.join(path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, text).unwrap();
        }

        pub(crate) fn git(&self, args: &[&str]) {
            git_in(&self.0, args);
        }

        /// A repository with `files` committed, one commit.
        pub(crate) fn repo(name: &str, files: &[&str]) -> Temp {
            let temp = Temp::new(name);
            temp.git(&["init", "-q", "-b", "main"]);
            for file in files {
                temp.write(file, "one\n");
            }
            temp.git(&["add", "-A"]);
            temp.git(&["commit", "-q", "-m", "seed"]);
            temp
        }
    }

    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"])
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    const GIT: &str = "git";
    const NO_GIT: &str = "lwk-no-such-git-binary";

    fn ask(root: &Path, cache: Option<&Path>, program: &str, ttl: Duration) -> Option<(Signals, From)> {
        let env = Env {
            cache,
            program: OsStr::new(program),
            ttl,
        };
        signals_with(root, &env)
    }

    /// Asked of git: no cache file, and a status never used again.
    fn fresh(root: &Path) -> Signals {
        let (signals, from) = ask(root, None, GIT, Duration::ZERO).unwrap();
        assert_eq!(from.status, Source::Git);
        signals
    }

    fn forget(workdir: &Path) {
        memory().lock().unwrap().remove(workdir);
    }

    #[test]
    fn modified_staged_and_untracked() {
        let temp = Temp::repo("status", &["clean.rs", "modified.rs", "staged.rs", "both.rs", "gone.rs"]);
        temp.write("modified.rs", "two\n");
        temp.write("staged.rs", "two\n");
        temp.write("both.rs", "two\n");
        temp.git(&["add", "staged.rs", "both.rs"]);
        temp.write("both.rs", "three\n");
        temp.write("new dir/untracked file.rs", "one\n");
        temp.write("added.rs", "one\n");
        temp.git(&["add", "added.rs"]);
        fs::remove_file(temp.0.join("gone.rs")).unwrap();

        let signals = fresh(&temp.0);
        let of = |path: &str| signals.status(path);
        assert!(of("clean.rs").is_clean() && !of("clean.rs").is_modified());
        assert_eq!(of("modified.rs"), Status(Status::WT_MODIFIED));
        assert!(of("modified.rs").is_modified() && !of("modified.rs").is_staged());
        assert_eq!(of("staged.rs"), Status(Status::INDEX_MODIFIED));
        assert!(of("staged.rs").is_modified() && of("staged.rs").is_staged());
        assert_eq!(of("both.rs"), Status(Status::INDEX_MODIFIED | Status::WT_MODIFIED));
        assert_eq!(of("new dir/untracked file.rs"), Status(Status::WT_NEW));
        assert!(of("new dir/untracked file.rs").is_untracked());
        assert_eq!(of("added.rs"), Status(Status::INDEX_NEW));
        assert!(!of("added.rs").is_untracked() && of("added.rs").is_staged());
        // Deleted is neither clean nor "modified" to fff.
        assert_eq!(of("gone.rs"), Status(Status::WT_DELETED));
        assert!(!of("gone.rs").is_modified() && !of("gone.rs").is_clean());
    }

    #[test]
    fn recent_commits_count_per_file() {
        let temp = Temp::repo("recency", &["a.rs", "b.rs", "sub/c.rs"]);
        for round in 0..2 {
            temp.write("a.rs", &format!("round {round}\n"));
            temp.git(&["commit", "-q", "-am", "a"]);
        }
        temp.write("sub/c.rs", "two\n");
        temp.git(&["commit", "-q", "-am", "c"]);

        let signals = fresh(&temp.0);
        assert_eq!(signals.recency("a.rs"), 3);
        assert_eq!(signals.recency("b.rs"), 1);
        assert_eq!(signals.recency("sub/c.rs"), 2);
        assert_eq!(signals.recency("none.rs"), 0);
        assert!(signals.status("a.rs").is_clean());

        // Searching a folder of the repository: paths relative to it.
        let sub = fresh(&temp.0.join("sub"));
        assert_eq!(sub.recency("c.rs"), 2);
        assert_eq!(sub.recency("a.rs"), 0);

        // Only the last 10 commits count.
        for round in 0..10 {
            temp.write("b.rs", &format!("round {round}\n"));
            temp.git(&["commit", "-q", "-am", "b"]);
        }
        let signals = fresh(&temp.0);
        assert_eq!(signals.recency("b.rs"), 10);
        assert_eq!(signals.recency("a.rs"), 0);
    }

    #[test]
    fn merges_and_bulk_commits_are_skipped_not_counted() {
        let temp = Temp::repo("bulk", &["a.rs", "sub/s.rs"]);
        // 51 files in one commit, 2 of them under sub/.
        for n in 0..49 {
            temp.write(&format!("bulk/{n}.rs"), "one\n");
        }
        temp.write("sub/b1.rs", "one\n");
        temp.write("sub/b2.rs", "one\n");
        temp.git(&["add", "-A"]);
        temp.git(&["commit", "-q", "-m", "bulk"]);
        // A merge that brings side.rs in.
        temp.git(&["checkout", "-q", "-b", "side"]);
        temp.write("side.rs", "one\n");
        temp.git(&["add", "-A"]);
        temp.git(&["commit", "-q", "-m", "side"]);
        temp.git(&["checkout", "-q", "main"]);
        temp.write("a.rs", "two\n");
        temp.git(&["commit", "-q", "-am", "a"]);
        temp.git(&["merge", "-q", "--no-ff", "-m", "merge", "side"]);
        // Nine more: with the bulk commit and the merge not counted, the
        // seed commit is the tenth.
        for round in 0..7 {
            temp.write("a.rs", &format!("round {round}\n"));
            temp.git(&["commit", "-q", "-am", "a"]);
        }

        let signals = fresh(&temp.0);
        assert_eq!(signals.recency("a.rs"), 9);
        assert_eq!(signals.recency("side.rs"), 1);
        assert_eq!(signals.recency("bulk/0.rs"), 0);
        assert_eq!(signals.recency("sub/b1.rs"), 0);
        assert_eq!(signals.recency("sub/s.rs"), 1);

        // Under sub/ the bulk commit touched 2 files: it counts, and so
        // does every commit that touched nothing there.
        let sub = fresh(&temp.0.join("sub"));
        assert_eq!(sub.recency("b1.rs"), 1);
        assert_eq!(sub.recency("s.rs"), 0);
    }

    #[test]
    fn off_the_base_branch_only_its_own_commits_count() {
        let temp = Temp::repo("branch", &["a.rs", "b.rs"]);
        temp.write("a.rs", "two\n");
        temp.git(&["commit", "-q", "-am", "main a"]);
        temp.git(&["checkout", "-q", "-b", "feature"]);
        // Nothing of its own yet: the merge base is HEAD, the whole
        // history is walked.
        assert_eq!(fresh(&temp.0).recency("a.rs"), 2);

        temp.write("b.rs", "two\n");
        temp.git(&["commit", "-q", "-am", "feature b"]);
        let signals = fresh(&temp.0);
        assert_eq!(signals.recency("b.rs"), 1);
        assert_eq!(signals.recency("a.rs"), 0);

        // Detached at the same commit: still off the base branch.
        temp.git(&["checkout", "-q", "--detach"]);
        assert_eq!(fresh(&temp.0).recency("a.rs"), 0);

        // origin's branch is the base before the local one: with
        // origin/main at the seed commit, main's second commit counts.
        temp.git(&["checkout", "-q", "feature"]);
        temp.git(&["update-ref", "refs/remotes/origin/main", "main~1"]);
        let signals = fresh(&temp.0);
        assert_eq!(signals.recency("a.rs"), 1);
        assert_eq!(signals.recency("b.rs"), 1);

        // Back on main the base branch is HEAD's: everything counts.
        temp.git(&["checkout", "-q", "main"]);
        assert_eq!(fresh(&temp.0).recency("a.rs"), 2);
    }

    #[test]
    fn rename_is_a_new_file() {
        let temp = Temp::repo("rename", &["old.rs", "moved.rs", "keep.rs"]);
        temp.git(&["mv", "moved.rs", "moved2.rs"]);
        temp.git(&["commit", "-q", "-m", "committed rename"]);
        temp.git(&["mv", "old.rs", "new.rs"]);

        let signals = fresh(&temp.0);
        // Staged rename: libgit2 without rename detection sees a new file
        // and a deletion.
        assert_eq!(signals.status("new.rs"), Status(Status::INDEX_NEW));
        assert!(signals.status("new.rs").is_modified());
        assert_eq!(signals.status("old.rs"), Status(Status::INDEX_DELETED));
        // Committed rename: both names were touched by that commit.
        assert_eq!(signals.recency("moved2.rs"), 1);
        assert_eq!(signals.recency("moved.rs"), 2);
        assert_eq!(signals.recency("keep.rs"), 1);
    }

    #[test]
    fn worktree_reads_its_own_index_and_head() {
        let temp = Temp::repo("worktree-main", &["a.rs", "b.rs"]);
        let tree = Temp::new("worktree-linked");
        fs::remove_dir(&tree.0).unwrap();
        temp.git(&["worktree", "add", "-q", "-b", "side", tree.0.to_str().unwrap()]);

        let place = discover(&tree.0).unwrap();
        let main = discover(&temp.0).unwrap();
        assert_eq!(place.workdir, tree.0);
        assert!(place.gitdir.join("HEAD").is_file());
        assert_ne!(place.gitdir, main.gitdir);
        assert_eq!(fs::canonicalize(&place.common).unwrap(), main.gitdir);
        assert_eq!(main.common, main.gitdir);

        tree.write("a.rs", "two\n");
        tree.write("only-here.rs", "one\n");
        let linked = fresh(&tree.0);
        assert!(linked.status("a.rs").is_modified());
        assert!(linked.status("only-here.rs").is_untracked());
        assert_eq!(linked.recency("a.rs"), 1);
        // The main work tree is untouched.
        assert!(fresh(&temp.0).status("a.rs").is_clean());

        // A commit in the worktree changes the worktree's keys only.
        let before = (status_key(&place), refs_key(&place), status_key(&main));
        git_in(&tree.0, &["commit", "-q", "-am", "side"]);
        assert_ne!(status_key(&place), before.0);
        assert_ne!(refs_key(&place), before.1);
        assert_eq!(status_key(&main), before.2);
        // `side` is off main: its one commit is all that counts.
        let linked = fresh(&tree.0);
        assert_eq!(linked.recency("a.rs"), 1);
        assert_eq!(linked.recency("b.rs"), 0);
        assert_eq!(fresh(&temp.0).recency("b.rs"), 1);
    }

    #[test]
    fn no_repository_or_no_git_is_no_signals() {
        let temp = Temp::new("norepo");
        temp.write("a.rs", "one\n");
        assert_eq!(discover(&temp.0), None, "the temp folder is inside a repository");
        assert!(ask(&temp.0, None, GIT, Duration::ZERO).is_none());

        let repo = Temp::repo("nogit", &["a.rs"]);
        assert!(ask(&repo.0, None, NO_GIT, Duration::ZERO).is_none());
        // A `.git` that is no repository: git fails, no signals.
        let broken = Temp::new("broken");
        broken.write(".git/HEAD", "garbage\n");
        assert!(ask(&broken.0, None, GIT, Duration::ZERO).is_none());
    }

    #[test]
    fn no_commit_yet_has_status_and_no_recency() {
        let temp = Temp::new("unborn");
        temp.git(&["init", "-q", "-b", "main"]);
        temp.write("a.rs", "one\n");
        let signals = fresh(&temp.0);
        assert!(signals.status("a.rs").is_untracked());
        assert_eq!(signals.recency("a.rs"), 0);
    }

    #[test]
    fn status_is_fresh_and_commits_are_cached() {
        let temp = Temp::repo("cache", &["a.rs", "b.rs"]);
        let cache = Temp::new("cache-dir");
        let dir = Some(cache.0.as_path());
        let long = Duration::from_secs(600);
        let from = |status, log| From { status, log };
        temp.write("a.rs", "two\n");

        let (first, came) = ask(&temp.0, dir, GIT, long).unwrap();
        assert_eq!(came, from(Source::Git, Source::Git));
        assert!(first.status("a.rs").is_modified() && first.status("b.rs").is_clean());
        let status_file = cache.0.join(file_name(&temp.0, "status"));
        let log_file = cache.0.join(file_name(&temp.0, "git"));
        assert!(status_file.is_file() && log_file.is_file());

        // Within the time and with index and HEAD as they were: memory,
        // then (another process) the files. git is not needed.
        let (_, came) = ask(&temp.0, dir, NO_GIT, long).unwrap();
        assert_eq!(came, from(Source::Memory, Source::Memory));
        forget(&temp.0);
        let (again, came) = ask(&temp.0, dir, NO_GIT, long).unwrap();
        assert_eq!(came, from(Source::Disk, Source::Disk));
        assert!(again.status("a.rs").is_modified());
        assert_eq!(again.recency("b.rs"), 1);

        // An edit touches neither index nor HEAD: seen once the status is
        // too old, in memory or on disk. The commits stay cached.
        temp.write("b.rs", "two\n");
        let (stale, _) = ask(&temp.0, dir, GIT, long).unwrap();
        assert!(stale.status("b.rs").is_clean());
        let (edited, came) = ask(&temp.0, dir, GIT, Duration::ZERO).unwrap();
        assert_eq!(came, from(Source::Git, Source::Memory));
        assert!(edited.status("b.rs").is_modified());
        forget(&temp.0);
        std::thread::sleep(Duration::from_millis(30));
        let (_, came) = ask(&temp.0, dir, GIT, Duration::from_millis(20)).unwrap();
        assert_eq!(came, from(Source::Git, Source::Disk));
        // Too old and no git: no signals, not stale ones.
        forget(&temp.0);
        assert!(ask(&temp.0, dir, NO_GIT, Duration::ZERO).is_none());

        // The index changes (git add): the status is asked again whatever
        // its age; the refs did not change.
        ask(&temp.0, dir, GIT, long).unwrap();
        temp.git(&["add", "a.rs"]);
        let (staged, came) = ask(&temp.0, dir, GIT, long).unwrap();
        assert_eq!(came, from(Source::Git, Source::Memory));
        assert!(staged.status("a.rs").is_staged());
        forget(&temp.0);
        let (_, came) = ask(&temp.0, dir, NO_GIT, long).unwrap();
        assert_eq!(came, from(Source::Disk, Source::Disk));

        // A commit moves the branch: both asked again.
        temp.git(&["commit", "-q", "-m", "a"]);
        let (committed, came) = ask(&temp.0, dir, GIT, long).unwrap();
        assert_eq!(came, from(Source::Git, Source::Git));
        assert_eq!(committed.recency("a.rs"), 2);
        // HEAD changes (a detached checkout): both asked again.
        temp.git(&["checkout", "-q", "--detach"]);
        let (_, came) = ask(&temp.0, dir, GIT, long).unwrap();
        assert_eq!(came, from(Source::Git, Source::Git));

        // A damaged file is a miss.
        forget(&temp.0);
        for file in [&status_file, &log_file] {
            let mut bytes = fs::read(file).unwrap();
            let last = bytes.len() - 9;
            bytes[last] ^= 1;
            fs::write(file, &bytes).unwrap();
        }
        assert!(ask(&temp.0, dir, NO_GIT, long).is_none());
        let (_, came) = ask(&temp.0, dir, GIT, long).unwrap();
        assert_eq!(came, from(Source::Git, Source::Git));
    }

    #[test]
    fn parses_git_output() {
        let out = b"1 .M N... 100644 100644 100644 aa bb a b.rs\0\
            2 R. N... 100644 100644 100644 aa bb R100 new name.rs\0old name.rs\0\
            2 CM N... 100644 100644 100644 aa bb C90 copy.rs\0source.rs\0\
            u UU N... 100644 100644 100644 100644 aa bb cc conflict.rs\0\
            1 .A N... 000000 000000 100644 aa bb intent.rs\0\
            1 T. N... 100644 120000 120000 aa bb link.rs\0\
            ? old name.rs\0! ignored.rs\0";
        let status: HashMap<String, Status> = parse_status(out).into_iter().collect();
        assert_eq!(status["a b.rs"], Status(Status::WT_MODIFIED));
        assert_eq!(status["new name.rs"], Status(Status::INDEX_NEW));
        assert_eq!(status["old name.rs"], Status(Status::INDEX_DELETED | Status::WT_NEW));
        assert_eq!(status["copy.rs"], Status(Status::INDEX_NEW | Status::WT_MODIFIED));
        assert!(!status.contains_key("source.rs") && !status.contains_key("ignored.rs"));
        assert_eq!(status["conflict.rs"], Status(Status::CONFLICTED));
        assert!(!status["conflict.rs"].is_modified() && !status["conflict.rs"].is_clean());
        assert_eq!(status["intent.rs"], Status(Status::WT_NEW));
        assert_eq!(status["link.rs"], Status(Status::INDEX_TYPECHANGE));
        assert!(status["link.rs"].is_staged() && !status["link.rs"].is_modified());

        // One parent, two parents (a merge), an empty commit, a root.
        let log = b"\x01aa\x02\0\na.rs\0b c.rs\0\x01aa bb\x02\0\x01cc\x02\0\x01\x02\0\na.rs\0";
        let commit = |merge, files: &[&str]| Commit {
            merge,
            files: files.iter().map(|file| (*file).to_owned()).collect(),
        };
        assert_eq!(
            parse_log(log),
            vec![
                commit(false, &["a.rs", "b c.rs"]),
                commit(true, &[]),
                commit(false, &[]),
                commit(false, &["a.rs"]),
            ]
        );
        assert!(parse_log(b"").is_empty());
    }
}
