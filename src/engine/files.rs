//! `list_files`: every file in a folder that ripgrep would search, with
//! `ignore`'s walker: .gitignore respected, hidden files skipped
//! (`electron/main/fs-kit/list-files.ts`, which runs `rg --files`).
//!
//! - `root` is real and already in scope; paths answered are relative to
//!   it, `/`-separated, sorted as JavaScript sorts strings (UTF-16 units).
//! - At most `max_files` paths; `truncated` when there were more (or the
//!   walk ran out of [`WALK_TIMEOUT`]).
//! - The walk is ripgrep's: `.gitignore` (in a repository or not, as
//!   `--no-require-git`), `.ignore`, `.rgignore`, `.git/info/exclude` and
//!   git's global excludes; hidden files skipped; symlinks not followed and
//!   not listed.
//!
//! NOTE: The app keeps the first `limit` files `rg` printed, which with
//! rg's parallel walk is no fixed set. This walk is sorted, so a cut
//! listing keeps the first `limit` files in path order, every time.

use super::cache;
use super::types::{FileListing, KitError, KitErrorCode};
use ignore::{DirEntry, ParallelVisitor, ParallelVisitorBuilder, WalkBuilder, WalkState};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as Atomic};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a listing may take before it is cut (`timeoutMs`, 10 s).
pub(crate) const WALK_TIMEOUT: Duration = Duration::from_secs(10);

/// How many files a walking thread gathers before it tells the others.
const BATCH: usize = 64;

// ── Cancelling (`lwk serve`) ────────────────────────────────────────────────

thread_local! {
    /// The cancel flag of the call this thread runs; None outside `lwk serve`.
    static CANCEL: RefCell<Option<Arc<AtomicBool>>> = const { RefCell::new(None) };
}

/// Whether the caller cancelled the call a walk or search works for.
///
/// - Taken with [`Cancel::current`] on the thread the call runs on, then
///   read from the walking threads.
/// - Never set unless the call runs inside [`Cancel::scope`], which only
///   `lwk serve` does: for `lwk` and `lwk mcp` it is always false.
/// - A cancelled walk stops as one out of time does; what it answers is
///   not sent.
#[derive(Clone, Default)]
pub(crate) struct Cancel(Option<Arc<AtomicBool>>);

impl Cancel {
    /// The flag of the call this thread runs.
    pub(crate) fn current() -> Cancel {
        Cancel(CANCEL.with(|flag| flag.borrow().clone()))
    }

    pub(crate) fn is_set(&self) -> bool {
        self.0.as_ref().is_some_and(|flag| flag.load(Atomic::Relaxed))
    }

    /// Run `call` with `flag` as this thread's cancel flag.
    pub(crate) fn scope<T>(flag: Arc<AtomicBool>, call: impl FnOnce() -> T) -> T {
        struct Restore(Option<Arc<AtomicBool>>);
        impl Drop for Restore {
            fn drop(&mut self) {
                let before = self.0.take();
                CANCEL.with(|flag| *flag.borrow_mut() = before);
            }
        }
        let _restore = Restore(CANCEL.with(|at| at.borrow_mut().replace(flag)));
        call()
    }

    /// The error a cancelled call fails with.
    pub(crate) fn error() -> KitError {
        KitError::new(KitErrorCode::Cancelled, "Cancelled.")
    }
}

/// The files under `root`, `.gitignore` respected, sorted.
///
/// The walk is ripgrep's parallel one; the files kept are the first
/// `max_files` in the order the sorted walk ([`walker`]) visits them, so a
/// cut listing keeps the same files every time. Once more than
/// `max_files` files were seen, a folder that sorts after the
/// `max_files`th file kept so far is not walked.
///
/// The files come from the file-list cache ([`cache`]) when it is on; its
/// answer is the walk's.
pub fn list_files(root: &Path, max_files: usize) -> Result<FileListing, KitError> {
    match cache::listing(root) {
        Some(listing) => Ok(first_files(listing, max_files)),
        None => list_files_uncached(root, max_files),
    }
}

