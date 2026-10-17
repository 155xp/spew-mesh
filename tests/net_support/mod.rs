//! Child-process daemons on real iroh links, shared by `tests/net.rs` and `tests/e2e.rs` (15). Each device is the
//! built `spew` binary run on its own temp home, with `[roster] kind = "file"` and `[network] mode = "local"`
//! bound to 127.0.0.1. mDNS is off and there are no relays, so nothing leaves the machine; the roster's `addrs`
//! come from each daemon's `daemon.status`. Every wait polls with a deadline, and a daemon is killed when its
//! test ends, even on a panic, after printing the end of its log.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use iroh::EndpointId;
use spew_mesh::cli::full_status;
use spew_mesh::control::Client;
use spew_mesh::control::client::ConnectOptions;
use spew_mesh::control::protocol::{
    DirectoryQuery, FetchParams, Fetched, SessionOpen, SessionRow, Status,
};
use spew_mesh::daemon::status::{DeviceLink, NetworkDetails};
use spew_mesh::identity::DeviceIdentity;
use spew_mesh::paths::Home;
use spew_mesh::wire::SessionKind;

/// Generous, for a loaded machine; every wait ends as soon as its condition holds.
pub const WAIT: Duration = Duration::from_secs(60);
/// A clean close (code 4) reaches the peer at once; an unclean one would take the 30 s idle timeout.
pub const CLEAN_CLOSE: Duration = Duration::from_secs(15);

/// One device: a temp folder holding its user home, mesh home, a `PATH` folder and work folders, and the daemon
/// running on them.
pub struct Node {
    pub person: &'static str,
    pub device: &'static str,
    pub tmp: tempfile::TempDir,
    pub home: Home,
    pub id: EndpointId,
    pub child: Option<Child>,
    pub runs: u32,
    roster: PathBuf,
}

/// A device's `settings.toml`: a file roster, and `local` mode on 127.0.0.1 with mDNS off and `port` when given.
pub fn settings_text(person: &str, roster: &Path, port: Option<u16>) -> String {
    let port = port.map(|p| format!("port = {p}\n")).unwrap_or_default();
    format!(
        "username = \"{person}\"\n\n[roster]\nkind = \"file\"\nfile = \"{}\"\n\n\
         [network]\nmode = \"local\"\nbind = \"127.0.0.1\"\nmdns = false\n{port}",
        roster.display()
    )
}

impl Node {
    /// A device whose identity and settings exist but whose daemon isn't running.
    pub fn prepare(person: &'static str, device: &'static str, roster: &Path) -> Node {
        let tmp = tempfile::tempdir().unwrap();
        for dir in ["user", "bin", "work"] {
            std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
        }
        let home = Home::at(tmp.path().join("home")).unwrap();
        home.ensure().unwrap();
        let (identity, _) = DeviceIdentity::load_or_create(&home, device, 1).unwrap();
        std::fs::write(home.settings_file(), settings_text(person, roster, None)).unwrap();
        Node {
            person,
            device,
            id: identity.endpoint_id(),
            tmp,
            home,
            child: None,
            runs: 0,
            roster: roster.to_path_buf(),
        }
    }

    /// Two devices, the first with the greater endpoint id, so it is the designated dialer of the pair (5.3).
    /// Both are made afresh on each try, so every try succeeds with even odds.
    pub fn dialer_pair(
        dialer: (&'static str, &'static str, &Path),
        other: (&'static str, &'static str, &Path),
    ) -> (Node, Node) {
        for _ in 0..64 {
            let a = Node::prepare(dialer.0, dialer.1, dialer.2);
            let b = Node::prepare(other.0, other.1, other.2);
            if a.id.as_bytes() > b.id.as_bytes() {
                return (a, b);
            }
        }
        panic!("no ordered pair of endpoint ids in 64 tries");
    }

    pub fn address(&self) -> String {
        format!("{}@{}", self.person, self.device)
    }

