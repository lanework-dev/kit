//! A persistent cache of the file list `list_files` and `find_files` walk,
//! one file per folder, checked on every call the way git checks its
//! untracked cache.
//!
//! - Where: `$XDG_CACHE_HOME/lwk`, else `~/Library/Caches/lwk` (macOS) or
//!   `~/.cache/lwk`; one file per real root and walk, named by a hash of
//!   its path: `<hash>.bin` for list_files' walk ([`files::Walk::Files`]),
//!   `<hash>-hidden.bin` for find_files' walk in a git work tree
//!   ([`files::Walk::Hidden`]), whose env key also says so. The two never
//!   share a file. `LWK_NO_CACHE=1` turns the cache off, reading and
//!   writing.
//! - What: every folder the walk ([`files::walker`]) went into, with its
//!   mtime, ctime and inode; the files in it; and every ignore source the
//!   walk consulted: `.gitignore`, `.ignore`, `.rgignore` and `.git` in each
//!   folder walked and each parent of the root, the `info/exclude` each
//!   `.git` leads to (through a worktree's `gitdir:` and `commondir`), git's
//!   config files and the global excludes file `ignore` resolved. With
//!   `require_git(false)` the walk never reads `.git/HEAD` or the index.
//! - Checked on a call: every ignore source is stat'ed, and any change
//!   (or a missing one appearing) means a full walk. Every folder is
//!   stat'ed; a folder whose stat changed is listed again (one level); a
//!   folder found in it that was not known is walked whole; a folder gone
//!   is dropped with all under it. A folder listed again whose ignore files
//!   differ means a full walk.
//! - Racily clean: a folder or source whose mtime or ctime is not older
//!   than the cache's stamp, the second the walk that wrote it started
//!   minus one (for file systems with 1 or 2 s mtimes), is treated as
//!   changed.
//! - The answer is the uncached walk's: same files, same sort. A walk that
//!   ran out of `WALK_TIMEOUT` answers what it found and writes nothing.
//!   A tree of more than [`MAX_FILES`] files, or with a path that is not
//!   UTF-8, is not cached: a marker sends the next hour's calls to the
//!   uncached walk.
//! - Written to a temporary file and renamed, so concurrent `lwk`s never
//!   read a half-written cache; a cache that fails to parse or whose
//!   checksum is wrong is a miss.
//!
//! The format (little-endian; `var` is LEB128, `bytes` a `var` length and
//! the bytes):
//!
//! ```text
//! magic "LWKFILES"  version u32  kind u8 (0 listing, 1 skip)
//! stamp i64 ns      root bytes   env bytes (HOME, XDG_CONFIG_HOME,
//!                                GIT_CONFIG_GLOBAL, GIT_CONFIG_SYSTEM, cwd)
//! kind 1: until i64 ns
//! kind 0: sources var × (path bytes, stat)
//!         folders var × (rel bytes, mtime i64, ctime i64, ino var,
//!                        marks var × (path bytes, stat))
//!         files   var × (folder var, path bytes), sorted as JS sorts
//! checksum u64 over everything before it
//! stat: u8 0 missing | 1 folder | 2 other | 3 file, mtime i64, ctime i64,
//!       size var, ino var
//! ```

use super::files::{self, Walk as WalkKind};
use ignore::{DirEntry, ParallelVisitor, ParallelVisitorBuilder, WalkState};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAGIC: &[u8; 8] = b"LWKFILES";
const VERSION: u32 = 1;

/// A tree with more files than this is not cached.
pub(crate) const MAX_FILES: usize = 500_000;

/// How long a tree found too big (or not UTF-8) goes uncached.
const SKIP_FOR: Duration = Duration::from_secs(3600);

/// The files of a folder, as an uncached walk lists them.
pub(crate) struct Listing {
    /// Relative, `/`-separated, sorted as JavaScript sorts strings.
    pub files: Vec<String>,
    /// False when the walk ran out of `WALK_TIMEOUT`.
    pub complete: bool,
}

/// `root`'s files as `kind` walks them, from the cache, brought up to
/// date; None when the cache is off for `root` and the caller walks it
/// uncached.
pub(crate) fn listing(root: &Path, kind: WalkKind) -> Option<Listing> {
    let settings = settings()?;
    let found = match RAM.get() {
        Some(ram) => ram.listing_with(root, &settings, kind),
        None => listing_with(root, &settings, kind),
    };
    found.map(|(listing, _)| listing)
}

// ── Kept in RAM (`lwk serve`) ───────────────────────────────────────────────

/// How long a folder's files stay in RAM unused.
const RAM_UNUSED_FOR: Duration = Duration::from_secs(600);

/// The caches a long-lived process keeps; unset in `lwk` and `lwk mcp`.
static RAM: OnceLock<Ram> = OnceLock::new();

/// From now on, keep the caches of the `capacity` roots used last in this
/// process's memory, so a call need not read and parse the cache file.
///
/// - Each call still checks the cache as [`listing_with`] does (every
///   folder and ignore source stat'ed) and answers the same files.
/// - Inside [`checked_meanwhile`] that check runs while the caller already
///   works on the files held; the caller then learns whether they held.
/// - The cache file is read when a root is not in memory, and written
///   whenever the check finds a change: another `lwk` starts warm.
/// - `LWK_NO_CACHE=1` turns this off with the rest.
pub(crate) fn keep_in_ram(capacity: usize) {
    let _ = RAM.set(Ram::new(capacity));
}

thread_local! {
    /// Inside [`checked_meanwhile`]: the checks of the listings this
    /// thread was given before their check ended.
    static AHEAD: std::cell::RefCell<Option<Vec<Arc<Check>>>> = const { std::cell::RefCell::new(None) };
}

/// Run `call`, a kit call on this thread, giving it the files held in
/// memory at once while their check (the same one) runs on other threads.
/// Returns its answer and whether every listing it was given that way held:
/// when false, a folder or ignore source changed, the answer is not to be
/// used, and the call is run again outside this, where the check comes
/// first.
///
/// Nothing is given ahead unless [`keep_in_ram`] is on.
pub(crate) fn checked_meanwhile<T>(call: impl FnOnce() -> T) -> (T, bool) {
    AHEAD.with(|ahead| *ahead.borrow_mut() = Some(Vec::new()));
    let answer = call();
    let checks = AHEAD.with(|ahead| ahead.borrow_mut().take()).unwrap_or_default();
    // Every check is waited for, so none is left running.
    let mut held = true;
    for check in &checks {
        held &= check.finish();
    }
    (answer, held)
}

struct Ram {
    capacity: usize,
    roots: Mutex<Vec<Kept>>,
    crew: Arc<Crew>,
}

struct Kept {
    root: PathBuf,
    kind: WalkKind,
    used: Instant,
    /// Locked for the whole of a call's check: two calls on one root check
    /// one after the other. None until a walk ends whole.
    held: Arc<Mutex<Option<Held>>>,
}

/// A cache held in memory, with what its check needs made once.
struct Held {
    cache: Cache,
    /// Each of `cache.folders`, as a path and as it was when listed.
    folders: Arc<Vec<(PathBuf, FolderStat)>>,
    /// Every ignore source: `cache.sources` and the folders' marks.
    sources: Arc<Vec<Source>>,
}

impl Held {
    fn new(root: &Path, cache: Cache) -> Held {
        let folders = cache
            .folders
            .iter()
            .map(|folder| (join_rel(root, &folder.rel), folder.stat))
            .collect();
        let marks = cache.folders.iter().flat_map(|folder| folder.marks.iter());
        let sources = cache.sources.iter().chain(marks).cloned().collect();
        Held {
            cache,
            folders: Arc::new(folders),
            sources: Arc::new(sources),
        }
    }

    /// Whether this is `root`'s cache as this process walks it now, with
    /// the root folder first as [`update`] needs it.
    fn usable(&self, root_bytes: &[u8], env: &[u8]) -> bool {
        let cache = &self.cache;
        cache.root == root_bytes
            && cache.env == env
            && cache.folders.first().is_some_and(|root| root.rel.is_empty())
    }
}

/// Threads that stat a held cache's folders, kept for the process's life:
/// a check takes about the time the stats do, with none spent starting
/// threads, and can run while its caller does something else.
struct Crew {
    checks: Mutex<VecDeque<Arc<Check>>>,
    ready: Condvar,
    helpers: usize,
}

