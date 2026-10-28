//! A DAP client as VS Code is one (ADR 0119 E9-2, E9-3): requests out, responses and events
//! in, the program's frames run while it waits. The editor's and the standalone player's
//! tests each include it (`#[path]`).

#![allow(dead_code, unreachable_pub)]

use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// How long a wait runs frames before it gives up: by the clock, as a small offscreen
/// player's frames take far less than the adapter's threads.
const WAIT: Duration = Duration::from_secs(20);

/// A client connected to a debug adapter's port.
pub struct Client {
    stream: TcpStream,
    read: Vec<u8>,
    seq: i64,
    /// What came and was not taken yet.
    pub came: VecDeque<Value>,
    /// The adapter closed the connection.
    pub closed: bool,
}

impl Client {
    /// Connected to 127.0.0.1:`port`.
    pub fn connect(port: u16) -> Self {
        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_nonblocking(true).unwrap();
        Self {
            stream,
            read: Vec::new(),
            seq: 0,
            came: VecDeque::new(),
            closed: false,
        }
    }

    /// Request `command` sent: its sequence number.
    pub fn send(&mut self, command: &str, arguments: Value) -> i64 {
        self.seq += 1;
        let body = json!({
            "seq": self.seq,
            "type": "request",
            "command": command,
            "arguments": arguments,
        })
        .to_string();
        let message = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        self.stream.write_all(message.as_bytes()).unwrap();
        self.seq
    }

    /// What came since: whole messages into `came`.
    pub fn pump(&mut self) {
        let mut buf = [0u8; 8192];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => {
                    self.closed = true;
                    break;
                }
                Ok(n) => self.read.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.closed = true;
                    break;
                }
            }
        }
        while let Some(end) = self.read.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&self.read[..end]).into_owned();
            let length: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length:"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            if self.read.len() < end + 4 + length {
                break;
            }
            let body = self.read[end + 4..end + 4 + length].to_vec();
            self.read.drain(..end + 4 + length);
            self.came.push_back(serde_json::from_slice(&body).unwrap());
        }
    }

    /// The first message `wanted` takes, running `frame` until it comes.
    pub fn wait(
        &mut self,
        frame: &mut dyn FnMut(),
        what: &str,
        wanted: impl Fn(&Value) -> bool,
    ) -> Value {
        let start = Instant::now();
        while start.elapsed() < WAIT {
            self.pump();
            if let Some(k) = self.came.iter().position(&wanted) {
                return self.came.remove(k).unwrap();
            }
            frame();
        }
        panic!("no {what} came; came: {:#?}", self.came);
    }

    /// Request `command` sent and its response waited for, running `frame`.
    pub fn request(&mut self, frame: &mut dyn FnMut(), command: &str, arguments: Value) -> Value {
        let seq = self.send(command, arguments);
        self.wait(frame, command, |m| {
            m["type"] == "response" && m["request_seq"] == seq
        })
    }
}

/// Takes event `name`.
pub fn event(name: &'static str) -> impl Fn(&Value) -> bool {
    move |m| m["type"] == "event" && m["event"] == name
}
