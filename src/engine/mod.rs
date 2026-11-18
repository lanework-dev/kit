//! The kit's engine, in-process (`electron/kit/core.ts` in Rust): the five
//! tools, the params check each one runs, and [`run`], the one dispatch
//! `lwk` (and later the MCP server) share.
//!
//! - The dispatcher checks params and resolves the folder ([`scope`]); the
//!   search modules get a real, in-scope folder and checked inputs.
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
use types::{
    FindOptions, KitError, Limit, SessionOptions, StructuralLang, TextOptions, Tool, FOLDER_MAX, GLOBS_MAX,
    GLOB_MAX, QUERY_MAX,
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

/// Run `tool` on `params` in this process's working folder.
pub fn run(tool: &str, params: &Value) -> Result<Value, KitError> {
    run_in(tool, params, &Context::current()?)
}

/// Run `tool` on `params` for `ctx`.
pub fn run_in(tool: &str, params: &Value, ctx: &Context) -> Result<Value, KitError> {
    let Some(tool) = Tool::parse(tool) else {
        return Err(KitError::invalid(format!("Unknown method: {tool}")));
    };
    let raw = object_of(params)?;
    let folder = optional_text(raw, "folder", FOLDER_MAX)?;
    let limit = limit_of(raw.get("limit"), tool.limits())?;
    let answer = match tool {
        Tool::FindFiles => {
            let query = text(raw, "query", QUERY_MAX)?;
            let options = FindOptions {
                limit,
                git: flag(raw, "git")?.unwrap_or(false),
            };
            let root = folder_for(folder, ctx)?;
            to_value(find::find_files(&root, query, &options)?)
        }
        Tool::SearchText => {
            let options = TextOptions {
                query: text(raw, "query", QUERY_MAX)?.to_owned(),
                regex: flag(raw, "regex")?.unwrap_or(false),
                case_sensitive: flag(raw, "case_sensitive")?,
                globs: globs_of(raw.get("globs"))?,
                max_files: limit,
            };
            let root = folder_for(folder, ctx)?;
            to_value(text::search_text(&root, &options)?)
        }
        Tool::SearchStructural => {
            let pattern = text(raw, "pattern", QUERY_MAX)?;
            let lang = lang_of(raw.get("lang"))?;
            let root = folder_for(folder, ctx)?;
            to_value(structural::search_structural(&root, pattern, lang, limit)?)
        }
        Tool::ListFiles => {
            let root = folder_for(folder, ctx)?;
            to_value(files::list_files(&root, limit)?)
        }
        Tool::SearchSessions => {
            let query = text(raw, "query", QUERY_MAX)?;
            let options = SessionOptions {
                folders: session_folders(folder, ctx)?,
                limit,
                home: ctx.home.clone(),
            };
            to_value(sessions::search_sessions(query, &options)?)
        }
    };
    answer
}

/// The folder `folder` names for this caller, checked against its roots.
fn folder_for(folder: Option<&str>, ctx: &Context) -> Result<PathBuf, KitError> {
    scope::resolve_folder(folder, &ctx.cwd, &ctx.roots)
}

/// The folders whose sessions `search_sessions` reads: `folder` as given
/// and as its real path, or every root both ways when unset.
fn session_folders(folder: Option<&str>, ctx: &Context) -> Result<Vec<String>, KitError> {
    let mut folders: Vec<String> = Vec::new();
    let cwd = ctx.cwd.to_string_lossy();
    match folder {
        Some(folder) => {
            let real = folder_for(Some(folder), ctx)?;
            folders.push(paths::resolve(&cwd, folder));
            folders.push(display(&real));
        }
        None => {
            // Refuses a cwd outside scope the same way a folder is refused.
            folder_for(None, ctx)?;
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
    if absent(value) {
        return Ok(limits.default);
    }
    let whole = value
        .and_then(Value::as_f64)
        .filter(|x| x.fract() == 0.0 && *x >= 1.0 && *x <= limits.max as f64);
    match whole {
        Some(x) => Ok(x as usize),
        None => Err(KitError::invalid(format!(
            "limit must be a whole number from 1 to {}.",
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
