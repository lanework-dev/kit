//! `lwk find|grep|sg|ls|sessions`: the flags, the call to the app's kit
//! socket or, with no app to answer, to the in-process engine (`engine/`),
//! and the output. Both answers have the same shape and print the same
//! bytes.
//!
//! - Flags mirror the tools' inputs (`KIT_TOOLS`, packages/protocol); the
//!   kit checks them, so a bad value fails with the kit's own words.
//! - `--json` prints the tool's answer as the MCP server does. Otherwise one
//!   line per hit: `path:line: text` for grep and sg, a path for find and
//!   ls, a heading and its snippets per session.
//! - Paths are relative to the folder searched, and prefixed with FOLDER as
//!   it was typed when there is one, as grep and rg print them. A file as
//!   PATH (grep, sg) is printed exactly as it was typed.
//! - `--limit` counts files (paths, sessions); `--max-lines` caps grep's
//!   and sg's lines over all files. A cut answer names the one that cut it.
//! - Exit code: 0 found, 1 nothing found (ls: 0 always), 2 an error. A cut
//!   answer says so on stderr.
//!
//! The app is asked first over its socket (`socket.rs`, which says when it
//! counts as absent); with no app, lwk runs the same params in-process
//! (`engine::run_noted`), from the working folder with it as the only root. A
//! call the app's contract (1.1.0) cannot answer (a file as PATH,
//! `--max-lines`, sg's `-g`) runs in-process without asking it
//! (`engine::for_socket`). A sg pattern that needs a body is fixed before
//! the app is asked (`engine::fix_ahead`), and the note printed as ever.

use crate::engine::{self, types::MAX_LINES};
use crate::socket::{self, Answer};
use crate::{jsonjs, out, paths};
use serde_json::value::RawValue;
use serde_json::{Map, Value};
use std::ffi::OsString;

/// A command's tool, as the socket names it.
fn tool(command: &str) -> &'static str {
    match command {
        "find" => "find_files",
        "grep" => "search_text",
        "sg" => "search_structural",
        "ls" => "list_files",
        _ => "search_sessions",
    }
}

/// The input a flag of `command` sets, or None when it has no such flag.
fn flag(command: &str, name: &str) -> Option<&'static str> {
    if name == "--limit" {
        return Some("limit");
    }
    match (command, name) {
        ("grep", "-E" | "--regex") => Some("regex"),
        ("grep", "-F" | "--fixed-strings") => Some("fixed"),
        ("grep", "-i" | "--ignore-case") => Some("ignore_case"),
        ("grep", "-s" | "--case-sensitive") => Some("case_sensitive"),
        ("grep" | "sg", "-g" | "--glob") => Some("globs"),
        ("grep" | "sg", "--max-lines") => Some("max_lines"),
        ("find", "--git") => Some("git"),
        ("sg", "--lang") => Some("lang"),
        _ => None,
    }
}

/// Flags that take a value.
fn valued(name: &str) -> bool {
    matches!(name, "-g" | "--glob" | "--lang" | "--limit" | "--max-lines")
}

struct Parsed {
    params: Map<String, Value>,
    /// FOLDER as typed, to prefix the paths printed.
    folder: Option<String>,
    json: bool,
}

impl Parsed {
    /// The line cap the call runs with: `--max-lines`, or the default.
    fn max_lines(&self) -> usize {
        match self.params.get("max_lines").and_then(Value::as_u64) {
            Some(n) => n as usize,
            None => MAX_LINES.default,
        }
    }
}

/// `--limit`'s value as `Number()` reads it; a bad number goes to the kit
/// as it was typed, to be refused in the kit's words.
fn limit_value(text: &str) -> Value {
    match jsonjs::number(text) {
        Some(x) if x.is_finite() => {
            if x.fract() == 0.0 && x.abs() < 9_007_199_254_740_992.0 {
                Value::from(x as i64)
            } else {
                serde_json::Number::from_f64(x).map_or(Value::Null, Value::Number)
            }
        }
        _ => Value::String(text.to_owned()),
    }
}