/// Whether [`update`] would find a held cache unchanged (`Unchanged`): no
/// ignore source stale, and every folder there with the stat it had, none
/// of them racy. The same stats, read the same way, without the tables an
/// update needs; shared by the threads working on it.
struct Check {
    folders: Arc<Vec<(PathBuf, FolderStat)>>,
    sources: Arc<Vec<Source>>,
    /// The racy line of the walk that recorded them.
    then: i64,
    /// The next part no thread took yet.
    next: AtomicUsize,
    changed: AtomicBool,
    /// Parts done.
    done: Mutex<usize>,
    all_done: Condvar,
}

/// Folders a thread stats before it takes more.
const CHECK_PART: usize = 64;

impl Check {
    /// The sources are part 0, the folders the parts after.
    fn parts(&self) -> usize {
        1 + self.folders.len().div_ceil(CHECK_PART)
    }

    fn part_as_it_was(&self, part: usize) -> bool {
        if part == 0 {
            return !self.sources.iter().any(|source| source.stale(self.then));
        }
        let from = (part - 1) * CHECK_PART;
        let to = self.folders.len().min(from + CHECK_PART);
        self.folders[from..to]
            .iter()
            .all(|(path, stat)| FolderStat::at(path) == Some(*stat) && !stat.racy(self.then))
    }

    /// Take parts until none is left. Once something changed the rest is
    /// not read: the update that follows stats it all.
    fn work(&self) {
        loop {
            let part = self.next.fetch_add(1, Ordering::Relaxed);
            if part >= self.parts() {
                return;
            }
            if !self.changed.load(Ordering::Relaxed) && !self.part_as_it_was(part) {
                self.changed.store(true, Ordering::Relaxed);
            }
            let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
            *done += 1;
            if *done >= self.parts() {
                self.all_done.notify_all();
            }
        }
    }

    /// Help with what is left, wait for the rest, and say whether nothing
    /// changed.
    fn finish(&self) -> bool {
        self.work();
        // A helper may still be on the part it took.
        let mut done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        while *done < self.parts() {
            done = self.all_done.wait(done).unwrap_or_else(|e| e.into_inner());
        }
        !self.changed.load(Ordering::Relaxed)
    }
}

impl Crew {
    fn new() -> Arc<Crew> {
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        // More than four threads stat no faster.
        let helpers = if cores > 1 { cores.min(4) } else { 0 };
        let crew = Arc::new(Crew {
            checks: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
            helpers,
        });
        let mut started = 0;
        for at in 0..helpers {
            let helper = crew.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("lwk-stat-{at}"))
                .spawn(move || helper.help());
            started += usize::from(spawned.is_ok());
        }
        if started == helpers {
            return crew;
        }
        // Whoever asks for a check does it all; a helper that did start
        // finds nothing to do.
        Arc::new(Crew {
            checks: Mutex::new(VecDeque::new()),
            ready: Condvar::new(),
            helpers: 0,
        })
    }

    fn help(&self) {
        loop {
            let check = {
                let mut checks = self.checks.lock().unwrap_or_else(|e| e.into_inner());
                loop {
                    match checks.pop_front() {
                        Some(check) => break check,
                        None => checks = self.ready.wait(checks).unwrap_or_else(|e| e.into_inner()),
                    }
                }
            };
            check.work();
        }
    }

    /// Start checking `held` on the helpers; [`Check::finish`] ends it.
    fn start(&self, held: &Held) -> Arc<Check> {
        let check = Arc::new(Check {
            folders: held.folders.clone(),
            sources: held.sources.clone(),
            then: held.cache.stamp,
            next: AtomicUsize::new(0),
            changed: AtomicBool::new(false),
            done: Mutex::new(0),
            all_done: Condvar::new(),
        });
        let helpers = self.helpers.min(check.parts());
        if helpers > 0 {
            let mut checks = self.checks.lock().unwrap_or_else(|e| e.into_inner());
            checks.extend((0..helpers).map(|_| check.clone()));
            drop(checks);
            self.ready.notify_all();
        }
        check
    }
}

impl Ram {
    fn new(capacity: usize) -> Ram {
        Ram {
            capacity: capacity.max(1),
            roots: Mutex::new(Vec::new()),
            crew: Crew::new(),
        }
    }

    /// `root`'s place for `kind`'s walk, made if need be; the one unused
    /// the longest goes when there are too many, and any unused for
    /// [`RAM_UNUSED_FOR`].
    fn place(&self, root: &Path, kind: WalkKind) -> Arc<Mutex<Option<Held>>> {
        let now = Instant::now();
        let mut roots = self.roots.lock().unwrap_or_else(|e| e.into_inner());
        roots.retain(|kept| now.duration_since(kept.used) < RAM_UNUSED_FOR);
        if let Some(kept) = roots.iter_mut().find(|kept| kept.root == root && kept.kind == kind) {
            kept.used = now;
            return kept.held.clone();
        }
        if roots.len() >= self.capacity {
            let oldest = roots.iter().enumerate().min_by_key(|(_, kept)| kept.used);
            if let Some((at, _)) = oldest {
                roots.swap_remove(at);
            }
        }
        let held = Arc::new(Mutex::new(None));
        roots.push(Kept {
            root: root.to_path_buf(),
            kind,
            used: now,
            held: held.clone(),
        });
        held
    }

    /// [`listing_with`], from the cache held for `root` when there is one.
    fn listing_with(&self, root: &Path, settings: &Settings, kind: WalkKind) -> Option<(Listing, Outcome)> {
        let files = |cache: &Cache| cache.files.iter().map(|file| file.path.clone()).collect();
        let place = self.place(root, kind);
        let mut held = place.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(kept) = held.as_ref().filter(|kept| kept.usable(bytes_of(root.as_os_str()), &env_key(kind))) {
            let check = self.crew.start(kept);
            // Inside `checked_meanwhile` the caller goes on with these
            // files and hears of the check there.
            let ahead = AHEAD.with(|ahead| {
                let mut ahead = ahead.borrow_mut();
                ahead.as_mut().map(|checks| checks.push(check.clone())).is_some()
            });
            let listing = Listing {
                files: files(&kept.cache),
                complete: true,
            };
            if ahead || check.finish() {
                return Some((listing, Outcome::Hit));
            }
        }
        let old = held.take().map(|kept| kept.cache);
        let (cache, outcome) = refresh(root, settings, old, kind)?;
        let listing = Listing {
            files: files(&cache),
            complete: outcome != Outcome::Partial,
        };
        // A walk cut short is not the folder's files: not kept.
        if listing.complete {
            *held = Some(Held::new(root, cache));
        }
        Some((listing, outcome))
    }
}

// ── Settings ────────────────────────────────────────────────────────────────

pub(crate) struct Settings {
    pub dir: PathBuf,
    pub max_files: usize,
    /// Take the racy line back to the whole second before, for file
    /// systems with coarse mtimes. Off only in tests.
    pub coarse: bool,
}

#[cfg(not(test))]
fn settings() -> Option<Settings> {
    Settings::from_env(|name| std::env::var_os(name))
}

/// Tests never touch the user's cache.
#[cfg(test)]
fn settings() -> Option<Settings> {
    Some(Settings {
        dir: std::env::temp_dir().join("lwk-test-cache"),
        max_files: MAX_FILES,
        coarse: true,
    })
}

impl Settings {
    /// The cache folder the environment names; None with `LWK_NO_CACHE`.
    pub(crate) fn from_env(var: impl Fn(&str) -> Option<OsString>) -> Option<Settings> {
        if var("LWK_NO_CACHE").is_some_and(|value| !value.is_empty() && value != "0") {
            return None;
        }
        let set = |name: &str| var(name).filter(|value| !value.is_empty()).map(PathBuf::from);
        let dir = match set("XDG_CACHE_HOME").filter(|dir| dir.is_absolute()) {
            Some(dir) => dir.join("lwk"),
            None => {
                #[allow(deprecated)]
                let home = set("HOME").or_else(std::env::home_dir)?;
                if cfg!(target_os = "macos") {
                    home.join("Library/Caches/lwk")
                } else {
                    home.join(".cache/lwk")
                }
            }
        };
        Some(Settings {
            dir,
            max_files: MAX_FILES,
            coarse: true,
        })
    }
}

// ── The call ────────────────────────────────────────────────────────────────

/// What a call did with the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// Nothing changed; nothing walked, nothing written.
    Hit,
    /// Some folders listed again or walked; the cache rewritten.
    Updated { relisted: usize, walked: usize },
    /// No usable cache, or an ignore source changed: a full walk.
    Full,
    /// Ran out of time: what was found, nothing written.
    Partial,
}

