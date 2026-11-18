//! OpenCode's sessions, from its own database (`opencode.db`, both schemas:
//! `session_v2` with `session_message`, and the older `session` with
//! `part`). A sub-agent's session counts as its parent's.
//!
//! NOTE: The database is read through the `sqlite3` command, opened
//! read-only (`-readonly`): lwk has no SQLite library in it. With no
//! `sqlite3` on the PATH, or a database that is missing, locked or of a
//! schema this does not know, OpenCode has no sessions here, as in the app.
//! Every value reaches a query as a quoted SQL string, never as SQL.
//!
//! Sessions are walked newest first, a few at a time, each one's rows read
//! through its `session_id` index, until `limit` sessions said it: one query
//! over every row, sorted by session, took seconds on a 300 MB database for
//! a common word.

use super::js::{self, Finder};
use super::jsonl::{tidy, tidy_max};
use super::scan::LINES_PER_SESSION;
use super::snippet::{said, snippet_from};
use super::util::iso8601;
use super::Scope;
use crate::engine::types::{MatchLine, SessionHarness, SessionMatch};
use crate::paths;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Rows read per OpenCode session, at most: a few more than are kept.
const ROWS: usize = 12;
/// The sessions whose rows the first query reads; each next one reads twice
/// as many, up to [`CHUNK_MAX`].
const CHUNK_FIRST: usize = 16;
const CHUNK_MAX: usize = 256;
/// The most characters of a session's title the rail is given.
const TITLE_MAX: usize = 200;

/// Where OpenCode keeps its data: `$OPENCODE_DATA_DIR`, else
/// `$XDG_DATA_HOME/<app>`, else `~/.local/share/<app>`, `<app>` being
/// `$OPENCODE_APPNAME` or "opencode".
///
/// The environment counts only for the real home: a test points `home`
/// somewhere else and must not read the machine's sessions through it.
fn data_dir(home: &Path) -> PathBuf {
    let real = paths::home_dir().is_some_and(|real| real == home);
    let var = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    if real {
        if let Some(dir) = var("OPENCODE_DATA_DIR") {
            return PathBuf::from(dir);
        }
    }
    let app = real
        .then(|| var("OPENCODE_APPNAME"))
        .flatten()
        .map(|name| name.to_string_lossy().trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "opencode".to_owned());
    if real {
        if let Some(xdg) = var("XDG_DATA_HOME") {
            return PathBuf::from(xdg).join(app);
        }
    }
    home.join(".local").join("share").join(app)
}

/// One of OpenCode's session tables, and where its words are.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Table {
    V2,
    Legacy,
}

impl Table {
    fn name(self) -> &'static str {
        match self {
            Table::V2 => "session_v2",
            Table::Legacy => "session",
        }
    }
}

/// The matching rows (`seq`, `data`) of each session, by table and id.
type Rows = HashMap<(Table, String), Vec<(u64, String)>>;

/// A session of one table, as the walk sees it.
struct Row {
    table: Table,
    id: String,
    parent: Option<String>,
    updated: f64,
}

/// A SQL string literal for `text`.
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// The rows of `sql` against `db`, read-only; `None` when it failed.
fn query(db: &Path, sql: &str, deadline: Instant) -> Option<Vec<Value>> {
    let mut child = Command::new("sqlite3")
        .args(["-readonly", "-batch", "-json"])
        .arg(db)
        .arg(sql)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).ok().map(|_| bytes)
    });
    let mut pause = Duration::from_micros(200);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(5));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let bytes = reader.join().ok().flatten()?;
    if !status.success() {
        return None;
    }
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Some(Vec::new());
    }
    match serde_json::from_slice(&bytes).ok()? {
        Value::Array(rows) => Some(rows),
        _ => None,
    }
}

fn text<'a>(row: &'a Value, key: &str) -> Option<&'a str> {
    row.get(key).and_then(Value::as_str)
}

/// A title OpenCode gave, unless it is its placeholder.
fn named(title: Option<&Value>) -> Option<String> {
    let title = title?.as_str()?;
    if placeholder(title) {
        return None;
    }
    Some(tidy_max(title, TITLE_MAX)).filter(|title| !title.is_empty())
}

