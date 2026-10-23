//! `slangd`, Slang's language server, as the completion, hover, signatures and definitions
//! of the code editor's Slang tabs (ADR 0107's E10-2b, `docs/research/slang-in-the-editor.md`):
//! a child process over LSP's JSON-RPC on its pipes, asked from the Slang checker's thread.
//! Errors stay the front end's (E10-2a): the server is started with its own checks off.
//!
//! - **A material type** is read inside a document of its own: `forward.slang`'s lines up to
//!   where it includes the material, then the type's text, so the server knows `IMaterial`,
//!   `MaterialContext` and the rest. Places in the type move by the prelude's lines; a
//!   definition in the prelude is in `forward.slang` at the same line, as the prelude is
//!   that file's own text. The document is written to `.ofelia/check/` once: the server
//!   reads what it was given, but gives definitions only in files that exist.
//! - **Any other module** is the document of its own path.
//! - **Changes** go as the range that changed: the server applies every change to a range,
//!   and one without a range would be put at the text's start.
//! - **Places** are LSP's: a line and UTF-16 units. The editor's are a line and a byte.
//! - **The server's questions** are answered: `workspace/configuration` with the engine's
//!   shader folder as a search path (what `forward.slang` includes), the rest with defaults.
//!   Its notices are passed over.
//! - **It starts** when the checker first checks a Slang module, its `initialize` answered
//!   while the front end checks. A question not answered in time is given up; a server that
//!   exited is started again at the next question.

use engine_luau_analysis::{Completion, CompletionKind};
use engine_shader::path_key;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How long a question waits for its answer.
const ANSWER_WITHIN: Duration = Duration::from_secs(3);
/// How long the server has to start.
const START_WITHIN: Duration = Duration::from_secs(10);

/// A document the server was given: its text and version.
struct Opened {
    text: String,
    version: i64,
}

/// A running `slangd`.
struct Server {
    child: Child,
    stdin: ChildStdin,
    messages: mpsc::Receiver<Value>,
    next_id: i64,
    /// Its `initialize` question, until the answer is read.
    starting: Option<i64>,
    /// The answer to its `slang.additionalSearchPaths` setting.
    search: Value,
    opened: HashMap<String, Opened>,
}

