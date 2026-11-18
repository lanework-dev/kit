//! Path scope for the kit (`electron/kit/scope.ts`): every tool runs in one
//! folder, and that folder must be one of the caller's allowed roots or
//! under one.
//!
//! - Both sides are resolved with realpath, so a symlink that leads out of a
//!   root is refused like the path it leads to.
//! - `..` is folded lexically before any symlink is followed (as Node's
//!   `path.resolve`), so `../x` names the cwd's sibling and is refused.
//! - The roots come from the caller, never from the request: in-process
//!   `lwk` passes its cwd.
//! - A root that is the file-system root or the home folder itself is too
//!   broad to search, and is dropped; with no root left, every folder is
//!   refused.
//! - A folder that does not exist is `not_found`, unless it also lies
//!   outside every root, which is `scope`: what lies outside is not probed.

use super::types::KitError;
use crate::paths;
use std::path::{Path, PathBuf};

/// `path`'s real path, or None when it does not exist or cannot be read.
pub fn real(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// Whether `path` is `root` or inside it, component by component.
pub fn is_within(root: &Path, path: &Path) -> bool {
    path.starts_with(root)
}

/// Whether `path` (real) is too broad to be a root: `/` or the home folder.
fn too_broad(path: &Path) -> bool {
    if path.parent().is_none() {
        return true;
    }
    paths::home_dir()
        .and_then(|home| real(&home))
        .is_some_and(|home| home == path)
}

/// The real paths of `roots` that exist and are folders, without
/// duplicates, `/` and the home folder dropped.
///
/// Fails `invalid` when a root is not absolute.
pub fn real_roots(roots: &[PathBuf]) -> Result<Vec<PathBuf>, KitError> {
    let mut out: Vec<PathBuf> = Vec::new();
    for root in roots {
        if !root.is_absolute() {
            return Err(KitError::invalid(format!(
                "A root must be an absolute path: {}",
                root.display()
            )));
        }
        let Some(found) = real(root) else { continue };
        if found.is_dir() && !too_broad(&found) && !out.contains(&found) {
            out.push(found);
        }
    }
    Ok(out)
}

/// The folder a tool runs in: `folder` (relative to `cwd` when relative),
/// or `cwd` when unset, as a real path that is one of `roots` or under one.
///
/// Fails `invalid` when `cwd` is not absolute; `not_found` when the folder
/// does not exist or is not a folder; `scope` when it is outside every
/// root, or no root is usable.
pub fn resolve_folder(
    folder: Option<&str>,
    cwd: &Path,
    roots: &[PathBuf],
) -> Result<PathBuf, KitError> {
    if !cwd.is_absolute() {
        return Err(KitError::invalid("The working folder must be absolute."));
    }
    let asked = PathBuf::from(paths::resolve(
        &cwd.to_string_lossy(),
        folder.unwrap_or("."),
    ));
    let allowed = real_roots(roots)?;
    let outside = || {
        let list: Vec<String> = allowed.iter().map(|r| r.display().to_string()).collect();
        KitError::scope(format!(
            "{} is outside this project's folders ({}).",
            asked.display(),
            list.join(", ")
        ))
    };
    let Some(found) = real(&asked) else {
        let lexically_inside = roots
            .iter()
            .chain(allowed.iter())
            .any(|root| is_within(root, &asked));
        if !allowed.is_empty() && !lexically_inside {
            return Err(outside());
        }
        return Err(KitError::not_found(format!(
            "That folder does not exist: {}",
            asked.display()
        )));
    };
    if !found.is_dir() {
        return Err(KitError::not_found(format!(
            "That is not a folder: {}",
            asked.display()
        )));
    }
    if allowed.is_empty() {
        return Err(KitError::scope(
            "No folder is open to search here. Run it inside a project folder.",
        ));
    }
    if !allowed.iter().any(|root| is_within(root, &found)) {
        return Err(outside());
    }
    Ok(found)
}
