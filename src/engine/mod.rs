//! The kit's engine, in-process (`electron/kit/core.ts` in Rust): the five
//! tools, the params check each one runs, and [`run_noted`], the one dispatch
//! `lwk` (and later the MCP server) share.
//!
//! - The dispatcher checks params and resolves the path ([`scope`]); the
//!   search modules get a real, in-scope folder (search_text and
//!   search_structural: or file) and checked inputs.
//! - `path` names the folder or file; `folder` is its older name, taken
//!   as an alias. Both given and different is `invalid`.
//! - Each answer has the socket's `result` shape: an array of hits for
//!   find_files, search_text, search_structural and search_sessions, and
//!   `{files, truncated}` for list_files.
//! - Every failure is a [`KitError`] with a code.

pub mod cache;
pub mod files;
pub mod find;
pub mod git;
pub mod scope;
pub mod sessions;
pub mod structural;
pub mod text;
pub mod types;

use crate::paths;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use scope::Target;
use types::{
    Answered, FindOptions, KitError, Limit, SessionOptions, StructuralLang, StructuralOptions,
    TextOptions, Tool, FOLDER_MAX, GLOBS_MAX, GLOB_MAX, MAX_LINES, QUERY_MAX,
};

/// Who is calling, and what it may reach.
#[derive(Debug, Clone)]
pub struct Context {
    /// Absolute; the folder a tool runs in when it names none.
    pub cwd: PathBuf,
    /// Absolute folders a tool may run in or under.
    pub roots: Vec<PathBuf>,
    /// The home folder the agents' transcripts are under.
    pub home: PathBuf,
}

impl Context {
    /// This process: its working folder as the only root.
    pub fn current() -> Result<Context, KitError> {
        let cwd = std::env::current_dir()?;
        Ok(Context {
            roots: vec![cwd.clone()],
            home: paths::home_dir().unwrap_or_default(),
            cwd,
        })
    }
}

/// Run `tool` on `params` for `ctx`: its answer, and the note that goes
/// with it (see [`Answered`]).
pub fn run_noted(tool: &str, params: &Value, ctx: &Context) -> Result<Answered, KitError> {
    let Some(tool) = Tool::parse(tool) else {
        return Err(KitError::invalid(format!("Unknown method: {tool}")));
    };
    let raw = object_of(params)?;
    let path = path_of(raw)?;
    let limit = limit_of(raw.get("limit"), tool.limits())?;
    let plain = |result| Ok(Answered { result, note: None });
    match tool {
        Tool::FindFiles => {
            let query = text(raw, "query", QUERY_MAX)?;
            let options = FindOptions {
                limit,
                git: flag(raw, "git")?.unwrap_or(false),
            };
            let root = folder_for(tool, path, ctx)?;
            plain(to_value(find::find_files(&root, query, &options)?)?)
        }
        Tool::SearchText => {
            let options = TextOptions {
                query: text(raw, "query", QUERY_MAX)?.to_owned(),
                regex: flag(raw, "regex")?.unwrap_or(false),
                case_sensitive: flag(raw, "case_sensitive")?,
                globs: globs_of(raw.get("globs"))?,
                max_files: limit,
                max_lines: whole_of(raw.get("max_lines"), "max_lines", MAX_LINES)?,
            };
            let target = scope::resolve_target(path, &ctx.cwd, &ctx.roots)?;
            plain(to_value(text::search_text(&target, &options)?)?)
        }
        Tool::SearchStructural => {
            let options = StructuralOptions {
                pattern: text(raw, "pattern", QUERY_MAX)?.to_owned(),
                lang: lang_of(raw.get("lang"))?,
                globs: globs_of(raw.get("globs"))?,
                max_files: limit,
                max_lines: whole_of(raw.get("max_lines"), "max_lines", MAX_LINES)?,
            };
            let target = scope::resolve_target(path, &ctx.cwd, &ctx.roots)?;
            let found = structural::search_structural(&target, &options)?;
            Ok(Answered {
                result: to_value(found.files)?,
                note: found.note,
            })
        }
        Tool::ListFiles => {
            let root = folder_for(tool, path, ctx)?;
            plain(to_value(files::list_files(&root, limit)?)?)
        }
        Tool::SearchSessions => {
            let query = text(raw, "query", QUERY_MAX)?;
            let options = SessionOptions {
                folders: session_folders(tool, path, ctx)?,
                limit,
                home: ctx.home.clone(),
            };
            plain(to_value(sessions::search_sessions(query, &options)?)?)
        }
    }
}

