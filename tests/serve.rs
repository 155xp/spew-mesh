//! serve end to end (9, 15) with a fake `claude`: the `spew` binary on a temp home answering the CLI's asks,
//! follow-ups resuming the same Claude session, failed runs, Ctrl-C, and a daemon restart in the middle of a
//! run; then the serve loop in-process on daemons linked by the in-memory network, for the run limits and
//! `from = "own"`. Nothing here reaches the network or runs a real agent.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use iroh::EndpointId;
use serde_json::Value;
use tokio::sync::{mpsc as tmpsc, watch};

use spew_mesh::control::client::{Client, ConnectOptions};
use spew_mesh::control::protocol::{
    AskParams, DirectoryQuery, FetchParams, ReplyParams, SendResult, SendStatus, SessionOpen,
    SessionRow,
};
use spew_mesh::daemon::testnet::{MemLinks, MemNet};
use spew_mesh::daemon::{self, Daemon, DaemonOptions};
use spew_mesh::identity::DeviceIdentity;
use spew_mesh::paths::Home;
use spew_mesh::responder::env::HostEnv;
use spew_mesh::responder::limits::{GateSettings, SystemClock};
use spew_mesh::responder::memory::StoreConversations;
use spew_mesh::responder::process::SystemRunner;
use spew_mesh::responder::runloop::{self, LoopConfig, Report, ServeEvent};
use spew_mesh::responder::{Responder, ServeClient, ServeConfig};
use spew_mesh::settings::{Env, Settings};
use spew_mesh::spew_api::SystemClock as DaemonClock;
use spew_mesh::wire::{ActivityState, MessageKind, SessionKind};

const WAIT: Duration = Duration::from_secs(30);

/// A fake `claude -p`: records argv and stdin of call N as `<rec>/argv.N` and `stdin.N`, sleeps `sleep.N`
/// seconds when that file exists, then prints `stdout.N` (with `@SESSION@` replaced by the session id from
/// argv) or a success result answering "answer N", prints `stderr.N` and exits with `exit.N`.
const FAKE_CLAUDE: &str = r#"#!/bin/sh
PATH=/usr/bin:/bin
rec="$FAKE_REC"
n=$(cat "$rec/count" 2>/dev/null || echo 0)
n=$((n+1))
echo "$n" > "$rec/count"
for a in "$@"; do printf '%s\n' "$a"; done > "$rec/argv.$n"
cat > "$rec/stdin.$n"
sid=""; prev=""
for a in "$@"; do
  case "$prev" in --session-id|--resume) sid="$a";; esac
  prev="$a"
done
[ -f "$rec/sleep.$n" ] && sleep "$(cat "$rec/sleep.$n")"
[ -f "$rec/stderr.$n" ] && cat "$rec/stderr.$n" >&2
if [ -f "$rec/stdout.$n" ]; then
  sed "s/@SESSION@/$sid/g" "$rec/stdout.$n"
else
  printf '{"type":"result","subtype":"success","is_error":false,"result":"answer %s","session_id":"%s","total_cost_usd":0.01}\n' "$n" "$sid"
fi
code=0
[ -f "$rec/exit.$n" ] && code=$(cat "$rec/exit.$n")
exit $code
"#;

fn claude_ok(answer: &str, cost: f64) -> String {
    format!(
        r#"{{"type":"result","subtype":"success","is_error":false,"result":"{answer}","session_id":"@SESSION@","total_cost_usd":{cost}}}"#
    )
}

/// The fake's folders: `bin/claude` and the recordings.
struct Fake {
    bin: PathBuf,
    rec: PathBuf,
}

impl Fake {
    fn new(root: &Path) -> Fake {
        let bin = root.join("bin");
        let rec = root.join("rec");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&rec).unwrap();
        let exe = bin.join("claude");
        std::fs::write(&exe, FAKE_CLAUDE).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        Fake { bin, rec }
    }

    fn set(&self, name: &str, n: usize, content: &str) {
        std::fs::write(self.rec.join(format!("{name}.{n}")), content).unwrap();
    }

    fn get(&self, name: &str, n: usize) -> String {
        std::fs::read_to_string(self.rec.join(format!("{name}.{n}")))
            .unwrap_or_else(|e| panic!("{name}.{n}: {e}"))
    }

    fn calls(&self) -> usize {
        std::fs::read_to_string(self.rec.join("count"))
            .map(|s| s.trim().parse().unwrap())
            .unwrap_or(0)
    }

    // The value after `flag` in call N's argv.
    fn flag(&self, n: usize, flag: &str) -> Option<String> {
        let argv: Vec<String> = self.get("argv", n).lines().map(str::to_string).collect();
        let at = argv.iter().position(|a| a == flag)?;
        argv.get(at + 1).cloned()
    }
}

fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------------------------------------------------
// The `spew` binary.

/// A temp home, the fake and a folder to serve; dropping it stops the daemon.
struct Mesh {
    tmp: tempfile::TempDir,
    fake: Fake,
}

impl Mesh {
    fn new() -> Mesh {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        for dir in ["user", "empty", "work/api"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let fake = Fake::new(&root);
        Mesh { tmp, fake }
    }

    fn root(&self) -> PathBuf {
        std::fs::canonicalize(self.tmp.path()).unwrap()
    }

    fn home(&self) -> PathBuf {
        self.root().join("user").join("mesh")
    }

    fn api(&self) -> PathBuf {
        self.root().join("work").join("api")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_spew"));
        c.arg("mesh")
            .args(args)
            .env_clear()
            .env("HOME", self.root().join("user"))
            .env("SPEW_MESH_DIR", self.home())
            .env("SPEW_MESH_LAN", "0")
            .env("USER", "sam")
            .env("PATH", &self.fake.bin)
            .env("FAKE_REC", &self.fake.rec)
            .current_dir(self.root().join("work"))
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn serve(&self, args: &[&str]) -> Serve {
        let err_path = self.root().join("serve.err");
        let mut child = self
            .command(&[&["serve"][..], args].concat())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(File::create(&err_path).unwrap()))
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Serve {
            child,
            lines,
            seen: Vec::new(),
            next: 0,
            err_path,
        }
    }

    // The responder rows of `who --json`.
    fn responders(&self) -> Vec<Value> {
        let who = self.run(&["who", "--json"]);
        assert_eq!(code(&who), 0, "{}", stderr(&who));
        let dir: Value = serde_json::from_slice(&who.stdout).unwrap();
        let mut rows = Vec::new();
        for room in dir["rooms"].as_array().unwrap() {
            for person in room["people"].as_array().unwrap() {
                for s in person["sessions"].as_array().unwrap() {
                    if s["kind"] == "responder" {
                        rows.push(s.clone());
                    }
                }
            }
        }
        rows
    }

    async fn client(&self) -> Client {
        let home = Home::at(self.home()).unwrap();
        Client::connect(&home, &ConnectOptions::existing())
            .await
            .unwrap()
    }
}

impl Drop for Mesh {
    fn drop(&mut self) {
        let _ = self.run(&["daemon", "stop"]);
    }
}

/// A running `spew mesh serve`; dropping it kills the process. Declare it after its [`Mesh`] so it goes first
/// and doesn't start the daemon again.
struct Serve {
    child: Child,
    lines: mpsc::Receiver<String>,
    seen: Vec<String>,
    next: usize,
    err_path: PathBuf,
}

impl Serve {
    // Waits for an output line containing `needle` after the last one found.
    fn wait_for(&mut self, needle: &str) -> String {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(i) = self.seen[self.next..]
                .iter()
                .position(|l| l.contains(needle))
            {
                self.next += i + 1;
                return self.seen[self.next - 1].clone();
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => self.seen.push(line),
                Err(_) => panic!(
                    "no line with {needle:?}; stdout:\n{}\nstderr:\n{}",
                    self.seen.join("\n"),
                    std::fs::read_to_string(&self.err_path).unwrap_or_default()
                ),
            }
        }
    }