/// `lwk COMMAND ARGS` as the tool's params, or the message for a bad one.
fn parse(command: &str, args: &[String]) -> Result<Parsed, String> {
    let mut positional: Vec<String> = Vec::new();
    let mut params = Map::new();
    let mut json = false;
    let mut literal = false;
    let mut i = 0;
    while i < args.len() {
        let raw = &args[i];
        i += 1;
        if literal || raw == "-" || !raw.starts_with('-') {
            positional.push(raw.clone());
            continue;
        }
        if raw == "--" {
            literal = true;
            continue;
        }
        let mut name = raw.as_str();
        let mut value: Option<String> = None;
        if raw.starts_with("--") {
            if let Some(eq) = raw.find('=') {
                value = Some(raw[eq + 1..].to_owned());
                name = &raw[..eq];
            }
        }
        if name == "--json" {
            json = true;
            continue;
        }
        let Some(input) = flag(command, name) else {
            return Err(format!("unknown option {name}. See lwk help."));
        };
        if valued(name) {
            if value.is_none() {
                value = args.get(i).cloned();
                i += 1;
            }
            if value.is_none() {
                return Err(format!("{name} needs a value."));
            }
        } else if value.is_some() {
            return Err(format!("{name} takes no value."));
        }
        let value = value.unwrap_or_default();
        match input {
            "limit" => {
                params.insert("limit".into(), limit_value(&value));
            }
            "max_lines" => {
                params.insert("max_lines".into(), limit_value(&value));
            }
            "globs" => {
                let globs = params
                    .entry("globs")
                    .or_insert_with(|| Value::Array(Vec::new()));
                if let Value::Array(list) = globs {
                    list.push(Value::String(value));
                }
            }
            "lang" => {
                params.insert("lang".into(), Value::String(value));
            }
            "git" => {
                params.insert("git".into(), Value::Bool(true));
            }
            "regex" => {
                params.insert("regex".into(), Value::Bool(true));
            }
            "fixed" => {
                params.insert("regex".into(), Value::Bool(false));
            }
            "ignore_case" => {
                params.insert("case_sensitive".into(), Value::Bool(false));
            }
            _ => {
                params.insert("case_sensitive".into(), Value::Bool(true));
            }
        }
    }
    let wants_query = command != "ls";
    let max = if wants_query { 2 } else { 1 };
    if positional.len() > max || (wants_query && positional.is_empty()) {
        let query = match (wants_query, command) {
            (false, _) => "",
            (true, "sg") => "PATTERN ",
            (true, _) => "QUERY ",
        };
        let place = if matches!(command, "grep" | "sg") { "PATH" } else { "FOLDER" };
        return Err(format!("Usage: lwk {command} {query}[{place}]"));
    }
    let mut positional = positional.into_iter();
    if wants_query {
        let key = if command == "sg" { "pattern" } else { "query" };
        params.insert(
            key.into(),
            Value::String(positional.next().unwrap_or_default()),
        );
    }
    let folder = positional.next();
    if let Some(folder) = &folder {
        params.insert("folder".into(), Value::String(folder.clone()));
    }
    Ok(Parsed {
        params,
        folder,
        json,
    })
}

/// `path` as printed: under FOLDER as typed, when one was; a file named
/// as PATH exactly as typed.
fn shown(path: Option<&Value>, folder: Option<&str>, file: bool) -> String {
    let path = jsonjs::template(path);
    match folder {
        Some(folder) if file => folder.to_owned(),
        None => path,
        Some(folder) => paths::join(folder, &path),
    }
}

/// Whether `typed` (from the working folder) is a file: then grep and sg
/// searched it alone, and print it as typed.
fn is_file(typed: Option<&str>) -> bool {
    let Some(typed) = typed else { return false };
    let Ok(cwd) = std::env::current_dir() else { return false };
    let full = paths::resolve(&cwd.to_string_lossy(), typed);
    std::fs::metadata(full).is_ok_and(|meta| meta.is_file())
}

/// Say on stderr that the answer was cut, and how to see more.
fn note_cut(what: &str) {
    eprintln!("lwk: stopped at the {what} limit; pass --limit N for more, or narrow the folder.");
}