/// `/^(New|Child) session - \d{4}-\d{2}-\d{2}T/`: the title OpenCode gives a
/// session before it names it.
fn placeholder(title: &str) -> bool {
    let Some(rest) = title
        .strip_prefix("New session - ")
        .or_else(|| title.strip_prefix("Child session - "))
    else {
        return false;
    };
    let shape = b"dddd-dd-ddT";
    let bytes = rest.as_bytes();
    bytes.len() >= shape.len()
        && shape.iter().zip(bytes).all(|(want, got)| match want {
            b'd' => got.is_ascii_digit(),
            _ => got == want,
        })
}

/// The text of a `session_message` row of type `user`.
fn prompt_text(data: &str) -> Option<String> {
    let value: Value = serde_json::from_str(data).ok()?;
    let text = value.get("text")?.as_str()?;
    (!js::trim(text).is_empty()).then(|| text.to_owned())
}

/// The sessions that said the query, from the database in `home`'s data
/// folder.
pub fn search(
    home: &Path,
    query_text: &str,
    finder: &Finder,
    scope: &Scope,
    limit: usize,
    deadline: Instant,
) -> Vec<SessionMatch> {
    let db = data_dir(home).join("opencode.db");
    if !db.is_file() {
        return Vec::new();
    }
    run(&db, query_text, finder, scope, limit, deadline).unwrap_or_default()
}

fn run(
    db: &Path,
    query_text: &str,
    finder: &Finder,
    scope: &Scope,
    limit: usize,
    deadline: Instant,
) -> Option<Vec<SessionMatch>> {
    let tables: HashSet<String> = query(
        db,
        "SELECT name FROM sqlite_master WHERE type = 'table'",
        deadline,
    )?
    .iter()
    .filter_map(|row| text(row, "name").map(str::to_owned))
    .collect();
    let mut sources: Vec<Table> = Vec::new();
    if tables.contains("session_v2") && tables.contains("session_message") {
        sources.push(Table::V2);
    }
    if tables.contains("session") && tables.contains("part") {
        sources.push(Table::Legacy);
    }

    // Every session, for a title; those in scope, to walk.
    let mut titles: HashMap<(Table, String), Value> = HashMap::new();
    let mut walk: Vec<Row> = Vec::new();
    for &table in &sources {
        let sql = format!(
            "SELECT id, parent_id AS parent, title, time_updated AS updated, directory FROM {}",
            table.name()
        );
        for row in query(db, &sql, deadline)? {
            let Some(id) = text(&row, "id") else { continue };
            titles.insert(
                (table, id.to_owned()),
                row.get("title").cloned().unwrap_or(Value::Null),
            );
            if !text(&row, "directory").is_some_and(|directory| scope.contains(directory)) {
                continue;
            }
            walk.push(Row {
                table,
                id: id.to_owned(),
                parent: text(&row, "parent").map(str::to_owned),
                updated: row.get("updated").and_then(Value::as_f64).unwrap_or(0.0),
            });
        }
    }
    walk.sort_by(|a, b| b.updated.total_cmp(&a.updated));

    // LIKE ignores the case of ASCII letters only; the snippet's own match
    // is what decides.
    let pattern = format!(
        "%{}%",
        query_text
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    );
    let test = format!("data LIKE {} ESCAPE '\\'", quote(&pattern));

    let mut out: Vec<(Table, SessionMatch)> = Vec::new();
    let mut at = 0;
    let mut chunk = CHUNK_FIRST;
    while at < walk.len() && out.len() < limit && Instant::now() < deadline {
        let upto = (at + chunk).min(walk.len());
        let rows = fetch(db, &walk[at..upto], &test, deadline)?;
        for session in &walk[at..upto] {
            if out.len() >= limit || Instant::now() > deadline {
                break;
            }
            // A sub-agent's session counts as its parent's.
            let id = session.parent.clone().unwrap_or_else(|| session.id.clone());
            let found = out.iter().position(|(_, m)| m.session_id == id);
            let mut kept: Vec<MatchLine> = match found {
                Some(at) if out[at].1.lines.len() >= LINES_PER_SESSION => continue,
                Some(at) => std::mem::take(&mut out[at].1.lines),
                None => Vec::new(),
            };
            for (seq, data) in rows.get(&(session.table, session.id.clone())).into_iter().flatten()
            {
                if kept.len() >= LINES_PER_SESSION {
                    break;
                }
                let Ok(value) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                let mut texts = Vec::new();
                said(&value, &mut texts, 0);
                if let Some(snippet) = snippet_from(&texts, *seq, finder) {
                    if !kept.iter().any(|line| line.text == snippet.text) {
                        kept.push(snippet);
                    }
                }
            }
            match found {
                Some(at) => out[at].1.lines = kept,
                None if kept.is_empty() => {}
                None => out.push((
                    session.table,
                    SessionMatch {
                        harness: SessionHarness::Opencode,
                        session_id: id,
                        title: String::new(),
                        at: iso8601(session.updated.max(0.0) as u64),
                        file: "opencode.db".to_owned(),
                        lines: kept,
                    },
                )),
            }
        }
        at = upto;
        chunk = (chunk * 2).min(CHUNK_MAX);
    }
    titled(db, &mut out, &titles, deadline);
    Some(out.into_iter().map(|(_, session)| session).collect())
}

