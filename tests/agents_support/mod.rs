//! Shared helpers for the agent-wiring tests (mesh#8): a temp home whose daemon the commands start on their own,
//! `spew mesh mcp-server` driven over its stdio, and `spew mesh hook` runs with payloads on stdin. Nothing here
//! reaches the network: the roster is local, mDNS is off and `PATH` is an empty folder.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use spew_mesh::control::client::{Client, ConnectOptions};
use spew_mesh::control::protocol::{Directory, DirectoryQuery, SessionOpen, SessionRow};
use spew_mesh::paths::Home;
use spew_mesh::wire::SessionKind;

/// A temp home and working folders; dropping it stops the daemon the commands started.
pub struct Mesh {
    pub tmp: tempfile::TempDir,
}

impl Mesh {
    pub fn new() -> Mesh {
        let tmp = tempfile::tempdir().unwrap();
        for dir in ["user", "bin", "work/api", "work/web", "work/cli"] {
            std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
        }
        Mesh { tmp }
    }

    pub fn home(&self) -> PathBuf {
        self.tmp.path().join("user").join("mesh")
    }

    pub fn work(&self, name: &str) -> PathBuf {
        self.tmp.path().join("work").join(name)
    }

    pub fn work_str(&self, name: &str) -> String {
        self.work(name).to_string_lossy().into_owned()
    }