pub(crate) fn listing_with(root: &Path, settings: &Settings, kind: WalkKind) -> Option<(Listing, Outcome)> {
    let (cache, outcome) = refresh(root, settings, None, kind)?;
    let complete = outcome != Outcome::Partial;
    let files = cache.files.into_iter().map(|file| file.path).collect();
    Some((Listing { files, complete }, outcome))
}

/// `root`'s cache brought up to date, and what that took: `held` (the one
/// a long-lived process kept) when it is this root's, else the cache file.
/// None when the cache is off for `root`.
fn refresh(root: &Path, settings: &Settings, held: Option<Cache>, kind: WalkKind) -> Option<(Cache, Outcome)> {
    if !cfg!(unix) || too_broad(root) {
        return None;
    }
    let start = SystemTime::now();
    let deadline = Instant::now() + files::WALK_TIMEOUT;
    let stamp = racy_stamp(start, settings.coarse);
    let file = settings.dir.join(file_name(root, kind));
    let env = env_key(kind);
    let root_bytes = bytes_of(root.as_os_str()).to_vec();

    let held = held.filter(|cache| cache.root == root_bytes && cache.env == env);
    let old = match held {
        Some(cache) => Some(Stored::Cache(cache)),
        None => std::fs::read(&file).ok().and_then(|bytes| decode(&bytes)),
    };
    let old = match old {
        Some(Stored::Skip(skip)) if skip.root == root_bytes && skip.env == env => {
            if nanos(start) < skip.until {
                return None;
            }
            None
        }
        Some(Stored::Cache(cache)) if cache.root == root_bytes && cache.env == env => Some(cache),
        _ => None,
    };

    let (cache, outcome) = match old.map(|old| update(root, old, stamp, deadline, kind)) {
        Some(Update::Unchanged(cache)) => (cache, Outcome::Hit),
        Some(Update::Changed(cache, relisted, walked)) => {
            write(&file, &encode_cache(&cache));
            (cache, Outcome::Updated { relisted, walked })
        }
        Some(Update::Partial(cache)) => (cache, Outcome::Partial),
        Some(Update::Full) | None => {
            let walked = full_walk(root, &root_bytes, env, stamp, deadline, settings.max_files, kind);
            match walked {
                Walked::Done(cache) => {
                    write(&file, &encode_cache(&cache));
                    (cache, Outcome::Full)
                }
                Walked::Partial(cache) => (cache, Outcome::Partial),
                Walked::Skip(skip) => {
                    write(&file, &encode_skip(&skip));
                    return None;
                }
                Walked::Failed => return None,
            }
        }
    };
    Some((cache, outcome))
}

/// `/` and the home folder are never cached (scope refuses them too).
fn too_broad(root: &Path) -> bool {
    root.parent().is_none()
        || crate::paths::home_dir()
            .and_then(|home| std::fs::canonicalize(home).ok())
            .is_some_and(|home| home == root)
}

fn file_name(root: &Path, kind: WalkKind) -> String {
    let hash = fnv1a(bytes_of(root.as_os_str()));
    match kind {
        WalkKind::Files => format!("{hash:016x}.bin"),
        WalkKind::Hidden => format!("{hash:016x}-hidden.bin"),
    }
}

/// Everything outside the tree that changes what the walk reads: the
/// variables git's config is found by, and the cwd the global excludes are
/// matched against. A hidden walk's key says so too, so a cache of one walk
/// is never taken for the other's.
fn env_key(kind: WalkKind) -> Vec<u8> {
    let mut key = Vec::new();
    if kind == WalkKind::Hidden {
        key.extend_from_slice(b"walk=hidden\0");
    }
    for name in ["HOME", "XDG_CONFIG_HOME", "GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM"] {
        key.extend_from_slice(name.as_bytes());
        match std::env::var_os(name) {
            Some(value) => {
                key.push(b'=');
                key.extend_from_slice(bytes_of(&value));
            }
            None => key.push(0xff),
        }
        key.push(0);
    }
    key.extend_from_slice(b"cwd=");
    if let Ok(cwd) = std::env::current_dir() {
        key.extend_from_slice(bytes_of(cwd.as_os_str()));
    }
    key
}

/// The racily-clean line: anything whose time is not before it may have
/// changed after the walk read it. The second the walk started, minus one,
/// so that a file system with coarse mtimes is covered.
fn racy_stamp(start: SystemTime, coarse: bool) -> i64 {
    let at = nanos(start);
    if !coarse {
        return at;
    }
    at - at.rem_euclid(1_000_000_000) - 1_000_000_000
}

fn nanos(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(since) => i64::try_from(since.as_nanos()).unwrap_or(i64::MAX),
        Err(before) => -i64::try_from(before.duration().as_nanos()).unwrap_or(i64::MAX),
    }
}

#[cfg(unix)]
fn bytes_of(text: &std::ffi::OsStr) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    text.as_bytes()
}

#[cfg(not(unix))]
fn bytes_of(text: &std::ffi::OsStr) -> &[u8] {
    text.as_encoded_bytes()
}

#[cfg(unix)]
fn path_of(bytes: &[u8]) -> &Path {
    use std::os::unix::ffi::OsStrExt;
    Path::new(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn path_of(bytes: &[u8]) -> &Path {
    // Never reached: the cache is off where this is not unix.
    Path::new(std::str::from_utf8(bytes).unwrap_or(""))
}

// ── Stats ───────────────────────────────────────────────────────────────────

/// What a file the walk read looked like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stat {
    Missing,
    /// A folder: only that it is one (`.git` changes all the time).
    Folder,
    Other,
    File {
        mtime: i64,
        ctime: i64,
        size: u64,
        ino: u64,
    },
}

impl Stat {
    /// `path`, symlinks followed, as `ignore` opens it.
    fn of(path: &Path) -> Stat {
        match std::fs::metadata(path) {
            Err(_) => Stat::Missing,
            Ok(meta) if meta.is_dir() => Stat::Folder,
            Ok(meta) if meta.is_file() => {
                let (mtime, ctime, ino) = times(&meta);
                Stat::File {
                    mtime,
                    ctime,
                    size: meta.len(),
                    ino,
                }
            }
            Ok(_) => Stat::Other,
        }
    }

    fn racy(&self, stamp: i64) -> bool {
        matches!(*self, Stat::File { mtime, ctime, .. } if mtime >= stamp || ctime >= stamp)
    }
}

#[cfg(unix)]
fn times(meta: &std::fs::Metadata) -> (i64, i64, u64) {
    use std::os::unix::fs::MetadataExt;
    let ns = |secs: i64, nsec: i64| secs.saturating_mul(1_000_000_000).saturating_add(nsec);
    (
        ns(meta.mtime(), meta.mtime_nsec()),
        ns(meta.ctime(), meta.ctime_nsec()),
        meta.ino(),
    )
}

#[cfg(not(unix))]
fn times(_: &std::fs::Metadata) -> (i64, i64, u64) {
    (i64::MAX, i64::MAX, 0)
}

/// An ignore source: a file the walk read, or looked for.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Source {
    path: Vec<u8>,
    stat: Stat,
}

impl Source {
    fn of(path: PathBuf) -> Source {
        let stat = Stat::of(&path);
        Source {
            path: bytes_of(path.as_os_str()).to_vec(),
            stat,
        }
    }

    /// Whether it may differ from what the walk read.
    fn stale(&self, stamp: i64) -> bool {
        self.stat.racy(stamp) || Stat::of(path_of(&self.path)) != self.stat
    }
}

/// The ignore files `ignore` reads in `dir`, as `add_child_path` finds
/// them with `require_git(false)`: `.rgignore`, `.ignore`, `.gitignore`,
/// and `.git`'s `info/exclude` (through a worktree's `.git` file and its
/// `commondir`). Those of the four names that are missing are kept only
/// with `missing`; a folder walked keeps them out, its mtime tells when one
/// appears.
fn marks(dir: &Path, missing: bool) -> Vec<Source> {
    let mut out = Vec::new();
    for name in [".gitignore", ".ignore", ".rgignore", ".git"] {
        let source = Source::of(dir.join(name));
        if missing || source.stat != Stat::Missing {
            out.push(source);
        }
    }
    let git = dir.join(".git");
    match out.iter().find(|s| s.path == bytes_of(git.as_os_str())).map(|s| s.stat) {
        None | Some(Stat::Missing) => {}
        Some(Stat::File { .. }) => {
            if let Some(common) = worktree_common_dir(&git, &mut out) {
                out.push(Source::of(common.join("info/exclude")));
            }
        }
        Some(_) => out.push(Source::of(git.join("info/exclude"))),
    }
    out
}

