//! `lwk mcp`: the lanework-kit MCP server over stdio (newline-delimited
//! JSON-RPC 2.0), serving the five kit tools of `contract/kit.json`
//! (`electron/mcp-kit/server.ts` in the app).
//!
//! - Each call goes to the app's kit socket first and runs in-process
//!   (`engine::run_noted`) when no app answers, as `kit.rs` does.
//! - Scope: the working folder at start, and folders under it that a call
//!   names; never wider. A cwd that is `/` or the home folder searches
//!   nothing (the engine refuses it).
//! - A tool that fails answers `isError: true` with `{error, message}`,
//!   never a JSON-RPC error; only an unknown method or tool is one.
//! - Single-threaded, no async runtime: calls run in the order they arrive.
//!
//! NOTE: stdout is the protocol channel; logs go to stderr.


use crate::engine::{self, types::KitError};
use crate::socket;
use serde_json::{json, Map, Value};
use std::io::{BufRead, Write};
use std::path::PathBuf;

const SERVER_NAME: &str = "lanework-kit";
const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
const LATEST_PROTOCOL_VERSION: &str = "2025-06-18";
const KNOWN_PROTOCOL_VERSIONS: [&str; 4] =
    ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];

/// What an agent is told at `initialize`. Agents search through the shell by
/// habit (`grep -r … | head`, `cat`, `sed -n`), and some load MCP tools only
/// on demand, so this says when to use each tool, not only what it does.
const INSTRUCTIONS: &str = concat!(
    "lanework-kit searches this project's files and past agent conversations. It respects .gitignore, skips binary files, and skips hidden files except in find_files, which lists them too in a git repository (never .git/). It never leaves the folder you started in and caps its answers. Prefer it to shell commands for searching:\n",
    "- A file by (part of) its name, typos allowed: find_files (`query`), not find, fd, ls -R or `git ls-files | grep`.\n",
    "- Text in files: search_text (`query` is literal; `regex: true` for a pattern; `case_sensitive`; `globs` such as `*.ts` or `!*.test.ts`), not grep -r, rg or git grep.\n",
    "- Code by its shape (definitions, calls, imports, JSX): search_structural with an ast-grep `pattern` and `lang`, e.g. `useEffect($A, [])` (tsx), `def $F($$$ARGS): $$$BODY` (python), `fn $F($$$ARGS) -> $R { $$$BODY }` (rust), not a regex grep.\n",
    "- The files under a folder: list_files, not ls -R or `find . -type f`.\n",
    "- What an earlier Claude Code, Codex or OpenCode conversation in this project said or did: search_sessions (`query`), not grep over ~/.claude or ~/.codex.\n",
    "Every tool runs in the folder you started in, or a folder under it passed as `path` (`folder` is its older name); search_text and search_structural also take one file as `path`. Paths in answers are relative to the folder searched; a file searched alone is answered by its name.\n",
    "Answers are already capped (`limit` counts files, `max_lines` lines), so do not pipe them through head. Once a search gives you the file and line, read just that part with your file-reading tool (an offset and a limit), not cat, head or sed -n. If your tools load on demand, load these before your first search."
);

/// The contract, embedded.
const CONTRACT: &str = include_str!("../../contract/kit.json");

const INVALID_REQUEST: i64 = -32600;
const PARSE_ERROR: i64 = -32700;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

struct Rpc {
    code: i64,
    message: String,
}

impl Rpc {
    fn new(code: i64, message: impl Into<String>) -> Rpc {
        Rpc {
            code,
            message: message.into(),
        }
    }
}

struct Session {
    /// `tools/list`'s tools, from the contract.
    tools: Vec<Value>,
    /// The folder the agent runs in; None when it could not be read.
    cwd: Option<PathBuf>,
}