    // Ctrl-C, then the exit code.
    fn interrupt(&mut self) -> i32 {
        let status = Command::new("kill")
            .args(["-INT", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code().unwrap_or(-1);
            }
            assert!(Instant::now() < deadline, "serve did not stop on Ctrl-C");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap_or(-1)
}

// The id in "(answer id X; ...)" or "(message id X)".
fn id_after(text: &str, marker: &str) -> String {
    let start = text
        .find(marker)
        .unwrap_or_else(|| panic!("no {marker:?} in {text}"))
        + marker.len();
    text[start..]
        .chars()
        .take_while(char::is_ascii_alphanumeric)
        .collect()
}

#[test]
fn serve_says_what_to_fix_when_it_cannot_start() {
    let mesh = Mesh::new();
    let api = mesh.api();
    let api = api.to_str().unwrap();

    let missing = mesh
        .command(&["serve", "claude", "--cwd", api])
        .env("PATH", mesh.root().join("empty"))
        .output()
        .unwrap();
    assert_eq!(code(&missing), 1);
    let err = stderr(&missing);
    assert!(
        err.contains("spew mesh serve: claude is not on PATH"),
        "{err}"
    );
    assert!(err.contains("\nfix: install claude"), "{err}");
    // The checks come first: nothing started a daemon.
    assert!(!mesh.home().join("run").join("socket").exists());

    for (args, says) in [
        (
            &["serve", "cursor"][..],
            "serve takes claude, codex or opencode",
        ),
        (
            &["serve", "claude", "--timeout", "0"][..],
            "the timeout must be positive",
        ),
        (
            &["serve", "claude", "--name", "Web_Help"][..],
            "is not a session name",
        ),
        (
            &["serve", "claude", "--model=--resume"][..],
            "is not a model name",
        ),
    ] {
        let out = mesh.run(args);
        assert_eq!(code(&out), 1, "{args:?}");
        assert!(stderr(&out).contains(says), "{args:?}: {}", stderr(&out));
        assert!(stderr(&out).contains("\nfix: "), "{args:?}");
    }
}

#[test]
fn serve_answers_asks_follow_ups_and_failed_runs() {
    let mesh = Mesh::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let api = mesh.api();
    let mut serve = mesh.serve(&["claude", "--cwd", api.to_str().unwrap()]);
    let intro = serve.wait_for("Serving claude in");
    assert!(intro.contains(api.to_str().unwrap()), "{intro}");
    assert!(intro.contains("read-only"), "{intro}");
    assert!(intro.contains("/claude. Ctrl-C stops."), "{intro}");

    let rows = mesh.responders();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["name"], "claude");
    assert_eq!(rows[0]["reach"], "serve");
    assert_eq!(rows[0]["state"], "idle");

    // An ask from the CLI, answered by a run.
    mesh.fake
        .set("stdout", 1, &claude_ok("It retries three times.", 0.0123));
    let asked = mesh.run(&["ask", "claude", "--wait", "30", "where is the retry logic?"]);
    assert_eq!(code(&asked), 0, "{}", stderr(&asked));
    let text = stdout(&asked);
    assert!(text.contains("It retries three times."), "{text}");
    let answer_id = id_after(&text, "(answer id ");
    let first = mesh.fake.flag(1, "--session-id").expect("a new session");
    let prompt = mesh.fake.get("stdin", 1);
    assert!(prompt.contains("where is the retry logic?"), "{prompt}");
    assert!(prompt.contains("(your person's own agent)"), "{prompt}");
    assert!(prompt.contains("[mesh "), "{prompt}");
    serve.wait_for("question from sam/terminal");
    serve.wait_for(": answered ($0.0123)");

    // A follow-up in the same conversation resumes the same Claude session.
    mesh.fake
        .set("stdout", 2, &claude_ok("Backoff doubles.", 0.01));
    let followed: SendResult = rt.block_on(async {
        let client = mesh.client().await;
        let terminal = client
            .session_open(&SessionOpen {
                client: "terminal".into(),
                client_session: None,
                client_pid: None,
                cwd: "/".into(),
                client_name: None,
                kind: SessionKind::Terminal,
                name: None,
                spew_project: None,
                serve: None,
            })
            .await
            .unwrap();
        client
            .reply(&ReplyParams {
                token: terminal.token,
                id: answer_id.clone(),
                text: "and how long between tries?".into(),
                wait_seconds: Some(30),
            })
            .await
            .unwrap()
    });
    assert_eq!(followed.status, SendStatus::Answered, "{}", followed.note);
    assert_eq!(followed.answer.unwrap().body, "Backoff doubles.");
    assert_eq!(mesh.fake.flag(2, "--resume"), Some(first.clone()));
    assert_eq!(mesh.fake.flag(2, "--session-id"), None);

    // A new conversation starts a new session; this run fails and the failure is the answer.
    mesh.fake.set("stdout", 3, "");
    mesh.fake
        .set("stderr", 3, "boom: the model is not available\n");
    mesh.fake.set("exit", 3, "1");
    let failed = mesh.run(&["ask", "claude", "--wait", "30", "what about auth?"]);
    assert_eq!(code(&failed), 0, "{}", stderr(&failed));
    let text = stdout(&failed);
    assert!(text.contains("The serve run failed: "), "{text}");
    let third = mesh.fake.flag(3, "--session-id").expect("a new session");
    assert_ne!(third, first);
    serve.wait_for(": the run failed: ");

    let rows = mesh.responders();
    assert_eq!(rows[0]["state"], "idle");
    assert_eq!(rows[0]["queue"], 0);

    // Ctrl-C closes the session.
    assert_eq!(serve.interrupt(), 0);
    serve.wait_for("spew mesh serve: stopped");
    assert!(mesh.responders().is_empty());
    // The failed run's raw output stays in the log.
    let log = std::fs::read_to_string(mesh.home().join("logs").join("mesh.log")).unwrap();
    assert!(log.contains("serve run output"), "{log}");
    assert!(log.contains("boom: the model is not available"), "{log}");
}

#[test]
fn serve_carries_on_when_the_daemon_restarts_mid_run() {
    let mesh = Mesh::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let api = mesh.api();
    let mut serve = mesh.serve(&["claude", "--cwd", api.to_str().unwrap()]);
    serve.wait_for("Serving claude in");

    mesh.fake.set("sleep", 1, "3");
    mesh.fake
        .set("stdout", 1, &claude_ok("Finished after the restart.", 0.02));
    let asked = mesh.run(&["ask", "claude", "--wait", "0", "a slow question"]);
    assert_eq!(code(&asked), 0, "{}", stderr(&asked));
    let ask_id = id_after(&stdout(&asked), "(message id ");
    wait_until("the run starts", || mesh.fake.calls() == 1);
    let rows = mesh.responders();
    assert_eq!(rows[0]["state"], "busy", "{rows:?}");

    // The daemon goes away mid-run; serve starts it again, re-attaches its session and sends the answer.
    rt.block_on(async { mesh.client().await.shutdown().await.unwrap() });
    serve.wait_for("Lost the daemon; reconnecting.");
    serve.wait_for("Back on the daemon as");
    serve.wait_for(": answered");
    let messages = mesh.run(&["messages", "--wait", "30"]);
    assert_eq!(code(&messages), 0, "{}", stderr(&messages));
    let text = stdout(&messages);
    assert!(text.contains("Finished after the restart."), "{text}");
    assert!(text.contains(&ask_id), "{text}");
    assert_eq!(mesh.fake.calls(), 1);

    let rows = mesh.responders();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["state"], "idle");

