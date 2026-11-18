//! The running app's kit socket (`electron/main/kit-socket.ts` has the wire
//! format): one call per connection, one JSON line each way. Shared by
//! `lwk find|grep|sg|ls|sessions` (`kit.rs`) and `lwk mcp` (`mcp/`).
//!
//! No socket file, a refused or slow connect (over [`CONNECT`]), a
//! connection closed before the answer, an answer that is not JSON, or an
//! app too busy to take the call all mean "no app": the caller runs the
//! same params in-process. An error the app answered is final: running the
//! call again would only repeat it. Every kit tool only reads, so a call
//! run twice is harmless.

use crate::engine::types::{KitError, KitErrorCode};
use crate::{jsonjs, paths};
use serde_json::value::RawValue;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long a connect may take before the tool runs without the app.
const CONNECT: Duration = Duration::from_millis(150);
/// The longest a call may take; the kit's own searches stop well before.
const ANSWER: Duration = Duration::from_secs(60);

/// What the app said.
pub enum Answer {
    /// No app to answer: run the tool without it.
    Absent,
    /// The app's error for this call.
    Failed(KitError),
    /// The app's result, as it came; None when the answer had none.
    Result(Option<Box<RawValue>>),
}

/// Connect to `path`, giving up after [`CONNECT`].
///
/// NOTE: On a thread, as std has no connect timeout for a unix socket. A
/// connect that hangs past it is left behind: lwk goes on to run the tool
/// in-process, and the thread goes with the process.
fn connect(path: &Path) -> Option<UnixStream> {
    let (sender, receiver) = mpsc::channel();
    let path: PathBuf = path.to_owned();
    std::thread::spawn(move || {
        let _ = sender.send(UnixStream::connect(path));
    });
    receiver.recv_timeout(CONNECT).ok()?.ok()
}

fn no_answer() -> Answer {
    Answer::Failed(KitError::new(
        KitErrorCode::Unavailable,
        "The app did not answer.",
    ))
}

/// Call `method` in the running app with `params`.
pub fn call(method: &str, params: Map<String, Value>) -> Answer {
    let path = paths::app_home().join("run").join("kit.sock");
    if !path.exists() {
        return Answer::Absent;
    }
    let Some(mut stream) = connect(&path) else {
        return Answer::Absent;
    };
    let mut request = Map::new();
    request.insert("id".into(), Value::from(1));
    request.insert("method".into(), Value::String(method.to_owned()));
    request.insert("params".into(), Value::Object(params));
    let mut line = Value::Object(request).to_string();
    line.push('\n');
    if stream.write_all(line.as_bytes()).is_err() {
        return Answer::Absent;
    }

    let deadline = Instant::now() + ANSWER;
    let mut buffered: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 64 * 1024];
    let end = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
            return no_answer();
        }
        match stream.read(&mut chunk) {
            // Closed before answering: the app quit mid-call.
            Ok(0) => return Answer::Absent,
            Ok(read) => {
                let from = buffered.len();
                buffered.extend_from_slice(&chunk[..read]);
                if let Some(at) = buffered[from..].iter().position(|b| *b == b'\n') {
                    break from + at;
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                return no_answer();
            }
            Err(_) => return Answer::Absent,
        }
    };
    let text = String::from_utf8_lossy(&buffered[..end]);
    let Ok(mut answer) = serde_json::from_str::<HashMap<String, Box<RawValue>>>(&text) else {
        return Answer::Absent;
    };
    let error: Option<Value> = answer
        .get("error")
        .and_then(|raw| serde_json::from_str(raw.get()).ok());
    if !jsonjs::truthy(error.as_ref()) {
        return Answer::Result(answer.remove("result"));
    }
    let error = error.unwrap_or(Value::Null);
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .and_then(KitErrorCode::parse)
        .unwrap_or(KitErrorCode::Internal);
    // Busy is the app's load, not this call's fault: run it without the app.
    if code == KitErrorCode::Busy {
        return Answer::Absent;
    }
    let message = match error.get("message") {
        None | Some(Value::Null) => String::new(),
        message => jsonjs::template(message),
    };
    Answer::Failed(KitError::new(code, message))
}
