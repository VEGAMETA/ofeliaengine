//! The script debugger over the Debug Adapter Protocol (ADR 0119 E9-2;
//! `docs/research/script-debugger.md`, "What VS Code needs"): VS Code attaches to the
//! editor and debugs the scripts in Play beside the editor's own Debugger panel, one set of
//! breakpoints for both.
//!
//! - **The endpoint:** 127.0.0.1, on a port the system picks, written with a token to
//!   `.ofelia/debug/editor.json`, where the extension finds it. Only this machine connects,
//!   and `attach` must give the token: an evaluation runs code in the game. One client at a
//!   time.
//! - **Threads:** one accepts and reads the client's messages, one writes them; they trade
//!   messages with the editor's frame through channels, so the frame never waits on the
//!   socket. The editor answers each request in its frame.
//! - **Messages:** a `Content-Length` header, a blank line, UTF-8 JSON.

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

/// What the client's thread hands the editor's frame.
enum Incoming {
    /// A client connected: what writes to it, and its reader's flag (to let it go).
    Connected(mpsc::Sender<Vec<u8>>, Arc<AtomicBool>),
    /// A request: its sequence number, its command and its arguments.
    Request(i64, String, Value),
    /// The client went away.
    Gone,
}

/// A request from the client: its sequence number, its command, its arguments.
pub(crate) struct Request {
    pub(crate) seq: i64,
    pub(crate) command: String,
    pub(crate) arguments: Value,
}

/// The editor's DAP endpoint.
pub(crate) struct Dap {
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
    /// `dir/editor.json` (`dir` is the project's `.ofelia/debug`).
    pub(crate) fn start(dir: &Path, project: &Path) -> std::io::Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
        let port = listener.local_addr()?.port();
        let token = token();
        std::fs::create_dir_all(dir)?;
        let file = dir.join("editor.json");
        let text = json!({
            "kind": "editor",
            "port": port,
            "token": token,
            "pid": std::process::id(),
            "project": project,
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
    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    /// The requests that came since the last call, into `out` (connections and their ends
    /// handled here).
    pub(crate) fn take(&mut self, out: &mut Vec<Request>) {
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

    /// Whether a client gave the token.
    pub(crate) fn attached(&self) -> bool {
        self.attached
    }

    /// Whether `token` is this session's: the client attaches.
    pub(crate) fn attach(&mut self, token: &str) -> bool {
        self.attached = !token.is_empty() && token == self.token;
        self.attached
    }

    /// Answers request `seq` (`command`): its body, or why it failed.
    pub(crate) fn respond(&mut self, seq: i64, command: &str, body: Result<Value, String>) {
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
    pub(crate) fn event(&mut self, event: &str, body: Value) {
        if !self.attached && event != "initialized" {
            return;
        }
        self.send(json!({ "type": "event", "event": event, "body": body }));
    }

    /// The scripts' log records since the last call, to an attached client's console.
    pub(crate) fn forward_log(&mut self) {
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
    pub(crate) fn drop_client(&mut self) {
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
