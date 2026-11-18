//! `lwk serve [--idle-exit SECS]`: the kit's tools over stdin and stdout,
//! one JSON line per request and per answer (`contract/serve.md`), for a
//! client that keeps one `lwk` running instead of starting one per call.
//!
//! - The main thread reads requests. A tool call runs on a worker
//!   ([`pool`]) through `engine::run_noted`, with the `cwd` and `roots` the
//!   request gives as its scope; `hello`, `cancel` and `shutdown` are
//!   answered by the main thread at once.
//! - Many calls may be in flight; each answer is one line written whole
//!   ([`wire::Out`]), in the order the calls end.
//! - `cancel` answers for its target at once (`cancelled`) and raises the
//!   target's flag, which the engine's walks and searches read
//!   (`engine::files::Cancel`); what the stopped call returns is dropped.
//! - The folders' file lists stay in memory between calls
//!   (`engine::cache::keep_in_ram`), checked on every call as the cache
//!   file is. The check runs while the call already works on the files
//!   held (`engine::cache::checked_meanwhile`); a call whose files had
//!   changed is run again, so no answer comes from a stale list.
//! - Exits 0 on `shutdown`, when stdin ends, when stdout is closed, or
//!   after `--idle-exit` seconds with nothing in flight and nothing read.
//!
//! NOTE: Never asks the app's kit socket: `lwk serve` is the engine itself.
//! The process's own working folder is not a request's `cwd`; git's global
//! excludes are matched against the former, as in `lwk mcp`.

mod pool;
mod wire;

use crate::engine::files::Cancel;
use crate::engine::types::{KitError, KitErrorCode, Tool};
use crate::engine::{self, Context};
use crate::{jsonjs, paths};
use pool::Pool;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Calls run at once: one per core, within these.
const WORKERS: std::ops::RangeInclusive<usize> = 4..=16;
/// Calls that may wait for a worker; one more is `busy`.
const WAITING_MAX: usize = 256;
/// The folders whose file lists are kept in memory.
const RAM_ROOTS: usize = 8;
/// The most `roots` a request may give.
const ROOTS_MAX: usize = 32;

/// A tool call not yet answered.
struct Flight {
    /// Tells this call from a later one with the same id.
    seq: u64,
    cancel: Arc<AtomicBool>,
    method: &'static str,
    started: Instant,
}

struct State {
    /// By id, as JSON text.
    flights: HashMap<String, Flight>,
    seq: u64,
    /// When a line was last read, or the last call in flight ended.
    quiet_since: Instant,
}

struct Server {
    state: Mutex<State>,
    out: wire::Out,
}

impl Server {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// Write answer lines; a closed stdout means the client is gone.
    fn send(&self, lines: &str) {
        if !self.out.send(lines) {
            self.stop(self.state(), None, 0);
        }
    }

    /// Abort every call in flight and exit with `code`, after writing
    /// `last` when there is one. Nothing is written after it.
    fn stop(&self, state: MutexGuard<'_, State>, last: Option<&str>, code: i32) -> ! {
        for flight in state.flights.values() {
            flight.cancel.store(true, Ordering::Relaxed);
        }
        let _closed = self.out.close();
        if let Some(last) = last {
            wire::write(last);
        }
        exit(code);
    }

    /// Answer `id` (JSON text) with an error, and log it.
    fn refuse(&self, id: &str, method: Option<&str>, started: Instant, error: &KitError) {
        self.send(&wire::error_line(id, error));
        wire::log(id, method, started, Some(error.code.as_str()));
    }

    /// Answer `id` with `result` (JSON text), and log it.
    fn answer(&self, id: &str, method: &str, started: Instant, result: &str) {
        self.send(&wire::result_line(id, result));
        wire::log(id, Some(method), started, None);
    }