impl Server {
    /// Starts `exe` for the project in `root`, its includes also searched in `search`;
    /// `initialize` asked, not yet answered ([`Self::ready`]).
    fn start(exe: &Path, root: &Path, search: &[PathBuf]) -> Option<Self> {
        let mut command = Command::new(exe);
        command
            // Its own checks off: errors are the front end's.
            .args(["-periodic-diagnostic-update", "off"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // No console window for it.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                log::warn!(target: "engine_editor", "{}: {e}", exe.display());
                return None;
            }
        };
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            return None;
        };
        let (sender, messages) = mpsc::channel();
        let reader = std::thread::Builder::new()
            .name(String::from("slangd reader"))
            .spawn(move || read_messages(stdout, &sender));
        if reader.is_err() {
            let _ = child.kill();
            return None;
        }
        let search = search
            .iter()
            .map(|p| Value::String(p.to_string_lossy().replace('\\', "/")))
            .collect();
        let mut server = Self {
            child,
            stdin,
            messages,
            next_id: 1,
            starting: None,
            search: Value::Array(search),
            opened: HashMap::new(),
        };
        let uri = uri_of(root);
        let params = json!({
            "processId": std::process::id(),
            "rootUri": uri,
            "workspaceFolders": [{ "uri": uri, "name": "game" }],
            "capabilities": {
                "workspace": { "configuration": true },
                "textDocument": {
                    "completion": { "completionItem": { "snippetSupport": false } },
                    "hover": { "contentFormat": ["plaintext", "markdown"] },
                    "signatureHelp": {
                        "signatureInformation": {
                            "parameterInformation": { "labelOffsetSupport": true }
                        }
                    },
                    "definition": {}
                }
            }
        });
        server.starting = Some(server.request("initialize", params)?);
        Some(server)
    }

    /// Whether it answered `initialize` (waiting for it the first time), `initialized`
    /// told.
    fn ready(&mut self) -> bool {
        let Some(id) = self.starting else {
            return true;
        };
        if self.wait(id, START_WITHIN).is_none() {
            return false;
        }
        self.starting = None;
        self.tell("initialized", json!({}));
        true
    }

    /// Whether it still runs.
    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Sends `message`; false when the pipe broke.
    fn send(&mut self, message: &Value) -> bool {
        let body = message.to_string();
        let frame = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        self.stdin.write_all(frame.as_bytes()).is_ok() && self.stdin.flush().is_ok()
    }

    /// A notice to the server.
    fn tell(&mut self, method: &str, params: Value) {
        let _ = self.send(&json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    /// Asks `method` with `params`: the question's id, `None` when the pipe broke.
    fn request(&mut self, method: &str, params: Value) -> Option<i64> {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.send(&message).then_some(id)
    }

    /// Question `method`'s answer, `None` for an error or none in `within`.
    fn ask(&mut self, method: &str, params: Value, within: Duration) -> Option<Value> {
        let id = self.request(method, params)?;
        self.wait(id, within)
    }

    /// The answer to question `id`, `None` for an error or none in `within`. The server's
    /// own questions meanwhile are answered, its notices passed over.
    fn wait(&mut self, id: i64, within: Duration) -> Option<Value> {
        let until = Instant::now() + within;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            let message = self.messages.recv_timeout(left).ok()?;
            match (message.get("id"), message.get("method")) {
                // An answer: this one, or a late one to a question given up.
                (Some(got), None) => {
                    if got.as_i64() == Some(id) {
                        return message
                            .get("error")
                            .is_none()
                            .then(|| message.get("result").cloned().unwrap_or(Value::Null));
                    }
                }
                (Some(question), Some(asked)) => {
                    let result = if asked.as_str() == Some("workspace/configuration") {
                        let items = message.pointer("/params/items").and_then(Value::as_array);
                        let answers = items.map_or_else(Vec::new, |items| {
                            items
                                .iter()
                                .map(|item| {
                                    let section = item.get("section").and_then(Value::as_str);
                                    match section {
                                        Some("slang.additionalSearchPaths") => self.search.clone(),
                                        _ => Value::Null,
                                    }
                                })
                                .collect()
                        });
                        Value::Array(answers)
                    } else {
                        Value::Null
                    };
                    let answer = json!({ "jsonrpc": "2.0", "id": question, "result": result });
                    if !self.send(&answer) {
                        return None;
                    }
                }
                _ => {}
            }
        }
    }

    /// The server's copy of document `uri` made `text`.
    fn sync(&mut self, uri: &str, text: &str) {
        let (method, params) = match self.opened.get_mut(uri) {
            Some(o) if o.text == text => return,
            Some(o) => {
                let (start, end, new) = changed(&o.text, text);
                o.version += 1;
                let params = json!({
                    "textDocument": { "uri": uri, "version": o.version },
                    "contentChanges": [{ "range": { "start": start, "end": end }, "text": new }],
                });
                text.clone_into(&mut o.text);
                ("textDocument/didChange", params)
            }
            None => {
                let opened = Opened {
                    text: text.to_owned(),
                    version: 1,
                };
                self.opened.insert(uri.to_owned(), opened);
                let params = json!({
                    "textDocument": { "uri": uri, "languageId": "slang", "version": 1, "text": text }
                });
                ("textDocument/didOpen", params)
            }
        };
        self.tell(method, params);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.starting.is_none() {
            let _ = self.ask("shutdown", Value::Null, Duration::from_millis(500));
            self.tell("exit", Value::Null);
        }
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// The server's messages, read from its output into `out` until it closes.
fn read_messages(stdout: impl Read, out: &mpsc::Sender<Value>) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        let mut length = None;
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let header = line.trim_end();
            if header.is_empty() {
                break;
            }
            let (name, value) = header.split_once(':').unwrap_or((header, ""));
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse::<usize>().ok();
            }
        }
        let Some(length) = length else {
            continue;
        };
        let mut body = vec![0u8; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        if let Ok(message) = serde_json::from_slice::<Value>(&body)
            && out.send(message).is_err()
        {
            return;
        }
    }
}

