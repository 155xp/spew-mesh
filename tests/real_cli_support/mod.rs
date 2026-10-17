//! Shared helpers for the opt-in checks against the real Claude Code, Codex and OpenCode (15). A check runs
//! only with `SPEW_MESH_REAL_CLI=1` and the CLI on `PATH`, and prints why it skipped otherwise.
//!
//! Every check gets fresh temp folders for the home and each client's config, and runs the CLI with a clean
//! environment. The model is a mock on 127.0.0.1 that answers with scripted turns and records what it was
//! sent. The proxy variables point at the mock too, so a request a CLI tries to send anywhere else lands
//! there, is refused, and fails the check.

#![allow(dead_code)]

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use spew_mesh::responder::process::{ProcessRunner, RunOutput, RunSpec, SystemRunner};
use spew_mesh::store::{SessionRecord, Store};

/// The variable that turns the checks on.
pub const GATE: &str = "SPEW_MESH_REAL_CLI";

/// The CLI's path when the check should run; prints why it skips otherwise.
pub fn cli(test: &str, name: &str) -> Option<PathBuf> {
    if std::env::var(GATE).as_deref() != Ok("1") {
        eprintln!("{test}: skipped; set {GATE}=1 to run it against the real `{name}`");
        return None;
    }
    let found = on_path(name);
    if found.is_none() {
        eprintln!("{test}: skipped; `{name}` isn't on PATH");
    }
    found
}

/// The first executable called `name` on the test's own `PATH`.
pub fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| {
            std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// A unique marker for a prompt or a canary, so a check can tell its texts apart in what the model saw.
pub fn token(label: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!(
        "{label}-{:x}{:x}",
        NEXT.fetch_add(1, Ordering::Relaxed),
        nanos
    )
}

// ---------------------------------------------------------------------------------------------------------
// The mock model.

/// Which API a request used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    /// Anthropic Messages (Claude Code).
    Anthropic,
    /// OpenAI Responses (Codex).
    Responses,
    /// OpenAI-compatible chat completions (OpenCode).
    Chat,
}

/// One model request the mock got.
#[derive(Clone, Debug)]
pub struct Request {
    pub api: Api,
    pub path: String,
    pub body: Value,
}