/// The first `max_files` files of a whole listing in walk order, sorted as
/// [`list_files`] sorts them; `truncated` when there were more or the walk
/// ran out of time.
fn first_files(listing: cache::Listing, max_files: usize) -> FileListing {
    let mut files = listing.files;
    let truncated = files.len() > max_files || !listing.complete;
    if files.len() > max_files {
        // Walk order is byte order with each separator made a NUL.
        fn key(path: &str) -> impl Iterator<Item = u8> + '_ {
            path.bytes().map(|byte| if byte == b'/' { 0 } else { byte })
        }
        let mut order: Vec<u32> = (0..files.len() as u32).collect();
        order.select_nth_unstable_by(max_files, |&a, &b| {
            key(&files[a as usize]).cmp(key(&files[b as usize]))
        });
        order.truncate(max_files);
        // `files` is sorted already; keep that order.
        order.sort_unstable();
        files = order
            .into_iter()
            .map(|at| std::mem::take(&mut files[at as usize]))
            .collect();
    }
    FileListing { files, truncated }
}

/// [`list_files`] without the cache: the walk itself.
fn list_files_uncached(root: &Path, max_files: usize) -> Result<FileListing, KitError> {
    let shared = ListShared {
        root,
        max_files,
        deadline: Instant::now() + WALK_TIMEOUT,
        cancel: Cancel::current(),
        timed_out: AtomicBool::new(false),
        seen: AtomicUsize::new(0),
        best: Mutex::new(BinaryHeap::new()),
        cutoff: Cutoff::default(),
        kept: Mutex::new(Vec::new()),
    };
    walker(root).build_parallel().visit(&mut ListBuilder(&shared));

    let mut kept = shared.kept.into_inner().unwrap_or_else(|e| e.into_inner());
    let seen = shared.seen.into_inner();
    if kept.len() > max_files {
        kept.select_nth_unstable(max_files);
        kept.truncate(max_files);
    }
    let truncated = seen > max_files || shared.timed_out.into_inner();
    let mut files: Vec<String> = kept.iter().map(|key| key_path(key)).collect();
    sort_js(&mut files);
    Ok(FileListing { files, truncated })
}

struct ListShared<'a> {
    root: &'a Path,
    max_files: usize,
    deadline: Instant,
    cancel: Cancel,
    timed_out: AtomicBool,
    /// Every file seen, kept or not.
    seen: AtomicUsize,
    /// The `max_files` first files in walk order told so far.
    best: Mutex<BinaryHeap<WalkKey>>,
    cutoff: Cutoff,
    kept: Mutex<Vec<WalkKey>>,
}

struct ListBuilder<'a>(&'a ListShared<'a>);

impl<'s> ParallelVisitorBuilder<'s> for ListBuilder<'s> {
    fn build(&mut self) -> Box<dyn ParallelVisitor + 's> {
        Box::new(Lister {
            shared: self.0,
            cutoff: CutoffView::default(),
            kept: Vec::new(),
            told: 0,
            seen: 0,
        })
    }
}

/// One walking thread of `list_files`.
struct Lister<'a> {
    shared: &'a ListShared<'a>,
    cutoff: CutoffView,
    kept: Vec<WalkKey>,
    /// How many of `kept` the others were told of.
    told: usize,
    /// Files seen since the others were last told.
    seen: usize,
}

impl Lister<'_> {
    /// Tell the others what this thread found since last time; once more
    /// than `max_files` files were seen, the `max_files`th first one kept
    /// becomes the cutoff.
    fn tell(&mut self) {
        let shared = self.shared;
        let seen = shared.seen.fetch_add(self.seen, Atomic::Relaxed) + self.seen;
        self.seen = 0;
        let fresh = &self.kept[self.told..];
        self.told = self.kept.len();
        if shared.max_files == 0 {
            if seen > 0 {
                shared.cutoff.lower(Arc::from(&[][..]));
            }
            return;
        }
        let Ok(mut best) = shared.best.lock() else { return };
        for key in fresh {
            if best.len() < shared.max_files {
                best.push(key.clone());
            } else if best.peek().is_some_and(|top| key < top) {
                best.pop();
                best.push(key.clone());
            }
        }
        if seen > shared.max_files && best.len() == shared.max_files {
            if let Some(top) = best.peek() {
                shared.cutoff.lower(top.clone());
            }
        }
    }
}