    /// The folder on this device's `PATH`, empty unless a test puts a fake agent there.
    pub fn bin(&self) -> PathBuf {
        self.tmp.path().join("bin")
    }

    /// `spew mesh <args>` with a clean environment pointing at this device's homes.
    pub fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_spew"));
        c.arg("mesh")
            .args(args)
            .env_clear()
            .env("HOME", self.tmp.path().join("user"))
            .env("SPEW_MESH_DIR", self.home.root())
            .env("SPEW_MESH_LAN", "0")
            .env("USER", self.person)
            .env("PATH", self.bin())
            .current_dir(self.tmp.path().join("work"))
            .stdin(Stdio::null());
        c
    }

    /// Runs a CLI command that needs the daemon already running (it never starts one here).
    pub async fn cli(&self, args: &[&str]) -> Output {
        tokio::process::Command::from(self.command(args))
            .output()
            .await
            .unwrap()
    }

    /// Starts `spew mesh daemon run` and waits until it answers on its control socket.
    pub async fn start(&mut self) {
        assert!(self.child.is_none(), "{} is already running", self.device);
        self.runs += 1;
        let log = std::fs::File::create(self.log_path(self.runs)).unwrap();
        let child = self
            .command(&["daemon", "run"])
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        self.child = Some(child);
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(child) = self.child.as_mut()
                && let Some(exit) = child.try_wait().unwrap()
            {
                self.child = None;
                panic!("{} exited with {exit}:\n{}", self.device, self.log());
            }
            if let Ok(client) = Client::connect(&self.home, &ConnectOptions::existing()).await
                && client.status().await.is_ok()
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{} didn't start:\n{}",
                self.device,
                self.log()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// `spew mesh daemon stop`, then waits for the process to exit.
    pub async fn stop(&mut self) {
        let out = self.cli(&["daemon", "stop"]).await;
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let deadline = Instant::now() + WAIT;
        let Some(mut child) = self.child.take() else {
            return;
        };
        while child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "{} didn't exit", self.device);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn client(&self) -> Client {
        Client::connect(&self.home, &ConnectOptions::existing())
            .await
            .unwrap()
    }

    pub async fn status(&self) -> (Status, NetworkDetails) {
        full_status(&self.client().await).await.unwrap()
    }

    /// The socket the endpoint is bound to, once it is up.
    pub async fn bound(&self) -> String {
        let mut found = None;
        eventually(&format!("{} binds its endpoint", self.device), async || {
            let (_, net) = self.status().await;
            found = net
                .bound_sockets
                .iter()
                .find(|s| s.starts_with("127.0.0.1:"))
                .cloned();
            found.is_some()
        })
        .await;
        found.unwrap()
    }

    /// Sets `[network] port`, for the next start.
    pub fn pin_port(&self, port: u16) {
        let settings = settings_text(self.person, &self.roster, Some(port));
        std::fs::write(self.home.settings_file(), settings).unwrap();
    }

    /// The device as this daemon's status shows it, if admitted.
    pub async fn link_to(&self, other: &Node) -> Option<DeviceLink> {
        let (_, net) = self.status().await;
        net.devices
            .into_iter()
            .find(|d| d.endpoint_id == other.id.to_string())
    }

    pub async fn sees_online(&self, other: &Node) -> bool {
        self.link_to(other).await.is_some_and(|d| d.online)
    }

    /// A folder for sessions, with a `.spew.json` naming `room`.
    pub fn folder(&self, name: &str, room: &str) -> String {
        let dir = self.tmp.path().join("work").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(".spew.json"),
            format!("{{\"project\": \"{room}\"}}"),
        )
        .unwrap();
        dir.to_string_lossy().into_owned()
    }

    fn log_path(&self, run: u32) -> PathBuf {
        self.tmp.path().join(format!("daemon-{run}.log"))
    }

    /// Everything the daemon logged, over all its runs.
    pub fn log(&self) -> String {
        (1..=self.runs)
            .map(|run| std::fs::read_to_string(self.log_path(run)).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if std::thread::panicking() {
            let log = self.log();
            let lines: Vec<&str> = log.lines().collect();
            let tail = lines[lines.len().saturating_sub(60)..].join("\n");
            eprintln!("--- {} log (last 60 lines) ---\n{tail}", self.device);
        }
    }
}

