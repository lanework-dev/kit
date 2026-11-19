//! `lwk`: Lanework's search kit for coding agents, on the command line
//! (FE-152).
//!
//! ```text
//! lwk find QUERY [FOLDER]        find_files         (kit.rs)
//! lwk grep QUERY [PATH]          search_text
//! lwk sg PATTERN [PATH]          search_structural
//! lwk ls [FOLDER]                list_files
//! lwk sessions QUERY [FOLDER]    search_sessions
//! lwk rewrite                    Claude Code's PreToolUse hook (rewrite.rs)
//! lwk mcp                        the kit as an MCP server (mcp/)
//! lwk serve [--idle-exit SECS]   the kit over stdin/stdout, long-lived (serve/)
//! lwk hook on|off|status         that hook in ~/.claude/settings.json (hook.rs)
//! lwk help | version
//! lwk ANYTHING ELSE ...          exec()s ANYTHING ELSE (passthrough.rs)
//! ```
//!
//! - The kit's tools ask the running app first, over its socket (`kit.rs`):
//!   the warm fff index. With no app to answer, lwk runs the same tools
//!   in-process (`engine/`), in the working folder.
//! - `lwk mcp` serves those tools as an MCP server over stdio (`mcp/`).
//! - `lwk serve` answers the same calls as JSON lines on stdin and stdout
//!   (`serve/`, `contract/serve.md`): many at once, the engine kept warm.
//! - `lwk hook` turns `lwk rewrite` on or off in Claude Code (`hook.rs`).
//! - Anything else is the command it names: lwk exec()s it, so it IS that
//!   command from then on (same pid, terminal, signals and exit code).
//!
//! The crate is `lanework-kit`; its one binary is `lwk`.

mod engine;
mod hook;
mod jsonjs;
mod kit;
mod mcp;
mod passthrough;
mod paths;
mod rewrite;
mod serve;
mod socket;

use std::ffi::OsString;
use std::io::Write;
use std::process::exit;

/// The version printed: `LWK_VERSION` when a build stamps one, else the
/// crate's.
const VERSION: &str = match option_env!("LWK_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

const HELP: &str = "lwk: Lanework's search kit for coding agents.

Usage:
  lwk find QUERY [FOLDER]       Fuzzy-find files by path, best first
  lwk grep QUERY [PATH]         Search file contents (a literal unless -E)
  lwk sg PATTERN [PATH]         Search code by syntax (ast-grep pattern)
  lwk ls [FOLDER]               List the files rg would search
  lwk sessions QUERY [FOLDER]   Search Claude Code, Codex and OpenCode sessions
  lwk hook on|off|status        Claude Code hook that runs plain grep -r as lwk
  lwk mcp                       Serve the kit's tools as an MCP server (stdio)
  lwk serve [--idle-exit SECS]  Answer kit calls as JSON lines on stdin/stdout
  lwk COMMAND [ARGS...]         Run any other command unchanged

Options:
  --json                Print the tool's JSON answer
  --limit N             How many results (files, paths or sessions). grep and
                        sg: files, each with all its matching lines, up to
                        --max-lines lines in total
  --max-lines N         grep, sg: matching lines in total, over all files
                        (default and most 2000)
  --git                 find: rank files with uncommitted changes and recently
                        committed files higher, as an editor's quick-open does
  -E, --regex           grep: QUERY is a regex (ripgrep syntax)
  -F, --fixed-strings   grep: QUERY is a literal (the default)
  -i, --ignore-case     grep: ignore case (the default)
  -s, --case-sensitive  grep: match case
  -g, --glob GLOB       grep, sg: only files matching GLOB; a leading ! excludes
  --lang LANG           sg: the pattern's language

PATH is a folder or, for grep and sg, one file, whose matches are printed
under the path as typed. sg runs a declaration pattern that lacks its body
(def $F($$$A), fn $N($$$A), impl $T, function $F($$$A)) with the body
added, and says so on stderr.

.gitignore is respected. In a git repository find lists hidden files and
folders too (never .git/); the other commands skip them. FOLDER and PATH
default to the working folder, and must be inside it unless the Lanework app
is running and the folder belongs to the same project (a file, --max-lines
and sg -g run without the app); / and your home folder itself are never
searched. Exit code: 0 found, 1 nothing found, 2 error.

find and ls keep the folder's file list in a cache ($XDG_CACHE_HOME/lwk,
else ~/Library/Caches/lwk on macOS or ~/.cache/lwk), checked against the
folders' and ignore files' change times on every call. LWK_NO_CACHE=1
turns it off; deleting the folder is always safe.

The tools ask the running Lanework app first, over its socket
($LANEWORK_HOME/run/kit.sock), and run in-process when it does not answer.
LWK_NO_SOCKET=1 (any value but empty or 0) never asks it: find, grep, sg, ls,
sessions and lwk mcp all run in-process.
";

/// Write `text` to stdout whole. A closed pipe (`lwk ls | head`) is not an
/// error worth a word: what was wanted has been read.
pub fn out(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

/// The kit contract's version (`contract/kit.json`), which the app pins.
fn contract_version() -> String {
    let contract: serde_json::Value =
        serde_json::from_str(include_str!("../contract/kit.json")).unwrap_or_default();
    contract["version"].as_str().unwrap_or("unknown").to_owned()
}

fn main() {
    let mut argv = std::env::args_os().skip(1);
    let Some(first) = argv.next() else {
        out(HELP);
        exit(0);
    };
    let rest: Vec<OsString> = argv.collect();
    let code = match first.to_str() {
        Some("help" | "--help" | "-h") => {
            out(HELP);
            0
        }
        Some("version" | "--version" | "-V") => {
            if rest.iter().any(|arg| arg == "--json") {
                out(&format!(
                    "{}\n",
                    serde_json::json!({ "lwk": VERSION, "contract": contract_version() })
                ));
            } else {
                out(&format!("lwk {VERSION} (contract {})\n", contract_version()));
            }
            0
        }
        Some(command @ ("find" | "grep" | "sg" | "ls" | "sessions")) => kit::run(command, &rest),
        Some("rewrite") => rewrite::run_hook(),
        Some("mcp") => mcp::run(),
        Some("serve") => serve::run(&rest),
        Some("hook") => {
            let args: Vec<String> = rest
                .iter()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            hook::run(&args)
        }
        // Returns only when the command could not be run.
        _ => passthrough::exec(&first, &rest),
    };
    exit(code);
}