    // Ctrl-C in the middle of a run stops it and still answers the asker.
    mesh.fake.set("sleep", 2, "30");
    let asked = mesh.run(&["ask", "claude", "--wait", "0", "another slow one"]);
    assert_eq!(code(&asked), 0, "{}", stderr(&asked));
    wait_until("the second run starts", || mesh.fake.calls() == 2);
    assert_eq!(serve.interrupt(), 0);
    let messages = mesh.run(&["messages", "--wait", "10"]);
    let text = stdout(&messages);
    assert!(
        text.contains("The serve run failed: the responder was stopped before the run finished."),
        "{text}"
    );
    assert!(mesh.responders().is_empty());
}

#[test]
fn a_reconnecting_serve_leaves_a_stopped_daemon_stopped() {
    let mesh = Mesh::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let api = mesh.api();
    let mut serve = mesh.serve(&["claude", "--cwd", api.to_str().unwrap()]);
    serve.wait_for("Serving claude in");

    // What `daemon stop` does (6.1): write run/stopped, then stop the daemon.
    std::fs::write(mesh.home().join("run").join("stopped"), "").unwrap();
    rt.block_on(async { mesh.client().await.shutdown().await.unwrap() });
    serve.wait_for("Lost the daemon; reconnecting.");
    // Reconnects at about 0.5 s and 1.5 s go by without starting a daemon.
    std::thread::sleep(Duration::from_secs(2));
    let status = mesh.run(&["daemon", "status"]);
    assert_eq!(code(&status), 3, "{}", stdout(&status));

    // A daemon started some other way is picked up again.
    let started = mesh.run(&["daemon", "start"]);
    assert_eq!(code(&started), 0, "{}", stderr(&started));
    serve.wait_for("Back on the daemon as");
    let asked = mesh.run(&["ask", "claude", "--wait", "30", "still there?"]);
    assert_eq!(code(&asked), 0, "{}", stderr(&asked));
    assert!(stdout(&asked).contains("answer 1"), "{}", stdout(&asked));
    assert_eq!(serve.interrupt(), 0);
}