/// search_structural's pattern fix ([`structural::fix_ahead`]), made in
/// `params` before they go to the app's kit socket, whose engine (contract
/// 1.1.0) does not make it: `pattern` replaced by the fixed one. Returns
/// the note to give with the answer; None, `params` untouched, when there
/// is nothing to fix or no language to fix it for (by `lang`, the file the
/// path names, or the pattern's shape).
pub fn fix_ahead(tool: &str, params: &mut Map<String, Value>, cwd: &Path) -> Option<String> {
    if tool != Tool::SearchStructural.name() {
        return None;
    }
    let pattern = params.get("pattern")?.as_str()?;
    // A bad `lang` is the search's to refuse.
    let lang = match params.get("lang") {
        None | Some(Value::Null) => None,
        Some(value) => Some(StructuralLang::parse(value.as_str()?)?),
    };
    let named = [params.get("path"), params.get("folder")]
        .into_iter()
        .flatten()
        .find_map(Value::as_str);
    let extension = named.and_then(|named| {
        let full = PathBuf::from(paths::resolve(&cwd.to_string_lossy(), named));
        let is_file = std::fs::metadata(&full).is_ok_and(|meta| meta.is_file());
        is_file.then(|| full.extension()?.to_str().map(str::to_owned))?
    });
    let (fixed, note) = structural::fix_ahead(pattern, lang, extension.as_deref())?;
    params.insert("pattern".into(), Value::String(fixed));
    Some(note)
}

/// `params` as an app on contract 1.1.0 takes them over its kit socket:
/// `path` sent as `folder`. None when the call needs this engine: it names
/// a file, or passes `max_lines`, or `globs` to search_structural, or both
/// `path` and `folder`. The caller then runs it in-process.
pub fn for_socket(tool: &str, params: &Map<String, Value>, cwd: &Path) -> Option<Map<String, Value>> {
    if !absent(params.get("max_lines")) {
        return None;
    }
    if tool == Tool::SearchStructural.name() && !absent(params.get("globs")) {
        return None;
    }
    let mut out = params.clone();
    let path = out.remove("path").filter(|path| !path.is_null());
    if let Some(path) = path {
        match out.get("folder") {
            Some(folder) if !folder.is_null() && *folder != path => return None,
            _ => {
                out.insert("folder".into(), path);
            }
        }
    }
    if let Some(Value::String(path)) = out.get("folder") {
        let full = paths::resolve(&cwd.to_string_lossy(), path);
        if std::fs::metadata(full).is_ok_and(|meta| meta.is_file()) {
            return None;
        }
    }
    Some(out)
}

/// The folder or file a call names: `path`, or `folder`, its older name.
fn path_of(raw: &Map<String, Value>) -> Result<Option<&str>, KitError> {
    let path = optional_text(raw, "path", FOLDER_MAX)?;
    let folder = optional_text(raw, "folder", FOLDER_MAX)?;
    match (path, folder) {
        (Some(path), Some(folder)) if path != folder => Err(KitError::invalid(
            "path and folder name different places; pass only path (folder is its older name).",
        )),
        (Some(path), _) => Ok(Some(path)),
        (None, folder) => Ok(folder),
    }
}

/// The folder `path` names for this caller, checked against its roots; a
/// file is `invalid` for `tool`, which searches folders only.
fn folder_for(tool: Tool, path: Option<&str>, ctx: &Context) -> Result<PathBuf, KitError> {
    match scope::resolve_target(path, &ctx.cwd, &ctx.roots)? {
        Target::Folder(found) => Ok(found),
        Target::File { .. } => Err(KitError::invalid(format!(
            "{} searches a folder, and {} is a file. Pass its folder, or use search_text or search_structural to search one file.",
            tool.name(),
            paths::resolve(&ctx.cwd.to_string_lossy(), path.unwrap_or(".")),
        ))),
    }
}