/// `ignore`'s `resolve_git_commondir` for a `.git` file, the files it read
/// added to `out`.
fn worktree_common_dir(git: &Path, out: &mut Vec<Source>) -> Option<PathBuf> {
    let text = std::fs::read(git).ok()?;
    let line = first_line(&text)?;
    let real = line.strip_prefix(b"gitdir: ")?;
    let real = path_of(real).to_path_buf();
    let commondir = real.join("commondir");
    out.push(Source::of(commondir.clone()));
    let text = std::fs::read(&commondir).ok()?;
    let line = first_line(&text)?;
    let common = path_of(line);
    Some(if line.starts_with(b".") {
        real.join(common)
    } else {
        common.to_path_buf()
    })
}

/// The first line as `BufRead::lines` gives it; None when it is not UTF-8.
fn first_line(text: &[u8]) -> Option<&[u8]> {
    if text.is_empty() {
        return None;
    }
    let end = text.iter().position(|&b| b == b'\n').unwrap_or(text.len());
    let mut line = &text[..end];
    if line.ends_with(b"\r") {
        line = &line[..line.len() - 1];
    }
    std::str::from_utf8(line).ok()?;
    Some(line)
}

/// The ignore sources outside the tree: each parent's ignore files, git's
/// config files and the global excludes file `ignore` resolved.
fn outer_sources(root: &Path) -> Vec<Source> {
    let mut out = Vec::new();
    for parent in root.ancestors().skip(1) {
        out.extend(marks(parent, true));
    }
    let var = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    #[allow(deprecated)]
    let home = std::env::home_dir();
    if let Some(global) = var("GIT_CONFIG_GLOBAL") {
        out.push(Source::of(PathBuf::from(global)));
    }
    if let Some(home) = &home {
        out.push(Source::of(home.join(".gitconfig")));
    }
    let config = var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home.as_ref().map(|home| home.join(".config")));
    if let Some(config) = config {
        out.push(Source::of(config.join("git/config")));
    }
    let system = var("GIT_CONFIG_SYSTEM").unwrap_or_else(|| "/etc/gitconfig".into());
    out.push(Source::of(PathBuf::from(system)));
    if let Some(excludes) = ignore::gitignore::gitconfig_excludes_path() {
        out.push(Source::of(excludes));
    }
    out
}

/// A folder's stat; a folder replaced by another is a new inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FolderStat {
    mtime: i64,
    ctime: i64,
    ino: u64,
}

impl FolderStat {
    /// For a folder whose stat could not be read: always racy.
    const UNKNOWN: FolderStat = FolderStat {
        mtime: i64::MAX,
        ctime: i64::MAX,
        ino: 0,
    };

    fn of(meta: &std::fs::Metadata) -> FolderStat {
        let (mtime, ctime, ino) = times(meta);
        FolderStat { mtime, ctime, ino }
    }

    /// `path` as a folder, symlinks not followed; None when it is not one.
    fn at(path: &Path) -> Option<FolderStat> {
        let meta = std::fs::symlink_metadata(path).ok()?;
        meta.is_dir().then(|| FolderStat::of(&meta))
    }

    fn racy(&self, stamp: i64) -> bool {
        self.mtime >= stamp || self.ctime >= stamp
    }
}

// ── The cache ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Folder {
    /// Relative to the root, `/`-separated; empty for the root.
    rel: Vec<u8>,
    stat: FolderStat,
    /// Its ignore files that exist (see [`marks`]).
    marks: Vec<Source>,
}

#[derive(Debug, Clone)]
struct FileEntry {
    folder: u32,
    path: String,
}

#[derive(Debug)]
struct Cache {
    stamp: i64,
    root: Vec<u8>,
    env: Vec<u8>,
    sources: Vec<Source>,
    /// Sorted by `rel`, so a folder comes before everything under it.
    folders: Vec<Folder>,
    /// Sorted as JavaScript sorts strings.
    files: Vec<FileEntry>,
}

#[derive(Debug)]
struct Skip {
    root: Vec<u8>,
    env: Vec<u8>,
    until: i64,
}

enum Stored {
    Cache(Cache),
    Skip(Skip),
}

/// `rel`'s folder: everything before its last `/`.
fn parent_rel(rel: &[u8]) -> &[u8] {
    match rel.iter().rposition(|&b| b == b'/') {
        Some(at) => &rel[..at],
        None => &[],
    }
}

fn join_rel(root: &Path, rel: &[u8]) -> PathBuf {
    if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(path_of(rel))
    }
}

/// Folders and files gathered by a walk, made into a cache; None when a
/// file's folder is not among `folders`.
fn assemble(
    head: (i64, Vec<u8>, Vec<u8>, Vec<Source>),
    mut folders: Vec<Folder>,
    files: Vec<String>,
) -> Option<Cache> {
    let (stamp, root, env, sources) = head;
    folders.sort_unstable_by(|a, b| a.rel.cmp(&b.rel));
    folders.dedup_by(|a, b| a.rel == b.rel);
    let index: HashMap<&[u8], u32> = folders
        .iter()
        .enumerate()
        .map(|(at, folder)| (folder.rel.as_slice(), at as u32))
        .collect();
    let mut entries = Vec::with_capacity(files.len());
    for path in files {
        let folder = *index.get(parent_rel(path.as_bytes()))?;
        entries.push(FileEntry { folder, path });
    }
    drop(index);
    entries.sort_unstable_by(|a, b| files::js_cmp(&a.path, &b.path));
    Some(Cache {
        stamp,
        root,
        env,
        sources,
        folders,
        files: entries,
    })
}

// ── Walking ─────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Gathered {
    folders: Vec<Folder>,
    files: Vec<String>,
    /// With `shallow`: the folders found one level down.
    children: Vec<Vec<u8>>,
    not_utf8: bool,
}

struct Gather<'s> {
    root: &'s Path,
    shallow: bool,
    deadline: Instant,
    /// Set when `lwk serve`'s caller cancelled the call; the walk then
    /// stops as one out of time does, and nothing is written.
    cancel: files::Cancel,
    max_files: usize,
    timed_out: &'s AtomicBool,
    too_many: &'s AtomicBool,
    seen: &'s AtomicUsize,
    out: &'s Mutex<Gathered>,
}

struct Gatherer<'s> {
    shared: &'s Gather<'s>,
    local: Gathered,
    seen: usize,
}

impl<'s> ParallelVisitorBuilder<'s> for &'s Gather<'s> {
    fn build(&mut self) -> Box<dyn ParallelVisitor + 's> {
        Box::new(Gatherer {
            shared: self,
            local: Gathered::default(),
            seen: 0,
        })
    }
}

impl ParallelVisitor for Gatherer<'_> {
    fn visit(&mut self, entry: Result<DirEntry, ignore::Error>) -> WalkState {
        let shared = self.shared;
        if Instant::now() >= shared.deadline || shared.cancel.is_set() {
            shared.timed_out.store(true, Ordering::Relaxed);
            return WalkState::Quit;
        }
        // A folder that cannot be read is skipped, as rg skips it.
        let Ok(entry) = entry else {
            return WalkState::Continue;
        };
        let Some(kind) = entry.file_type() else {
            return WalkState::Continue;
        };
        let Ok(rest) = entry.path().strip_prefix(shared.root) else {
            return WalkState::Continue;
        };
        let rel = rel_bytes(rest);
        if kind.is_dir() {
            if shared.shallow && entry.depth() > 0 {
                self.local.children.push(rel);
                return WalkState::Continue;
            }
            let stat = entry
                .metadata()
                .map_or(FolderStat::UNKNOWN, |meta| FolderStat::of(&meta));
            let marks = marks(entry.path(), false);
            self.local.folders.push(Folder { rel, stat, marks });
        } else if kind.is_file() {
            match String::from_utf8(rel) {
                Ok(path) => self.local.files.push(path),
                Err(_) => self.local.not_utf8 = true,
            }
            self.seen += 1;
            if self.seen >= 256 {
                let seen = shared.seen.fetch_add(self.seen, Ordering::Relaxed) + self.seen;
                self.seen = 0;
                if seen > shared.max_files {
                    shared.too_many.store(true, Ordering::Relaxed);
                    return WalkState::Quit;
                }
            }
        }
        WalkState::Continue
    }
}

