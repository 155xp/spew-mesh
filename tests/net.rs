//! Two and three daemons on real iroh links (15): each is the built `spew` binary run as a child process on its
//! own temp home, with `[roster] kind = "file"` and `[network] mode = "local"` bound to 127.0.0.1. mDNS is off
//! and there are no relays, so nothing leaves the machine; the roster's `addrs` come from each daemon's
//! `daemon.status`. Every wait polls with a deadline, and a daemon is killed when its test ends, even on a panic.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use iroh::EndpointId;
use spew_mesh::cli::full_status;
use spew_mesh::control::Client;
use spew_mesh::control::client::ConnectOptions;
use spew_mesh::control::protocol::{
    AskParams, DirectoryQuery, FetchParams, Fetched, ReplyParams, SendStatus, SessionOpen, Status,
    TellParams,
};
use spew_mesh::daemon::status::NetworkDetails;
use spew_mesh::identity::DeviceIdentity;
use spew_mesh::paths::Home;
use spew_mesh::wire::{MessageKind, SessionKind};

/// Generous, for a loaded machine; every wait ends as soon as its condition holds.
const WAIT: Duration = Duration::from_secs(60);
/// A clean close (code 4) reaches the peer at once; an unclean one would take the 30 s idle timeout.
const CLEAN_CLOSE: Duration = Duration::from_secs(15);

/// One device: a temp folder holding its user home, mesh home, an empty `PATH` folder and work folders, and
/// the daemon running on them.
struct Node {
    person: &'static str,
    device: &'static str,
    tmp: tempfile::TempDir,
    home: Home,
    id: EndpointId,
    child: Option<Child>,
    runs: u32,
}

impl Node {
    /// A device whose identity and settings exist but whose daemon isn't running.
    fn prepare(person: &'static str, device: &'static str, roster: &Path) -> Node {
        let tmp = tempfile::tempdir().unwrap();
        for dir in ["user", "bin", "work"] {
            std::fs::create_dir_all(tmp.path().join(dir)).unwrap();
        }
        let home = Home::at(tmp.path().join("home")).unwrap();
        home.ensure().unwrap();
        let (identity, _) = DeviceIdentity::load_or_create(&home, device, 1).unwrap();
        let settings = format!(
            "username = \"{person}\"\n\n[roster]\nkind = \"file\"\nfile = \"{}\"\n\n\
             [network]\nmode = \"local\"\nbind = \"127.0.0.1\"\nmdns = false\n",
            roster.display()
        );
        std::fs::write(home.settings_file(), settings).unwrap();
        Node {
            person,
            device,
            id: identity.endpoint_id(),
            tmp,
            home,
            child: None,
            runs: 0,
        }
    }