    /// `spew mesh <args>` with a clean environment pointing at this home.
    pub fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_spew"));
        c.arg("mesh")
            .args(args)
            .env_clear()
            .env("HOME", self.tmp.path().join("user"))
            .env("SPEW_MESH_DIR", self.home())
            .env("SPEW_MESH_LAN", "0")
            .env("USER", "sam")
            .env("PATH", self.tmp.path().join("bin"))
            .current_dir(self.work("api"))
            .stdin(Stdio::null());
        c
    }

    pub fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Runs `spew mesh hook <client> <event>` with `payload` on stdin and extra environment.
    pub fn hook(&self, client: &str, event: &str, payload: &Value, env: &[(&str, &str)]) -> Output {
        let mut cmd = self.command(&["hook", client, event]);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    /// Starts the daemon and waits until it answers.
    pub fn start_daemon(&self) {
        let out = self.run(&["daemon", "start"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Whether a daemon answers for this home, without starting one.
    pub async fn daemon_running(&self) -> bool {
        let home = Home::at(self.home()).unwrap();
        Client::connect(&home, &ConnectOptions::existing())
            .await
            .is_ok()
    }

    /// Kills the daemon the way a crash would: no clean shutdown and no `run/stopped`.
    pub async fn crash_daemon(&self) {
        let pid = self.control().await.status().await.unwrap().pid;
        let pid = i32::try_from(pid).unwrap();
        // SAFETY: kill(2) takes plain integers and touches no memory of ours; the pid is the test daemon's.
        #[allow(unsafe_code)]
        let r = unsafe { libc::kill(pid, libc::SIGKILL) };
        assert_eq!(r, 0, "could not kill the daemon");
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.daemon_running().await {
            assert!(Instant::now() < deadline, "the daemon outlived SIGKILL");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// `<home>/run/stopped`, which `daemon stop` writes (6.1).
    pub fn stopped_file(&self) -> PathBuf {
        self.home().join("run").join("stopped")
    }

    /// A control client of the running daemon.
    pub async fn control(&self) -> Client {
        let home = Home::at(self.home()).unwrap();
        Client::connect(&home, &ConnectOptions::existing())
            .await
            .unwrap()
    }

    /// Spawns `spew mesh mcp-server` in `cwd` with extra environment.
    pub fn mcp(&self, cwd: &str, env: &[(&str, &str)]) -> Mcp {
        let mut cmd = self.command(&["mcp-server"]);
        cmd.current_dir(self.work(cwd));
        for (k, v) in env {
            cmd.env(k, v);
        }
        Mcp::spawn(cmd)
    }

    /// The sessions on this device, as a terminal-less listing sees them.
    pub async fn sessions(&self) -> Vec<SessionRow> {
        let dir: Directory = self
            .control()
            .await
            .directory(&DirectoryQuery::default())
            .await
            .unwrap();
        dir.rooms
            .into_iter()
            .flat_map(|r| r.people)
            .flat_map(|p| p.sessions)
            .collect()
    }

    /// Waits until `check` holds for the session list, also while the daemon is still starting.
    pub async fn wait_for_sessions(&self, what: &str, check: impl Fn(&[SessionRow]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        let home = Home::at(self.home()).unwrap();
        loop {
            let mut rows = Vec::new();
            if let Ok(client) = Client::connect(&home, &ConnectOptions::existing()).await
                && let Ok(dir) = client.directory(&DirectoryQuery::default()).await
            {
                rows = dir
                    .rooms
                    .into_iter()
                    .flat_map(|r| r.people)
                    .flat_map(|p| p.sessions)
                    .collect();
                if check(&rows) {
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {rows:#?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

impl Drop for Mesh {
    fn drop(&mut self) {
        let _ = self.run(&["daemon", "stop"]);
    }
}

/// Opens a live session over the control socket, as a client with no hooks would.
pub async fn open_session(client: &Client, name_client: &str, cwd: &str) -> (String, String) {
    let opened = client
        .session_open(&SessionOpen {
            client: name_client.into(),
            client_session: None,
            client_pid: None,
            cwd: cwd.into(),
            client_name: None,
            kind: SessionKind::Live,
            name: None,
            spew_project: None,
            serve: None,
        })
        .await
        .unwrap();
    (opened.name, opened.token)
}

/// One `spew mesh mcp-server` process spoken to over stdio.
pub struct Mcp {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<Value>,
    /// Notifications that arrived while waiting for responses.
    pub notes: Vec<Value>,
    next_id: i64,
}

impl Mcp {
    /// Starts a server from a prepared `spew mesh mcp-server` command.
    pub fn spawn(mut cmd: Command) -> Mcp {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let value: Value = serde_json::from_str(&line).unwrap_or_else(|e| {
                    panic!("the server wrote a line that isn't JSON ({e}): {line}")
                });
                if tx.send(value).is_err() {
                    break;
                }
            }
        });
        Mcp {
            child,
            stdin: Some(stdin),
            lines,
            notes: Vec::new(),
            next_id: 1,
        }
    }

    pub fn send(&mut self, message: &Value) {
        let mut line = message.to_string();
        line.push('\n');
        let stdin = self.stdin.as_mut().expect("stdin is still open");
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.flush().unwrap();
    }

    /// Closes stdin, as a client that goes away does, and waits up to `limit` for the server to exit.
    pub fn close(&mut self, limit: Duration) -> ExitStatus {
        drop(self.stdin.take());
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "the server didn't exit within {limit:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Sends a request and waits up to `limit` for its response, keeping notifications that come first.
    pub fn request_within(&mut self, method: &str, params: Value, limit: Duration) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        self.response(&json!(id), limit)
    }

    pub fn request(&mut self, method: &str, params: Value) -> Value {
        self.request_within(method, params, Duration::from_secs(30))
    }

    /// Waits for the response with `id`, which may have arrived already while waiting for a notification.
    pub fn response(&mut self, id: &Value, limit: Duration) -> Value {
        let is_it = |m: &Value| m.get("id") == Some(id) && m.get("method").is_none();
        if let Some(i) = self.notes.iter().position(is_it) {
            return self.notes.remove(i);
        }
        let deadline = Instant::now() + limit;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let msg = self
                .lines
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no response to request {id} within {limit:?}"));
            if msg.get("id") == Some(id) && msg.get("method").is_none() {
                return msg;
            }
            self.notes.push(msg);
        }
    }

    /// Waits up to `limit` for a notification with `method`, keeping everything that arrives in `notes`.
    pub fn wait_for_note(&mut self, method: &str, limit: Duration) -> Value {
        if let Some(n) = self.notes.iter().find(|n| n["method"] == method) {
            return n.clone();
        }
        let deadline = Instant::now() + limit;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let msg = self
                .lines
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no {method} within {limit:?}: {:?}", self.notes));
            self.notes.push(msg.clone());
            if msg["method"] == method {
                return msg;
            }
        }
    }

    pub fn notify(&mut self, method: &str) {
        self.send(&json!({"jsonrpc": "2.0", "method": method}));
    }

    /// The legacy handshake (2025-06-18 or 2025-11-25): `initialize`, then `notifications/initialized`.
    pub fn initialize(&mut self, protocol: &str, client: &str) -> Value {
        let r = self.request_within(
            "initialize",
            json!({
                "protocolVersion": protocol,
                "capabilities": {},
                "clientInfo": {"name": client, "version": "1.0.0"}
            }),
            Duration::from_secs(5),
        );
        self.notify("notifications/initialized");
        r
    }

    /// Calls a tool and returns the `result`, panicking on a protocol error.
    pub fn call(&mut self, tool: &str, arguments: Value) -> Value {
        self.call_with(tool, arguments, None, Duration::from_secs(60))
    }

    /// Calls a tool with an optional `_meta` object.
    pub fn call_with(
        &mut self,
        tool: &str,
        arguments: Value,
        meta: Option<Value>,
        limit: Duration,
    ) -> Value {
        let mut params = json!({"name": tool, "arguments": arguments});
        if let Some(meta) = meta {
            params["_meta"] = meta;
        }
        let r = self.request_within("tools/call", params, limit);
        r.get("result")
            .cloned()
            .unwrap_or_else(|| panic!("tools/call {tool} failed: {r}"))
    }

    /// Starts a tool call without waiting for it; [`Mcp::response`] collects it later.
    pub fn start_call(&mut self, tool: &str, arguments: Value, meta: Option<Value>) -> Value {
        let id = json!(self.next_id);
        self.next_id += 1;
        let mut params = json!({"name": tool, "arguments": arguments});
        if let Some(meta) = meta {
            params["_meta"] = meta;
        }
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": params}));
        id
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The text content of a tool result.
pub fn text(result: &Value) -> String {
    result["content"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|c| c["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Whether a tool result is an error result.
pub fn is_error(result: &Value) -> bool {
    result["isError"] == true
}

/// The `hookSpecificOutput.additionalContext` of a Claude Code or Codex hook's stdout, if any.
pub fn context_of(out: &Output) -> Option<String> {
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.trim().is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("hook output isn't one JSON object ({e}): {stdout}"));
    v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .map(str::to_string)
}

/// This process's id, which the MCP servers the tests spawn see as their parent.
pub fn my_pid() -> String {
    std::process::id().to_string()
}

pub fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}
