//! The script debugger over the Debug Adapter Protocol (ADR 0119 E9-2, E9-3;
//! `docs/research/script-debugger.md`, "What VS Code needs"): VS Code attaches to the
//! editor, or to a standalone player started with `--debug`, and debugs the scripts it runs.
//!
//! - **The endpoint:** 127.0.0.1, on a port the system picks, written with a token to the
//!   project's `.ofelia/debug/` (`editor.json`, `player-<pid>.json`), where the extension
//!   finds it. Only this machine connects, and `attach` must give the token: an evaluation
//!   runs code in the game. One client at a time.
//! - **Threads:** one accepts and reads the client's messages, one writes them; they trade
//!   messages with the program's frame through channels, so the frame never waits on the
//!   socket. The program answers each request in its frame: the requests about a stop
//!   (the stack, the scopes, the values, evaluations) alike ([`References::answer`]).
//! - **Messages:** a `Content-Length` header, a blank line, UTF-8 JSON.

use engine_script::{Breakpoint, ErrorStop, ErrorStops, Hits, Stopped, Variable};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

/// The longest message read: anything longer is not a client of ours.
const LONGEST: usize = 16 << 20;

/// How long a read waits before the reader looks whether its client was let go: Windows
/// wakes no read blocked on a socket another handle shut.
const READ_WAIT: Duration = Duration::from_millis(100);

/// What the client's thread hands the program's frame.
enum Incoming {
    /// A client connected: what writes to it, and its reader's flag (to let it go).
    Connected(mpsc::Sender<Vec<u8>>, Arc<AtomicBool>),
    /// A request: its sequence number, its command and its arguments.
    Request(i64, String, Value),
    /// The client went away.
    Gone,
}

/// A request from the client: its sequence number, its command, its arguments.
pub struct Request {
    /// Its sequence number, which its response names.
    pub seq: i64,
    /// What it asks (`setBreakpoints`, `stackTrace`, ...).
    pub command: String,
    /// Its arguments.
    pub arguments: Value,
}

/// Which program debugs, and so where its endpoint is written in `.ofelia/debug/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Debuggee {
    /// The editor, whose session has one: `editor.json`.
    Editor,
    /// A standalone player (ADR 0119 E9-3), one per process: `player-<pid>.json`.
    Player,
}

/// A DAP endpoint: the editor's, or a standalone player's.
pub struct Dap {
    port: u16,
    token: String,
    file: PathBuf,
    incoming: mpsc::Receiver<Incoming>,
    /// The connected client: what writes to it, and the flag that ends its reader. Its
    /// writer closes the connection once it has written what it was given.
    client: Option<(mpsc::Sender<Vec<u8>>, Arc<AtomicBool>)>,
    /// The client gave the token.
    attached: bool,
    seq: i64,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// The scripts' records (their logpoints' and `print`'s), read for the client's console.
    log: engine_log::LogReader,
}

impl Dap {
    /// Listens on 127.0.0.1, on a port the system picks, and writes it with the token to
    /// `dir` (the project's `.ofelia/debug`), in the file `debuggee` has there; `title`
    /// names the program in the extension's list.
    ///
    /// # Errors
    ///
    /// No port to listen on, or the file cannot be written.
    pub fn start(
        dir: &Path,
        project: &Path,
        debuggee: Debuggee,
        title: &str,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
        let port = listener.local_addr()?.port();
        let token = token();
        std::fs::create_dir_all(dir)?;
        let pid = std::process::id();
        let (file, kind) = match debuggee {
            Debuggee::Editor => (dir.join("editor.json"), "editor"),
            Debuggee::Player => (dir.join(format!("player-{pid}.json")), "player"),
        };
        let text = json!({
            "kind": kind,
            "port": port,
            "token": token,
            "pid": pid,
            "project": project,
            "title": title,
        })
        .to_string();
        std::fs::write(&file, text)?;
        let (tx, incoming) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::Builder::new()
            .name(String::from("dap"))
            .spawn(move || serve(&listener, &tx, &stopped))?;
        Ok(Self {
            port,
            token,
            file,
            incoming,
            client: None,
            attached: false,
            seq: 0,
            stop,
            thread: Some(thread),
            log: engine_log::LogReader::new(),
        })
    }