impl Request {
    /// Whether the request offers tools (the main loop; side requests such as titles don't).
    pub fn has_tools(&self) -> bool {
        self.body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|t| !t.is_empty())
    }

    /// The whole body as text.
    pub fn text(&self) -> String {
        self.body.to_string()
    }

    /// Whether `needle` appears anywhere in the body.
    pub fn contains(&self, needle: &str) -> bool {
        self.text().contains(needle)
    }

    /// The names of the tools offered.
    pub fn tool_names(&self) -> Vec<String> {
        let tools = self.body.get("tools").and_then(Value::as_array);
        tools
            .into_iter()
            .flatten()
            .filter_map(|t| {
                t.get("name")
                    .or_else(|| t.get("function").and_then(|f| f.get("name")))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect()
    }

    /// The last prompt holding one of `markers` (its index) and how many tool calls the model made after it.
    /// Only text a person or the client wrote counts, never a tool result.
    pub fn turn(&self, markers: &[&str]) -> Option<(usize, usize)> {
        let mut current: Option<(usize, usize)> = None;
        let seen = |text: &str, current: &mut Option<(usize, usize)>| {
            if let Some(i) = markers.iter().position(|m| text.contains(m)) {
                *current = Some((i, 0));
            }
        };
        let bump = |current: &mut Option<(usize, usize)>, n: usize| {
            if let Some((_, calls)) = current {
                *calls += n;
            }
        };
        match self.api {
            Api::Anthropic => {
                for m in self.items("messages") {
                    let role = m.get("role").and_then(Value::as_str);
                    let content = m.get("content");
                    match (role, content) {
                        (Some("user"), Some(Value::String(s))) => seen(s, &mut current),
                        (Some("user"), Some(Value::Array(blocks))) => {
                            for b in blocks {
                                if b.get("type").and_then(Value::as_str) == Some("text") {
                                    seen(str_of(b, "text"), &mut current);
                                }
                            }
                        }
                        (Some("assistant"), Some(Value::Array(blocks))) => {
                            let n = blocks
                                .iter()
                                .filter(|b| {
                                    b.get("type").and_then(Value::as_str) == Some("tool_use")
                                })
                                .count();
                            bump(&mut current, n);
                        }
                        _ => {}
                    }
                }
            }
            Api::Responses => {
                for item in self.items("input") {
                    let kind = item
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("message");
                    match kind {
                        "message" if item.get("role").and_then(Value::as_str) == Some("user") => {
                            seen(&content_text(item.get("content")), &mut current)
                        }
                        "function_call" | "custom_tool_call" | "local_shell_call" => {
                            bump(&mut current, 1)
                        }
                        _ => {}
                    }
                }
            }
            Api::Chat => {
                for m in self.items("messages") {
                    match m.get("role").and_then(Value::as_str) {
                        Some("user") => seen(&content_text(m.get("content")), &mut current),
                        Some("assistant") => {
                            let n = m
                                .get("tool_calls")
                                .and_then(Value::as_array)
                                .map_or(0, Vec::len);
                            bump(&mut current, n);
                        }
                        _ => {}
                    }
                }
            }
        }
        current
    }

    /// The text of every tool result in the request, in order.
    pub fn tool_results(&self) -> Vec<String> {
        let mut out = Vec::new();
        match self.api {
            Api::Anthropic => {
                for m in self.items("messages") {
                    for b in m
                        .get("content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        if b.get("type").and_then(Value::as_str) == Some("tool_result") {
                            out.push(match b.get("content") {
                                Some(Value::String(s)) => s.clone(),
                                other => content_text(other),
                            });
                        }
                    }
                }
            }
            Api::Responses => {
                for item in self.items("input") {
                    if item.get("type").and_then(Value::as_str) == Some("function_call_output") {
                        out.push(match item.get("output") {
                            Some(Value::String(s)) => s.clone(),
                            Some(other) => other.to_string(),
                            None => String::new(),
                        });
                    }
                }
            }
            Api::Chat => {
                for m in self.items("messages") {
                    if m.get("role").and_then(Value::as_str) == Some("tool") {
                        out.push(content_text(m.get("content")));
                    }
                }
            }
        }
        out
    }

    fn items(&self, key: &str) -> impl Iterator<Item = &Value> {
        self.body
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
    }
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// The text of a message's content: a string, or the text of each block.
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// What the mock answers.
#[derive(Clone, Debug)]
pub enum Reply {
    /// A text answer that ends the turn.
    Text(String),
    /// A call to a tool. `namespace` is for Codex, which files MCP tools under `mcp__<server>`.
    Tool {
        name: String,
        args: Value,
        namespace: Option<String>,
    },
    /// The same reply, sent after a pause.
    After(Duration, Box<Reply>),
}

/// A text reply.
pub fn say(text: &str) -> Reply {
    Reply::Text(text.to_string())
}

/// A tool call.
pub fn call(name: &str, args: Value) -> Reply {
    Reply::Tool {
        name: name.to_string(),
        args,
        namespace: None,
    }
}

/// A call to a tool in a Codex namespace.
pub fn call_in(namespace: &str, name: &str, args: Value) -> Reply {
    Reply::Tool {
        name: name.to_string(),
        args,
        namespace: Some(namespace.to_string()),
    }
}

/// Picks the reply to a request.
pub type Script = Arc<dyn Fn(&Request) -> Reply + Send + Sync>;

/// The mock model server.
pub struct Mock {
    pub port: u16,
    log: Arc<Mutex<Vec<Request>>>,
    escaped: Arc<Mutex<Vec<String>>>,
}

impl Mock {
    /// Starts the server on an ephemeral 127.0.0.1 port.
    pub fn start(script: Script) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let log = Arc::new(Mutex::new(Vec::new()));
        let escaped = Arc::new(Mutex::new(Vec::new()));
        let (l, e) = (log.clone(), escaped.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (script, l, e) = (script.clone(), l.clone(), e.clone());
                std::thread::spawn(move || {
                    let _ = serve_one(stream, port, &script, &l, &e);
                });
            }
        });
        Mock { port, log, escaped }
    }

    /// `http://127.0.0.1:<port>`.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Every model request so far.
    pub fn requests(&self) -> Vec<Request> {
        self.log.lock().map(|l| l.clone()).unwrap_or_default()
    }

    /// The main-loop requests for the prompt holding `marker`.
    pub fn requests_for(&self, marker: &str) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(|r| r.has_tools() && r.turn(&[marker]).is_some())
            .collect()
    }

    /// Requests a CLI tried to send somewhere other than the mock.
    pub fn escaped(&self) -> Vec<String> {
        self.escaped.lock().map(|e| e.clone()).unwrap_or_default()
    }

    /// Waits until some request satisfies `check`.
    pub fn wait_for(&self, what: &str, limit: Duration, check: impl Fn(&Request) -> bool) {
        let deadline = Instant::now() + limit;
        while !self.requests().iter().any(&check) {
            assert!(Instant::now() < deadline, "the mock never saw {what}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn serve_one(
    stream: TcpStream,
    port: u16,
    script: &Script,
    log: &Mutex<Vec<Request>>,
    escaped: &Mutex<Vec<String>>,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut out = stream;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let mut target = parts.next().unwrap_or("").to_string();
    let mut length = 0usize;
    let mut chunked = false;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            if k == "content-length" {
                length = v.parse().unwrap_or(0);
            } else if k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked") {
                chunked = true;
            }
        }
    }
    // A request through the proxy variables: one for the mock itself is fine, anything else escaped.
    if method == "CONNECT" || target.starts_with("http://") || target.starts_with("https://") {
        let rest = target
            .trim_start_matches("http://")
            .trim_start_matches("https://");
        let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
        let local = [format!("127.0.0.1:{port}"), format!("localhost:{port}")];
        if method == "CONNECT" || !local.iter().any(|l| l == host) {
            if let Ok(mut e) = escaped.lock() {
                e.push(format!("{method} {host}"));
            }
            return respond(&mut out, 403, "text/plain", b"blocked by the test");
        }
        target = format!("/{path}");
    }
    let body = if chunked {
        read_chunked(&mut reader)?
    } else {
        let mut b = vec![0; length];
        reader.read_exact(&mut b)?;
        b
    };
    let path = target.split('?').next().unwrap_or("").to_string();
    if method != "POST" {
        return respond(&mut out, 404, "text/plain", b"");
    }
    let api = if path.ends_with("/messages/count_tokens") {
        return respond(&mut out, 200, "application/json", br#"{"input_tokens":10}"#);
    } else if path.ends_with("/messages") {
        Api::Anthropic
    } else if path.ends_with("/responses") {
        Api::Responses
    } else if path.ends_with("/chat/completions") {
        Api::Chat
    } else {
        return respond(&mut out, 404, "text/plain", b"");
    };
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let request = Request { api, path, body };
    if let Ok(mut l) = log.lock() {
        l.push(request.clone());
    }
    let mut reply = script(&request);
    while let Reply::After(pause, inner) = reply {
        std::thread::sleep(pause);
        reply = *inner;
    }
    let stream = request.body.get("stream").and_then(Value::as_bool) == Some(true);
    match api {
        Api::Anthropic => anthropic(&mut out, &request, &reply, stream),
        Api::Responses => responses(&mut out, &reply),
        Api::Chat => chat(&mut out, &request, &reply, stream),
    }
}