// ---------------------------------------------------------------------------------------------------------
// In-process daemons on the in-memory network.

/// One device with its own home and daemon.
struct Device {
    tmp: tempfile::TempDir,
    home: Home,
    id: EndpointId,
    daemon: Option<Daemon<MemLinks>>,
}

impl Device {
    fn prepare(name: &str) -> Device {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home")).unwrap();
        home.ensure().unwrap();
        let (identity, _) = DeviceIdentity::load_or_create(&home, name, 1).unwrap();
        Device {
            id: identity.endpoint_id(),
            tmp,
            home,
            daemon: None,
        }
    }

    fn root(&self) -> PathBuf {
        std::fs::canonicalize(self.tmp.path()).unwrap()
    }

    fn write_settings(&self, person: &str, roster: &str, serve: &str) {
        let file = self.root().join("roster.toml");
        std::fs::write(&file, roster).unwrap();
        let text = format!(
            "username = \"{person}\"\n\n[roster]\nkind = \"file\"\nfile = \"{}\"\n{serve}",
            file.display()
        );
        std::fs::write(self.home.settings_file(), text).unwrap();
    }

    async fn start(&mut self, net: &MemNet) {
        let opts = DaemonOptions {
            home: self.home.clone(),
            user_home: Some(self.root()),
            env: Env {
                lan_off: true,
                ..Env::default()
            },
            runtime_dirs: Vec::new(),
            clock: Arc::new(DaemonClock),
        };
        let net = net.clone();
        let started = daemon::start(opts, move |id| net.join(id))
            .await
            .unwrap()
            .expect("the lock is free");
        self.daemon = Some(started);
    }

    async fn stop(&mut self, net: &MemNet) {
        if let Some(d) = self.daemon.take() {
            d.finish().await;
        }
        net.leave(&self.id);
    }

    async fn client(&self) -> Client {
        Client::connect_path(&self.daemon.as_ref().unwrap().socket)
            .await
            .unwrap()
    }

    // A folder, in `room` when given.
    fn folder(&self, name: &str, room: Option<&str>) -> PathBuf {
        let dir = self.root().join("work").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(room) = room {
            std::fs::write(
                dir.join(".spew.json"),
                format!("{{\"project\": \"{room}\"}}"),
            )
            .unwrap();
        }
        dir
    }
}

/// sam on sam-mac (serving) and sam-linux, maya on maya-mbp; sam and maya share `team`.
struct Team {
    net: MemNet,
    mac: Device,
    linux: Device,
    maya: Device,
}

impl Team {
    // `serve` is sam-mac's `[serve]` table, written after its daemon started.
    async fn start(serve: &str) -> Team {
        let net = MemNet::new();
        let mut mac = Device::prepare("sam-mac");
        let mut linux = Device::prepare("sam-linux");
        let mut maya = Device::prepare("maya-mbp");
        let roster = format!(
            "[[person]]\nname = \"sam\"\ndevices = [ {{ id = \"{}\", name = \"sam-mac\" }}, \
             {{ id = \"{}\", name = \"sam-linux\" }} ]\n\n\
             [[person]]\nname = \"maya\"\ndevices = [ {{ id = \"{}\", name = \"maya-mbp\" }} ]\n\n\
             [[room]]\nname = \"team\"\npeople = [\"sam\", \"maya\"]\n",
            mac.id, linux.id, maya.id
        );
        mac.write_settings("sam", &roster, "");
        linux.write_settings("sam", &roster, "");
        maya.write_settings("maya", &roster, "");
        mac.start(&net).await;
        linux.start(&net).await;
        maya.start(&net).await;
        mac.write_settings("sam", &roster, serve);
        Team {
            net,
            mac,
            linux,
            maya,
        }
    }