    /// The port it listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The file its port and token are written to.
    pub fn file(&self) -> &Path {
        &self.file
    }

    /// The requests that came since the last call, into `out` (connections and their ends
    /// handled here).
    pub fn take(&mut self, out: &mut Vec<Request>) {
        while let Ok(m) = self.incoming.try_recv() {
            match m {
                Incoming::Connected(writer, gone) => {
                    self.client = Some((writer, gone));
                    self.attached = false;
                }
                Incoming::Request(seq, command, arguments) => out.push(Request {
                    seq,
                    command,
                    arguments,
                }),
                Incoming::Gone => {
                    self.client = None;
                    self.attached = false;
                }
            }
        }
    }

    /// Whether a client is connected.
    pub fn connected(&self) -> bool {
        self.client.is_some()
    }

    /// Whether a client gave the token.
    pub fn attached(&self) -> bool {
        self.attached
    }

    /// Whether `token` is this endpoint's: the client attaches.
    pub fn attach(&mut self, token: &str) -> bool {
        self.attached = !token.is_empty() && token == self.token;
        self.attached
    }

    /// Answers request `seq` (`command`): its body, or why it failed.
    pub fn respond(&mut self, seq: i64, command: &str, body: Result<Value, String>) {
        let mut m = json!({
            "type": "response",
            "request_seq": seq,
            "command": command,
        });
        match body {
            Ok(body) => {
                m["success"] = Value::Bool(true);
                if !body.is_null() {
                    m["body"] = body;
                }
            }
            Err(why) => {
                m["success"] = Value::Bool(false);
                m["message"] = Value::String(why);
            }
        }
        self.send(m);
    }

    /// Event `event` with `body`, to an attached client (`initialized` to a connected one).
    pub fn event(&mut self, event: &str, body: Value) {
        if !self.attached && event != "initialized" {
            return;
        }
        self.send(json!({ "type": "event", "event": event, "body": body }));
    }

    /// The scripts' log records since the last call, to an attached client's console.
    pub fn forward_log(&mut self) {
        if !self.attached {
            // Passed over: a client that attaches later gets what comes after.
            self.log.read(|_| {});
            return;
        }
        let mut said = Vec::new();
        self.log.read(|r| {
            if r.target == "script" {
                said.push(r.message.to_owned());
            }
        });
        for text in said {
            self.event(
                "output",
                json!({ "category": "console", "output": format!("{text}\n") }),
            );
        }
    }

    fn send(&mut self, mut m: Value) {
        let Some((writer, _)) = &self.client else {
            return;
        };
        self.seq += 1;
        m["seq"] = json!(self.seq);
        let body = m.to_string();
        let mut bytes = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        bytes.extend_from_slice(body.as_bytes());
        if writer.send(bytes).is_err() {
            self.client = None;
            self.attached = false;
        }
    }

    /// Lets the client go (it asked to disconnect, or gave a wrong token).
    pub fn drop_client(&mut self) {
        // Its writer ends once it has written what it was given, closing the connection.
        if let Some((_, gone)) = self.client.take() {
            gone.store(true, Ordering::Release);
        }
        self.attached = false;
    }
}

impl Drop for Dap {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.drop_client();
        // The accepting thread waits on the listener: a connection of our own wakes it.
        let _ = TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, self.port)));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.file);
    }
}