/// A roster file the daemons share.
pub struct Roster {
    dir: tempfile::TempDir,
}

impl Roster {
    pub fn new() -> Roster {
        Roster {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.path().join("roster.toml")
    }

    /// Writes the roster in one rename, so a daemon polling it never reads half a file.
    pub fn write(&self, people: &[(&Node, Option<&str>)], rooms: &[(&str, &[&str])]) {
        let mut text = String::new();
        for (node, addr) in people {
            let addrs = addr
                .map(|a| format!(", addrs = [\"{a}\"]"))
                .unwrap_or_default();
            text.push_str(&format!(
                "[[person]]\nname = \"{}\"\ndevices = [ {{ id = \"{}\", name = \"{}\"{addrs} }} ]\n\n",
                node.person, node.id, node.device
            ));
        }
        for (room, members) in rooms {
            let list: Vec<String> = members.iter().map(|m| format!("\"{m}\"")).collect();
            text.push_str(&format!(
                "[[room]]\nname = \"{room}\"\npeople = [{}]\n\n",
                list.join(", ")
            ));
        }
        let next = self.dir.path().join("roster.toml.next");
        std::fs::write(&next, text).unwrap();
        std::fs::rename(&next, self.path()).unwrap();
    }

    /// Lists `people` with the bound sockets of the running ones as their `addrs`, and has every running daemon
    /// read it now rather than at its next poll.
    pub async fn publish(&self, people: &[&Node], rooms: &[(&str, &[&str])]) {
        let mut entries = Vec::new();
        for node in people {
            let addr = match node.child {
                Some(_) => Some(node.bound().await),
                None => None,
            };
            entries.push((*node, addr));
        }
        let listed: Vec<(&Node, Option<&str>)> =
            entries.iter().map(|(n, a)| (*n, a.as_deref())).collect();
        self.write(&listed, rooms);
        for node in people.iter().filter(|n| n.child.is_some()) {
            node.client().await.refresh().await.unwrap();
        }
    }
}

/// Polls `check` every 100 ms until it holds, failing after [`WAIT`].
pub async fn eventually(what: &str, mut check: impl AsyncFnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A session opened through a control connection that stays open.
pub struct Session {
    pub client: Client,
    pub token: String,
}

pub async fn open(node: &Node, cwd: &str) -> Session {
    let client = node.client().await;
    let opened = client
        .session_open(&SessionOpen {
            client: "claude".into(),
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
    Session {
        client,
        token: opened.token,
    }
}

pub async fn fetch(s: &Session) -> Fetched {
    s.client
        .fetch(&FetchParams {
            token: s.token.clone(),
            wait_seconds: None,
            keep_unread: false,
        })
        .await
        .unwrap()
}

/// Every session row the device's directory lists, as a session with `token` (or the terminal) sees it.
pub async fn rows(client: &Client, token: Option<&str>) -> Vec<SessionRow> {
    let dir = client
        .directory(&DirectoryQuery {
            token: token.map(str::to_string),
            filter: None,
            include_offline: true,
        })
        .await
        .unwrap();
    dir.rooms
        .into_iter()
        .flat_map(|r| r.people)
        .flat_map(|p| p.sessions)
        .collect()
}

/// Whether the session's directory lists `address` online.
pub async fn sees(s: &Session, address: &str) -> bool {
    rows(&s.client, Some(&s.token))
        .await
        .iter()
        .any(|row| row.address == address && row.online)
}

/// Whether the session's directory names `address` at all, online or not.
pub async fn knows(s: &Session, address: &str) -> bool {
    rows(&s.client, Some(&s.token))
        .await
        .iter()
        .any(|row| row.address == address)
}

/// Stdout and stderr of a finished command.
pub fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}