impl ParallelVisitor for Lister<'_> {
    fn visit(&mut self, entry: Result<DirEntry, ignore::Error>) -> WalkState {
        let shared = self.shared;
        if Instant::now() >= shared.deadline || shared.cancel.is_set() {
            shared.timed_out.store(true, Atomic::Relaxed);
            return WalkState::Quit;
        }
        // A folder that cannot be read is skipped, as rg skips it.
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
            return match walk_key(shared.root, entry.path()) {
                Some(key) if self.cutoff.after(&shared.cutoff, &key) => WalkState::Skip,
                _ => WalkState::Continue,
            };
        }
        if !kind.is_file() {
            return WalkState::Continue;
        }
        let Some(key) = walk_key(shared.root, entry.path()) else {
            return WalkState::Continue;
        };
        self.seen += 1;
        if !self.cutoff.after(&shared.cutoff, &key) {
            self.kept.push(Arc::from(key));
        }
        if self.kept.len() - self.told >= BATCH || self.seen >= BATCH {
            self.tell();
        }
        WalkState::Continue
    }
}

impl Drop for Lister<'_> {
    fn drop(&mut self) {
        self.seen_flush();
        if let Ok(mut kept) = self.shared.kept.lock() {
            kept.append(&mut self.kept);
        }
    }
}

impl Lister<'_> {
    /// Count what this thread saw; what it kept goes in whole at the end.
    fn seen_flush(&mut self) {
        self.shared.seen.fetch_add(self.seen, Atomic::Relaxed);
        self.seen = 0;
    }
}

// ── Walk order ──────────────────────────────────────────────────────────────

/// A path's place in [`walker`]'s sorted walk: its path relative to the
/// root, as bytes, each separator made a NUL. Byte order on these is the
/// sorted walk's order (folders first-to-last by name, a folder's files
/// right after it), and a folder's key sorts before everything under it.
pub(crate) type WalkKey = Arc<[u8]>;

/// `path`'s [`WalkKey`] under `root`; None for `root` itself.
pub(crate) fn walk_key(root: &Path, path: &Path) -> Option<Vec<u8>> {
    let rest = path.strip_prefix(root).ok()?;
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        rest.as_os_str().as_bytes()
    };
    #[cfg(not(unix))]
    let lossy = rest.to_string_lossy();
    #[cfg(not(unix))]
    let bytes = lossy.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let separator = std::path::MAIN_SEPARATOR as u8;
    Some(
        bytes
            .iter()
            .map(|&byte| if byte == separator { 0 } else { byte })
            .collect(),
    )
}

/// A [`WalkKey`] as [`relative`] gives its path: `/`-separated.
pub(crate) fn key_path(key: &[u8]) -> String {
    let bytes: Vec<u8> = key.iter().map(|&byte| if byte == 0 { b'/' } else { byte }).collect();
    match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    }
}

/// The walk-order key past which nothing can change an answer, shared by
/// the walking threads. It only ever moves down.
#[derive(Default)]
pub(crate) struct Cutoff {
    version: AtomicUsize,
    key: Mutex<Option<WalkKey>>,
}

impl Cutoff {
    /// Make `key` the cutoff, when it is below the one there.
    pub(crate) fn lower(&self, key: WalkKey) {
        let Ok(mut at) = self.key.lock() else { return };
        if at.as_ref().is_none_or(|old| key < *old) {
            *at = Some(key);
            self.version.fetch_add(1, Atomic::Release);
        }
    }
}

/// One thread's copy of a [`Cutoff`], read again only when it moved.
#[derive(Default)]
pub(crate) struct CutoffView {
    version: usize,
    key: Option<WalkKey>,
}

impl CutoffView {
    /// Whether `key` sorts after the cutoff, so neither it nor anything
    /// under it can change the answer.
    pub(crate) fn after(&mut self, cutoff: &Cutoff, key: &[u8]) -> bool {
        self.active(cutoff) && self.key.as_ref().is_some_and(|cut| key > &cut[..])
    }

    /// Whether there is a cutoff yet.
    pub(crate) fn active(&mut self, cutoff: &Cutoff) -> bool {
        let version = cutoff.version.load(Atomic::Acquire);
        if version != self.version {
            if let Ok(at) = cutoff.key.lock() {
                self.key = at.clone();
            }
            self.version = version;
        }
        self.key.is_some()
    }
}