impl Drop for Gatherer<'_> {
    fn drop(&mut self) {
        let seen = self.shared.seen.fetch_add(self.seen, Ordering::Relaxed) + self.seen;
        if seen > self.shared.max_files {
            self.shared.too_many.store(true, Ordering::Relaxed);
        }
        let mut out = self.shared.out.lock().unwrap_or_else(|e| e.into_inner());
        out.folders.append(&mut self.local.folders);
        out.files.append(&mut self.local.files);
        out.children.append(&mut self.local.children);
        out.not_utf8 |= self.local.not_utf8;
    }
}

fn rel_bytes(rest: &Path) -> Vec<u8> {
    let bytes = bytes_of(rest.as_os_str());
    if std::path::MAIN_SEPARATOR == '/' {
        bytes.to_vec()
    } else {
        let separator = std::path::MAIN_SEPARATOR as u8;
        bytes.iter().map(|&b| if b == separator { b'/' } else { b }).collect()
    }
}

struct Walk {
    gathered: Gathered,
    timed_out: bool,
    too_many: bool,
}

/// The walk of `starts` (folders under `root`), as `kind` walks; `shallow`
/// lists each one level down only.
fn gather(
    root: &Path,
    starts: &[PathBuf],
    shallow: bool,
    deadline: Instant,
    max_files: usize,
    kind: WalkKind,
) -> Walk {
    let timed_out = AtomicBool::new(false);
    let too_many = AtomicBool::new(false);
    let seen = AtomicUsize::new(0);
    let out = Mutex::new(Gathered::default());
    let shared = Gather {
        root,
        shallow,
        deadline,
        cancel: files::Cancel::current(),
        max_files,
        timed_out: &timed_out,
        too_many: &too_many,
        seen: &seen,
        out: &out,
    };
    if let Some((first, rest)) = starts.split_first() {
        let mut builder = files::walker(first, kind);
        for start in rest {
            builder.add(start);
        }
        if shallow {
            builder.max_depth(Some(1));
        }
        builder.build_parallel().visit(&mut &shared);
    }
    Walk {
        gathered: out.into_inner().unwrap_or_else(|e| e.into_inner()),
        timed_out: timed_out.into_inner(),
        too_many: too_many.into_inner(),
    }
}

enum Walked {
    Done(Cache),
    Partial(Cache),
    Skip(Skip),
    Failed,
}

fn full_walk(
    root: &Path,
    root_bytes: &[u8],
    env: Vec<u8>,
    stamp: i64,
    deadline: Instant,
    max_files: usize,
    kind: WalkKind,
) -> Walked {
    let sources = outer_sources(root);
    let walk = gather(root, &[root.to_path_buf()], false, deadline, max_files, kind);
    if walk.too_many || walk.gathered.not_utf8 || walk.gathered.files.len() > max_files {
        return Walked::Skip(Skip {
            root: root_bytes.to_vec(),
            env,
            until: nanos(SystemTime::now() + SKIP_FOR),
        });
    }
    let head = (stamp, root_bytes.to_vec(), env, sources);
    match assemble(head, walk.gathered.folders, walk.gathered.files) {
        Some(cache) if walk.timed_out => Walked::Partial(cache),
        Some(cache) => Walked::Done(cache),
        None => Walked::Failed,
    }
}

// ── Bringing a cache up to date ─────────────────────────────────────────────

enum Update {
    Unchanged(Cache),
    Changed(Cache, usize, usize),
    Partial(Cache),
    Full,
}

/// Stat every folder, in parallel when there are many.
fn stat_folders(root: &Path, folders: &[Folder]) -> Vec<Option<FolderStat>> {
    let stat = |folder: &Folder| FolderStat::at(&join_rel(root, &folder.rel));
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(8);
    if folders.len() < 256 || threads < 2 {
        return folders.iter().map(stat).collect();
    }
    let chunk = folders.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let handles: Vec<_> = folders
            .chunks(chunk)
            .map(|part| scope.spawn(move || part.iter().map(stat).collect::<Vec<_>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap_or_default())
            .collect()
    })
}

fn update(root: &Path, old: Cache, stamp: i64, deadline: Instant, kind: WalkKind) -> Update {
    // Read against the stamp of the walk that recorded them.
    let then = old.stamp;
    let marks = old.folders.iter().flat_map(|folder| folder.marks.iter());
    if old.sources.iter().chain(marks).any(|source| source.stale(then)) {
        return Update::Full;
    }
    let count = old.folders.len();
    let index: HashMap<&[u8], usize> = old
        .folders
        .iter()
        .enumerate()
        .map(|(at, folder)| (folder.rel.as_slice(), at))
        .collect();
    let parent_of = |at: usize| -> Option<usize> {
        let rel = &old.folders[at].rel;
        if rel.is_empty() {
            return None;
        }
        index.get(parent_rel(rel)).copied()
    };
    if old.folders.first().is_none_or(|root| !root.rel.is_empty()) {
        return Update::Full;
    }

    let stats = stat_folders(root, &old.folders);
    let mut gone = vec![false; count];
    let mut relist = vec![false; count];
    for (at, (folder, now)) in old.folders.iter().zip(&stats).enumerate() {
        match now {
            Some(now) if now.ino == folder.stat.ino => {
                relist[at] = *now != folder.stat || folder.stat.racy(then);
            }
            _ => gone[at] = true,
        }
    }
    if gone[0] {
        return Update::Full;
    }
    // A folder gone is noticed in its parent: list the nearest one left.
    for at in 0..count {
        if !gone[at] {
            continue;
        }
        let mut up = at;
        loop {
            let Some(parent) = parent_of(up) else {
                return Update::Full;
            };
            if !gone[parent] {
                relist[parent] = true;
                break;
            }
            up = parent;
        }
    }
    // Gone, or under a folder gone (parents sort first).
    let mut dead = gone.clone();
    for at in 1..count {
        match parent_of(at) {
            Some(parent) => dead[at] |= dead[parent],
            None => return Update::Full,
        }
    }
    let starts: Vec<usize> = (0..count).filter(|&at| relist[at] && !dead[at]).collect();
    if starts.is_empty() && !dead.iter().any(|&d| d) {
        return Update::Unchanged(old);
    }
    if starts.len() > count / 4 + 16 {
        return Update::Full;
    }

    // Each folder that changed, one level down.
    let paths: Vec<PathBuf> = starts.iter().map(|&at| join_rel(root, &old.folders[at].rel)).collect();
    let shallow = gather(root, &paths, true, deadline, usize::MAX, kind);
    let mut timed_out = shallow.timed_out;
    if shallow.gathered.not_utf8 {
        return Update::Full;
    }
    let mut listed: HashMap<Vec<u8>, Folder> = shallow
        .gathered
        .folders
        .into_iter()
        .map(|folder| (folder.rel.clone(), folder))
        .collect();
    for &at in &starts {
        let old_folder = &old.folders[at];
        match listed.get(&old_folder.rel) {
            // Its ignore files changed: a full walk.
            Some(now) if now.marks == old_folder.marks && now.stat.ino == old_folder.stat.ino => {}
            None if timed_out => {}
            _ => return Update::Full,
        }
    }
    let children: HashSet<Vec<u8>> = shallow.gathered.children.into_iter().collect();
    // A folder no longer listed in its listed parent is dropped.
    for at in 1..count {
        if dead[at] {
            continue;
        }
        let parent = parent_of(at).unwrap_or(0);
        dead[at] = dead[parent] || (relist[parent] && !children.contains(&old.folders[at].rel));
    }
    // A folder listed that was not known (or was replaced) is walked whole.
    let mut fresh: Vec<&Vec<u8>> = children
        .iter()
        .filter(|rel| index.get(rel.as_slice()).is_none_or(|&at| dead[at]))
        .filter(|rel| index.get(parent_rel(rel)).is_some_and(|&p| !dead[p]))
        .collect();
    fresh.sort_unstable();
    let fresh_paths: Vec<PathBuf> = fresh.iter().map(|rel| join_rel(root, rel)).collect();
    let deep = gather(root, &fresh_paths, false, deadline, usize::MAX, kind);
    timed_out |= deep.timed_out;
    if deep.gathered.not_utf8 {
        return Update::Full;
    }

    let relisted = starts.len();
    let walked = fresh.len();
    let keep = |at: usize| !dead[at] && !relist[at];
    let relisted_rel: HashSet<&[u8]> = starts
        .iter()
        .filter(|&&at| !dead[at])
        .map(|&at| old.folders[at].rel.as_slice())
        .collect();
    let mut paths: Vec<String> = Vec::with_capacity(old.files.len() + 64);
    paths.extend(
        shallow
            .gathered
            .files
            .into_iter()
            .filter(|path| relisted_rel.contains(parent_rel(path.as_bytes()))),
    );
    paths.extend(deep.gathered.files);
    let mut folders: Vec<Folder> = Vec::with_capacity(count + deep.gathered.folders.len());
    for (at, folder) in old.folders.iter().enumerate() {
        if keep(at) {
            folders.push(folder.clone());
        } else if !dead[at] {
            if let Some(now) = listed.remove(&folder.rel) {
                folders.push(now);
            }
        }
    }
    folders.extend(deep.gathered.folders);
    drop(relisted_rel);
    drop(index);
    let Cache {
        root: root_bytes,
        env,
        sources,
        files: old_files,
        ..
    } = old;
    paths.extend(
        old_files
            .into_iter()
            .filter(|file| keep(file.folder as usize))
            .map(|file| file.path),
    );
    let head = (stamp, root_bytes, env, sources);
    match assemble(head, folders, paths) {
        Some(cache) if timed_out => Update::Partial(cache),
        Some(cache) => Update::Changed(cache, relisted, walked),
        None => Update::Full,
    }
}