    async fn stop(mut self) {
        for d in [&mut self.mac, &mut self.linux, &mut self.maya] {
            d.stop(&self.net).await;
        }
    }
}

/// sam-mac's responder, set up the way `spew mesh serve claude` does it, with the fake on PATH.
async fn responder(
    device: &Device,
    fake: &Fake,
) -> (
    Responder<SystemRunner, StoreConversations, SystemClock>,
    LoopConfig,
) {
    let settings = Settings::load(&device.home, Some(&device.root())).unwrap();
    let cwd = device.folder("api", Some("team"));
    let cfg = ServeConfig::new(
        ServeClient::Claude,
        &cwd,
        device.home.serve_run_dir(),
        &settings.serve,
    );
    let host = HostEnv::new([
        ("PATH", fake.bin.display().to_string()),
        ("FAKE_REC", fake.rec.display().to_string()),
    ]);
    let responder = Responder::start(
        cfg,
        &host,
        SystemRunner,
        StoreConversations::open_later(device.home.store_file()),
        SystemClock,
    )
    .await
    .unwrap();
    let mut lc = LoopConfig::new(
        device.home.clone(),
        ServeClient::Claude,
        responder.config().cwd.to_string_lossy().into_owned(),
        None,
        None,
        GateSettings::from(&settings.serve),
    );
    lc.connect = ConnectOptions::existing();
    (responder, lc)
}

fn events() -> (Report, tmpsc::UnboundedReceiver<ServeEvent>) {
    let (tx, rx) = tmpsc::unbounded_channel();
    let report: Report = Arc::new(move |e: &ServeEvent| {
        let _ = tx.send(e.clone());
    });
    (report, rx)
}

async fn next_event(
    rx: &mut tmpsc::UnboundedReceiver<ServeEvent>,
    what: &str,
    mut want: impl FnMut(&ServeEvent) -> bool,
) -> ServeEvent {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let e = tokio::time::timeout_at(deadline, rx.recv())
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
            .expect("the loop is running");
        if want(&e) {
            return e;
        }
    }
}

/// A live session on a device, over a connection that stays open.
struct Session {
    client: Client,
    token: String,
}

impl Session {
    async fn open(device: &Device, folder: &Path) -> Session {
        let client = device.client().await;
        let opened = client
            .session_open(&SessionOpen {
                client: "claude".into(),
                client_session: None,
                client_pid: None,
                cwd: folder.to_string_lossy().into_owned(),
                client_name: None,
                kind: SessionKind::Live,
                name: None,
                spew_project: None,
                serve: None,
            })
            .await
            .unwrap();
        Session {
            client,
            token: opened.token,
        }
    }

    async fn ask(&self, to: &str, text: &str, wait: u64) -> SendResult {
        self.client
            .ask(&AskParams {
                token: self.token.clone(),
                to: to.into(),
                text: text.into(),
                card: None,
                wait_seconds: Some(wait),
            })
            .await
            .unwrap()
    }

    async fn responder_row(&self, include_offline: bool) -> Option<SessionRow> {
        let dir = self
            .client
            .directory(&DirectoryQuery {
                token: Some(self.token.clone()),
                filter: None,
                include_offline,
            })
            .await
            .unwrap();
        dir.rooms
            .into_iter()
            .flat_map(|r| r.people)
            .flat_map(|p| p.sessions)
            .find(|s| s.kind == SessionKind::Responder && s.online)
    }