    /// Two devices, the first with the greater endpoint id, so it is the designated dialer of the pair (5.3).
    /// Both are made afresh on each try, so every try succeeds with even odds.
    fn dialer_pair(
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

    fn address(&self) -> String {
        format!("{}@{}", self.person, self.device)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_spew"));
        c.arg("mesh")
            .args(args)
            .env_clear()
            .env("HOME", self.tmp.path().join("user"))
            .env("SPEW_MESH_DIR", self.home.root())
            .env("SPEW_MESH_LAN", "0")
            .env("USER", self.person)
            .env("PATH", self.tmp.path().join("bin"))
            .current_dir(self.tmp.path().join("work"))
            .stdin(Stdio::null());
        c
    }

    /// Runs a CLI command that needs the daemon already running (it never starts one here).
    async fn cli(&self, args: &[&str]) -> Output {
        tokio::process::Command::from(self.command(args))
            .output()
            .await
            .unwrap()
    }

    /// Starts `spew mesh daemon run` and waits until it answers on its control socket.
    async fn start(&mut self) {
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
    async fn stop(&mut self) {
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

    async fn client(&self) -> Client {
        Client::connect(&self.home, &ConnectOptions::existing())
            .await
            .unwrap()
    }

    async fn status(&self) -> (Status, NetworkDetails) {
        full_status(&self.client().await).await.unwrap()
    }

    /// The socket the endpoint is bound to, once it is up.
    async fn bound(&self) -> String {
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
    fn pin_port(&self, port: &str) {
        let mut settings = std::fs::read_to_string(self.home.settings_file()).unwrap();
        // `[network]` is the last table in the file, so the key lands in it.
        settings.push_str(&format!("port = {port}\n"));
        std::fs::write(self.home.settings_file(), settings).unwrap();
    }

    /// The device as this daemon's status shows it, if admitted.
    async fn link_to(&self, other: &Node) -> Option<spew_mesh::daemon::status::DeviceLink> {
        let (_, net) = self.status().await;
        net.devices
            .into_iter()
            .find(|d| d.endpoint_id == other.id.to_string())
    }

    async fn sees_online(&self, other: &Node) -> bool {
        self.link_to(other).await.is_some_and(|d| d.online)
    }

    /// A folder for sessions, with a `.spew.json` naming `room`.
    fn folder(&self, name: &str, room: &str) -> String {
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
    fn log(&self) -> String {
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
struct Roster {
    dir: tempfile::TempDir,
}

impl Roster {
    fn new() -> Roster {
        Roster {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self) -> PathBuf {
        self.dir.path().join("roster.toml")
    }

    /// Writes the roster in one rename, so a daemon polling it never reads half a file.
    fn write(&self, people: &[(&Node, Option<&str>)], rooms: &[(&str, &[&str])]) {
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
    async fn publish(&self, people: &[&Node], rooms: &[(&str, &[&str])]) {
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

async fn eventually(what: &str, mut check: impl AsyncFnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A session opened through a control connection that stays open.
struct Session {
    client: Client,
    token: String,
}

async fn open(node: &Node, cwd: &str) -> Session {
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
        })
        .await
        .unwrap();
    Session {
        client,
        token: opened.token,
    }
}

async fn fetch(s: &Session) -> Fetched {
    s.client
        .fetch(&FetchParams {
            token: s.token.clone(),
            wait_seconds: None,
            keep_unread: false,
        })
        .await
        .unwrap()
}

/// Whether the session's directory lists `address` online.
async fn sees(s: &Session, address: &str) -> bool {
    let dir = s
        .client
        .directory(&DirectoryQuery {
            token: Some(s.token.clone()),
            filter: None,
            include_offline: true,
        })
        .await
        .unwrap();
    dir.rooms
        .iter()
        .flat_map(|r| &r.people)
        .flat_map(|p| &p.sessions)
        .any(|row| row.address == address && row.online)
}

/// Whether the session's directory names `address` at all, online or not.
async fn knows(s: &Session, address: &str) -> bool {
    let dir = s
        .client
        .directory(&DirectoryQuery {
            token: Some(s.token.clone()),
            filter: None,
            include_offline: true,
        })
        .await
        .unwrap();
    dir.rooms
        .iter()
        .flat_map(|r| &r.people)
        .flat_map(|p| &p.sessions)
        .any(|row| row.address == address)
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_devices_link_directly_see_their_rooms_and_ask_across() {
    let roster = Roster::new();
    let mut sam = Node::prepare("sam", "sam-mac", &roster.path());
    let mut maya = Node::prepare("maya", "maya-mbp", &roster.path());
    let mut lee = Node::prepare("lee", "lee-linux", &roster.path());
    let rooms: &[(&str, &[&str])] = &[("team", &["sam", "maya"]), ("ops", &["maya", "lee"])];
    roster.write(&[(&sam, None), (&maya, None), (&lee, None)], rooms);
    sam.start().await;
    maya.start().await;
    lee.start().await;
    roster.publish(&[&sam, &maya, &lee], rooms).await;

    eventually("sam and maya link", async || sam.sees_online(&maya).await).await;
    eventually("maya and lee link", async || maya.sees_online(&lee).await).await;
    eventually("maya sees sam online", async || {
        maya.sees_online(&sam).await
    })
    .await;

    // What daemon.status says about the network (5.1, 5.3).
    let (status, net) = sam.status().await;
    assert_eq!(net.network, "local");
    assert_eq!(status.network.network, "local");
    assert_eq!(
        net.endpoint_id.as_deref(),
        Some(sam.id.to_string().as_str())
    );
    assert!(net.relay.is_none() && net.relays.is_empty(), "{net:?}");
    assert!(
        !net.bound_sockets.is_empty()
            && net
                .bound_sockets
                .iter()
                .all(|s| s.starts_with("127.0.0.1:")),
        "{net:?}"
    );
    assert_eq!(status.bound_sockets, net.bound_sockets);
    let to_maya = sam.link_to(&maya).await.unwrap();
    assert_eq!(to_maya.path.as_deref(), Some("direct"), "{to_maya:?}");
    assert_eq!(
        (to_maya.person.as_str(), to_maya.name.as_str()),
        ("maya", "maya-mbp")
    );
    // sam shares no room with lee: lee isn't admitted, so there is no link and no row.
    assert!(sam.link_to(&lee).await.is_none(), "{net:?}");
    assert!(
        net.links.iter().all(|l| l.device != lee.id.to_string()),
        "{net:?}"
    );
    let shown = sam.cli(&["daemon", "status"]).await;
    assert!(shown.status.success(), "{}", text(&shown));
    let shown = text(&shown);
    assert!(shown.contains("network: local"), "{shown}");
    assert!(
        shown.contains("link maya@maya-mbp, online, direct"),
        "{shown}"
    );

    // Presence follows the rooms: each session is seen only by people who share its room.
    let api = open(&sam, &sam.folder("api", "team")).await;
    let web = open(&maya, &maya.folder("web", "team")).await;
    let infra = open(&maya, &maya.folder("infra", "ops")).await;
    let deploy = open(&lee, &lee.folder("deploy", "ops")).await;
    eventually("sam sees maya's team session", async || {
        sees(&api, "maya@maya-mbp/claude-web").await
    })
    .await;
    eventually("lee sees maya's ops session", async || {
        sees(&deploy, "maya@maya-mbp/claude-infra").await
    })
    .await;
    eventually("maya sees sam's and lee's sessions", async || {
        sees(&web, "sam@sam-mac/claude-api").await
            && sees(&infra, "lee@lee-linux/claude-deploy").await
    })
    .await;
    assert!(!knows(&api, "maya@maya-mbp/claude-infra").await);
    assert!(!knows(&api, "lee@lee-linux/claude-deploy").await);
    assert!(!knows(&deploy, "maya@maya-mbp/claude-web").await);
    assert!(!knows(&deploy, "sam@sam-mac/claude-api").await);

    // An ask from sam's device answered on maya's, back in the same call.
    let asker = api.client.clone();
    let token = api.token.clone();
    let pending = tokio::spawn(async move {
        asker
            .ask(&AskParams {
                token,
                to: "maya/claude-web".into(),
                text: "which port does the dev server use?".into(),
                card: None,
                wait_seconds: Some(45),
            })
            .await
            .unwrap()
    });
    let mut got = None;
    eventually("the ask reaches maya", async || {
        got = fetch(&web).await.messages.into_iter().next();
        got.is_some()
    })
    .await;
    let question = got.unwrap();
    assert_eq!(question.kind, MessageKind::Ask);
    assert_eq!(question.person, "sam");
    assert_eq!(question.room.as_deref(), Some("team"));
    let replied = web
        .client
        .reply(&ReplyParams {
            token: web.token.clone(),
            id: question.id.clone(),
            text: "5220".into(),
            wait_seconds: None,
        })
        .await
        .unwrap();
    assert_eq!(replied.status, SendStatus::Delivered, "{}", replied.note);
    let result = pending.await.unwrap();
    assert_eq!(result.status, SendStatus::Answered, "{}", result.note);
    assert_eq!(result.answer.unwrap().body, "5220");

    // `who --links` shows the direct path to maya's device and nothing of lee's.
    let who = sam.cli(&["who", "--links", "--offline"]).await;
    assert!(who.status.success(), "{}", text(&who));
    let who = text(&who);
    assert!(who.contains("maya@maya-mbp  online  direct"), "{who}");
    assert!(!who.contains("lee"), "{who}");
    drop((infra, deploy));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tell_to_a_stopped_device_is_delivered_when_it_starts_again() {
    // sam is the designated dialer, so it is sam that brings the link back once maya is up again.
    let roster = Roster::new();
    let (mut sam, mut maya) = Node::dialer_pair(
        ("sam", "sam-mac", &roster.path()),
        ("maya", "maya-mbp", &roster.path()),
    );
    let rooms: &[(&str, &[&str])] = &[("team", &["sam", "maya"])];
    roster.write(&[(&sam, None), (&maya, None)], rooms);
    sam.start().await;
    maya.start().await;
    roster.publish(&[&sam, &maya], rooms).await;

    let api = open(&sam, &sam.folder("api", "team")).await;
    let web = open(&maya, &maya.folder("web", "team")).await;
    eventually("sam sees maya's session", async || {
        sees(&api, "maya@maya-mbp/claude-web").await
    })
    .await;
    // maya comes back on the port the roster names, as a device with `[network] port` set does.
    let port = maya.bound().await.rsplit(':').next().unwrap().to_string();
    maya.pin_port(&port);

    // `daemon stop` records that the daemon is meant to stay stopped and closes its links cleanly, so sam sees
    // maya go offline long before an idle timeout would say so.
    let stopped_at = Instant::now();
    maya.stop().await;
    assert!(maya.home.is_stopped(), "daemon stop writes run/stopped");
    eventually("sam sees maya offline", async || {
        !sam.sees_online(&maya).await
    })
    .await;
    assert!(
        stopped_at.elapsed() < CLEAN_CLOSE,
        "maya went offline only after {:?}",
        stopped_at.elapsed()
    );
    assert!(!sees(&api, "maya@maya-mbp/claude-web").await);

    let told = api
        .client
        .tell(&TellParams {
            token: api.token.clone(),
            to: "maya/claude-web".into(),
            text: "the schema changed; rebase before you push".into(),
            card: None,
        })
        .await
        .unwrap();
    assert_eq!(told.status, SendStatus::Queued, "{}", told.note);

    // The daemon that starts again clears run/stopped, and sam's dialer reaches it at the same address.
    maya.start().await;
    assert!(
        !maya.home.is_stopped(),
        "a daemon holding the lock clears run/stopped"
    );
    let web_token = web.token.clone();
    drop(web);
    let web = Session {
        client: maya.client().await,
        token: web_token,
    };
    let mut got = Vec::new();
    eventually("the queued tell reaches maya's session", async || {
        got.extend(fetch(&web).await.messages);
        !got.is_empty()
    })
    .await;
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].id, told.id);
    assert_eq!(got[0].kind, MessageKind::Tell);
    assert_eq!(got[0].body, "the schema changed; rebase before you push");
    assert!(sam.sees_online(&maya).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_the_roster_does_not_list_is_refused() {
    // sam's roster names sam and maya; lee's own roster names lee and sam, so lee dials sam and is refused.
    let sams_roster = Roster::new();
    let lees_roster = Roster::new();
    let (mut lee, mut sam) = Node::dialer_pair(
        ("lee", "lee-linux", &lees_roster.path()),
        ("sam", "sam-mac", &sams_roster.path()),
    );
    let maya = Node::prepare("maya", "maya-mbp", &sams_roster.path());
    sams_roster.write(
        &[(&sam, None), (&maya, None)],
        &[("team", &["sam", "maya"])],
    );
    sam.start().await;
    let sam_addr = sam.bound().await;
    lees_roster.write(
        &[(&lee, None), (&sam, Some(&sam_addr))],
        &[("ops", &["sam", "lee"])],
    );
    lee.start().await;

    let sam_id = sam.id.to_string();
    eventually("lee records that sam refused it", async || {
        let (_, net) = lee.status().await;
        net.problems
            .iter()
            .any(|p| p.device == sam_id && p.problem == "refused")
    })
    .await;
    // A refused connection from a device sam's roster doesn't know makes sam read its roster again (4.2).
    eventually("sam refreshes its roster", async || {
        sam.log().contains("refreshing the roster")
    })
    .await;

    let (_, net) = sam.status().await;
    assert!(net.links.is_empty(), "{net:?}");
    assert!(sam.link_to(&lee).await.is_none(), "{net:?}");
    let to_sam = lee.link_to(&sam).await.unwrap();
    assert!(!to_sam.online, "{to_sam:?}");
    let shown = lee.cli(&["daemon", "status"]).await;
    let shown = text(&shown);
    assert!(
        shown.contains(&format!("fix: {}", sam.address()))
            && shown.contains("its roster doesn't admit this device"),
        "{shown}"
    );
    let doctor = lee.cli(&["doctor"]).await;
    assert_eq!(doctor.status.code(), Some(1), "{}", text(&doctor));
    assert!(
        text(&doctor).contains(&format!("fix: {}", sam.address())),
        "{}",
        text(&doctor)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removing_devices_from_the_roster_fails_their_mail_and_closes_links() {
    let roster = Roster::new();
    let mut sam = Node::prepare("sam", "sam-mac", &roster.path());
    let mut maya = Node::prepare("maya", "maya-mbp", &roster.path());
    let mut lee = Node::prepare("lee", "lee-linux", &roster.path());
    let everyone: &[(&str, &[&str])] = &[("team", &["sam", "maya", "lee"])];
    roster.write(&[(&sam, None), (&maya, None), (&lee, None)], everyone);
    sam.start().await;
    maya.start().await;
    lee.start().await;
    roster.publish(&[&sam, &maya, &lee], everyone).await;

    let api = open(&sam, &sam.folder("api", "team")).await;
    let _deploy = open(&lee, &lee.folder("deploy", "team")).await;
    eventually("sam sees lee's session", async || {
        sees(&api, "lee@lee-linux/claude-deploy").await
    })
    .await;
    eventually("sam and maya link", async || sam.sees_online(&maya).await).await;

    // Mail for lee waits while lee is stopped...
    lee.stop().await;
    eventually("sam sees lee offline", async || {
        !sam.sees_online(&lee).await
    })
    .await;
    let told = api
        .client
        .tell(&TellParams {
            token: api.token.clone(),
            to: "lee/claude-deploy".into(),
            text: "can you take the deploy tonight?".into(),
            card: None,
        })
        .await
        .unwrap();
    assert_eq!(told.status, SendStatus::Queued, "{}", told.note);

    // ...and fails with not-shared once the roster drops lee (4.3), while maya's link stays up.
    let team: &[(&str, &[&str])] = &[("team", &["sam", "maya"])];
    roster.publish(&[&sam, &maya], team).await;
    let mut notices = Vec::new();
    eventually("sam hears that the tell to lee failed", async || {
        notices.extend(fetch(&api).await.messages);
        !notices.is_empty()
    })
    .await;
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, MessageKind::Notice);
    assert!(
        notices[0].body.contains(&told.id)
            && notices[0].body.contains("no longer in a shared room"),
        "{}",
        notices[0].body
    );
    assert!(sam.link_to(&lee).await.is_none());
    assert!(sam.sees_online(&maya).await);

    // Without a shared room maya isn't admitted either: the link closes, and with nobody left to reach, each
    // endpoint goes down.
    let apart: &[(&str, &[&str])] = &[("team", &["sam"]), ("notes", &["maya"])];
    roster.publish(&[&sam, &maya], apart).await;
    eventually(
        "sam's link to maya closes and its endpoint goes down",
        async || {
            let (_, net) = sam.status().await;
            net.network == "off" && net.links.is_empty() && net.devices.is_empty()
        },
    )
    .await;
    eventually("maya's side closes too", async || {
        let (_, net) = maya.status().await;
        net.network == "off" && net.links.is_empty()
    })
    .await;
    let shown = text(&sam.cli(&["daemon", "status"]).await);
    assert!(shown.contains("network: off"), "{shown}");
}
