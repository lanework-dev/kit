//! The transcripts' text search (what `search.ts` asked rg for): each file's
//! lines that have the query, turned into snippets of what was said.
//!
//! rg's own rules are kept: a literal, case ignored, at most
//! [`RG_MAX_COUNT`] matching lines read per file (counted before the
//! "was it said" test), and a line over [`LINE_PARSE_MAX`] passed over.

use super::js::{contains_bytes, Finder};
use super::snippet::snippet_of;
use super::{Candidate, Kind};
use crate::engine::types::MatchLine;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Snippets kept per session.
pub const LINES_PER_SESSION: usize = 3;
/// Matching lines read per transcript.
///
/// More than are kept: a line that matched only on its metadata (the `cwd`
/// every Claude line carries, Codex's system prompt) is dropped after.
const RG_MAX_COUNT: usize = 40;
/// The longest rg event parsed, in characters. A longer line is a file's
/// contents or an image, and is passed over.
const LINE_PARSE_MAX: usize = 2 * 1024 * 1024;
/// Under this many bytes a line cannot reach [`LINE_PARSE_MAX`] once it is
/// an rg event (escaping at most doubles it), so is not measured.
const MEASURE_FROM: usize = 800_000;
/// How many lines are read between looks at the clock.
const CLOCK_EVERY: u64 = 256;

/// The line types that hold what was said. A line with none of them
/// (Claude's attachments and API request blobs, Codex's `turn_context`) is
/// passed over unparsed: most of what a common word matches is those.
fn spoken_types(kind: Kind) -> [&'static [u8]; 2] {
    match kind {
        Kind::Claude => [br#""type":"user""#, br#""type":"assistant""#],
        Kind::Codex => [br#""type":"response_item""#, br#""type":"event_msg""#],
    }
}

/// What a search of one batch found.
pub struct Scanned {
    /// Each candidate's snippets, in the batch's order.
    pub lines: Vec<Vec<MatchLine>>,
    /// The deadline came before every file was read.
    pub timed_out: bool,
}

/// `batch`'s files searched for the query, several at once.
pub fn search_files(batch: &[Candidate], finder: &Finder, deadline: Instant) -> Scanned {
    let timed_out = AtomicBool::new(false);
    let lines = super::util::parallel_map(batch, |file| {
        scan_file(file, finder, deadline, &timed_out)
    });
    Scanned {
        lines,
        timed_out: timed_out.load(Ordering::Relaxed),
    }
}

fn scan_file(
    file: &Candidate,
    finder: &Finder,
    deadline: Instant,
    timed_out: &AtomicBool,
) -> Vec<MatchLine> {
    let mut kept: Vec<MatchLine> = Vec::new();
    let Ok(handle) = File::open(&file.path) else {
        return kept;
    };
    let mut reader = BufReader::with_capacity(256 * 1024, handle);
    let mut buf: Vec<u8> = Vec::new();
    let (mut number, mut matched) = (0u64, 0usize);
    loop {
        if number % CLOCK_EVERY == 0
            && (timed_out.load(Ordering::Relaxed) || Instant::now() >= deadline)
        {
            timed_out.store(true, Ordering::Relaxed);
            break;
        }
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        number += 1;
        if buf.last() == Some(&b'\n') {
            buf.pop();
        }
        if !finder.prefilter(&buf) {
            continue;
        }
        matched += 1;
        if let Some(snippet) = snippet_for(file, &buf, number, finder) {
            if !kept.iter().any(|line| line.text == snippet.text) {
                kept.push(snippet);
            }
        }
        // Past either limit nothing more is read: the rest could only be
        // dropped (rg stops at its count; a session keeps three).
        if kept.len() >= LINES_PER_SESSION || matched >= RG_MAX_COUNT {
            break;
        }
    }
    kept
}

/// The snippet a matching line of `file` gives, if it was said.
fn snippet_for(file: &Candidate, raw: &[u8], number: u64, finder: &Finder) -> Option<MatchLine> {
    if raw.len() > MEASURE_FROM && event_len(raw, file) > LINE_PARSE_MAX {
        return None;
    }
    if !spoken_types(file.kind)
        .iter()
        .any(|kind| contains_bytes(raw, kind))
    {
        return None;
    }
    snippet_of(file.kind, raw, number, finder)
}

/// About how long rg's JSON event for `raw` is, in UTF-16 units: the line
/// escaped as a JSON string, the file's path, and the event's own fields.
fn event_len(raw: &[u8], file: &Candidate) -> usize {
    let mut len = 160 + file.path.as_os_str().len() + 2;
    for &byte in raw {
        len += match byte {
            b'"' | b'\\' | b'\n' | b'\r' | b'\t' => 2,
            0..=0x1f => 6,
            // A continuation byte is part of a unit already counted.
            0x80..=0xbf => 0,
            // A four-byte character is two units.
            0xf0..=0xff => 2,
            _ => 1,
        };
    }
    len
}