    /// One request line.
    fn handle(self: &Arc<Self>, line: &[u8], pool: &Pool) {
        let started = Instant::now();
        self.state().quiet_since = started;
        if line.iter().all(u8::is_ascii_whitespace) {
            return;
        }
        let request = match serde_json::from_slice::<Value>(line) {
            Ok(Value::Object(request)) => request,
            Ok(_) => {
                let error = KitError::invalid("A request must be a JSON object.");
                return self.refuse("null", None, started, &error);
            }
            Err(_) => {
                let error = KitError::invalid("The line is not JSON.");
                return self.refuse("null", None, started, &error);
            }
        };
        let method = request.get("method").and_then(Value::as_str);
        let id = match request.get("id") {
            Some(id @ (Value::String(_) | Value::Number(_))) => jsonjs::stringify(id),
            _ => {
                let error = KitError::invalid("id must be a string or a number.");
                return self.refuse("null", method, started, &error);
            }
        };
        let refuse = |error: KitError| self.refuse(&id, method, started, &error);
        if self.state().flights.contains_key(&id) {
            return refuse(KitError::invalid(format!("A call with the id {id} is still in flight.")));
        }
        let params = match request.get("params") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(params)) => params.clone(),
            Some(_) => return refuse(KitError::invalid("The params must be an object.")),
        };
        match method {
            Some("hello") => {
                let hello = json!({
                    "lwk": crate::VERSION,
                    "contract": crate::contract_version(),
                    "pid": std::process::id(),
                });
                self.answer(&id, "hello", started, &hello.to_string());
            }
            Some("cancel") => match params.get("id") {
                Some(target @ (Value::String(_) | Value::Number(_))) => {
                    self.cancel(&id, &jsonjs::stringify(target), started);
                }
                _ => refuse(KitError::invalid("id must be a string or a number.")),
            },
            Some("shutdown") => {
                wire::log(&id, method, started, None);
                self.stop(self.state(), Some(&wire::result_line(&id, "null")), 0);
            }
            Some(name) => match Tool::parse(name) {
                Some(tool) => {
                    if let Err(error) = self.start(&id, tool, params, started, pool) {
                        refuse(error);
                    }
                }
                None => refuse(KitError::invalid(format!("Unknown method: {name}"))),
            },
            None => refuse(KitError::invalid(format!(
                "Unknown method: {}",
                jsonjs::template(request.get("method"))
            ))),
        }
    }

    /// `cancel`: answer for `target` when it is in flight, then for `id`.
    fn cancel(&self, id: &str, target: &str, started: Instant) {
        let flight = {
            let mut state = self.state();
            let flight = state.flights.remove(target);
            if flight.is_some() && state.flights.is_empty() {
                state.quiet_since = Instant::now();
            }
            flight
        };
        let mut lines = String::new();
        if let Some(flight) = &flight {
            flight.cancel.store(true, Ordering::Relaxed);
            lines.push_str(&wire::error_line(target, &Cancel::error()));
        }
        let cancelled = if flight.is_some() { "true" } else { "false" };
        lines.push_str(&wire::result_line(id, &format!("{{\"cancelled\":{cancelled}}}")));
        self.send(&lines);
        if let Some(flight) = flight {
            let code = KitErrorCode::Cancelled.as_str();
            wire::log(target, Some(flight.method), flight.started, Some(code));
        }
        wire::log(id, Some("cancel"), started, None);
    }

    /// Put a tool call in flight; an error when its scope is not usable or
    /// too many calls wait.
    fn start(
        self: &Arc<Self>,
        id: &str,
        tool: Tool,
        mut params: Map<String, Value>,
        started: Instant,
        pool: &Pool,
    ) -> Result<(), KitError> {
        let ctx = context(&mut params)?;
        let cancel = Arc::new(AtomicBool::new(false));
        let seq = {
            let mut state = self.state();
            state.seq += 1;
            let flight = Flight {
                seq: state.seq,
                cancel: cancel.clone(),
                method: tool.name(),
                started,
            };
            state.flights.insert(id.to_owned(), flight);
            state.seq
        };
        let server = self.clone();
        let key = id.to_owned();
        let queued = pool.submit(Box::new(move || {
            // Cancelled while it waited: answered already.
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            let params = Value::Object(params);
            let run = || engine::run_noted(tool.name(), &params, &ctx);
            let found = Cancel::scope(cancel, || {
                // The files held in memory are used while their check runs;
                // had they changed, the call is run again on fresh ones.
                match engine::cache::checked_meanwhile(run) {
                    (found, true) => found,
                    (_, false) => run(),
                }
            });
            let line = match &found {
                Ok(answered) => match &answered.note {
                    None => wire::result_line(&key, &jsonjs::stringify(&answered.result)),
                    Some(note) => wire::noted_result_line(&key, &jsonjs::stringify(&answered.result), note),
                },
                Err(error) => wire::error_line(&key, error),
            };
            if server.land(&key, seq) {
                server.send(&line);
                let code = found.as_ref().err().map(|error| error.code.as_str());
                wire::log(&key, Some(tool.name()), started, code);
            }
        }));
        if !queued {
            self.land(id, seq);
            return Err(KitError::new(KitErrorCode::Busy, "Too many calls at once."));
        }
        Ok(())
    }

    /// Take call `seq` of `id` out of flight; false when it is not there
    /// any more: it was cancelled, and answered then.
    fn land(&self, id: &str, seq: u64) -> bool {
        let mut state = self.state();
        if state.flights.get(id).is_none_or(|flight| flight.seq != seq) {
            return false;
        }
        state.flights.remove(id);
        if state.flights.is_empty() {
            state.quiet_since = Instant::now();
        }
        true
    }

    /// Exit 0 once `idle` passes with nothing in flight and nothing read.
    fn exit_when_idle(&self, idle: Duration) -> ! {
        loop {
            let state = self.state();
            let wait = if state.flights.is_empty() {
                let left = idle.saturating_sub(state.quiet_since.elapsed());
                if left.is_zero() {
                    self.stop(state, None, 0);
                }
                left
            } else {
                // The clock starts again when the last call ends.
                idle
            };
            drop(state);
            std::thread::sleep(wait);
        }
    }
}

