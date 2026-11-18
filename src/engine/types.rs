//! The kit's contract in Rust: its tools, their limits, the answers they
//! give and the error every failure is. Mirrors
//! `packages/protocol/src/tools/kit.ts` (`contract/kit.json`) and
//! `electron/kit/errors.ts`.
//!
//! NOTE: Every path an answer carries is relative to the folder searched,
//! `/`-separated. Ranges are `[start, end)` UTF-16 offsets.

use serde::Serialize;
use serde_json::{json, Value};
use std::fmt;
use std::path::PathBuf;

// ── Tools and limits ────────────────────────────────────────────────────────

/// A kit tool, as the socket, the MCP server and `lwk` name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    FindFiles,
    SearchText,
    SearchStructural,
    ListFiles,
    SearchSessions,
}

impl Tool {
    /// Every tool, in the order `tools/list` advertises them.
    pub const ALL: [Tool; 5] = [
        Tool::FindFiles,
        Tool::SearchText,
        Tool::SearchStructural,
        Tool::ListFiles,
        Tool::SearchSessions,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Tool::FindFiles => "find_files",
            Tool::SearchText => "search_text",
            Tool::SearchStructural => "search_structural",
            Tool::ListFiles => "list_files",
            Tool::SearchSessions => "search_sessions",
        }
    }

    pub fn parse(name: &str) -> Option<Tool> {
        Tool::ALL.into_iter().find(|tool| tool.name() == name)
    }

    /// Its `limit`: `KIT_TOOL_LIMITS[tool]`.
    pub fn limits(self) -> Limit {
        match self {
            Tool::FindFiles => FIND_FILES_LIMIT,
            Tool::SearchText => SEARCH_TEXT_LIMIT,
            Tool::SearchStructural => SEARCH_STRUCTURAL_LIMIT,
            Tool::ListFiles => LIST_FILES_LIMIT,
            Tool::SearchSessions => SEARCH_SESSIONS_LIMIT,
        }
    }
}

/// A tool's `limit`: what it is when omitted, and the most it may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub default: usize,
    pub max: usize,
}

pub const FIND_FILES_LIMIT: Limit = Limit { default: 50, max: 200 };
pub const SEARCH_TEXT_LIMIT: Limit = Limit { default: 50, max: 200 };
pub const SEARCH_STRUCTURAL_LIMIT: Limit = Limit { default: 50, max: 200 };
pub const LIST_FILES_LIMIT: Limit = Limit {
    default: 1000,
    max: 50_000,
};
pub const SEARCH_SESSIONS_LIMIT: Limit = Limit { default: 20, max: 50 };

/// The longest query or pattern taken, in UTF-16 units (JS `length`).
pub const QUERY_MAX: usize = 1_000;
/// The most globs one search takes.
pub const GLOBS_MAX: usize = 50;
/// The longest glob taken, in UTF-16 units.
pub const GLOB_MAX: usize = 500;
/// The longest folder path taken, in UTF-16 units.
pub const FOLDER_MAX: usize = 4_096;

/// Languages a structural pattern can be written in (`STRUCTURAL_LANGS`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuralLang {
    Ts,
    Tsx,
    Js,
    Jsx,
    Json,
    Css,
    Html,
    Python,
    Rust,
    Go,
}

impl StructuralLang {
    pub const ALL: [StructuralLang; 10] = [
        StructuralLang::Ts,
        StructuralLang::Tsx,
        StructuralLang::Js,
        StructuralLang::Jsx,
        StructuralLang::Json,
        StructuralLang::Css,
        StructuralLang::Html,
        StructuralLang::Python,
        StructuralLang::Rust,
        StructuralLang::Go,
    ];

    pub fn name(self) -> &'static str {
        match self {
            StructuralLang::Ts => "ts",
            StructuralLang::Tsx => "tsx",
            StructuralLang::Js => "js",
            StructuralLang::Jsx => "jsx",
            StructuralLang::Json => "json",
            StructuralLang::Css => "css",
            StructuralLang::Html => "html",
            StructuralLang::Python => "python",
            StructuralLang::Rust => "rust",
            StructuralLang::Go => "go",
        }
    }

    pub fn parse(name: &str) -> Option<StructuralLang> {
        StructuralLang::ALL
            .into_iter()
            .find(|lang| lang.name() == name)
    }
}

// ── Inputs the engines take (already checked by the dispatcher) ────────────

/// `search_text`'s input, checked.
// Read once the stub streams land (text.rs, sessions/, mcp/).
#[allow(dead_code)]
#[derive(Debug, Clone, Default)]
pub struct TextOptions {
    pub query: String,
    /// `query` is a regex (ripgrep's syntax); a literal otherwise.
    pub regex: bool,
    /// Match case; None (omitted) means ignore case.
    pub case_sensitive: Option<bool>,
    /// ripgrep globs; a leading `!` excludes.
    pub globs: Vec<String>,
    /// The most files answered.
    pub max_files: usize,
}

/// `search_sessions`'s input, checked.
// Read once the stub streams land (text.rs, sessions/, mcp/).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// Only sessions run in one of these folders, as given and as real
    /// paths (a transcript keeps the folder as the CLI saw it).
    pub folders: Vec<String>,
    /// The most sessions answered.
    pub limit: usize,
    /// The home folder the harnesses' transcripts are under.
    pub home: PathBuf,
}