fn read_chunked(reader: &mut impl BufRead) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    loop {
        let mut size = String::new();
        reader.read_line(&mut size)?;
        let n =
            usize::from_str_radix(size.trim().split(';').next().unwrap_or("0"), 16).unwrap_or(0);
        if n == 0 {
            let mut end = String::new();
            while reader.read_line(&mut end)? > 0 && !end.trim().is_empty() {
                end.clear();
            }
            return Ok(body);
        }
        let mut chunk = vec![0; n + 2];
        reader.read_exact(&mut chunk)?;
        chunk.truncate(n);
        body.extend(chunk);
    }
}

fn respond(out: &mut TcpStream, status: u16, kind: &str, body: &[u8]) -> std::io::Result<()> {
    write!(
        out,
        "HTTP/1.1 {status} X\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )?;
    out.write_all(body)?;
    out.flush()
}

struct Sse<'a>(&'a mut TcpStream);

impl Sse<'_> {
    fn start(out: &mut TcpStream) -> std::io::Result<Sse<'_>> {
        write!(
            out,
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n"
        )?;
        Ok(Sse(out))
    }

    fn event(&mut self, name: Option<&str>, data: &Value) -> std::io::Result<()> {
        if let Some(name) = name {
            writeln!(self.0, "event: {name}")?;
        }
        write!(self.0, "data: {data}\n\n")?;
        self.0.flush()
    }

    fn done(&mut self) -> std::io::Result<()> {
        write!(self.0, "data: [DONE]\n\n")?;
        self.0.flush()
    }
}