/// Say on stderr that the line cap cut the answer.
fn note_line_cut() {
    eprintln!(
        "lwk: stopped at the line limit; pass --max-lines N for more ({} at most), or narrow the search.",
        MAX_LINES.max
    );
}

/// Print the answer to `tool` (the app's, or the engine's). Returns the exit code, or None for an
/// answer not of the tool's shape.
fn print(tool: &str, raw: &RawValue, note: Option<&str>, parsed: &Parsed) -> Option<i32> {
    let result: Value = serde_json::from_str(raw.get()).ok()?;
    let folder = parsed.folder.as_deref();
    let json = parsed.json;
    let file = matches!(tool, "search_text" | "search_structural") && is_file(folder);
    let shown = |path: Option<&Value>, folder: Option<&str>| shown(path, folder, file);
    match tool {
        "find_files" => {
            let files = result.as_array()?;
            if json {
                out(&format!("{{\"files\":{}}}\n", raw.get()));
            } else if !files.is_empty() {
                let lines: Vec<String> = files
                    .iter()
                    .map(|hit| shown(hit.get("path"), folder))
                    .collect();
                out(&format!("{}\n", lines.join("\n")));
            }
            Some(if files.is_empty() { 1 } else { 0 })
        }
        "search_text" | "search_structural" => {
            let matches = result.as_array()?;
            if let (Some(note), false) = (note, json) {
                eprintln!("lwk sg: {note}");
            }
            if json {
                match note {
                    None => out(&format!("{{\"matches\":{}}}\n", raw.get())),
                    Some(note) => out(&format!(
                        "{{\"matches\":{},\"note\":{}}}\n",
                        raw.get(),
                        jsonjs::stringify(&Value::String(note.to_owned()))
                    )),
                }
            } else {
                let mut lines: Vec<String> = Vec::new();
                for file in matches {
                    let path = shown(file.get("path"), folder);
                    for line in file.get("lines")?.as_array()? {
                        lines.push(format!(
                            "{path}:{}: {}",
                            jsonjs::template(line.get("line")),
                            jsonjs::template(line.get("text")),
                        ));
                    }
                }
                if !lines.is_empty() {
                    out(&format!("{}\n", lines.join("\n")));
                }
            }
            let cut = matches.last().and_then(|last| last.get("truncated"));
            if jsonjs::truthy(cut) && !json {
                let lines: usize = matches
                    .iter()
                    .filter_map(|file| file.get("lines").and_then(Value::as_array))
                    .map(Vec::len)
                    .sum();
                if lines >= parsed.max_lines() {
                    note_line_cut();
                } else {
                    note_cut("file");
                }
            }
            Some(if matches.is_empty() { 1 } else { 0 })
        }
        "list_files" => {
            let files = result.get("files")?.as_array()?;
            if json {
                out(&format!("{}\n", raw.get()));
            } else if !files.is_empty() {
                let lines: Vec<String> =
                    files.iter().map(|path| shown(Some(path), folder)).collect();
                out(&format!("{}\n", lines.join("\n")));
            }
            if jsonjs::truthy(result.get("truncated")) && !json {
                note_cut("path");
            }
            Some(0)
        }
        _ => {
            let sessions = result.as_array()?;
            if json {
                out(&format!("{{\"sessions\":{}}}\n", raw.get()));
            } else {
                let mut blocks: Vec<String> = Vec::new();
                for session in sessions {
                    let mut block = [
                        session.get("harness"),
                        session.get("sessionId"),
                        session.get("at"),
                        session.get("title"),
                    ]
                    .map(jsonjs::template)
                    .join(" ");
                    for line in session.get("lines")?.as_array()? {
                        block.push_str(&format!(
                            "\n  {}: {}",
                            jsonjs::template(line.get("line")),
                            jsonjs::template(line.get("text")),
                        ));
                    }
                    blocks.push(block);
                }
                if !blocks.is_empty() {
                    out(&format!("{}\n", blocks.join("\n\n")));
                }
            }
            Some(if sessions.is_empty() { 1 } else { 0 })
        }
    }
}