/// The part of `new` that is not `old`'s: the range of `old` it takes the place of, as
/// LSP's places, and its text.
fn changed<'a>(old: &str, new: &'a str) -> (Value, Value, &'a str) {
    let mut a = old
        .bytes()
        .zip(new.bytes())
        .take_while(|(x, y)| x == y)
        .count();
    while !old.is_char_boundary(a) {
        a -= 1;
    }
    let most = old.len().min(new.len()) - a;
    let mut b = old
        .bytes()
        .rev()
        .zip(new.bytes().rev())
        .take(most)
        .take_while(|(x, y)| x == y)
        .count();
    while !old.is_char_boundary(old.len() - b) || !new.is_char_boundary(new.len() - b) {
        b -= 1;
    }
    let (start, end) = (place_in(old, a), place_in(old, old.len() - b));
    (start, end, &new[a..new.len() - b])
}

/// Byte `byte` of `text` as LSP's place: a line and the UTF-16 units before it in the line.
fn place_in(text: &str, byte: usize) -> Value {
    let before = &text[..byte];
    let line = before.matches('\n').count();
    let start = before.rfind('\n').map_or(0, |n| n + 1);
    let column: usize = before[start..].chars().map(char::len_utf16).sum();
    json!({ "line": line, "character": column })
}

/// A file's `file:` URI (`file:///H:/game/water.slang`).
fn uri_of(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file:///");
    for c in text.trim_start_matches('/').chars() {
        match c {
            ' ' => out.push_str("%20"),
            '#' => out.push_str("%23"),
            '%' => out.push_str("%25"),
            c => out.push(c),
        }
    }
    out
}

/// The path of a `file:` URI, the server's escapes undone (`file:///C%3A/…`); `None` for
/// another scheme (the core module's `slang-synth:`).
fn path_of(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file:///")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = rest.get(i + 1..i + 3)
            && let Ok(b) = u8::from_str_radix(hex, 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).ok().map(PathBuf::from)
}

/// The UTF-16 column of byte `byte` of line `line` in `text`.
fn utf16_of(text: &str, line: u32, byte: u32) -> u32 {
    let Some(l) = text.split('\n').nth(line as usize) else {
        return 0;
    };
    let end = (byte as usize).min(l.len());
    l.char_indices()
        .take_while(|&(i, _)| i < end)
        .map(|(_, c)| c.len_utf16() as u32)
        .sum()
}

/// The byte of UTF-16 column `column` of line `line` in `text`.
fn byte_of(text: &str, line: u32, column: u32) -> u32 {
    let Some(l) = text.split('\n').nth(line as usize) else {
        return 0;
    };
    let mut units = 0u32;
    for (i, c) in l.char_indices() {
        if units >= column {
            return i as u32;
        }
        units += c.len_utf16() as u32;
    }
    l.len() as u32
}

/// The byte range of UTF-16 units `start..end` in `label`.
fn label_bytes(label: &str, start: usize, end: usize) -> (usize, usize) {
    let mut units = 0usize;
    let (mut a, mut b) = (label.len(), label.len());
    for (i, c) in label.char_indices() {
        if units == start {
            a = i;
        }
        if units == end {
            b = i;
            break;
        }
        units += c.len_utf16();
    }
    (a.min(b), b)
}