/// The scope a request gives, taken out of its params: `cwd`, and `roots`
/// or else the cwd alone.
fn context(params: &mut Map<String, Value>) -> Result<Context, KitError> {
    let absolute = |value: &Value| match value {
        Value::String(text) if Path::new(text).is_absolute() => Some(PathBuf::from(text)),
        _ => None,
    };
    let cwd = params
        .shift_remove("cwd")
        .as_ref()
        .and_then(absolute)
        .ok_or_else(|| KitError::invalid("cwd must be an absolute path."))?;
    let roots = match params.shift_remove("roots") {
        None | Some(Value::Null) => vec![cwd.clone()],
        Some(Value::Array(list)) if (1..=ROOTS_MAX).contains(&list.len()) => list
            .iter()
            .map(absolute)
            .collect::<Option<Vec<PathBuf>>>()
            .ok_or_else(|| KitError::invalid("Each root must be an absolute path."))?,
        Some(_) => {
            return Err(KitError::invalid(format!(
                "roots must be a list of 1 to {ROOTS_MAX} folders."
            )))
        }
    };
    Ok(Context {
        cwd,
        roots,
        home: paths::home_dir().unwrap_or_default(),
    })
}

/// `--idle-exit SECS` from the arguments; None for never.
fn idle_exit(args: &[OsString]) -> Result<Option<Duration>, String> {
    let mut idle = None;
    let mut args = args.iter().map(|arg| arg.to_string_lossy());
    while let Some(arg) = args.next() {
        let value = match arg.strip_prefix("--idle-exit") {
            Some("") => args.next().map(|value| value.into_owned()),
            Some(rest) if rest.starts_with('=') => Some(rest[1..].to_owned()),
            _ => return Err(format!("unknown option {arg}. Usage: lwk serve [--idle-exit SECS]")),
        };
        let secs = value
            .as_deref()
            .and_then(|text| text.parse::<f64>().ok())
            .filter(|secs| secs.is_finite() && *secs >= 0.0 && *secs <= 1e9)
            .ok_or("--idle-exit needs a number of seconds, 0 for never.")?;
        idle = (secs > 0.0).then(|| Duration::from_secs_f64(secs));
    }
    Ok(idle)
}

