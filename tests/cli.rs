//! The `spew` binary on a temp home: it starts its own daemon in the background, acts as the terminal session
//! next to an agent session opened over the control socket, and stops the daemon again. Nothing here reaches
//! the network: the daemon has no links yet, mDNS is off and `PATH` is an empty folder.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use spew_mesh::control::client::{Client, ConnectOptions};
use spew_mesh::control::protocol::{FetchParams, Fetched, ReplyParams, SessionOpen};
use spew_mesh::paths::Home;
use spew_mesh::wire::{MessageKind, SessionKind};

/// A temp home and a working folder; dropping it stops the daemon the commands started.
struct Mesh {
    tmp: tempfile::TempDir,
}

impl Mesh {
    fn new() -> Mesh {
        let tmp = tempfile::tempdir().unwrap();
        for dir in ["user", "bin", "work"] {
            std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
        }
        Mesh { tmp }
    }

    fn home(&self) -> PathBuf {
        self.tmp.path().join("user").join("mesh")
    }

    fn work(&self) -> PathBuf {
        self.tmp.path().join("work")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_spew"));
        c.arg("mesh")
            .args(args)
            .env_clear()
            .env("HOME", self.tmp.path().join("user"))
            .env("SPEW_MESH_DIR", self.home())
            .env("SPEW_MESH_LAN", "0")
            .env("USER", "sam")
            .env("PATH", self.tmp.path().join("bin"))
            .current_dir(self.work())
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn run_with_input(&self, args: &[&str], input: &str) -> Output {
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }
}

impl Drop for Mesh {
    fn drop(&mut self) {
        let _ = self.run(&["daemon", "stop"]);
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

// The id in a send result's "(message id ...)" line.
fn message_id(text: &str) -> String {
    let start = text.find("(message id ").expect("a message id line") + "(message id ".len();
    let rest = &text[start..];
    rest[..rest.find(')').unwrap()].to_string()
}

async fn agent_session(home: &Path, cwd: &Path) -> (Client, String) {
    let home = Home::at(home).unwrap();
    let client = Client::connect(&home, &ConnectOptions::existing())
        .await
        .unwrap();
    let opened = client
        .session_open(&SessionOpen {
            client: "claude".into(),
            client_session: None,
            client_pid: None,
            cwd: cwd.to_string_lossy().into_owned(),
            client_name: None,
            kind: SessionKind::Live,
            name: None,
            spew_project: None,
        })
        .await
        .unwrap();
    assert_eq!(opened.name, "claude-api");
    (client, opened.token)
}

async fn fetch(client: &Client, token: &str) -> Fetched {
    client
        .fetch(&FetchParams {
            token: token.into(),
            wait_seconds: None,
            keep_unread: false,
        })
        .await
        .unwrap()
}

#[test]
fn the_cli_starts_the_daemon_talks_to_a_session_and_stops_it() {
    let mesh = Mesh::new();
    let rt = tokio::runtime::Runtime::new().unwrap();

    let status = mesh.run(&["daemon", "status"]);
    assert_eq!(code(&status), 3, "{}", stderr(&status));
    assert!(stdout(&status).contains("isn't running"));

    // Any command starts the daemon, which creates the identity on its first start.
    let identity = mesh.run(&["identity"]);
    assert_eq!(code(&identity), 0, "{}", stderr(&identity));
    let text = stdout(&identity);
    assert!(text.starts_with("sam-"), "{text}");
    assert!(text.contains("endpoint id: ") && text.contains("short id: "));

    let status = mesh.run(&["daemon", "status"]);
    assert_eq!(code(&status), 0, "{}", stderr(&status));
    let text = stdout(&status);
    assert!(text.contains("roster: local"), "{text}");
    assert!(text.contains("network: off"), "{text}");

    let who = mesh.run(&["who"]);
    assert_eq!(code(&who), 0, "{}", stderr(&who));
    assert!(stdout(&who).contains("No agent sessions are online."));

    let nobody = mesh.run(&["tell", "nobody", "hi"]);
    assert_eq!(code(&nobody), 1);
    assert!(
        stderr(&nobody).starts_with("spew mesh: "),
        "{}",
        stderr(&nobody)
    );

    let api_dir = mesh.work().join("api");
    std::fs::create_dir_all(&api_dir).unwrap();
    let (agent, token) = rt.block_on(agent_session(&mesh.home(), &api_dir));

    let who = mesh.run(&["who"]);
    assert!(stdout(&who).contains("claude-api"), "{}", stdout(&who));

    // A tell whose text comes from stdin.
    let told = mesh.run_with_input(&["tell", "claude-api"], "the build is green\n");
    assert_eq!(code(&told), 0, "{}", stderr(&told));
    let f = rt.block_on(fetch(&agent, &token));
    assert_eq!(f.messages.len(), 1);
    assert_eq!(f.messages[0].kind, MessageKind::Tell);
    assert_eq!(f.messages[0].body.trim_end(), "the build is green");
    assert!(
        f.messages[0].from.ends_with("terminal"),
        "{}",
        f.messages[0].from
    );

    // An ask that doesn't wait, answered by the agent and read with `messages`.
    let asked = mesh.run(&["ask", "claude-api", "--wait", "0", "which", "port?"]);
    assert_eq!(code(&asked), 0, "{}", stderr(&asked));
    let ask_id = message_id(&stdout(&asked));
    let f = rt.block_on(fetch(&agent, &token));
    assert_eq!(f.messages.len(), 1);
    assert_eq!(f.messages[0].id, ask_id);
    assert_eq!(f.messages[0].kind, MessageKind::Ask);
    let answered = rt
        .block_on(agent.reply(&ReplyParams {
            token: token.clone(),
            id: ask_id,
            text: "5220".into(),
            wait_seconds: None,
        }))
        .unwrap();
    let messages = mesh.run(&["messages"]);
    assert_eq!(code(&messages), 0, "{}", stderr(&messages));
    let text = stdout(&messages);
    assert!(
        text.contains("5220") && text.contains(&answered.id),
        "{text}"
    );
    let again = mesh.run(&["messages"]);
    assert!(stdout(&again).contains("No messages."));

    // The terminal follows up on the answer.
    let replied = mesh.run(&["reply", &answered.id, "thanks"]);
    assert_eq!(code(&replied), 0, "{}", stderr(&replied));
    let f = rt.block_on(fetch(&agent, &token));
    assert_eq!(f.messages.len(), 1);
    assert_eq!(f.messages[0].body, "thanks");

    let stopped = mesh.run(&["daemon", "stop"]);
    assert_eq!(code(&stopped), 0, "{}", stderr(&stopped));
    assert!(stdout(&stopped).contains("Stopped"));
    assert!(!mesh.home().join("run").join("socket").exists());
    let status = mesh.run(&["daemon", "status"]);
    assert_eq!(code(&status), 3);
}

#[test]
fn the_home_flag_and_doctor_work_without_a_daemon() {
    let mesh = Mesh::new();
    let other = mesh.tmp.path().join("elsewhere");
    let status = mesh.run(&["--home", other.to_str().unwrap(), "daemon", "status"]);
    assert_eq!(code(&status), 3, "{}", stderr(&status));
    let doctor = mesh.run(&["doctor"]);
    let text = stdout(&doctor);
    assert!(text.contains("identity: none yet"), "{text}");
    assert!(text.contains("daemon: not running"), "{text}");
    // Doctor never starts the daemon.
    assert!(!mesh.home().join("run").join("socket").exists());
}

#[test]
fn the_commands_other_cards_build_say_so() {
    let mesh = Mesh::new();
    for args in [
        &["mcp-server"][..],
        &["hook", "claude", "SessionStart"][..],
        &["serve", "codex", "--name", "web-help"][..],
    ] {
        let out = mesh.run(args);
        assert_eq!(code(&out), 1, "{args:?}");
        assert!(stderr(&out).contains("not built yet"), "{args:?}");
    }
    let bad = mesh.run(&["ask"]);
    assert_eq!(code(&bad), 2);
}