/// What a hover shows: the declaration, then its documentation; code fences and the
/// server's "Defined in" left out (Go to Definition goes there).
fn hover_text(contents: &Value) -> Option<String> {
    let raw = match contents {
        Value::String(s) => s.clone(),
        Value::Object(o) => o.get("value")?.as_str()?.to_owned(),
        Value::Array(list) => list
            .iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.clone()),
                Value::Object(o) => o.get("value").and_then(Value::as_str).map(str::to_owned),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let lines: Vec<&str> = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("```") && !l.starts_with("Defined in "))
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// What the editor shows of a completion item.
fn completion_of(item: &Value) -> Option<Completion> {
    let label = item.get("label")?.as_str()?.to_owned();
    // LSP's kinds: 2 method, 3 function, 4 constructor, 5 field, 6 variable, 7 class,
    // 8 interface, 9 module, 10 property, 12 value, 13 enum, 14 keyword, 20 enum member,
    // 21 constant, 22 struct, 25 type parameter.
    let code = item.get("kind").and_then(Value::as_u64).unwrap_or(1);
    let kind = match code {
        2 | 5 | 10 | 20 => CompletionKind::Property,
        3 | 4 | 6 | 12 | 21 => CompletionKind::Binding,
        7 | 8 | 13 | 22 | 25 => CompletionKind::Type,
        9 => CompletionKind::Module,
        14 => CompletionKind::Keyword,
        _ => CompletionKind::Other,
    };
    Some(Completion {
        label,
        kind,
        detail: item
            .get("detail")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        fits: false,
        call: matches!(code, 2..=4),
        no_arguments: false,
        deprecated: item.get("deprecated").and_then(Value::as_bool) == Some(true),
    })
}

/// A module as the server reads it.
struct Document {
    uri: String,
    path: PathBuf,
    text: String,
    /// The lines before the module's own text (a material type's prelude).
    shift: u32,
}

/// The project's Slang modules through `slangd` (see the module's documentation).
pub(crate) struct Slangd {
    exe: Option<PathBuf>,
    root: PathBuf,
    /// The engine's shader folder.
    engine: PathBuf,
    /// Where a material type's document is written.
    copies: PathBuf,
    /// `forward.slang`'s text up to where it includes a material, and its lines.
    prelude: Option<(String, u32)>,
    server: Option<Server>,
    /// Said once in the log: no server to ask.
    said: bool,
}

impl Slangd {
    /// `slangd` beside Slang's library at `library` (the pinned build's or the Vulkan
    /// SDK's), for the project in `root` and the engine's shaders in `engine`; a material
    /// type's document in `copies`.
    pub(crate) fn new(library: &Path, root: &Path, engine: &Path, copies: &Path) -> Self {
        let exe = library
            .parent()
            .map(|dir| dir.join("slangd.exe"))
            .filter(|p| p.is_file());
        Self {
            exe,
            root: root.to_path_buf(),
            engine: engine.to_path_buf(),
            copies: copies.to_path_buf(),
            prelude: None,
            server: None,
            said: false,
        }
    }

    /// Whether `slangd` is beside the library.
    #[cfg(test)]
    pub(crate) fn found(&self) -> bool {
        self.exe.is_some()
    }

    /// Starts the server if it does not run, not waiting for it: its start overlaps what
    /// the checker does next.
    pub(crate) fn warm(&mut self) {
        if self.server.as_mut().is_some_and(|s| !s.alive()) {
            self.server = None;
        }
        if self.server.is_some() {
            return;
        }
        let Some(exe) = &self.exe else {
            if !self.said {
                self.said = true;
                log::warn!(
                    target: "engine_editor",
                    "slangd was not found beside Slang's library: Slang tabs have no completion"
                );
            }
            return;
        };
        self.server = Server::start(exe, &self.root, std::slice::from_ref(&self.engine));
    }

    /// The running server, started and ready.
    fn server(&mut self) -> Option<&mut Server> {
        self.warm();
        let ready = self.server.as_mut()?.ready();
        if !ready {
            log::warn!(target: "engine_editor", "slangd did not start in time");
            self.server = None;
        }
        self.server.as_mut()
    }

    /// Module `name`'s document for `text`: inside the prelude for a material type.
    fn document(&mut self, name: &str, text: &str, material: bool) -> Option<Document> {
        if !material {
            let path = self.root.join(name);
            return Some(Document {
                uri: uri_of(&path),
                path,
                text: text.to_owned(),
                shift: 0,
            });
        }
        if self.prelude.is_none() {
            let forward = std::fs::read_to_string(self.engine.join("forward.slang")).ok()?;
            let end = forward.find("#ifdef MATERIAL_FILE")?;
            let prelude = forward[..end].to_owned();
            let lines = prelude.matches('\n').count() as u32;
            self.prelude = Some((prelude, lines));
        }
        let (prelude, shift) = self.prelude.as_ref()?;
        let path = self
            .copies
            .join(format!("editing.{}", name.replace(['/', '\\', ':'], ".")));
        let text = format!("{prelude}{text}");
        // On disk once: the server gives definitions only in files that exist.
        if !path.is_file() {
            let written =
                std::fs::create_dir_all(&self.copies).and_then(|()| std::fs::write(&path, &text));
            if let Err(e) = written {
                log::warn!(target: "engine_editor", "{}: {e}", path.display());
            }
        }
        Some(Document {
            uri: uri_of(&path),
            path,
            text,
            shift: *shift,
        })
    }

