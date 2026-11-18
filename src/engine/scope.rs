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

/// What a tool's `path` names: a folder, or one regular file in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A real folder in scope.
    Folder(PathBuf),
    /// A regular file in scope.
    File {
        /// The file's real path.
        file: PathBuf,
        /// Its name as the path named it (the last part, before symlinks
        /// are followed): the path an answer gives it.
        name: String,
    },
}

/// What `path` names (relative to `cwd` when relative; `cwd` when unset):
/// a real folder or regular file that is one of `roots` or under one.
///
/// Fails `invalid` when `cwd` is not absolute; `not_found` when nothing is
/// there, or it is neither a folder nor a regular file; `scope` when it is
/// outside every root (a symlink by where it leads), or no root is usable.
pub fn resolve_target(
    path: Option<&str>,
    cwd: &Path,
    roots: &[PathBuf],
) -> Result<Target, KitError> {
    if !cwd.is_absolute() {
        return Err(KitError::invalid("The working folder must be absolute."));
    }
    let asked = PathBuf::from(paths::resolve(
        &cwd.to_string_lossy(),
        path.unwrap_or("."),
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
    if found.is_dir() {
        if allowed.is_empty() {
            return Err(KitError::scope(
                "No folder is open to search here. Run it inside a project folder.",
            ));
        }
        if !allowed.iter().any(|root| is_within(root, &found)) {
            return Err(outside());
        }
        return Ok(Target::Folder(found));
    }
    // Not a folder: out of scope first, so what lies outside is not probed.
    if allowed.is_empty() {
        return Err(KitError::scope(
            "No folder is open to search here. Run it inside a project folder.",
        ));
    }
    if !allowed.iter().any(|root| is_within(root, &found)) {
        return Err(outside());
    }
    if !found.is_file() {
        return Err(KitError::not_found(format!(
            "That is not a folder or a file: {}",
            asked.display()
        )));
    }
    let name = asked
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(Target::File { file: found, name })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("lwk-test-scope-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("in/src")).unwrap();
        std::fs::create_dir_all(base.join("out")).unwrap();
        std::fs::write(base.join("in/src/a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(base.join("out/b.rs"), "fn b() {}\n").unwrap();
        real(&base).unwrap()
    }

    #[test]
    fn a_file_in_scope_is_a_target() {
        let base = tree("file");
        let cwd = base.join("in");
        let roots = vec![cwd.clone()];
        let got = resolve_target(Some("src/a.rs"), &cwd, &roots).unwrap();
        assert_eq!(
            got,
            Target::File {
                file: cwd.join("src/a.rs"),
                name: "a.rs".to_owned()
            }
        );
        assert_eq!(
            resolve_target(Some("src"), &cwd, &roots).unwrap(),
            Target::Folder(cwd.join("src"))
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_file_outside_scope_is_refused() {
        let base = tree("outside");
        let cwd = base.join("in");
        let roots = vec![cwd.clone()];
        let scope = super::super::types::KitErrorCode::Scope;
        // By `..`, by an absolute path, and through a symlink.
        assert_eq!(resolve_target(Some("../out/b.rs"), &cwd, &roots).unwrap_err().code, scope);
        let absolute = base.join("out/b.rs");
        assert_eq!(
            resolve_target(Some(absolute.to_str().unwrap()), &cwd, &roots).unwrap_err().code,
            scope
        );
        std::os::unix::fs::symlink(&absolute, cwd.join("link.rs")).unwrap();
        assert_eq!(resolve_target(Some("link.rs"), &cwd, &roots).unwrap_err().code, scope);
        let _ = std::fs::remove_dir_all(&base);
    }
}