// ── The file ────────────────────────────────────────────────────────────────

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

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// A quick checksum, eight bytes at a time.
fn checksum(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0x9e37_79b9_7f4a_7c15 ^ bytes.len() as u64;
    let (words, rest) = bytes.as_chunks::<8>();
    for word in words {
        let word = u64::from_le_bytes(*word);
        hash = (hash ^ word).wrapping_mul(0xff51_afd7_ed55_8ccd).rotate_left(29);
    }
    for &byte in rest {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    }
    hash ^ (hash >> 32)
}

struct Writer(Vec<u8>);

impl Writer {
    fn var(&mut self, mut value: u64) {
        while value >= 0x80 {
            self.0.push((value as u8) | 0x80);
            value >>= 7;
        }
        self.0.push(value as u8);
    }
    fn i64(&mut self, value: i64) {
        self.0.extend_from_slice(&value.to_le_bytes());
    }
    fn bytes(&mut self, bytes: &[u8]) {
        self.var(bytes.len() as u64);
        self.0.extend_from_slice(bytes);
    }
    fn stat(&mut self, stat: &Stat) {
        match *stat {
            Stat::Missing => self.0.push(0),
            Stat::Folder => self.0.push(1),
            Stat::Other => self.0.push(2),
            Stat::File {
                mtime,
                ctime,
                size,
                ino,
            } => {
                self.0.push(3);
                self.i64(mtime);
                self.i64(ctime);
                self.var(size);
                self.var(ino);
            }
        }
    }
    fn source(&mut self, source: &Source) {
        self.bytes(&source.path);
        self.stat(&source.stat);
    }
    fn head(&mut self, kind: u8, stamp: i64, root: &[u8], env: &[u8]) {
        self.0.extend_from_slice(MAGIC);
        self.0.extend_from_slice(&VERSION.to_le_bytes());
        self.0.push(kind);
        self.i64(stamp);
        self.bytes(root);
        self.bytes(env);
    }
    fn finish(mut self) -> Vec<u8> {
        let sum = checksum(&self.0);
        self.0.extend_from_slice(&sum.to_le_bytes());
        self.0
    }
}

fn encode_cache(cache: &Cache) -> Vec<u8> {
    let size = cache.files.iter().map(|f| f.path.len() + 4).sum::<usize>() + cache.folders.len() * 40;
    let mut out = Writer(Vec::with_capacity(size + 1024));
    out.head(0, cache.stamp, &cache.root, &cache.env);
    out.var(cache.sources.len() as u64);
    cache.sources.iter().for_each(|source| out.source(source));
    out.var(cache.folders.len() as u64);
    for folder in &cache.folders {
        out.bytes(&folder.rel);
        out.i64(folder.stat.mtime);
        out.i64(folder.stat.ctime);
        out.var(folder.stat.ino);
        out.var(folder.marks.len() as u64);
        folder.marks.iter().for_each(|source| out.source(source));
    }
    out.var(cache.files.len() as u64);
    for file in &cache.files {
        out.var(u64::from(file.folder));
        out.bytes(file.path.as_bytes());
    }
    out.finish()
}

fn encode_skip(skip: &Skip) -> Vec<u8> {
    let mut out = Writer(Vec::with_capacity(256));
    out.head(1, 0, &skip.root, &skip.env);
    out.i64(skip.until);
    out.finish()
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(count)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn var(&mut self) -> Option<u64> {
        let mut value: u64 = 0;
        for shift in (0..64).step_by(7) {
            let byte = self.u8()?;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }
    fn i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn bytes(&mut self) -> Option<&'a [u8]> {
        let len = usize::try_from(self.var()?).ok()?;
        self.take(len)
    }
    /// A count of items each at least `min` bytes long, checked against
    /// what is left so a bad count cannot reserve much.
    fn count(&mut self, min: usize) -> Option<usize> {
        let count = usize::try_from(self.var()?).ok()?;
        (count.checked_mul(min)? <= self.bytes.len() - self.at).then_some(count)
    }
    fn stat(&mut self) -> Option<Stat> {
        Some(match self.u8()? {
            0 => Stat::Missing,
            1 => Stat::Folder,
            2 => Stat::Other,
            3 => Stat::File {
                mtime: self.i64()?,
                ctime: self.i64()?,
                size: self.var()?,
                ino: self.var()?,
            },
            _ => return None,
        })
    }
    fn source(&mut self) -> Option<Source> {
        Some(Source {
            path: self.bytes()?.to_vec(),
            stat: self.stat()?,
        })
    }
}