    /// Asks `method` about the place (`line`, `byte`) of module `name` holding `text`.
    fn ask_at(
        &mut self,
        method: &str,
        name: &str,
        text: &str,
        material: bool,
        (line, byte): (u32, u32),
    ) -> Option<(Value, Document)> {
        let doc = self.document(name, text, material)?;
        let column = utf16_of(text, line, byte);
        let server = self.server()?;
        server.sync(&doc.uri, &doc.text);
        let params = json!({
            "textDocument": { "uri": doc.uri },
            "position": { "line": line + doc.shift, "character": column },
        });
        let answer = server.ask(method, params, ANSWER_WITHIN)?;
        Some((answer, doc))
    }

    /// What may be written at the place `at` of module `name` holding `text` (a material
    /// type when `material`).
    pub(crate) fn complete(
        &mut self,
        name: &str,
        text: &str,
        material: bool,
        at: (u32, u32),
    ) -> Vec<Completion> {
        let Some((answer, _)) = self.ask_at("textDocument/completion", name, text, material, at)
        else {
            return Vec::new();
        };
        let items = match &answer {
            Value::Array(items) => items.as_slice(),
            Value::Object(list) => list
                .get("items")
                .and_then(Value::as_array)
                .map_or(&[][..], Vec::as_slice),
            _ => &[],
        };
        items.iter().filter_map(completion_of).collect()
    }

    /// What is at the place.
    pub(crate) fn hover(
        &mut self,
        name: &str,
        text: &str,
        material: bool,
        at: (u32, u32),
    ) -> Option<String> {
        let (answer, _) = self.ask_at("textDocument/hover", name, text, material, at)?;
        hover_text(answer.get("contents")?)
    }

    /// The signature of the call the place is inside, and the byte range of its argument
    /// there in it.
    pub(crate) fn signature(
        &mut self,
        name: &str,
        text: &str,
        material: bool,
        at: (u32, u32),
    ) -> Option<(String, (usize, usize))> {
        let (answer, _) = self.ask_at("textDocument/signatureHelp", name, text, material, at)?;
        let signatures = answer.get("signatures")?.as_array()?;
        let active = answer
            .get("activeSignature")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let signature = signatures.get(active).or(signatures.first())?;
        let label = signature.get("label")?.as_str()?.to_owned();
        let parameter = signature
            .get("activeParameter")
            .or(answer.get("activeParameter"))
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let range = signature
            .get("parameters")
            .and_then(Value::as_array)
            .and_then(|p| p.get(parameter))
            .and_then(|p| p.get("label"))
            .and_then(|l| match l {
                Value::String(s) => label.find(s.as_str()).map(|a| (a, a + s.len())),
                Value::Array(pair) => {
                    let start = pair.first()?.as_u64()? as usize;
                    let end = pair.get(1)?.as_u64()? as usize;
                    Some(label_bytes(&label, start, end))
                }
                _ => None,
            })
            .unwrap_or((0, 0));
        Some((label, range))
    }