    async fn sees_responder(&self) {
        let deadline = Instant::now() + WAIT;
        while self.responder_row(false).await.is_none() {
            assert!(Instant::now() < deadline, "the responder never showed up");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    // The answers among the session's unread messages.
    async fn answers(&self) -> Vec<String> {
        let f = self
            .client
            .fetch(&FetchParams {
                token: self.token.clone(),
                wait_seconds: Some(5),
                keep_unread: false,
            })
            .await
            .unwrap();
        f.messages
            .into_iter()
            .filter(|m| m.kind == MessageKind::Answer)
            .map(|m| m.body)
            .collect()
    }
}

const SERVED: &str = "sam@sam-mac/claude";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_per_person_limit_refuses_with_busy_and_own_devices_count_only_toward_the_total() {
    // The daemon started with the default limit of 10; serve reads 1 and the daemon applies it.
    let team = Team::start("\n[serve]\nmax_runs_per_person_per_hour = 1\n").await;
    let fake = Fake::new(&team.mac.root());
    fake.set("sleep", 1, "4");
    let (mut responder, lc) = responder(&team.mac, &fake).await;
    let (report, mut rx) = events();
    let (stop, stop_rx) = watch::channel(false);
    let serving = runloop::serve(&mut responder, lc, None, stop_rx, report);
    let body = async {
        next_event(&mut rx, "the session to open", |e| {
            matches!(e, ServeEvent::Ready { again: false, .. })
        })
        .await;
        let maya = Session::open(&team.maya, &team.maya.folder("web", Some("team"))).await;
        let linux = Session::open(&team.linux, &team.linux.folder("ops", None)).await;
        maya.sees_responder().await;
        linux.sees_responder().await;

        // sam-linux's question runs; maya's waits in the queue and counts as her one run this hour.
        let first = linux.ask(SERVED, "which port?", 0).await;
        assert_eq!(first.status, SendStatus::Delivered, "{}", first.note);
        next_event(
            &mut rx,
            "the first run",
            |e| matches!(e, ServeEvent::Question { from, .. } if from == "sam/claude-ops"),
        )
        .await;
        let queued = maya.ask(SERVED, "where is the retry logic?", 0).await;
        assert_eq!(queued.status, SendStatus::Delivered, "{}", queued.note);
        let mac = Session::open(&team.mac, &team.mac.folder("notes", None)).await;
        let deadline = Instant::now() + WAIT;
        loop {
            let row = mac.responder_row(false).await.unwrap();
            if row.state == ActivityState::Busy && row.queue == 1 {
                break;
            }
            assert!(Instant::now() < deadline, "presence: {row:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let over = maya.ask(SERVED, "and the backoff?", 5).await;
        assert_eq!(over.status, SendStatus::Refused, "{}", over.note);
        assert_eq!(over.code.as_deref(), Some("busy"));

        // sam's own devices aren't held to the per-person limit.
        let again = linux.ask(SERVED, "and the host?", 20).await;
        assert_eq!(again.status, SendStatus::Answered, "{}", again.note);
        assert_eq!(again.answer.unwrap().body, "answer 3");
        assert_eq!(maya.answers().await, ["answer 2"]);
        let row = mac.responder_row(false).await.unwrap();
        assert_eq!((row.state, row.queue), (ActivityState::Idle, 0));
        assert_eq!(fake.calls(), 3);
        stop.send(true).unwrap();
    };
    let (served, ()) = tokio::join!(serving, body);
    served.unwrap();
    team.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn from_own_turns_a_teammate_away_as_no_such_session() {
    let team = Team::start("\n[serve]\nfrom = \"own\"\n").await;
    let fake = Fake::new(&team.mac.root());
    let (mut responder, lc) = responder(&team.mac, &fake).await;
    let (report, mut rx) = events();
    let (stop, stop_rx) = watch::channel(false);
    let serving = runloop::serve(&mut responder, lc, None, stop_rx, report);
    let body = async {
        next_event(&mut rx, "the session to open", |e| {
            matches!(e, ServeEvent::Ready { .. })
        })
        .await;
        let maya = Session::open(&team.maya, &team.maya.folder("web", Some("team"))).await;
        let linux = Session::open(&team.linux, &team.linux.folder("ops", None)).await;
        maya.sees_responder().await;
        linux.sees_responder().await;

        let refused = maya.ask(SERVED, "where is the retry logic?", 5).await;
        assert_eq!(refused.status, SendStatus::Refused, "{}", refused.note);
        assert_eq!(refused.code.as_deref(), Some("no-such-session"));
        let answered = linux.ask(SERVED, "where is the retry logic?", 20).await;
        assert_eq!(answered.status, SendStatus::Answered, "{}", answered.note);
        assert_eq!(answered.answer.unwrap().body, "answer 1");
        assert_eq!(fake.calls(), 1);
        stop.send(true).unwrap();
    };
    let (served, ()) = tokio::join!(serving, body);
    served.unwrap();
    team.stop().await;
}