/// The folders whose sessions `search_sessions` reads: `folder` as given
/// and as its real path, or every root both ways when unset.
fn session_folders(
    tool: Tool,
    folder: Option<&str>,
    ctx: &Context,
) -> Result<Vec<String>, KitError> {
    let mut folders: Vec<String> = Vec::new();
    let cwd = ctx.cwd.to_string_lossy();
    match folder {
        Some(folder) => {
            let real = folder_for(tool, Some(folder), ctx)?;
            folders.push(paths::resolve(&cwd, folder));
            folders.push(display(&real));
        }
        None => {
            // Refuses a cwd outside scope the same way a folder is refused.
            folder_for(tool, None, ctx)?;
            folders.extend(ctx.roots.iter().map(|root| display(root)));
            folders.extend(scope::real_roots(&ctx.roots)?.iter().map(|r| display(r)));
        }
    }
    let mut seen: Vec<String> = Vec::new();
    folders.retain(|folder| {
        let fresh = !seen.contains(folder);
        if fresh {
            seen.push(folder.clone());
        }
        fresh
    });
    Ok(folders)
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn to_value<T: serde::Serialize>(answer: T) -> Result<Value, KitError> {
    serde_json::to_value(answer).map_err(|error| KitError::internal(error.to_string()))
}

// ── Params (core.ts's checks, the same words) ───────────────────────────────

/// The length JavaScript gives `text`: UTF-16 units.
fn js_length(text: &str) -> usize {
    text.encode_utf16().count()
}

static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();

fn object_of(value: &Value) -> Result<&Map<String, Value>, KitError> {
    match value {
        Value::Null => Ok(EMPTY.get_or_init(Map::new)),
        Value::Object(map) => Ok(map),
        _ => Err(KitError::invalid("The params must be an object.")),
    }
}

/// Whether `raw[key]` is unset, as `undefined` or `null` is.
fn absent(value: Option<&Value>) -> bool {
    matches!(value, None | Some(Value::Null))
}

fn text<'a>(raw: &'a Map<String, Value>, key: &str, max: usize) -> Result<&'a str, KitError> {
    let Some(Value::String(value)) = raw.get(key) else {
        return Err(KitError::invalid(format!("{key} must be a string.")));
    };
    if js_length(value) > max {
        return Err(KitError::invalid(format!(
            "{key} is too long ({max} at most)."
        )));
    }
    Ok(value)
}

fn optional_text<'a>(
    raw: &'a Map<String, Value>,
    key: &str,
    max: usize,
) -> Result<Option<&'a str>, KitError> {
    if absent(raw.get(key)) {
        return Ok(None);
    }
    text(raw, key, max).map(Some)
}

fn flag(raw: &Map<String, Value>, key: &str) -> Result<Option<bool>, KitError> {
    match raw.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(KitError::invalid(format!("{key} must be true or false."))),
    }
}

/// `limit`, defaulted, and refused outside `1..=max`.
fn limit_of(value: Option<&Value>, limits: Limit) -> Result<usize, KitError> {
    whole_of(value, "limit", limits)
}

/// A whole-number input `key`, defaulted, and refused outside `1..=max`.
fn whole_of(value: Option<&Value>, key: &str, limits: Limit) -> Result<usize, KitError> {
    if absent(value) {
        return Ok(limits.default);
    }
    let whole = value
        .and_then(Value::as_f64)
        .filter(|x| x.fract() == 0.0 && *x >= 1.0 && *x <= limits.max as f64);
    match whole {
        Some(x) => Ok(x as usize),
        None => Err(KitError::invalid(format!(
            "{key} must be a whole number from 1 to {}.",
            limits.max
        ))),
    }
}