impl Session {
    fn new() -> Session {
        let contract: Value = serde_json::from_str(CONTRACT).unwrap_or(Value::Null);
        let tools = contract
            .get("tools")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .map(|tool| {
                        json!({
                            "name": tool.get("name"),
                            "description": tool.get("description"),
                            "inputSchema": tool.get("inputSchema"),
                            "annotations": {
                                "readOnlyHint": tool.get("readOnly").and_then(Value::as_bool).unwrap_or(true),
                                "destructiveHint": false,
                                "openWorldHint": false,
                            },
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Session {
            tools,
            cwd: std::env::current_dir().ok(),
        }
    }

    /// The answer to one message, or None for a notification.
    fn handle(&self, message: &Value) -> Option<Value> {
        let Some(object) = message.as_object() else {
            return Some(error(Value::Null, INVALID_REQUEST, "not a JSON-RPC request"));
        };
        let id = object.get("id").cloned();
        let method = object.get("method").and_then(Value::as_str);
        let (Some(id), Some(method)) = (id, method) else {
            // A notification (no id) is never answered; an id with no method is.
            return object
                .get("id")
                .map(|id| error(id.clone(), INVALID_REQUEST, "missing method"));
        };
        let params = object.get("params").cloned().unwrap_or(Value::Null);
        Some(match self.dispatch(method, &params) {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(rpc) => error(id, rpc.code, &rpc.message),
        })
    }

    fn dispatch(&self, method: &str, params: &Value) -> Result<Value, Rpc> {
        match method {
            "initialize" => {
                let asked = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let version = if KNOWN_PROTOCOL_VERSIONS.contains(&asked) {
                    asked
                } else {
                    LATEST_PROTOCOL_VERSION
                };
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
                    "instructions": INSTRUCTIONS,
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": self.tools })),
            "tools/call" => self.call(params),
            "resources/list" => Ok(json!({ "resources": [] })),
            "prompts/list" => Ok(json!({ "prompts": [] })),
            _ => Err(Rpc::new(METHOD_NOT_FOUND, format!("unknown method {method}"))),
        }
    }

    fn call(&self, params: &Value) -> Result<Value, Rpc> {
        let name = params.get("name").and_then(Value::as_str);
        let known = name.filter(|name| {
            self.tools
                .iter()
                .any(|tool| tool.get("name").and_then(Value::as_str) == Some(*name))
        });
        let Some(name) = known else {
            let shown = match params.get("name") {
                Some(Value::String(text)) => text.clone(),
                Some(Value::Null) | None => "undefined".to_owned(),
                Some(other) => other.to_string(),
            };
            return Err(Rpc::new(INVALID_PARAMS, format!("unknown tool {shown}")));
        };
        Ok(match self.run_tool(name, params.get("arguments")) {
            Ok((result, note)) => text(&answer_of(name, result, note), false),
            Err(failure) => {
                if failure.code == engine::types::KitErrorCode::Internal {
                    eprintln!("lanework-kit mcp: {name} failed: {}", failure.message);
                }
                text(
                    &json!({ "error": failure.code.as_str(), "message": failure.message }),
                    true,
                )
            }
        })
    }

    /// Run `name` socket-first, then in-process: its result, and the
    /// engine's note. A call the app's contract cannot answer
    /// (`engine::for_socket`) runs in-process only.
    fn run_tool(&self, name: &str, arguments: Option<&Value>) -> Result<(Value, Option<String>), KitError> {
        let mut args = match arguments {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(map)) => map.clone(),
            Some(_) => return Err(KitError::invalid("arguments must be an object.")),
        };
        let cwd = self
            .cwd
            .clone()
            .ok_or_else(|| KitError::internal("The working folder cannot be read."))?;
        let cwd_text = cwd.to_string_lossy().into_owned();
        // The app (contract 1.1.0) does not fix a pattern: fixed here first.
        let fixed_note = engine::fix_ahead(name, &mut args, &cwd);
        let asked = match engine::for_socket(name, &args, &cwd) {
            Some(mut wire) => {
                wire.insert("cwd".into(), Value::String(cwd_text.clone()));
                // Narrows the app's scope (the cwd's whole project) to the cwd.
                wire.insert("roots".into(), json!([cwd_text]));
                socket::call(name, wire)
            }
            None => socket::Answer::Absent,
        };
        match asked {
            socket::Answer::Result(Some(raw)) => {
                if let Ok(result) = serde_json::from_str::<Value>(raw.get()) {
                    return Ok((result, fixed_note));
                }
            }
            socket::Answer::Failed(failure) => return Err(failure),
            // No result, or one that is not JSON: run it here.
            socket::Answer::Result(None) | socket::Answer::Absent => {}
        }
        let ctx = engine::Context {
            roots: vec![cwd.clone()],
            home: crate::paths::home_dir().unwrap_or_default(),
            cwd,
        };
        engine::run_noted(name, &Value::Object(args), &ctx)
            .map(|answered| (answered.result, fixed_note.or(answered.note)))
    }
}

/// A core result in the answer shape the contract names, with the
/// engine's note at the top level when there is one.
fn answer_of(name: &str, result: Value, note: Option<String>) -> Value {
    let mut answer = match name {
        "find_files" => json!({ "files": result }),
        "search_text" | "search_structural" => json!({ "matches": result }),
        "search_sessions" => json!({ "sessions": result }),
        _ => result,
    };
    if let (Some(note), Some(object)) = (note, answer.as_object_mut()) {
        object.insert("note".into(), Value::String(note));
    }
    answer
}

fn text(value: &Value, is_error: bool) -> Value {
    let mut out = json!({
        "content": [{ "type": "text", "text": crate::jsonjs::stringify(value) }],
    });
    if is_error {
        out["isError"] = Value::Bool(true);
    }
    out
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// The line to write back for one line of input, if any.
fn handle_line(session: &Session, line: &str) -> Option<String> {
    if line.trim().is_empty() {
        return None;
    }
    let Ok(parsed) = serde_json::from_str::<Value>(line) else {
        return Some(error(Value::Null, PARSE_ERROR, "parse error").to_string());
    };
    // A JSON-RPC batch (2024-11-05 clients may send one).
    if let Value::Array(messages) = parsed {
        let answers: Vec<Value> = messages
            .iter()
            .filter_map(|message| session.handle(message))
            .collect();
        return (!answers.is_empty()).then(|| Value::Array(answers).to_string());
    }
    session.handle(&parsed).map(|answer| answer.to_string())
}

/// Serve MCP on stdin/stdout until stdin closes. Returns the exit code.
pub fn run() -> i32 {
    let session = Session::new();
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let stdout = std::io::stdout();
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        match input.read_until(b'\n', &mut buf) {
            Ok(0) => return 0,
            Ok(_) => {}
            Err(error) => {
                eprintln!("lanework-kit mcp: {error}");
                return 1;
            }
        }
        let line = String::from_utf8_lossy(&buf);
        let Some(answer) = handle_line(&session, line.trim_end_matches(['\n', '\r'])) else {
            continue;
        };
        let mut out = stdout.lock();
        if writeln!(out, "{answer}").and_then(|()| out.flush()).is_err() {
            return 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_note_goes_beside_the_matches() {
        let matches = json!([{ "path": "a.py", "lines": [], "truncated": false }]);
        assert_eq!(
            answer_of("search_structural", matches.clone(), None),
            json!({ "matches": matches })
        );
        assert_eq!(
            answer_of("search_structural", matches.clone(), Some("searched `x`".into())),
            json!({ "matches": matches, "note": "searched `x`" })
        );
    }

    #[test]
    fn every_tool_takes_path_and_folder() {
        let session = Session::new();
        assert_eq!(session.tools.len(), 5);
        for tool in &session.tools {
            let properties = &tool["inputSchema"]["properties"];
            assert!(properties.get("path").is_some(), "{}", tool["name"]);
            assert!(properties.get("folder").is_some(), "{}", tool["name"]);
        }
        let sg = session.tools.iter().find(|t| t["name"] == "search_structural").unwrap();
        for key in ["globs", "max_lines"] {
            assert!(sg["inputSchema"]["properties"].get(key).is_some(), "{key}");
        }
    }
}
