//! Tests of the session search, against a fake home folder it is pointed at.
//! The real machine's transcripts are never read.

use super::js::{self, Finder};
use super::{prompt, search_sessions, snippet, util, Scope};
use crate::engine::types::{SessionHarness, SessionMatch, SessionOptions};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;

const ID_A: &str = "11111111-1111-4111-8111-111111111111";
const ID_B: &str = "22222222-2222-4222-8222-222222222222";
const ID_C: &str = "33333333-3333-4333-8333-333333333333";
const ID_D: &str = "44444444-4444-4444-8444-444444444444";
const ID_E: &str = "55555555-5555-4555-8555-555555555555";

/// A fresh fake home under the temp folder; `None` when the environment
/// points the CLIs somewhere else, which would leak the real machine in.
fn fake_home(name: &str) -> Option<PathBuf> {
    if std::env::var_os("CLAUDE_CONFIG_DIR").is_some() || std::env::var_os("CODEX_HOME").is_some()
    {
        return None;
    }
    let home = std::env::temp_dir().join(format!("lwk-sessions-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    Some(home)
}

fn write(path: &Path, lines: &[serde_json::Value]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let text: Vec<String> = lines.iter().map(ToString::to_string).collect();
    std::fs::write(path, format!("{}\n", text.join("\n"))).unwrap();
}

fn claude(home: &Path, dir: &str, id: &str, cwd: &str, said: &str) {
    write(
        &home.join(".claude/projects").join(dir).join(format!("{id}.jsonl")),
        &[
            json!({"type": "ai-title", "aiTitle": format!("Title {id}")}),
            json!({"type": "user", "cwd": cwd, "message": {"content": said}}),
        ],
    );
}

fn codex(home: &Path, name: &str, meta: serde_json::Value, said: &str) {
    write(
        &home.join(".codex/sessions/2026/11/18").join(format!("rollout-{name}.jsonl")),
        &[
            json!({"type": "session_meta", "payload": meta}),
            json!({"type": "event_msg", "payload": {"type": "user_message", "message": said}}),
        ],
    );
}

fn options(home: &Path, folders: &[&str]) -> SessionOptions {
    SessionOptions {
        folders: folders.iter().map(|f| (*f).to_owned()).collect(),
        limit: 20,
        home: home.to_owned(),
    }
}

fn ids(found: &[SessionMatch]) -> Vec<String> {
    let mut ids: Vec<String> = found.iter().map(|s| s.session_id.clone()).collect();
    ids.sort();
    ids
}

#[test]
fn iso8601_is_to_iso_string() {
    assert_eq!(util::iso8601(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(util::iso8601(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
    // A leap day.
    assert_eq!(util::iso8601(1_709_164_800_000), "2024-02-29T00:00:00.000Z");
}

#[test]
fn js_whitespace_and_case_rules() {
    assert_eq!(js::trim("\u{feff} a \u{a0}"), "a");
    // NEL is not JavaScript whitespace.
    assert_eq!(js::trim("\u{85}a"), "\u{85}a");
    let finder = Finder::new("kelvin σ");
    let text = js::utf16("a \u{212a}ELVIN ς b");
    assert_eq!(finder.find(&text, 0), Some(2));
    assert_eq!(finder.len(), 8);
    assert!(Finder::new("ǅ").find(&js::utf16("ǆ"), 0).is_some());
    // Dotless i has no simple fold.
    assert!(Finder::new("i").find(&js::utf16("ı"), 0).is_none());
}

#[test]
fn prompts_are_what_the_rail_calls_them() {
    assert_eq!(prompt::typed("  hi\n  there  ").as_deref(), Some("hi there"));
    assert_eq!(prompt::typed("<local-command-caveat>x"), None);
    assert_eq!(
        prompt::typed(r#"fix @"/Users/me/a b/c.csv" and @/x/y/ and @plain and a@b.c"#).as_deref(),
        Some("fix c.csv and y and @plain and a@b.c")
    );
    let wrapped = "<lanework_orchestrate>\nrules\n<user_request>\nbuild it\n</user_request>";
    assert_eq!(prompt::typed(wrapped).as_deref(), Some("/orchestrate build it"));
    let long = "x".repeat(300);
    let title = prompt::typed(&long).unwrap();
    assert_eq!(title.encode_utf16().count(), 120);
    assert!(title.ends_with('…'));
}

#[test]
fn snippets_mark_every_match_and_cut_around_the_first() {
    let finder = Finder::new("needle");
    let text = format!("{}  Needle \n\t and a NEEDLE {}", "a".repeat(300), "b".repeat(300));
    let found = snippet::snippet_from(&[text.as_str()], 7, &finder).unwrap();
    assert_eq!(found.line, 7);
    assert!(found.text.starts_with('…') && found.text.ends_with('…'));
    assert_eq!(found.ranges.len(), 2);
    let units = js::utf16(&found.text);
    for [start, end] in &found.ranges {
        assert!(finder.find(&units[*start..*end], 0).is_some());
        assert_eq!(end - start, 6);
    }
    // Whitespace is one space, so the two sit "Needle" and " and a " apart.
    assert_eq!(found.ranges[1][0] - found.ranges[0][0], 6 + " and a ".len());
}

#[test]
fn only_what_was_said_matches() {
    let finder = Finder::new("needle");
    let value = json!({
        "id": "needle", "type": "needle", "cwd": "/needle",
        "a": "<system-reminder>needle</system-reminder>",
        "b": "# AGENTS.md needle",
        "c": [{"data": "needle"}, "said a needle here"],
    });
    let mut texts = Vec::new();
    snippet::said(&value, &mut texts, 0);
    assert_eq!(texts, vec!["said a needle here"]);
    assert!(snippet::snippet_from(&texts, 1, &finder).is_some());
}

#[test]
fn scope_is_the_folder_and_what_is_inside_it() {
    let scope = Scope::new(&["/w/proj".to_owned(), "/w/proj/".to_owned(), "/private/w/proj".to_owned()]);
    assert_eq!(scope.folders.len(), 2);
    assert!(scope.contains("/w/proj"));
    assert!(scope.contains("/w/proj/sub/deeper"));
    assert!(scope.contains("/private/w/proj/x"));
    assert!(!scope.contains("/w/proj-other"));
    assert!(!scope.contains("/w/pro"));
    assert!(!scope.contains("/w"));
}

#[test]
fn claude_and_codex_sessions_in_the_folder_or_inside_it() {
    let Some(home) = fake_home("scope") else { return };
    claude(&home, "-w-proj", ID_A, "/w/proj", "the needle is here");
    claude(&home, "-w-proj-sub", ID_B, "/w/proj/sub", "a needle in a subfolder");
    // Named like a subfolder, but a sibling: `/w/proj-other`.
    claude(&home, "-w-proj-other", ID_C, "/w/proj-other", "needle in a sibling");
    // The folder name is lossy: this one is `/w/proj_x`, not inside.
    claude(&home, "-w-proj-x", ID_D, "/w/proj_x", "needle in another");
    claude(&home, "-w-elsewhere", ID_E, "/w/elsewhere", "needle elsewhere");
    codex(&home, "a", json!({"id": ID_A, "cwd": "/w/proj/deep/er"}), "codex needle");
    codex(&home, "b", json!({"id": ID_B, "cwd": "/w/proj-other"}), "codex needle other");

    let found = search_sessions("NEEDLE", &options(&home, &["/w/proj"])).unwrap();
    let claude_ids: Vec<&str> = found
        .iter()
        .filter(|s| s.harness == SessionHarness::Claude)
        .map(|s| s.session_id.as_str())
        .collect();
    assert_eq!(claude_ids.len(), 2, "{found:?}");
    assert!(claude_ids.contains(&ID_A) && claude_ids.contains(&ID_B));
    let codex_ids: Vec<&str> = found
        .iter()
        .filter(|s| s.harness == SessionHarness::Codex)
        .map(|s| s.session_id.as_str())
        .collect();
    assert_eq!(codex_ids, vec![ID_A]);

    let one = found.iter().find(|s| s.session_id == ID_B && s.harness == SessionHarness::Claude);
    let one = one.unwrap();
    assert_eq!(one.title, format!("Title {ID_B}"));
    assert_eq!(one.file, format!("projects/-w-proj-sub/{ID_B}.jsonl"));
    assert_eq!(one.lines[0].line, 2);
    assert_eq!(one.lines[0].text, "a needle in a subfolder");
    assert_eq!(one.lines[0].ranges, vec![[2, 8]]);

    // Newest first, as ISO 8601.
    assert!(found.windows(2).all(|pair| pair[0].at >= pair[1].at));
    assert!(found[0].at.ends_with('Z') && found[0].at.len() == 24);

    // A narrower folder finds only what is in it; a limit cuts the list.
    let sub = search_sessions("needle", &options(&home, &["/w/proj/sub"])).unwrap();
    assert_eq!(ids(&sub), vec![ID_B.to_owned()]);
    let mut limited = options(&home, &["/w/proj"]);
    limited.limit = 1;
    assert_eq!(search_sessions("needle", &limited).unwrap().len(), 1);
    // No folder, blank query, nothing found.
    assert!(search_sessions("needle", &options(&home, &[])).unwrap().is_empty());
    assert!(search_sessions("  \t", &options(&home, &["/w/proj"])).unwrap().is_empty());
    assert!(search_sessions("absent", &options(&home, &["/w/proj"])).unwrap().is_empty());
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn metadata_alone_is_no_match_and_a_session_keeps_three_snippets() {
    let Some(home) = fake_home("said") else { return };
    // The query is only in the `cwd` of every line, and in a meta turn.
    write(
        &home.join(".claude/projects/-w-needle").join(format!("{ID_A}.jsonl")),
        &[
            json!({"type": "user", "cwd": "/w/needle", "message": {"content": "nothing"}}),
            json!({"type": "user", "cwd": "/w/needle", "isMeta": true, "message": {"content": "needle"}}),
            json!({"type": "attachment", "cwd": "/w/needle", "message": {"content": "needle"}}),
        ],
    );
    let lines: Vec<serde_json::Value> = (0..6)
        .map(|n| json!({"type": "assistant", "cwd": "/w/needle", "message": {"content": [{"type": "text", "text": format!("needle {n}")}]}}))
        .collect();
    write(&home.join(".claude/projects/-w-needle").join(format!("{ID_B}.jsonl")), &lines);
    let found = search_sessions("needle", &options(&home, &["/w/needle"])).unwrap();
    assert_eq!(ids(&found), vec![ID_B.to_owned()]);
    assert_eq!(found[0].lines.len(), 3);
    assert_eq!(found[0].title, "Untitled conversation");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn codex_helpers_are_skipped_and_spawned_threads_join_their_conversation() {
    let Some(home) = fake_home("codex") else { return };
    codex(&home, "root", json!({"id": ID_A, "cwd": "/w/p"}), "needle from the root");
    codex(
        &home,
        "child",
        json!({"id": ID_B, "cwd": "/w/p", "session_id": ID_A,
               "source": {"subagent": {"thread_spawn": {"parent_thread_id": ID_A}}}}),
        "needle from a spawned thread",
    );
    codex(
        &home,
        "helper",
        json!({"id": ID_C, "cwd": "/w/p", "source": {"subagent": "guardian"}}),
        "needle from a helper",
    );
    write(
        &home.join(".codex/session_index.jsonl"),
        &[json!({"id": ID_A, "thread_name": "Named\n thread"})],
    );
    let found = search_sessions("needle", &options(&home, &["/w/p"])).unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].session_id, ID_A);
    assert_eq!(found[0].title, "Named thread");
    assert_eq!(found[0].lines.len(), 2);
    assert!(found[0].file.starts_with("sessions/2026/11/18/rollout-"));
    let _ = std::fs::remove_dir_all(&home);
}

/// OpenCode's database, made with the `sqlite3` command; `false` when it is
/// not installed, and the test has nothing to run.
fn opencode_db(home: &Path) -> bool {
    let dir = home.join(".local/share/opencode");
    std::fs::create_dir_all(&dir).unwrap();
    let sql = r#"CREATE TABLE session_v2 (id TEXT, parent_id TEXT, title TEXT, time_updated INTEGER, directory TEXT);
         CREATE TABLE session_message (session_id TEXT, seq INTEGER, type TEXT, data TEXT);
         INSERT INTO session_v2 VALUES ('ses_root', NULL, 'Named session', 1700000000000, '/w/o');
         INSERT INTO session_v2 VALUES ('ses_child', 'ses_root', 'x', 1700000100000, '/w/o/sub');
         INSERT INTO session_v2 VALUES ('ses_plain', NULL, 'New session - 2026-11-18T00:00:00Z', 1700000200000, '/w/o');
         INSERT INTO session_v2 VALUES ('ses_out', NULL, 'Out', 1700000300000, '/w/other');
         INSERT INTO session_message VALUES ('ses_root', 1, 'user', '{"text": "a Needle at the root"}');
         INSERT INTO session_message VALUES ('ses_child', 1, 'assistant', '{"text": "another needle"}');
         INSERT INTO session_message VALUES ('ses_child', 2, 'system', '{"text": "needle but a system row"}');
         INSERT INTO session_message VALUES ('ses_plain', 1, 'user', '{"text": "  the first needle prompt "}');
         INSERT INTO session_message VALUES ('ses_out', 1, 'user', '{"text": "needle, out of scope"}');"#;
    Command::new("sqlite3")
        .arg(dir.join("opencode.db"))
        .arg(sql)
        .status()
        .is_ok_and(|status| status.success())
}

#[test]
fn opencode_sessions_in_scope_and_sub_agents_count_as_their_parent() {
    let Some(home) = fake_home("opencode") else { return };
    if !opencode_db(&home) {
        return;
    }
    let found = search_sessions("needle", &options(&home, &["/w/o"])).unwrap();
    assert_eq!(found.len(), 2, "{found:?}");
    // Newest first: the plain session, then the root. The walk is newest
    // first too, so the child's row (the child is newer) comes before the
    // root's own, as in the app.
    assert_eq!(found[0].session_id, "ses_plain");
    assert_eq!(found[0].title, "the first needle prompt");
    assert_eq!(found[1].session_id, "ses_root");
    assert_eq!(found[1].title, "Named session");
    assert_eq!(found[1].harness, SessionHarness::Opencode);
    assert_eq!(found[1].file, "opencode.db");
    assert_eq!(found[1].at, "2023-11-14T22:15:00.000Z");
    let lines: Vec<&str> = found[1].lines.iter().map(|l| l.text.as_str()).collect();
    assert_eq!(lines, vec!["another needle", "a Needle at the root"]);
    assert_eq!(found[1].lines[1].line, 1);
    let _ = std::fs::remove_dir_all(&home);
}