    /// Where what is at the place is defined: the module (a path in the project, or
    /// another file's whole path), its line and byte. `None` for the language's own (its
    /// core module) and for nothing found.
    pub(crate) fn definition(
        &mut self,
        name: &str,
        text: &str,
        material: bool,
        at: (u32, u32),
    ) -> Option<(String, u32, u32)> {
        let (answer, doc) = self.ask_at("textDocument/definition", name, text, material, at)?;
        let place = match answer {
            Value::Array(list) => list.into_iter().next()?,
            other => other,
        };
        let uri = place.get("uri").or(place.get("targetUri"))?.as_str()?;
        let range = place.get("range").or(place.get("targetSelectionRange"))?;
        let line = range.pointer("/start/line")?.as_u64()? as u32;
        let column = range.pointer("/start/character")?.as_u64()? as u32;
        let path = path_of(uri)?;
        if path_key(&path) == path_key(&doc.path) {
            if line >= doc.shift {
                let line = line - doc.shift;
                return Some((name.to_owned(), line, byte_of(text, line, column)));
            }
            // In the prelude: `forward.slang`'s own line.
            let (prelude, _) = self.prelude.as_ref()?;
            let forward = self.engine.join("forward.slang");
            let byte = byte_of(prelude, line, column);
            return Some((forward.to_string_lossy().replace('\\', "/"), line, byte));
        }
        let source = std::fs::read_to_string(&path).unwrap_or_default();
        let byte = byte_of(&source, line, column);
        let shown = match path.strip_prefix(&self.root) {
            Ok(relative) => relative.to_path_buf(),
            Err(_) => path,
        };
        Some((shown.to_string_lossy().replace('\\', "/"), line, byte))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_go_between_bytes_and_utf16() {
        let text = "a\n\u{e9}t\u{1f600}x = 1\n";
        // `é` is two bytes, one unit; the emoji four bytes, two units.
        assert_eq!(utf16_of(text, 1, 0), 0);
        assert_eq!(utf16_of(text, 1, 2), 1);
        assert_eq!(utf16_of(text, 1, 7), 4);
        assert_eq!(byte_of(text, 1, 4), 7);
        assert_eq!(byte_of(text, 1, 1), 2);
        assert_eq!(label_bytes("f(\u{e9}a, b)", 2, 4), (2, 5));
        let uri = uri_of(Path::new("H:\\my game\\water.slang"));
        assert_eq!(uri, "file:///H:/my%20game/water.slang");
        assert_eq!(path_of(&uri), Some(PathBuf::from("H:/my game/water.slang")));
        // The server's own escapes.
        assert_eq!(
            path_of("file:///C%3A/game/water.slang"),
            Some(PathBuf::from("C:/game/water.slang"))
        );
        assert_eq!(path_of("slang-synth://core/core.builtin"), None);
    }

    #[test]
    fn a_change_goes_as_the_range_that_changed() {
        let old = "struct S { float a; };\nfloat f(S s)\n{\n    return s.a;\n}\n";
        let new = "struct S { float a; };\nfloat f(S s)\n{\n    s.\n    return s.a;\n}\n";
        let (start, end, text) = changed(old, new);
        assert_eq!(start, json!({ "line": 3, "character": 4 }));
        assert_eq!(end, json!({ "line": 3, "character": 4 }));
        assert_eq!(text, "s.\n    ");
        // Units, not bytes, after a two-byte letter; a deletion is an empty text.
        let (start, end, text) = changed("// \u{e9}ab\n", "// \u{e9}b\n");
        assert_eq!(start, json!({ "line": 0, "character": 4 }));
        assert_eq!(end, json!({ "line": 0, "character": 5 }));
        assert_eq!(text, "");
        // Within a letter: whole letters.
        let (start, end, text) = changed("x\u{e9}y", "x\u{e8}y");
        assert_eq!(start, json!({ "line": 0, "character": 1 }));
        assert_eq!(end, json!({ "line": 0, "character": 2 }));
        assert_eq!(text, "\u{e8}");
        assert_eq!(changed("same", "same").2, "");
    }

    #[test]
    fn hovers_drop_their_code_fences_and_where_it_is_defined() {
        let contents = json!({
            "kind": "markdown",
            "value": "```\n(field) float4 Surface.base\n```\n\nlinear base color and alpha (coverage)  \n\nDefined in .ofelia\\check\\editing.water.slang(1816)\n"
        });
        assert_eq!(
            hover_text(&contents).as_deref(),
            Some("(field) float4 Surface.base\nlinear base color and alpha (coverage)")
        );
        let c = completion_of(&json!({ "label": "sample", "kind": 2 })).unwrap();
        assert_eq!((c.kind, c.call), (CompletionKind::Property, true));
    }
}