/// Serve requests on stdin until told to stop. Returns only for a usage
/// error (the exit code); otherwise the process exits from here.
pub fn run(args: &[OsString]) -> i32 {
    let idle = match idle_exit(args) {
        Ok(idle) => idle,
        Err(message) => {
            eprintln!("lwk serve: {message}");
            return 2;
        }
    };
    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    let Some(pool) = Pool::new(cores.clamp(*WORKERS.start(), *WORKERS.end()), WAITING_MAX) else {
        eprintln!("lwk serve: no worker thread could be started.");
        return 2;
    };
    engine::cache::keep_in_ram(RAM_ROOTS);
    let server = Arc::new(Server {
        state: Mutex::new(State {
            flights: HashMap::new(),
            seq: 0,
            quiet_since: Instant::now(),
        }),
        out: wire::Out::new(),
    });
    if let Some(idle) = idle {
        let server = server.clone();
        let watch = std::thread::Builder::new()
            .name("lwk-serve-idle".to_owned())
            .spawn(move || server.exit_when_idle(idle));
        if watch.is_err() {
            eprintln!("lwk serve: the idle watch could not be started.");
            return 2;
        }
    }

    let mut input = std::io::stdin().lock();
    let mut line: Vec<u8> = Vec::new();
    loop {
        match wire::read_line(&mut input, &mut line, wire::LINE_MAX) {
            wire::Read::Line => server.handle(&line, &pool),
            wire::Read::TooLong => {
                let started = Instant::now();
                server.state().quiet_since = started;
                let error = KitError::invalid("The line is too long (1 MB at most).");
                server.refuse("null", None, started, &error);
            }
            wire::Read::End => server.stop(server.state(), None, 0),
            wire::Read::Failed(error) => {
                eprintln!("lwk serve: {error}");
                server.stop(server.state(), None, 1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn reads_idle_exit() {
        assert_eq!(idle_exit(&args(&[])), Ok(None));
        assert_eq!(idle_exit(&args(&["--idle-exit", "0"])), Ok(None));
        assert_eq!(idle_exit(&args(&["--idle-exit", "2"])), Ok(Some(Duration::from_secs(2))));
        assert_eq!(idle_exit(&args(&["--idle-exit=1.5"])), Ok(Some(Duration::from_millis(1500))));
        assert!(idle_exit(&args(&["--idle-exit"])).is_err());
        assert!(idle_exit(&args(&["--idle-exit", "-1"])).is_err());
        assert!(idle_exit(&args(&["--port", "1"])).is_err());
    }

    #[test]
    fn takes_the_scope_out_of_the_params() {
        let mut params = json!({ "query": "x", "cwd": "/work/app", "roots": ["/work", "/else"] });
        let ctx = context(params.as_object_mut().unwrap()).unwrap();
        assert_eq!(ctx.cwd, PathBuf::from("/work/app"));
        assert_eq!(ctx.roots, [PathBuf::from("/work"), PathBuf::from("/else")]);
        assert_eq!(params, json!({ "query": "x" }));

        let mut alone = json!({ "cwd": "/work/app" });
        let ctx = context(alone.as_object_mut().unwrap()).unwrap();
        assert_eq!(ctx.roots, [PathBuf::from("/work/app")]);

        for bad in [
            json!({}),
            json!({ "cwd": "app" }),
            json!({ "cwd": 1 }),
            json!({ "cwd": "/a", "roots": [] }),
            json!({ "cwd": "/a", "roots": ["b"] }),
            json!({ "cwd": "/a", "roots": "/a" }),
            json!({ "cwd": "/a", "roots": vec!["/a"; ROOTS_MAX + 1] }),
        ] {
            let mut bad = bad;
            let error = context(bad.as_object_mut().unwrap()).unwrap_err();
            assert_eq!(error.code, KitErrorCode::Invalid, "{bad}");
        }
    }
}