/// The matching rows (`seq`, `data`) of each of `sessions`, up to [`ROWS`].
fn fetch(
    db: &Path,
    sessions: &[Row],
    test: &str,
    deadline: Instant,
) -> Option<Rows> {
    let mut rows: Rows = HashMap::new();
    for table in [Table::V2, Table::Legacy] {
        let ids: Vec<String> = sessions
            .iter()
            .filter(|s| s.table == table)
            .map(|s| quote(&s.id))
            .collect();
        if ids.is_empty() {
            continue;
        }
        let ids = ids.join(", ");
        let sql = match table {
            Table::V2 => format!(
                "SELECT sid, seq, data FROM (
                   SELECT session_id AS sid, seq, data,
                          row_number() OVER (PARTITION BY session_id ORDER BY seq) AS rn
                     FROM session_message
                    WHERE session_id IN ({ids}) AND type IN ('user', 'assistant')
                      AND {test})
                  WHERE rn <= {ROWS} ORDER BY sid, rn"
            ),
            Table::Legacy => format!(
                "SELECT sid, 0 AS seq, data FROM (
                   SELECT session_id AS sid, data,
                          row_number() OVER (PARTITION BY session_id ORDER BY time_created) AS rn
                     FROM part
                    WHERE session_id IN ({ids}) AND {test})
                  WHERE rn <= {ROWS} ORDER BY sid, rn"
            ),
        };
        for row in query(db, &sql, deadline)? {
            let (Some(sid), Some(data)) = (text(&row, "sid"), text(&row, "data")) else {
                continue;
            };
            let seq = row.get("seq").and_then(Value::as_u64).unwrap_or(0);
            rows.entry((table, sid.to_owned()))
                .or_default()
                .push((seq, data.to_owned()));
        }
    }
    Some(rows)
}

/// What the rail calls each session: its title, else its first prompt.
fn titled(
    db: &Path,
    out: &mut [(Table, SessionMatch)],
    titles: &HashMap<(Table, String), Value>,
    deadline: Instant,
) {
    let mut unnamed: Vec<String> = Vec::new();
    for (table, session) in out.iter_mut() {
        match named(titles.get(&(*table, session.session_id.clone()))) {
            Some(title) => session.title = title,
            None if *table == Table::V2 => unnamed.push(session.session_id.clone()),
            None => {}
        }
    }
    let mut prompts: HashMap<String, String> = HashMap::new();
    if !unnamed.is_empty() {
        let ids: Vec<String> = unnamed.iter().map(|id| quote(id)).collect();
        let sql = format!(
            "SELECT sid, data FROM (
               SELECT session_id AS sid, data,
                      row_number() OVER (PARTITION BY session_id ORDER BY seq) AS rn
                 FROM session_message
                WHERE session_id IN ({}) AND type = 'user')
              WHERE rn = 1",
            ids.join(", ")
        );
        for row in query(db, &sql, deadline).unwrap_or_default() {
            if let (Some(sid), Some(data)) = (text(&row, "sid"), text(&row, "data")) {
                if let Some(prompt) = prompt_text(data) {
                    prompts.insert(sid.to_owned(), tidy(&prompt));
                }
            }
        }
    }
    for (_, session) in out.iter_mut() {
        if session.title.is_empty() {
            session.title = prompts
                .get(&session.session_id)
                .filter(|prompt| !prompt.is_empty())
                .cloned()
                .unwrap_or_else(|| "OpenCode session".to_owned());
        }
    }
}