/// What the debugger does for VS Code (`initialize`'s answer): configurationDone, the
/// breakpoints' conditions, hits and logs, hovers, and which errors stop, `errors` their
/// defaults.
pub fn capabilities(errors: ErrorStops) -> Value {
    json!({
        "supportsConfigurationDoneRequest": true,
        "supportsConditionalBreakpoints": true,
        "supportsHitConditionalBreakpoints": true,
        "supportsLogPoints": true,
        "supportsEvaluateForHovers": true,
        "exceptionBreakpointFilters": [
            {
                "filter": "uncaught",
                "label": "Errors the script does not catch",
                "default": errors.uncaught,
            },
            {
                "filter": "caught",
                "label": "Errors caught by pcall",
                "default": errors.caught,
            },
        ],
    })
}

/// The `stopped` event's body: why (`breakpoint`, `step`, `pause`; `exception` at an
/// error, with its message).
pub fn stopped(reason: &str, error: Option<&ErrorStop>) -> Value {
    let mut body = json!({
        "reason": if error.is_some() { "exception" } else { reason },
        "threadId": 1,
        "allThreadsStopped": true,
    });
    if let Some(e) = error {
        body["description"] = json!("Error");
        body["text"] = json!(e.message);
    }
    body
}

/// The `continued` event's body.
pub fn continued() -> Value {
    json!({ "threadId": 1, "allThreadsContinued": true })
}

/// Which errors stop, as `setExceptionBreakpoints` asks.
pub fn error_stops(arguments: &Value) -> ErrorStops {
    let filters = arguments["filters"]
        .as_array()
        .map_or(&[][..], Vec::as_slice);
    let has = |f: &str| filters.iter().any(|v| v == f);
    ErrorStops {
        uncaught: has("uncaught"),
        caught: has("caught"),
    }
}

/// The project's file VS Code names by `path` (absolute), relative to the project folder
/// `root`; none for one outside it.
pub fn project_file(root: &Path, path: &str) -> Option<PathBuf> {
    let root = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let root = root.to_string_lossy().replace('\\', "/");
    let root = root.trim_end_matches('/');
    let given = path.replace('\\', "/");
    let head = given.get(..root.len())?;
    if !head.eq_ignore_ascii_case(root) {
        return None;
    }
    let rest = given[root.len()..].trim_start_matches('/');
    (!rest.is_empty()).then(|| PathBuf::from(rest))
}

/// A breakpoint VS Code asks for (`setBreakpoints`): its line in the file as saved (from
/// 1), what it does, and its condition and hit rule as typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    /// Its line, from 1.
    pub line: u32,
    /// A logpoint's message (`{expressions}` in it); none for a breakpoint that stops.
    pub log: Option<String>,
    /// Its condition, empty for none.
    pub condition: String,
    /// Its hit rule as typed, empty for every hit ([`Hits::parse`]).
    pub hits: String,
}