fn next_id(prefix: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    format!("{prefix}{:016x}", NEXT.fetch_add(1, Ordering::Relaxed))
}

fn anthropic(out: &mut TcpStream, r: &Request, reply: &Reply, stream: bool) -> std::io::Result<()> {
    let model = r.body.get("model").cloned().unwrap_or(json!("mock"));
    let (block, stop) = match reply {
        Reply::Tool { name, args, .. } => (
            json!({"type": "tool_use", "id": next_id("toolu_"), "name": name, "input": args}),
            "tool_use",
        ),
        Reply::Text(t) => (json!({"type": "text", "text": t}), "end_turn"),
        Reply::After(..) => (json!({"type": "text", "text": ""}), "end_turn"),
    };
    let usage = json!({"input_tokens": 10, "output_tokens": 5, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0});
    let id = next_id("msg_");
    if !stream {
        let body = json!({"id": id, "type": "message", "role": "assistant", "model": model, "content": [block],
                          "stop_reason": stop, "stop_sequence": null, "usage": usage});
        return respond(out, 200, "application/json", body.to_string().as_bytes());
    }
    let mut s = Sse::start(out)?;
    s.event(
        Some("message_start"),
        &json!({"type": "message_start", "message": {"id": id, "type": "message", "role": "assistant",
                "model": model, "content": [], "stop_reason": null, "stop_sequence": null, "usage": usage}}),
    )?;
    let (start, delta) = if block["type"] == "tool_use" {
        (
            json!({"type": "tool_use", "id": block["id"], "name": block["name"], "input": {}}),
            json!({"type": "input_json_delta", "partial_json": block["input"].to_string()}),
        )
    } else {
        (
            json!({"type": "text", "text": ""}),
            json!({"type": "text_delta", "text": block["text"]}),
        )
    };
    s.event(
        Some("content_block_start"),
        &json!({"type": "content_block_start", "index": 0, "content_block": start}),
    )?;
    s.event(
        Some("content_block_delta"),
        &json!({"type": "content_block_delta", "index": 0, "delta": delta}),
    )?;
    s.event(
        Some("content_block_stop"),
        &json!({"type": "content_block_stop", "index": 0}),
    )?;
    s.event(
        Some("message_delta"),
        &json!({"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": null},
                "usage": {"output_tokens": 5}}),
    )?;
    s.event(Some("message_stop"), &json!({"type": "message_stop"}))
}