fn decode(bytes: &[u8]) -> Option<Stored> {
    let (body, sum) = bytes.split_at_checked(bytes.len().checked_sub(8)?)?;
    if checksum(body) != u64::from_le_bytes(sum.try_into().ok()?) {
        return None;
    }
    let mut read = Reader { bytes: body, at: 0 };
    if read.take(8)? != MAGIC || read.take(4)? != VERSION.to_le_bytes() {
        return None;
    }
    let kind = read.u8()?;
    let stamp = read.i64()?;
    let root = read.bytes()?.to_vec();
    let env = read.bytes()?.to_vec();
    if kind == 1 {
        let until = read.i64()?;
        return (read.at == body.len()).then_some(Stored::Skip(Skip { root, env, until }));
    }
    if kind != 0 {
        return None;
    }
    let sources = (0..read.count(2)?).map(|_| read.source()).collect::<Option<Vec<_>>>()?;
    let count = read.count(18)?;
    let mut folders = Vec::with_capacity(count);
    for _ in 0..count {
        let rel = read.bytes()?.to_vec();
        let stat = FolderStat {
            mtime: read.i64()?,
            ctime: read.i64()?,
            ino: read.var()?,
        };
        let marks = (0..read.count(2)?).map(|_| read.source()).collect::<Option<Vec<_>>>()?;
        folders.push(Folder { rel, stat, marks });
    }
    let count = read.count(2)?;
    let mut files = Vec::with_capacity(count);
    for _ in 0..count {
        let folder = u32::try_from(read.var()?).ok()?;
        if folder as usize >= folders.len() {
            return None;
        }
        let path = std::str::from_utf8(read.bytes()?).ok()?.to_owned();
        files.push(FileEntry { folder, path });
    }
    if read.at != body.len() {
        return None;
    }
    Some(Stored::Cache(Cache {
        stamp,
        root,
        env,
        sources,
        folders,
        files,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A fresh tree under its own parent (whose ignore files tests edit),
    /// with a cache folder of its own.
    struct Tree {
        base: PathBuf,
        root: PathBuf,
        settings: Settings,
    }

    impl Tree {
        fn new(name: &str) -> Tree {
            let base = std::env::temp_dir().join(format!("lwk-cache-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&base);
            let root = base.join("root");
            for path in [
                "a/x.txt", "a/y/z.txt", "a-b", "a.c", "b/\u{1F600}.txt", "b/\u{FF5E}.txt",
                "c/d/e/f.txt", "c/d.txt", "Z.txt", "ignored/k.txt", "logs/app.log", "logs/keep.txt",
                ".hidden/h.txt",
            ] {
                let file = root.join(path);
                fs::create_dir_all(file.parent().unwrap()).unwrap();
                fs::write(&file, path).unwrap();
            }
            fs::write(root.join(".ignore"), "ignored/\n").unwrap();
            fs::write(root.join(".gitignore"), "*.log\n").unwrap();
            let root = fs::canonicalize(&root).unwrap();
            let base = root.parent().unwrap().to_path_buf();
            let settings = Settings {
                dir: base.join("cache"),
                max_files: MAX_FILES,
                coarse: false,
            };
            let tree = Tree { base, root, settings };
            tree.age();
            tree
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.root.join(rel)
        }

        /// Set every folder's mtime an hour back, so none is racy.
        fn age(&self) {
            let old = SystemTime::now() - Duration::from_secs(3600);
            for entry in walk_all(&self.root) {
                if entry.is_dir() {
                    set_mtime(&entry, old);
                }
            }
            for name in [".gitignore", ".ignore"] {
                let path = self.root.join(name);
                if path.exists() {
                    set_mtime(&path, old);
                }
            }
        }

        fn call(&self) -> (Vec<String>, Outcome) {
            let (listing, outcome) = listing_with(&self.root, &self.settings, WalkKind::Files).expect("cached");
            assert!(listing.complete);
            (listing.files, outcome)
        }

        /// The call's answer must be the uncached walk's.
        fn check(&self, what: &str) -> Outcome {
            let (files, outcome) = self.call();
            assert_eq!(files, uncached(&self.root), "{what}: {outcome:?}");
            outcome
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn walk_all(root: &Path) -> Vec<PathBuf> {
        let mut out = vec![root.to_path_buf()];
        let mut at = 0;
        while at < out.len() {
            if out[at].is_dir() {
                for entry in fs::read_dir(&out[at]).unwrap() {
                    out.push(entry.unwrap().path());
                }
            }
            at += 1;
        }
        out
    }

    fn set_mtime(path: &Path, time: SystemTime) {
        let file = fs::File::open(path).unwrap();
        file.set_modified(time).unwrap();
    }

    fn mtime(path: &Path) -> SystemTime {
        fs::metadata(path).unwrap().modified().unwrap()
    }

    fn uncached(root: &Path) -> Vec<String> {
        let mut files: Vec<String> = files::walker(root, WalkKind::Files)
            .build()
            .filter_map(|entry| files::file_of(root, entry))
            .collect();
        files::sort_js(&mut files);
        files
    }

    fn updated(outcome: Outcome) -> bool {
        matches!(outcome, Outcome::Updated { .. })
    }

    /// The hidden walk as an uncached walk gives it.
    fn uncached_hidden(root: &Path) -> Vec<String> {
        let mut files: Vec<String> = files::walker(root, WalkKind::Hidden)
            .build()
            .filter_map(|entry| files::file_of(root, entry))
            .collect();
        files::sort_js(&mut files);
        files
    }

    #[test]
    fn the_two_walks_are_cached_apart() {
        let tree = Tree::new("walks");
        fs::create_dir_all(tree.path(".git/objects")).unwrap();
        fs::write(tree.path(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::create_dir_all(tree.path("sub")).unwrap();
        fs::write(tree.path("sub/.git"), "gitdir: ../.git\n").unwrap();
        fs::write(tree.path("sub/s.txt"), "s").unwrap();
        tree.age();
        let hidden = |tree: &Tree| {
            let (listing, outcome) =
                listing_with(&tree.root, &tree.settings, WalkKind::Hidden).expect("cached");
            (listing.files, outcome)
        };
        // Each walk fills its own file; neither answers for the other.
        assert_eq!(tree.check("files cold"), Outcome::Full);
        let (found, outcome) = hidden(&tree);
        assert_eq!(outcome, Outcome::Full);
        assert_eq!(found, uncached_hidden(&tree.root));
        assert!(found.contains(&".hidden/h.txt".to_owned()));
        assert!(found.contains(&".gitignore".to_owned()));
        assert!(!found.iter().any(|f| f.starts_with(".git/") || f.ends_with("/.git") || f.ends_with(".log")));
        assert!(!found.iter().any(|f| f.starts_with("ignored/")));
        assert_eq!(tree.check("files warm"), Outcome::Hit);
        assert!(!tree.call().0.iter().any(|f| f.contains(".hidden")));
        assert_eq!(hidden(&tree).1, Outcome::Hit);
        assert_ne!(file_name(&tree.root, WalkKind::Files), file_name(&tree.root, WalkKind::Hidden));
        assert_ne!(env_key(WalkKind::Files), env_key(WalkKind::Hidden));
        // A hidden folder changed is seen by the hidden walk only.
        fs::write(tree.path(".hidden/new.txt"), "new").unwrap();
        let (found, outcome) = hidden(&tree);
        assert!(updated(outcome), "{outcome:?}");
        assert_eq!(found, uncached_hidden(&tree.root));
        assert!(found.contains(&".hidden/new.txt".to_owned()));
        // The files walk never went into it, so it is still a hit.
        assert_eq!(tree.check("files after a hidden change"), Outcome::Hit);
    }

    #[test]
    fn a_second_call_is_a_hit() {
        let tree = Tree::new("hit");
        assert_eq!(tree.check("cold"), Outcome::Full);
        assert_eq!(tree.check("warm"), Outcome::Hit);
        let files = tree.call().0;
        assert!(files.contains(&"logs/keep.txt".to_owned()));
        assert!(!files.iter().any(|f| f.ends_with(".log") || f.starts_with("ignored/")));
        assert!(!files.iter().any(|f| f.contains(".hidden")));
    }

    #[test]
    fn a_cache_kept_in_ram_is_checked_on_every_call() {
        let tree = Tree::new("ram");
        let ram = Ram::new(2);
        let call = || {
            let (listing, outcome) = ram.listing_with(&tree.root, &tree.settings, WalkKind::Files).expect("cached");
            assert_eq!(listing.files, uncached(&tree.root), "{outcome:?}");
            outcome
        };
        assert_eq!(call(), Outcome::Full);
        // Held: the cache file is not needed again.
        fs::remove_dir_all(&tree.settings.dir).unwrap();
        assert_eq!(call(), Outcome::Hit);
        // A change is seen, and written for the next `lwk`.
        fs::write(tree.path("a/new.txt"), "new").unwrap();
        assert!(updated(call()));
        assert_eq!(tree.check("after serve"), Outcome::Hit);
        // Given ahead of its check, a listing is known sound or not after.
        let ahead = || {
            checked_meanwhile(|| ram.listing_with(&tree.root, &tree.settings, WalkKind::Files).map(|(listing, _)| listing.files))
        };
        let before = uncached(&tree.root);
        assert_eq!(ahead(), (Some(before.clone()), true));
        fs::remove_file(tree.path("a/new.txt")).unwrap();
        assert_eq!(ahead(), (Some(before), false));
        assert!(updated(call()));
        fs::write(tree.root.join(".gitignore"), "*.log\n*.c\n").unwrap();
        assert!(!ahead().1);
        assert_eq!(call(), Outcome::Full);
        assert!(ahead().1);
        // A cancelled walk is neither kept nor written.
        fs::write(tree.root.join(".ignore"), "ignored/\nc/\n").unwrap();
        let flag = Arc::new(AtomicBool::new(true));
        let cut = files::Cancel::scope(flag, || ram.listing_with(&tree.root, &tree.settings, WalkKind::Files));
        assert_eq!(cut.map(|(_, outcome)| outcome), Some(Outcome::Partial));
        assert_eq!(call(), Outcome::Full);
        // Only the roots used last are held.
        for name in ["one", "two"] {
            let other = Tree::new(&format!("ram-{name}"));
            ram.listing_with(&other.root, &other.settings, WalkKind::Files).expect("cached");
        }
        let held = ram.roots.lock().unwrap();
        assert_eq!(held.len(), 2);
        assert!(held.iter().all(|kept| kept.root != tree.root));
    }

    #[test]
    fn adds_removes_and_renames_files() {
        let tree = Tree::new("files");
        tree.check("cold");
        fs::write(tree.path("a/y/new.txt"), "new").unwrap();
        assert!(updated(tree.check("add")));
        assert_eq!(tree.check("add, warm"), Outcome::Hit);
        tree.age();
        tree.check("aged");
        fs::remove_file(tree.path("c/d.txt")).unwrap();
        assert!(updated(tree.check("remove")));
        tree.age();
        tree.check("aged");
        fs::rename(tree.path("a/x.txt"), tree.path("a/x2.txt")).unwrap();
        assert!(updated(tree.check("rename in place")));
        tree.age();
        tree.check("aged");
        fs::rename(tree.path("a/x2.txt"), tree.path("c/d/x3.txt")).unwrap();
        assert!(updated(tree.check("rename across")));
        // A file renamed to an ignored name.
        tree.age();
        tree.check("aged");
        fs::rename(tree.path("logs/keep.txt"), tree.path("logs/keep.log")).unwrap();
        assert!(updated(tree.check("rename to ignored")));
    }

    #[test]
    fn adds_and_removes_folders() {
        let tree = Tree::new("folders");
        tree.check("cold");
        fs::create_dir_all(tree.path("n/m/o")).unwrap();
        fs::write(tree.path("n/m/o/p.txt"), "p").unwrap();
        fs::write(tree.path("n/q.txt"), "q").unwrap();
        fs::create_dir_all(tree.path("n/ignored-not")).unwrap();
        fs::write(tree.path("n/m/x.log"), "x").unwrap();
        assert_eq!(tree.check("add"), Outcome::Updated { relisted: 1, walked: 1 });
        assert_eq!(tree.check("add, warm"), Outcome::Hit);
        tree.age();
        tree.check("aged");
        fs::remove_dir_all(tree.path("c")).unwrap();
        assert!(updated(tree.check("remove")));
        tree.age();
        tree.check("aged");
        // A folder replaced by another of the same name.
        fs::remove_dir_all(tree.path("a/y")).unwrap();
        fs::create_dir_all(tree.path("a/y/w")).unwrap();
        fs::write(tree.path("a/y/w/v.txt"), "v").unwrap();
        tree.check("replace");
        tree.age();
        tree.check("aged");
        // A folder moved under another.
        fs::rename(tree.path("n/m"), tree.path("a/m")).unwrap();
        tree.check("move");
        tree.age();
        tree.check("aged");
        // A folder renamed to a hidden name.
        fs::rename(tree.path("b"), tree.path(".b")).unwrap();
        tree.check("hide");
    }

    #[test]
    fn an_edited_ignore_file_walks_again() {
        let tree = Tree::new("ignore");
        tree.check("cold");
        let gitignore = tree.path(".gitignore");
        let before = mtime(&gitignore);
        // Same size, in place.
        fs::write(&gitignore, "*.txt\n").unwrap();
        assert_eq!(fs::metadata(&gitignore).unwrap().len(), 6);
        assert_eq!(tree.check("same size"), Outcome::Full);
        tree.age();
        tree.check("aged");
        // Same size and the old mtime back: the ctime still moved.
        fs::write(&gitignore, "*.log\n").unwrap();
        set_mtime(&gitignore, before);
        assert_eq!(tree.check("same size, same mtime"), Outcome::Full);
        tree.age();
        tree.check("aged");
        // Another size.
        fs::write(&gitignore, "*.log\nc/\n").unwrap();
        assert_eq!(tree.check("another size"), Outcome::Full);
        tree.age();
        tree.check("aged");
        // A new ignore file in a folder walked.
        fs::write(tree.path("a/.rgignore"), "y/\n").unwrap();
        assert_eq!(tree.check("new .rgignore"), Outcome::Full);
        tree.age();
        tree.check("aged");
        // A parent's ignore file.
        fs::write(tree.base.join(".ignore"), "Z.txt\n").unwrap();
        assert_eq!(tree.check("parent .ignore"), Outcome::Full);
        tree.age();
        tree.check("aged");
        // .git/info/exclude
        fs::create_dir_all(tree.path(".git/info")).unwrap();
        assert_eq!(tree.check("new .git"), Outcome::Full);
        tree.age();
        tree.check("aged");
        fs::write(tree.path(".git/info/exclude"), "a.c\n").unwrap();
        assert_eq!(tree.check("new exclude"), Outcome::Full);
        assert!(!tree.call().0.contains(&"a.c".to_owned()));
    }

    #[test]
    fn a_racy_folder_is_listed_again() {
        let mut tree = Tree::new("racy");
        // The line a whole second back, as for a file system with coarse
        // mtimes.
        tree.settings.coarse = true;
        // Let the ignore files' ctimes fall behind that line.
        std::thread::sleep(Duration::from_millis(2100));
        // Make `a` racy: changed now, after the hour-old stamp of the rest.
        fs::write(tree.path("a/racy-1.txt"), "1").unwrap();
        let racy = mtime(&tree.path("a"));
        tree.check("cold");
        // A change that leaves the folder's mtime where it was (a coarse
        // clock): only the racy rule catches it.
        fs::write(tree.path("a/racy-2.txt"), "2").unwrap();
        set_mtime(&tree.path("a"), racy);
        let (files, outcome) = tree.call();
        assert!(files.contains(&"a/racy-2.txt".to_owned()), "{outcome:?}");
        assert_eq!(outcome, Outcome::Updated { relisted: 1, walked: 0 });
        assert_eq!(files, uncached(&tree.root));
    }

    #[test]
    fn a_corrupt_cache_is_a_miss() {
        let tree = Tree::new("corrupt");
        tree.check("cold");
        let file = tree.settings.dir.join(file_name(&tree.root, WalkKind::Files));
        let mut bytes = fs::read(&file).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0x55;
        fs::write(&file, &bytes).unwrap();
        assert_eq!(tree.check("flipped"), Outcome::Full);
        assert_eq!(tree.check("warm"), Outcome::Hit);
        fs::write(&file, &bytes[..middle]).unwrap();
        assert_eq!(tree.check("cut"), Outcome::Full);
        fs::write(&file, b"garbage").unwrap();
        assert_eq!(tree.check("garbage"), Outcome::Full);
        fs::write(&file, b"").unwrap();
        assert_eq!(tree.check("empty"), Outcome::Full);
        assert_eq!(tree.check("warm"), Outcome::Hit);
    }

    #[test]
    fn a_tree_too_big_is_not_cached() {
        let mut tree = Tree::new("big");
        tree.settings.max_files = 3;
        assert!(listing_with(&tree.root, &tree.settings, WalkKind::Files).is_none());
        let file = tree.settings.dir.join(file_name(&tree.root, WalkKind::Files));
        assert!(matches!(decode(&fs::read(&file).unwrap()), Some(Stored::Skip(_))));
        assert!(listing_with(&tree.root, &tree.settings, WalkKind::Files).is_none());
    }

    #[test]
    fn no_cache_turns_it_off() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| OsString::from(v))
        };
        assert!(Settings::from_env(env(&[("LWK_NO_CACHE", "1"), ("HOME", "/h")])).is_none());
        let on = Settings::from_env(env(&[("LWK_NO_CACHE", "0"), ("HOME", "/h")])).unwrap();
        let home = if cfg!(target_os = "macos") { "/h/Library/Caches/lwk" } else { "/h/.cache/lwk" };
        assert_eq!(on.dir, PathBuf::from(home));
        let xdg = Settings::from_env(env(&[("XDG_CACHE_HOME", "/x"), ("HOME", "/h")])).unwrap();
        assert_eq!(xdg.dir, PathBuf::from("/x/lwk"));
        let relative = Settings::from_env(env(&[("XDG_CACHE_HOME", "x"), ("HOME", "/h")])).unwrap();
        assert_eq!(relative.dir, PathBuf::from(home));
    }

    #[test]
    fn never_caches_the_file_system_root() {
        let settings = Settings {
            dir: std::env::temp_dir().join("lwk-never"),
            max_files: MAX_FILES,
            coarse: true,
        };
        assert!(listing_with(Path::new("/"), &settings, WalkKind::Files).is_none());
        assert!(!settings.dir.exists());
    }

    #[test]
    fn encodes_and_decodes() {
        let cache = Cache {
            stamp: -5,
            root: b"/r".to_vec(),
            env: b"e".to_vec(),
            sources: vec![Source {
                path: b"/r/.gitignore".to_vec(),
                stat: Stat::File {
                    mtime: 1,
                    ctime: 2,
                    size: 300,
                    ino: u64::MAX,
                },
            }],
            folders: vec![Folder {
                rel: Vec::new(),
                stat: FolderStat::UNKNOWN,
                marks: vec![Source {
                    path: b"/r/.git".to_vec(),
                    stat: Stat::Folder,
                }],
            }],
            files: vec![FileEntry {
                folder: 0,
                path: "x".to_owned(),
            }],
        };
        let Some(Stored::Cache(back)) = decode(&encode_cache(&cache)) else {
            panic!("decodes");
        };
        assert_eq!(format!("{cache:?}"), format!("{back:?}"));
    }
}