impl Asked {
    /// The breakpoints a `setBreakpoints` request asks for, in its order.
    pub fn all(arguments: &Value) -> Vec<Self> {
        let asked = arguments["breakpoints"]
            .as_array()
            .map_or(&[][..], Vec::as_slice);
        asked
            .iter()
            .map(|b| Self {
                line: b["line"]
                    .as_u64()
                    .and_then(|l| u32::try_from(l).ok())
                    .unwrap_or(1)
                    .max(1),
                log: b["logMessage"].as_str().map(str::to_owned),
                condition: b["condition"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
                hits: b["hitCondition"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
            })
            .collect()
    }

    /// The breakpoint the script host is given, on its line.
    ///
    /// # Errors
    ///
    /// Its hit rule does not read ([`Hits::parse`]).
    pub fn to_host(&self) -> Result<Breakpoint, String> {
        Ok(Breakpoint {
            line: self.line,
            action: match &self.log {
                Some(text) => engine_script::BreakAction::Log(text.clone()),
                None => engine_script::BreakAction::Stop,
            },
            condition: (!self.condition.is_empty()).then(|| self.condition.clone()),
            hits: Hits::parse(&self.hits)?,
            enabled: true,
        })
    }
}

/// The references VS Code reads a stop's values by (`variablesReference`, from 1): a
/// frame's locals or upvalues, an evaluation's value, then down what they hold by index.
/// They are valid until the stop goes on ([`Self::clear`]).
#[derive(Debug, Default)]
pub struct References {
    /// (frame, or evaluation; section: 0 locals, 1 upvalues, 2 an evaluation; path).
    keys: Vec<(usize, u8, Vec<usize>)>,
    evaluated: Vec<Variable>,
}

impl References {
    /// The last stop's references gone: it went on, or another came.
    pub fn clear(&mut self) {
        self.keys.clear();
        self.evaluated.clear();
    }

    /// The answer to request `r` when it asks about the stop (`threads`, `stackTrace`,
    /// `scopes`, `variables`, `evaluate`): read from `stop` (none while nothing is stopped),
    /// sources under the project folder `root`, evaluations by `evaluate` (a frame, the
    /// code). None for any other request.
    pub fn answer(
        &mut self,
        r: &Request,
        stop: Option<&Stopped>,
        root: &Path,
        evaluate: impl FnOnce(usize, &str) -> Result<Variable, String>,
    ) -> Option<Result<Value, String>> {
        let args = &r.arguments;
        let frame_of = |key: &str| {
            args[key]
                .as_u64()
                .and_then(|k| usize::try_from(k).ok())
                .unwrap_or(0)
        };
        Some(Ok(match r.command.as_str() {
            "threads" => json!({ "threads": [{ "id": 1, "name": "Scripts" }] }),
            "stackTrace" => stack_trace(stop, root),
            "scopes" => {
                let frame = frame_of("frameId");
                let locals = self.reference(frame, 0, Vec::new());
                let upvalues = self.reference(frame, 1, Vec::new());
                json!({ "scopes": [
                    { "name": "Locals", "variablesReference": locals, "expensive": false },
                    { "name": "Upvalues", "variablesReference": upvalues, "expensive": false },
                ] })
            }
            "variables" => {
                let reference = frame_of("variablesReference");
                let Some((frame, section, path)) = reference
                    .checked_sub(1)
                    .and_then(|k| self.keys.get(k))
                    .cloned()
                else {
                    return Some(Ok(json!({ "variables": [] })));
                };
                let shown: Vec<(String, String, bool)> = self
                    .children(stop, frame, section, &path)
                    .iter()
                    .map(|v| (v.name.clone(), v.value.clone(), !v.children.is_empty()))
                    .collect();
                let mut out = Vec::with_capacity(shown.len());
                for (i, (name, value, opens)) in shown.into_iter().enumerate() {
                    let child = if opens {
                        let mut down = path.clone();
                        down.push(i);
                        self.reference(frame, section, down)
                    } else {
                        0
                    };
                    out.push(json!({ "name": name, "value": value, "variablesReference": child }));
                }
                json!({ "variables": out })
            }
            "evaluate" => {
                let code = args["expression"].as_str().unwrap_or_default();
                let v = match evaluate(frame_of("frameId"), code) {
                    Ok(v) => v,
                    Err(why) => return Some(Err(why)),
                };
                let reference = if v.children.is_empty() {
                    0
                } else {
                    self.evaluated.push(v.clone());
                    self.reference(self.evaluated.len() - 1, 2, Vec::new())
                };
                json!({ "result": v.value, "variablesReference": reference })
            }
            _ => return None,
        }))
    }

    /// The reference of the values at `(frame, section, path)`.
    fn reference(&mut self, frame: usize, section: u8, path: Vec<usize>) -> usize {
        let key = (frame, section, path);
        match self.keys.iter().position(|r| *r == key) {
            Some(k) => k + 1,
            None => {
                self.keys.push(key);
                self.keys.len()
            }
        }
    }

    /// The values at `(frame, section, path)`: a frame's locals (section 0) or upvalues
    /// (1), an evaluation's value's children (2), then down `path` by index.
    fn children<'a>(
        &'a self,
        stop: Option<&'a Stopped>,
        frame: usize,
        section: u8,
        path: &[usize],
    ) -> &'a [Variable] {
        let mut level: &[Variable] = match section {
            0 | 1 => match stop.and_then(|s| s.stack.get(frame)) {
                Some(f) if section == 0 => &f.locals,
                Some(f) => &f.upvalues,
                None => &[],
            },
            _ => self
                .evaluated
                .get(frame)
                .map_or(&[], |v| v.children.as_slice()),
        };
        for &i in path {
            level = level.get(i).map_or(&[], |v| v.children.as_slice());
        }
        level
    }
}