fn globs_of(value: Option<&Value>) -> Result<Vec<String>, KitError> {
    if absent(value) {
        return Ok(Vec::new());
    }
    let list = match value {
        Some(Value::Array(list)) if list.len() <= GLOBS_MAX => list,
        _ => {
            return Err(KitError::invalid(format!(
                "globs must be a list of {GLOBS_MAX} at most."
            )))
        }
    };
    list.iter()
        .map(|glob| match glob {
            Value::String(glob) if !glob.is_empty() && js_length(glob) <= GLOB_MAX => {
                Ok(glob.clone())
            }
            _ => Err(KitError::invalid("Each glob must be a non-empty string.")),
        })
        .collect()
}

fn lang_of(value: Option<&Value>) -> Result<Option<StructuralLang>, KitError> {
    if absent(value) {
        return Ok(None);
    }
    match value.and_then(Value::as_str).and_then(StructuralLang::parse) {
        Some(lang) => Ok(Some(lang)),
        None => {
            let names: Vec<&str> = StructuralLang::ALL.iter().map(|l| l.name()).collect();
            Err(KitError::invalid(format!(
                "lang must be one of {}.",
                names.join(", ")
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use types::KitErrorCode;

    /// A project with a `.rs` and a `.py` file, as a context rooted in it.
    fn project(tag: &str) -> Context {
        let base = std::env::temp_dir().join(format!("lwk-test-mod-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::write(base.join("src/a.rs"), "fn a() {}\nfn b() { a() }\n").unwrap();
        std::fs::write(base.join("b.py"), "def f(x):\n    return x\n").unwrap();
        let cwd = scope::real(&base).unwrap();
        Context {
            roots: vec![cwd.clone()],
            home: cwd.join("home"),
            cwd,
        }
    }

    fn code(result: Result<Answered, KitError>) -> KitErrorCode {
        result.unwrap_err().code
    }

    #[test]
    fn path_and_folder_name_the_same_input() {
        let ctx = project("alias");
        let by = |params: Value| run_noted("search_text", &params, &ctx);
        let path = by(json!({"query": "fn", "path": "src"})).unwrap();
        let folder = by(json!({"query": "fn", "folder": "src"})).unwrap();
        let both = by(json!({"query": "fn", "path": "src", "folder": "src"})).unwrap();
        assert_eq!(path, folder);
        assert_eq!(path, both);
        assert_eq!(code(by(json!({"query": "fn", "path": "src", "folder": "."}))), KitErrorCode::Invalid);
        assert_eq!(code(by(json!({"query": "fn", "path": 3}))), KitErrorCode::Invalid);
        let _ = std::fs::remove_dir_all(&ctx.cwd);
    }

    #[test]
    fn a_file_is_searched_alone_by_the_search_tools() {
        let ctx = project("file");
        let text = run_noted("search_text", &json!({"query": "fn", "path": "src/a.rs"}), &ctx).unwrap();
        assert_eq!(text.result[0]["path"], "a.rs");
        assert_eq!(text.result[0]["lines"].as_array().unwrap().len(), 2);
        let sg = run_noted(
            "search_structural",
            &json!({"pattern": "return $X", "path": "b.py"}),
            &ctx,
        )
        .unwrap();
        assert_eq!(sg.result[0]["path"], "b.py");
        assert_eq!(sg.note, None);
        let _ = std::fs::remove_dir_all(&ctx.cwd);
    }

    #[test]
    fn a_file_is_invalid_for_the_folder_tools() {
        let ctx = project("folders");
        for (tool, params) in [
            ("find_files", json!({"query": "a", "path": "src/a.rs"})),
            ("list_files", json!({"path": "src/a.rs"})),
            ("search_sessions", json!({"query": "a", "folder": "src/a.rs"})),
        ] {
            let error = run_noted(tool, &params, &ctx).unwrap_err();
            assert_eq!(error.code, KitErrorCode::Invalid, "{tool}");
            assert!(error.message.contains("is a file"), "{tool}: {}", error.message);
        }
        // Outside scope is scope, file or not.
        let outside = run_noted("search_text", &json!({"query": "a", "path": "/etc/hosts"}), &ctx);
        assert_eq!(code(outside), KitErrorCode::Scope);
        let _ = std::fs::remove_dir_all(&ctx.cwd);
    }

    #[test]
    fn max_lines_is_checked() {
        let ctx = project("lines");
        for bad in [json!(0), json!(2001), json!(1.5), json!("5")] {
            let params = json!({"query": "fn", "max_lines": bad});
            assert_eq!(code(run_noted("search_text", &params, &ctx)), KitErrorCode::Invalid);
            let params = json!({"pattern": "fn $N() {}", "max_lines": bad});
            assert_eq!(code(run_noted("search_structural", &params, &ctx)), KitErrorCode::Invalid);
        }
        let one = run_noted("search_text", &json!({"query": "fn", "max_lines": 1}), &ctx).unwrap();
        assert_eq!(one.result[0]["lines"].as_array().unwrap().len(), 1);
        assert_eq!(one.result[0]["truncated"], true);
        let error = run_noted("search_text", &json!({"query": "fn", "max_lines": 0}), &ctx).unwrap_err();
        assert_eq!(error.message, "max_lines must be a whole number from 1 to 2000.");
        let _ = std::fs::remove_dir_all(&ctx.cwd);
    }

    #[test]
    fn the_socket_gets_folder_and_never_new_inputs() {
        let ctx = project("socket");
        let map = |value: Value| value.as_object().unwrap().clone();
        let socket = |tool: &str, value: Value| for_socket(tool, &map(value), &ctx.cwd);
        assert_eq!(
            socket("search_text", json!({"query": "a", "path": "src"})),
            Some(map(json!({"query": "a", "folder": "src"})))
        );
        assert_eq!(
            socket("search_text", json!({"query": "a", "folder": "src", "globs": ["*.rs"]})),
            Some(map(json!({"query": "a", "folder": "src", "globs": ["*.rs"]})))
        );
        assert_eq!(socket("search_text", json!({"query": "a", "path": "src/a.rs"})), None);
        assert_eq!(socket("search_text", json!({"query": "a", "max_lines": 5})), None);
        assert_eq!(socket("search_structural", json!({"pattern": "a", "globs": ["*.rs"]})), None);
        assert_eq!(socket("list_files", json!({"path": "src", "folder": "."})), None);
        let _ = std::fs::remove_dir_all(&ctx.cwd);
    }

    #[test]
    fn fixes_the_pattern_before_the_socket() {
        let ctx = project("ahead");
        let fix = |value: Value| {
            let mut params = value.as_object().unwrap().clone();
            let note = fix_ahead("search_structural", &mut params, &ctx.cwd);
            (params.get("pattern").cloned(), note.is_some())
        };
        let fixed = |text: &str| (Some(json!(text)), true);
        let kept = |text: &str| (Some(json!(text)), false);
        assert_eq!(fix(json!({"pattern": "def $F($$$A)", "lang": "python"})), fixed("def $F($$$A): $$$BODY"));
        // The language from the file named, or from the pattern's shape.
        assert_eq!(fix(json!({"pattern": "fn $N($$$A)", "path": "src/a.rs"})), fixed("fn $N($$$A) { $$$BODY }"));
        assert_eq!(fix(json!({"pattern": "fn $N($$$A)", "folder": "src"})), fixed("fn $N($$$A) { $$$BODY }"));
        assert_eq!(fix(json!({"pattern": "def $F($$$A)", "path": "src/a.rs"})), kept("def $F($$$A)"));
        assert_eq!(fix(json!({"pattern": "console.log($A)"})), kept("console.log($A)"));
        assert_eq!(fix(json!({"pattern": "def $F($$$A)", "lang": "cobol"})), kept("def $F($$$A)"));
        let mut text = json!({"query": "def $F($$$A)"}).as_object().unwrap().clone();
        assert_eq!(fix_ahead("search_text", &mut text, &ctx.cwd), None);
        let _ = std::fs::remove_dir_all(&ctx.cwd);
    }
}
