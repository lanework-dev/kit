//! `lwk serve`'s wire: request lines in, answer lines out, and the log.
//!
//! - A request is one line ending in `\n`, at most [`LINE_MAX`] bytes.
//! - An answer is one line, written whole under [`Out`]'s lock, so answers
//!   from several threads never mix. stdout carries nothing else.
//! - The log is one JSON line per request on stderr.

use crate::engine::types::KitError;
use crate::jsonjs;
use serde_json::Value;
use std::io::{BufRead, ErrorKind, Write};
use std::sync::{Mutex, MutexGuard};
use std::time::Instant;

/// The longest request line taken, without its newline (1 MB).
pub const LINE_MAX: usize = 1024 * 1024;

/// What [`read_line`] read.
#[derive(Debug)]
pub enum Read {
    /// A whole line, in the buffer without its newline.
    Line,
    /// A line over the limit, read to its end and dropped.
    TooLong,
    /// The input ended. Bytes after the last newline are not a line.
    End,
    Failed(std::io::Error),
}

/// Read the next line of `input` into `line`, holding `max` bytes of it at
/// most however long it is.
pub fn read_line(input: &mut impl BufRead, line: &mut Vec<u8>, max: usize) -> Read {
    line.clear();
    let mut too_long = false;
    loop {
        let (used, ended) = {
            let buffered = match input.fill_buf() {
                Ok(buffered) => buffered,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Read::Failed(error),
            };
            if buffered.is_empty() {
                return Read::End;
            }
            let newline = buffered.iter().position(|byte| *byte == b'\n');
            let part = &buffered[..newline.unwrap_or(buffered.len())];
            if !too_long {
                if line.len() + part.len() > max {
                    too_long = true;
                    line.clear();
                } else {
                    line.extend_from_slice(part);
                }
            }
            (newline.map_or(buffered.len(), |at| at + 1), newline.is_some())
        };
        input.consume(used);
        if ended {
            return if too_long { Read::TooLong } else { Read::Line };
        }
    }
}

/// `{"id": ID, "result": RESULT}` as a line; both are JSON text already.
pub fn result_line(id: &str, result: &str) -> String {
    format!("{{\"id\":{id},\"result\":{result}}}\n")
}

/// `{"id": ID, "error": {code, message}}` as a line; `id` is JSON text.
pub fn error_line(id: &str, error: &KitError) -> String {
    let error = jsonjs::stringify(&error.to_json());
    format!("{{\"id\":{id},\"error\":{error}}}\n")
}

/// stdout, one writer at a time.
pub struct Out(Mutex<()>);

impl Out {
    pub fn new() -> Out {
        Out(Mutex::new(()))
    }

    /// Wait for the line being written, and let none follow until the guard
    /// goes: held while the process exits.
    pub fn close(&self) -> MutexGuard<'_, ()> {
        self.0.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// Write `lines` (each ending in a newline) whole. False when stdout is
    /// closed: the client went away.
    pub fn send(&self, lines: &str) -> bool {
        let _one = self.close();
        write(lines)
    }
}

/// Write `lines` to stdout; the caller holds [`Out`]'s lock.
pub fn write(lines: &str) -> bool {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(lines.as_bytes()).and_then(|()| stdout.flush()).is_ok()
}

/// Log one request on stderr: `{"id", "method", "ms", "ok": true}`, or
/// `"error": CODE` in place of `ok`. `id` is JSON text.
pub fn log(id: &str, method: Option<&str>, started: Instant, error: Option<&str>) {
    let method = method.map_or(Value::Null, |name| Value::String(name.to_owned()));
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    let outcome = match error {
        None => "\"ok\":true".to_owned(),
        Some(code) => format!("\"error\":\"{code}\""),
    };
    let line = format!("{{\"id\":{id},\"method\":{method},\"ms\":{ms:.3},{outcome}}}\n");
    let _ = std::io::stderr().lock().write_all(line.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    #[test]
    fn reads_lines_and_drops_one_too_long() {
        let text = b"one\n\ntoo long by far\r\nfour\nno newline";
        // A small buffer, so a line comes in several parts.
        let mut input = BufReader::with_capacity(4, &text[..]);
        let mut line = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        loop {
            match read_line(&mut input, &mut line, 5) {
                Read::Line => seen.push(String::from_utf8(line.clone()).unwrap()),
                Read::TooLong => seen.push("<too long>".to_owned()),
                Read::End => break,
                Read::Failed(error) => panic!("{error}"),
            }
        }
        assert_eq!(seen, ["one", "", "<too long>", "four"]);
    }

    #[test]
    fn a_line_of_exactly_the_limit_is_read() {
        let mut input = BufReader::with_capacity(3, &b"12345\n123456\n"[..]);
        let mut line = Vec::new();
        assert!(matches!(read_line(&mut input, &mut line, 5), Read::Line));
        assert_eq!(line, b"12345");
        assert!(matches!(read_line(&mut input, &mut line, 5), Read::TooLong));
    }

    #[test]
    fn writes_answers_as_the_socket_does() {
        assert_eq!(result_line("1", "null"), "{\"id\":1,\"result\":null}\n");
        let error = KitError::invalid("No \"x\".");
        assert_eq!(
            error_line("\"a\"", &error),
            "{\"id\":\"a\",\"error\":{\"code\":\"invalid\",\"message\":\"No \\\"x\\\".\"}}\n"
        );
    }
}