/// `stackTrace`'s answer: the stopped script's frames, the engine's (no Luau) shown faint.
fn stack_trace(stop: Option<&Stopped>, root: &Path) -> Value {
    let Some(stop) = stop else {
        return json!({ "stackFrames": [], "totalFrames": 0 });
    };
    let frames: Vec<Value> = stop
        .stack
        .iter()
        .enumerate()
        .map(|(k, f)| {
            let name = f.function.as_deref().unwrap_or("(function)");
            match f.line {
                Some(line) => {
                    let file = crate::scripts::module_file(&f.source);
                    let path =
                        std::path::absolute(root.join(&file)).unwrap_or_else(|_| root.join(&file));
                    let shown = Path::new(&file)
                        .file_name()
                        .map_or_else(|| file.clone(), |n| n.to_string_lossy().into_owned());
                    json!({
                        "id": k,
                        "name": name,
                        "line": line,
                        "column": 1,
                        "source": { "name": shown, "path": path },
                    })
                }
                None => json!({
                    "id": k,
                    "name": "engine",
                    "line": 0,
                    "column": 0,
                    "presentationHint": "subtle",
                }),
            }
        })
        .collect();
    let total = frames.len();
    json!({ "stackFrames": frames, "totalFrames": total })
}

/// A session's token: 16 bytes of the system's random generator, as hex.
fn token() -> String {
    let mut bytes = [0u8; 16];
    engine_net_transport::crypto::random_bytes(&mut bytes);
    bytes.iter().fold(String::with_capacity(32), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// The accepting thread: one client at a time, its messages read here and written by a
/// thread of its own, until the endpoint stops.
fn serve(listener: &TcpListener, tx: &mpsc::Sender<Incoming>, stop: &AtomicBool) {
    for stream in listener.incoming() {
        if stop.load(Ordering::Acquire) {
            break;
        }
        let Ok(stream) = stream else {
            continue;
        };
        let _ = stream.set_nodelay(true);
        let Ok(mut writer) = stream.try_clone() else {
            continue;
        };
        let (wtx, wrx) = mpsc::channel::<Vec<u8>>();
        let writing = std::thread::Builder::new()
            .name(String::from("dap writer"))
            .spawn(move || {
                for bytes in wrx {
                    if writer
                        .write_all(&bytes)
                        .and_then(|()| writer.flush())
                        .is_err()
                    {
                        break;
                    }
                }
                // Let go: what was given is written, the client sees the end.
                let _ = writer.shutdown(Shutdown::Both);
            });
        if writing.is_err() {
            continue;
        }
        let gone = Arc::new(AtomicBool::new(false));
        if tx.send(Incoming::Connected(wtx, gone.clone())).is_err() {
            break;
        }
        read_messages(stream, tx, &gone, stop);
        if tx.send(Incoming::Gone).is_err() {
            break;
        }
    }
}

/// The client's requests, read until it goes away, is let go (`gone`), the endpoint stops,
/// or it sends what is not ours.
fn read_messages(
    mut stream: TcpStream,
    tx: &mpsc::Sender<Incoming>,
    gone: &AtomicBool,
    stop: &AtomicBool,
) {
    let _ = stream.set_read_timeout(Some(READ_WAIT));
    let mut read: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        while let Some((message, used)) = message_in(&read) {
            read.drain(..used);
            let Some(message) = message else {
                continue;
            };
            if message["type"] != "request" {
                continue;
            }
            let request = Incoming::Request(
                message["seq"].as_i64().unwrap_or(0),
                message["command"].as_str().unwrap_or_default().to_owned(),
                message["arguments"].clone(),
            );
            if tx.send(request).is_err() {
                return;
            }
        }
        if read.len() > LONGEST {
            return;
        }
        match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(n) => read.extend_from_slice(&chunk[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                if gone.load(Ordering::Acquire) || stop.load(Ordering::Acquire) {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

/// The first whole message in `read`: its JSON (none for one that does not parse) and how
/// many bytes it took; none while it has not all come.
fn message_in(read: &[u8]) -> Option<(Option<Value>, usize)> {
    let end = read.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&read[..end]).ok();
    let length = head.and_then(|h| {
        h.lines()
            .find_map(|l| l.strip_prefix("Content-Length:"))
            .and_then(|n| n.trim().parse::<usize>().ok())
    });
    let Some(length) = length.filter(|&n| n <= LONGEST) else {
        // Not a header of ours: skipped.
        return Some((None, end + 4));
    };
    let whole = end + 4 + length;
    if read.len() < whole {
        return None;
    }
    Some((serde_json::from_slice(&read[end + 4..whole]).ok(), whole))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_is_read_once_whole_and_a_stranger_header_is_skipped() {
        let body = br#"{"type":"request","seq":1,"command":"threads"}"#;
        let mut bytes = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        bytes.extend_from_slice(body);
        assert!(
            message_in(&bytes[..bytes.len() - 1]).is_none(),
            "not all here"
        );
        let (m, used) = message_in(&bytes).unwrap();
        assert_eq!(
            (m.unwrap()["command"].as_str(), used),
            (Some("threads"), bytes.len())
        );
        let (m, used) = message_in(b"Hello: there\r\n\r\nrest").unwrap();
        assert_eq!((m, used), (None, 16));
    }

    #[test]
    fn breakpoints_asked_become_the_hosts_with_their_rules() {
        let asked = Asked::all(&json!({ "breakpoints": [
            { "line": 3 },
            { "line": 4, "logMessage": "rate {self.rate}", "condition": " dt > 0 " },
            { "line": 5, "hitCondition": ">= 10" },
            { "line": 6, "hitCondition": "often" },
        ] }));
        let host: Vec<_> = asked.iter().map(Asked::to_host).collect();
        let first = host[0].as_ref().unwrap();
        assert_eq!(
            (first.line, &first.action, first.hits),
            (3, &engine_script::BreakAction::Stop, Hits::Every)
        );
        let log = host[1].as_ref().unwrap();
        assert_eq!(
            log.action,
            engine_script::BreakAction::Log(String::from("rate {self.rate}"))
        );
        assert_eq!(log.condition.as_deref(), Some("dt > 0"));
        assert_eq!(host[2].as_ref().unwrap().hits, Hits::From(10));
        assert!(host[3].is_err(), "not a hit rule");
    }

    #[test]
    fn files_are_the_projects_only() {
        let root = std::env::temp_dir().join("ofelia-dap-root");
        let inside = std::path::absolute(root.join("scripts").join("door.luau")).unwrap();
        assert_eq!(
            project_file(&root, &inside.to_string_lossy()),
            Some(PathBuf::from("scripts/door.luau"))
        );
        let elsewhere = std::path::absolute(std::env::temp_dir().join("other.luau")).unwrap();
        assert_eq!(project_file(&root, &elsewhere.to_string_lossy()), None);
        assert_eq!(
            error_stops(&json!({ "filters": ["caught"] })),
            ErrorStops {
                uncaught: false,
                caught: true
            }
        );
    }
}