fn responses(out: &mut TcpStream, reply: &Reply) -> std::io::Result<()> {
    let id = next_id("resp_");
    let item = match reply {
        Reply::Tool {
            name,
            args,
            namespace,
        } => {
            let mut item = json!({"type": "function_call", "id": next_id("fc_"), "call_id": next_id("call_"),
                                  "name": name, "arguments": args.to_string()});
            if let Some(ns) = namespace {
                item["namespace"] = json!(ns);
            }
            item
        }
        Reply::Text(t) => json!({"type": "message", "role": "assistant", "id": next_id("msg_"),
                                 "content": [{"type": "output_text", "text": t, "annotations": []}]}),
        Reply::After(..) => {
            json!({"type": "message", "role": "assistant", "id": next_id("msg_"), "content": []})
        }
    };
    let mut s = Sse::start(out)?;
    s.event(
        Some("response.created"),
        &json!({"type": "response.created", "response": {"id": id}}),
    )?;
    s.event(
        Some("response.output_item.added"),
        &json!({"type": "response.output_item.added", "output_index": 0, "item": item}),
    )?;
    s.event(
        Some("response.output_item.done"),
        &json!({"type": "response.output_item.done", "output_index": 0, "item": item}),
    )?;
    s.event(
        Some("response.completed"),
        &json!({"type": "response.completed", "response": {"id": id, "usage": {
            "input_tokens": 10, "input_tokens_details": {"cached_tokens": 0}, "output_tokens": 5,
            "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 15}}}),
    )
}

fn chat(out: &mut TcpStream, r: &Request, reply: &Reply, stream: bool) -> std::io::Result<()> {
    let id = next_id("chatcmpl-");
    let model = r.body.get("model").cloned().unwrap_or(json!("mock"));
    let usage = json!({"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15});
    let (message, finish) = match reply {
        Reply::Tool { name, args, .. } => (
            json!({"role": "assistant", "content": null, "tool_calls": [{"index": 0, "id": next_id("call_"),
                   "type": "function", "function": {"name": name, "arguments": args.to_string()}}]}),
            "tool_calls",
        ),
        Reply::Text(t) => (json!({"role": "assistant", "content": t}), "stop"),
        Reply::After(..) => (json!({"role": "assistant", "content": ""}), "stop"),
    };
    if !stream {
        let body = json!({"id": id, "object": "chat.completion", "created": 0, "model": model,
                          "choices": [{"index": 0, "message": message, "finish_reason": finish}], "usage": usage});
        return respond(out, 200, "application/json", body.to_string().as_bytes());
    }
    let base = |choices: Value, extra: Option<&Value>| {
        let mut chunk = json!({"id": id, "object": "chat.completion.chunk", "created": 0, "model": model,
                               "choices": choices});
        if let Some(u) = extra {
            chunk["usage"] = u.clone();
        }
        chunk
    };
    let mut s = Sse::start(out)?;
    s.event(
        None,
        &base(
            json!([{"index": 0, "delta": message, "finish_reason": null}]),
            None,
        ),
    )?;
    s.event(
        None,
        &base(
            json!([{"index": 0, "delta": {}, "finish_reason": finish}]),
            Some(&usage),
        ),
    )?;
    s.done()
}

// ---------------------------------------------------------------------------------------------------------
// The sandbox: temp folders, a clean environment and the mock.

/// Fresh folders for one check, the mock model, and the CLI under test.
pub struct Sandbox {
    pub tmp: tempfile::TempDir,
    pub mock: Mock,
    pub cli: PathBuf,
}

const DIRS: &[&str] = &[
    "home",
    "claude",
    "codex",
    "xdg/config",
    "xdg/data",
    "xdg/cache",
    "xdg/state",
    "mesh",
    "bin",
    "work",
];

impl Sandbox {
    /// Folders, the mock with `script`, and `bin/` holding the CLI (and `node`, which the npm wrappers need).
    pub fn new(cli: &Path, script: impl Fn(&Request) -> Reply + Send + Sync + 'static) -> Sandbox {
        let tmp = tempfile::tempdir().unwrap();
        for d in DIRS {
            std::fs::create_dir_all(tmp.path().join(d)).unwrap();
        }
        let bin = tmp.path().join("bin");
        let name = cli.file_name().unwrap();
        std::os::unix::fs::symlink(cli, bin.join(name)).unwrap();
        if let Some(node) = on_path("node") {
            std::os::unix::fs::symlink(node, bin.join("node")).unwrap();
        }
        Sandbox {
            tmp,
            mock: Mock::start(Arc::new(script)),
            cli: cli.to_path_buf(),
        }
    }

    /// A path inside the sandbox.
    pub fn path(&self, rel: &str) -> PathBuf {
        self.tmp.path().join(rel)
    }

    /// `work/<name>`, created.
    pub fn work(&self, name: &str) -> PathBuf {
        let p = self.path("work").join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// The whole environment of every process a check starts.
    pub fn env(&self) -> Vec<(OsString, OsString)> {
        let p = |rel: &str| self.path(rel).into_os_string();
        let mut path = p("bin");
        path.push(":/usr/bin:/bin");
        let proxy = OsString::from(self.mock.url());
        let mut env = vec![
            ("PATH".into(), path),
            ("HOME".into(), p("home")),
            ("USER".into(), "sam".into()),
            ("SHELL".into(), "/bin/sh".into()),
            ("TERM".into(), "dumb".into()),
            ("CLAUDE_CONFIG_DIR".into(), p("claude")),
            ("CODEX_HOME".into(), p("codex")),
            ("XDG_CONFIG_HOME".into(), p("xdg/config")),
            ("XDG_DATA_HOME".into(), p("xdg/data")),
            ("XDG_CACHE_HOME".into(), p("xdg/cache")),
            ("XDG_STATE_HOME".into(), p("xdg/state")),
            ("SPEW_MESH_DIR".into(), p("mesh")),
            ("SPEW_MESH_LAN".into(), "0".into()),
            ("NO_PROXY".into(), "127.0.0.1,localhost".into()),
            ("no_proxy".into(), "127.0.0.1,localhost".into()),
        ];
        for k in [
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
        ] {
            env.push((k.into(), proxy.clone()));
        }
        env
    }

    /// The environment with `extra` added or replacing what's there.
    pub fn env_with(&self, extra: &[(&str, String)]) -> Vec<(OsString, OsString)> {
        let mut env = self.env();
        for (k, v) in extra {
            env.retain(|(name, _)| name != k);
            env.push(((*k).into(), v.into()));
        }
        env
    }

    /// `program args` in `cwd` with exactly `env`.
    pub fn command(
        &self,
        program: &Path,
        args: &[&str],
        cwd: &Path,
        env: &[(OsString, OsString)],
    ) -> Command {
        let mut c = Command::new(program);
        c.args(args)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k, v)))
            .current_dir(cwd)
            .stdin(Stdio::null());
        c
    }

    /// `spew mesh <args>`.
    pub fn spew(&self, args: &[&str]) -> Output {
        self.spew_with(args, &self.env())
    }

    /// `spew mesh <args>` with exactly `env`.
    pub fn spew_with(&self, args: &[&str], env: &[(OsString, OsString)]) -> Output {
        let mut c = self.command(
            Path::new(env!("CARGO_BIN_EXE_spew")),
            &[],
            &self.path("work"),
            env,
        );
        c.arg("mesh").args(args);
        c.output().unwrap()
    }

    /// `spew mesh setup --agents <agent> --no-login` with exactly `env` (setup runs the CLI, so it gets the
    /// CLI's variables), which must succeed.
    pub fn setup(&self, agent: &str, env: &[(OsString, OsString)]) -> String {
        let out = self.spew_with(&["setup", "--agents", agent, "--no-login"], env);
        assert!(
            out.status.success(),
            "setup failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// `spew mesh tell <to> <text>`, which must be accepted.
    pub fn tell(&self, to: &str, text: &str) {
        let out = self.spew(&["tell", to, text]);
        assert!(
            out.status.success(),
            "tell failed:\n{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The live sessions in this home's store.
    pub fn live_sessions(&self) -> Vec<SessionRecord> {
        let store = Store::open(&self.path("mesh").join("store.sqlite3")).unwrap();
        store.live_sessions().unwrap()
    }

    /// Waits until the live sessions satisfy `check`, and returns them.
    pub fn wait_for_sessions(
        &self,
        what: &str,
        check: impl Fn(&[SessionRecord]) -> bool,
    ) -> Vec<SessionRecord> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let rows = self.live_sessions();
            if check(&rows) {
                return rows;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {:?}",
                rows.iter()
                    .map(|r| (&r.name, &r.client, &r.client_session, r.client_pid, r.reach))
                    .collect::<Vec<_>>()
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// The `spew mesh mcp-server` processes started for this home.
    pub fn mcp_servers(&self) -> Vec<Proc> {
        let mesh = self.path("mesh").to_string_lossy().into_owned();
        processes()
            .into_iter()
            .filter(|p| {
                p.cmdline.iter().any(|a| a == "mcp-server")
                    && p.cmdline.first().is_some_and(|a| a.ends_with("/spew"))
                    && (p.cmdline.iter().any(|a| a.contains(&mesh))
                        || p.env_value("SPEW_MESH_DIR").as_deref() == Some(mesh.as_str()))
            })
            .collect()
    }

    /// A `#!/bin/sh` script in `bin/` that copies its stdin to `log` and runs its arguments: put in front of
    /// an MCP server's command, it records what the client sends.
    pub fn tap(&self, log: &Path) -> PathBuf {
        let script = self.path("bin").join("mcp-tap");
        let text = format!(
            "#!/bin/sh\ntee -a '{}' | \"$@\"\n",
            log.to_string_lossy().replace('\'', "'\\''")
        );
        std::fs::write(&script, text).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// Runs `program` to completion with exactly `env`, `stdin`, and a timeout.
    pub fn run(
        &self,
        program: &Path,
        args: Vec<OsString>,
        cwd: &Path,
        env: Vec<(OsString, OsString)>,
        stdin: &str,
        timeout: Duration,
    ) -> RunOutput {
        block_on(SystemRunner.run(RunSpec {
            program: program.to_path_buf(),
            args,
            cwd: cwd.to_path_buf(),
            env,
            stdin: stdin.as_bytes().to_vec(),
            timeout,
            stdout_cap: 16 << 20,
        }))
        .unwrap()
    }

    /// Fails if a CLI tried to reach anything but the mock.
    pub fn assert_stayed_local(&self) {
        let escaped = self.mock.escaped();
        assert!(escaped.is_empty(), "requests left 127.0.0.1: {escaped:?}");
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = self.spew(&["daemon", "stop"]);
    }
}

/// Runs a future on a fresh single-threaded runtime.
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

/// Whether a tool result is `who`'s: its text ("You are ...") or its `structuredContent`, which some
/// clients show the model instead.
pub fn is_who(result: &str) -> bool {
    result.contains("You are ") || result.contains("\"this\":true")
}

/// The JSON-RPC messages in an MCP tap log, one per line.
pub fn tap_messages(log: &Path) -> Vec<Value> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// The first value under a key named `key` anywhere in `v`, or under a key ending in `/<key>`.
pub fn find_key<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                if k == key || k.ends_with(&format!("/{key}")) {
                    return Some(val);
                }
            }
            map.values().find_map(|val| find_key(val, key))
        }
        Value::Array(items) => items.iter().find_map(|val| find_key(val, key)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------------------------
// Processes.

/// A process, as `/proc` shows it.
#[derive(Clone)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub cmdline: Vec<String>,
    environ: Vec<String>,
}

// By hand, so a failing check never prints the environment.
impl std::fmt::Debug for Proc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Proc")
            .field("pid", &self.pid)
            .field("ppid", &self.ppid)
            .field("cmdline", &self.cmdline)
            .finish_non_exhaustive()
    }
}

impl Proc {
    /// The value of one variable in the process's environment. Only the asked-for variable is ever read
    /// out, so a failing check never prints an environment.
    pub fn env_value(&self, name: &str) -> Option<String> {
        let prefix = format!("{name}=");
        self.environ
            .iter()
            .find_map(|e| e.strip_prefix(&prefix).map(str::to_string))
    }
}

/// Every process this user can read.
pub fn processes() -> Vec<Proc> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let split = |b: Vec<u8>| -> Vec<String> {
        b.split(|c| *c == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect()
    };
    dir.flatten()
        .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(|pid| {
            let base = PathBuf::from(format!("/proc/{pid}"));
            let status = std::fs::read_to_string(base.join("status")).ok()?;
            let ppid = status
                .lines()
                .find_map(|l| l.strip_prefix("PPid:"))?
                .trim()
                .parse()
                .ok()?;
            Some(Proc {
                pid,
                ppid,
                cmdline: split(std::fs::read(base.join("cmdline")).ok()?),
                environ: split(std::fs::read(base.join("environ")).unwrap_or_default()),
            })
        })
        .collect()
}

/// Whether `pid` is `ancestor` or one of its descendants.
pub fn descends_from(pid: u32, ancestor: u32) -> bool {
    let procs = processes();
    let mut cur = pid;
    for _ in 0..64 {
        if cur == ancestor {
            return true;
        }
        match procs.iter().find(|p| p.pid == cur) {
            Some(p) if p.ppid > 1 => cur = p.ppid,
            _ => return false,
        }
    }
    false
}

// ---------------------------------------------------------------------------------------------------------
// Long-running children that talk in lines.

/// A child whose stdout is read line by line on a thread; dropping it kills the child.
pub struct LineChild {
    pub child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
}

impl LineChild {
    /// Spawns `cmd` with piped stdin and stdout, and stderr to `stderr_file`.
    pub fn spawn(mut cmd: Command, stderr_file: &Path) -> LineChild {
        let err = std::fs::File::create(stderr_file).unwrap();
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(err)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        LineChild {
            child,
            stdin,
            lines,
        }
    }

    /// The child's pid.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Writes one line to the child's stdin.
    pub fn send(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }

    /// Writes one JSON message.
    pub fn send_json(&mut self, v: &Value) {
        self.send(&v.to_string());
    }

    /// Closes stdin.
    pub fn close_stdin(&mut self) {
        self.stdin = None;
    }

    /// Reads JSON lines until one satisfies `check`; other lines go to `seen`.
    pub fn until(
        &mut self,
        what: &str,
        limit: Duration,
        seen: &mut Vec<Value>,
        check: impl Fn(&Value) -> bool,
    ) -> Value {
        let deadline = Instant::now() + limit;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => {
                    let Ok(v) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    if check(&v) {
                        return v;
                    }
                    seen.push(v);
                }
                Err(_) => panic!("timed out waiting for {what}"),
            }
        }
    }

    /// Waits for the child to exit, killing it after `limit`; true when it exited on its own.
    pub fn wait(&mut self, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        false
    }
}

impl Drop for LineChild {
    fn drop(&mut self) {
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A free port on 127.0.0.1 (bound and released; good enough for a test).
pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// One HTTP/1.1 request to 127.0.0.1 with a JSON body; returns the status and the parsed body.
pub fn http_json(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&Value>,
    limit: Duration,
) -> (u16, Value) {
    try_http_json(port, method, path, body, limit)
        .unwrap_or_else(|e| panic!("{method} {path}: {e}"))
}

/// [`http_json`], with connection errors returned.
pub fn try_http_json(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&Value>,
    limit: Duration,
) -> std::io::Result<(u16, Value)> {
    let mut s = TcpStream::connect(("127.0.0.1", port))?;
    s.set_read_timeout(Some(limit))?;
    let payload = body.map(Value::to_string).unwrap_or_default();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\ncontent-type: application/json\r\n\
         accept: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
        payload.len()
    )?;
    let mut reader = BufReader::new(s);
    let mut status = String::new();
    reader.read_line(&mut status)?;
    let code = status
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let mut length: Option<usize> = None;
    let mut chunked = false;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            if k == "content-length" {
                length = v.parse().ok();
            } else if k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked") {
                chunked = true;
            }
        }
    }
    let raw = if chunked {
        read_chunked(&mut reader)?
    } else if let Some(n) = length {
        let mut b = vec![0; n];
        reader.read_exact(&mut b)?;
        b
    } else {
        let mut b = Vec::new();
        reader.read_to_end(&mut b)?;
        b
    };
    Ok((code, serde_json::from_slice(&raw).unwrap_or(Value::Null)))
}

/// Polls `check` until it holds.
pub fn wait_until(what: &str, limit: Duration, check: impl Fn() -> bool) {
    let deadline = Instant::now() + limit;
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}