// ── Answers ─────────────────────────────────────────────────────────────────

/// `[start, end)` UTF-16 offsets.
pub type Range = [usize; 2];

/// A file whose path matches a fuzzy query.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FileHit {
    /// Relative to the searched folder, `/`-separated.
    pub path: String,
    /// Higher is a better match. Only comparable within one answer.
    pub score: f64,
    /// The matched characters, offsets into `path`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ranges: Option<Vec<Range>>,
}

/// One matching line.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct MatchLine {
    /// 1-based line number.
    pub line: u64,
    /// The line, without its newline; a very long line is cut to a window
    /// around its first match.
    pub text: String,
    /// Each match, offsets into `text`.
    pub ranges: Vec<Range>,
}

/// A file with matches.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FileMatch {
    /// Relative to the searched folder, `/`-separated.
    pub path: String,
    pub lines: Vec<MatchLine>,
    /// The search stopped in or right after this file (a match or file
    /// limit, or the timeout): there may be more here or after it.
    pub truncated: bool,
}

/// The agent harnesses whose transcripts `search_sessions` reads.
// Read once the stub streams land (text.rs, sessions/, mcp/).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SessionHarness {
    Claude,
    Codex,
    Opencode,
}

/// A session transcript with matches.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SessionMatch {
    pub harness: SessionHarness,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    /// The session's title, or its first prompt.
    pub title: String,
    /// When the session was last written, ISO 8601.
    pub at: String,
    /// The transcript, relative to the harness's own folder.
    pub file: String,
    pub lines: Vec<MatchLine>,
}

/// `list_files`'s answer.
#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct FileListing {
    /// Relative to the folder, `/`-separated, sorted.
    pub files: Vec<String>,
    /// More files were there than `limit`, or the listing ran out of time.
    pub truncated: bool,
}

// ── Errors ──────────────────────────────────────────────────────────────────

/// Why a kit call failed (`KitErrorCode`).
///
/// - `invalid`: the params are wrong.
/// - `scope`: the folder is outside the allowed roots.
/// - `not_found`: the folder does not exist.
/// - `unavailable`: a tool this one needs is missing here.
/// - `timeout`: the search ran out of time before any match.
/// - `aborted`: the caller went away.
/// - `busy`: too many calls at once (the socket only).
/// - `internal`: anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KitErrorCode {
    Invalid,
    Scope,
    NotFound,
    Unavailable,
    Timeout,
    Aborted,
    Busy,
    Internal,
}

impl KitErrorCode {
    pub const ALL: [KitErrorCode; 8] = [
        KitErrorCode::Invalid,
        KitErrorCode::Scope,
        KitErrorCode::NotFound,
        KitErrorCode::Unavailable,
        KitErrorCode::Timeout,
        KitErrorCode::Aborted,
        KitErrorCode::Busy,
        KitErrorCode::Internal,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            KitErrorCode::Invalid => "invalid",
            KitErrorCode::Scope => "scope",
            KitErrorCode::NotFound => "not_found",
            KitErrorCode::Unavailable => "unavailable",
            KitErrorCode::Timeout => "timeout",
            KitErrorCode::Aborted => "aborted",
            KitErrorCode::Busy => "busy",
            KitErrorCode::Internal => "internal",
        }
    }

    pub fn parse(code: &str) -> Option<KitErrorCode> {
        KitErrorCode::ALL
            .into_iter()
            .find(|known| known.as_str() == code)
    }
}

/// A failed kit call: a code and a message for a person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KitError {
    pub code: KitErrorCode,
    pub message: String,
}

impl KitError {
    pub fn new(code: KitErrorCode, message: impl Into<String>) -> KitError {
        KitError {
            code,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> KitError {
        KitError::new(KitErrorCode::Invalid, message)
    }

    pub fn scope(message: impl Into<String>) -> KitError {
        KitError::new(KitErrorCode::Scope, message)
    }

    pub fn not_found(message: impl Into<String>) -> KitError {
        KitError::new(KitErrorCode::NotFound, message)
    }

    #[allow(dead_code)] // used by text.rs and mcp/ once they land
    pub fn timeout() -> KitError {
        KitError::new(
            KitErrorCode::Timeout,
            "The search took too long. Try a narrower query or folder.",
        )
    }

    pub fn internal(message: impl Into<String>) -> KitError {
        KitError::new(KitErrorCode::Internal, message)
    }

    /// `lwk`'s exit code for this failure: 2, as for any usage, scope or
    /// other error (0 is found, 1 nothing found).
    pub fn exit_code(&self) -> i32 {
        2
    }

    /// The error as the socket and the MCP server send it.
    #[allow(dead_code)] // used by mcp/ once it lands
    pub fn to_json(&self) -> Value {
        json!({ "code": self.code.as_str(), "message": self.message })
    }
}

impl fmt::Display for KitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for KitError {}

impl From<std::io::Error> for KitError {
    fn from(error: std::io::Error) -> KitError {
        KitError::internal(error.to_string())
    }
}