/// What a tool answered: its result as JSON text, and the engine's note.
type Raw = (Box<RawValue>, Option<String>);

/// Run the tool in this process, its answer as JSON text the way the app
/// writes it (`JSON.stringify`) with its note, or the error's exit code.
fn run_here(command: &str, tool: &str, params: Map<String, Value>) -> Result<Raw, i32> {
    let ctx = match engine::Context::current() {
        Ok(ctx) => ctx,
        Err(error) => {
            eprintln!("lwk {command}: {}", error.message);
            return Err(error.exit_code());
        }
    };
    match engine::run_noted(tool, &Value::Object(params), &ctx) {
        Ok(answer) => match RawValue::from_string(jsonjs::stringify(&answer.result)) {
            Ok(raw) => Ok((raw, answer.note)),
            Err(error) => {
                eprintln!("lwk {command}: {error}");
                Err(2)
            }
        },
        Err(error) => {
            eprintln!("lwk {command}: {}", error.message);
            Err(error.exit_code())
        }
    }
}

/// Run `lwk COMMAND ARGS`. Returns the exit code.
pub fn run(command: &str, args: &[OsString]) -> i32 {
    let text: Vec<String> = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let mut parsed = match parse(command, &text) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("lwk {command}: {message}");
            return 2;
        }
    };
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd.to_string_lossy().into_owned(),
        Err(error) => {
            eprintln!("lwk {command}: {error}");
            return 2;
        }
    };
    let tool = tool(command);
    // The app (contract 1.1.0) does not fix a pattern: fixed here first.
    let fixed_note = engine::fix_ahead(tool, &mut parsed.params, std::path::Path::new(&cwd));
    let asked = match engine::for_socket(tool, &parsed.params, std::path::Path::new(&cwd)) {
        Some(mut call_params) => {
            call_params.insert("cwd".into(), Value::String(cwd));
            socket::call(tool, call_params)
        }
        None => Answer::Absent,
    };
    let raw: Option<Raw> = match asked {
        Answer::Absent => match run_here(command, tool, parsed.params.clone()) {
            Ok(raw) => Some(raw),
            Err(code) => return code,
        },
        Answer::Failed(error) => {
            eprintln!("lwk {command}: {}", error.message);
            return 2;
        }
        Answer::Result(raw) => raw.map(|raw| (raw, None)),
    };
    let printed = raw
        .as_ref()
        .and_then(|(raw, note)| print(tool, raw, fixed_note.as_deref().or(note.as_deref()), &parsed));
    match printed {
        Some(code) => code,
        None => {
            eprintln!("lwk {command}: the answer was not understood.");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn sg_takes_globs_and_max_lines() {
        let parsed = parse("sg", &args(&["fn $N()", "-g", "*.rs", "--glob=!tests/**", "--max-lines", "5", "src"])).unwrap();
        assert_eq!(
            Value::Object(parsed.params.clone()),
            json!({"globs": ["*.rs", "!tests/**"], "max_lines": 5, "pattern": "fn $N()", "folder": "src"})
        );
        assert_eq!(parsed.max_lines(), 5);
        let grep = parse("grep", &args(&["x", "--max-lines=7"])).unwrap();
        assert_eq!(grep.params.get("max_lines"), Some(&json!(7)));
        assert_eq!(parse("grep", &args(&["x"])).unwrap().max_lines(), MAX_LINES.default);
        // Only grep and sg have them.
        assert!(parse("find", &args(&["x", "--max-lines", "5"])).is_err());
        assert!(parse("ls", &args(&["-g", "*.rs"])).is_err());
        assert_eq!(
            parse("sg", &args(&[])).err().as_deref(),
            Some("Usage: lwk sg PATTERN [PATH]")
        );
    }

    #[test]
    fn a_file_is_shown_as_typed() {
        let path = json!("kit.rs");
        assert_eq!(shown(Some(&path), Some("./src/kit.rs"), true), "./src/kit.rs");
        assert_eq!(shown(Some(&path), Some("./src"), false), "src/kit.rs");
        assert_eq!(shown(Some(&path), None, false), "kit.rs");
    }
}