/// ripgrep's walk of `root`, as `rg --no-config --no-require-git` sets it
/// up, sorted by name within each folder.
pub(crate) fn walker(root: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(true)
        .parents(true)
        .ignore(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .require_git(false)
        .follow_links(false)
        .add_custom_ignore_filename(".rgignore")
        .sort_by_file_name(|a, b| a.cmp(b));
    builder
}

/// `entry`'s path relative to `root`, `/`-separated, when it is a file
/// ripgrep would search: a regular file, not a folder or a symlink.
pub(crate) fn file_of(root: &Path, entry: Result<DirEntry, ignore::Error>) -> Option<String> {
    // A folder that cannot be read is skipped, as rg skips it.
    let entry = entry.ok()?;
    if !entry.file_type().is_some_and(|kind| kind.is_file()) {
        return None;
    }
    relative(root, entry.path())
}

/// `path` relative to `root`, `/`-separated; None for `root` itself.
pub(crate) fn relative(root: &Path, path: &Path) -> Option<String> {
    let rest = path.strip_prefix(root).ok()?;
    let text = rest.to_string_lossy();
    if text.is_empty() {
        return None;
    }
    Some(if std::path::MAIN_SEPARATOR == '/' {
        text.into_owned()
    } else {
        text.replace(std::path::MAIN_SEPARATOR, "/")
    })
}

/// Sort as JavaScript's `Array.prototype.sort` does strings: by UTF-16
/// code units.
pub(crate) fn sort_js(paths: &mut [String]) {
    paths.sort_unstable_by(|a, b| js_cmp(a, b));
}

pub(crate) fn js_cmp(a: &str, b: &str) -> Ordering {
    // Byte order is code point order, which differs from UTF-16 order only
    // when one side has a code point above U+FFFF.
    if a.is_ascii() && b.is_ascii() {
        return a.cmp(b);
    }
    a.encode_utf16().cmp(b.encode_utf16())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_by_utf16_units() {
        // U+FF5E sorts after U+1F600's high surrogate (0xD83D) in UTF-16,
        // though before it by code point.
        let mut paths = vec!["\u{1F600}".to_owned(), "\u{FF5E}".to_owned(), "a".to_owned()];
        sort_js(&mut paths);
        assert_eq!(paths, vec!["a", "\u{1F600}", "\u{FF5E}"]);
    }

    /// A tree whose walk order is not its path order: `a-b` sorts before
    /// `a/x` by path, after it by walk.
    pub(crate) fn tree(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("lwk-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for path in [
            "a/x.txt", "a/y/z.txt", "a-b", "a.c", "b/\u{1F600}.txt", "b/\u{FF5E}.txt",
            "c/d/e/f.txt", "c/d.txt", "Z.txt", "ignored/k.txt", "empty/sub/.keep",
        ] {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, format!("needle in {path}\nnone\nneedle again\n")).unwrap();
        }
        std::fs::write(root.join(".ignore"), "ignored/\n").unwrap();
        root
    }

    /// The listing as the sequential sorted walk made it.
    fn listed_in_order(root: &Path, max_files: usize) -> FileListing {
        let mut files: Vec<String> = Vec::new();
        let mut truncated = false;
        for entry in walker(root).build() {
            let Some(path) = file_of(root, entry) else { continue };
            if files.len() >= max_files {
                truncated = true;
                break;
            }
            files.push(path);
        }
        sort_js(&mut files);
        FileListing { files, truncated }
    }

    #[test]
    fn keeps_the_first_files_of_the_sorted_walk() {
        let root = tree("list");
        for max_files in 0..12 {
            for _ in 0..5 {
                let want = listed_in_order(&root, max_files);
                // Cached (cold, then warm) and uncached.
                for got in [
                    list_files(&root, max_files).unwrap(),
                    list_files_uncached(&root, max_files).unwrap(),
                ] {
                    assert_eq!(got.files, want.files, "limit {max_files}");
                    assert_eq!(got.truncated, want.truncated, "limit {max_files}");
                }
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn walk_keys_sort_in_walk_order() {
        let root = tree("keys");
        let walked: Vec<Vec<u8>> = walker(&root)
            .build()
            .filter_map(|entry| walk_key(&root, entry.ok()?.path()))
            .collect();
        let mut sorted = walked.clone();
        sorted.sort();
        assert_eq!(walked, sorted);
        let _ = std::fs::remove_dir_all(&root);
    }
}
