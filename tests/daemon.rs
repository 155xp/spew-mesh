//! Daemons in one process on temp homes, linked by the in-memory network and driven through their control
//! sockets: asking and answering on one device and between devices, sessions ending with unread mail,
//! address errors, the outbox across a link that goes down and up, visibility and receipts.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use spew_mesh::control::Client;
use spew_mesh::control::protocol::{
    AskParams, DirectoryQuery, FetchParams, Fetched, Reason, ReplyParams, SendStatus, SessionOpen,
    SessionUpdate, TellParams,
};
use spew_mesh::daemon::testnet::{MemLinks, MemNet};
use spew_mesh::daemon::{self, Daemon, DaemonOptions};
use spew_mesh::identity::DeviceIdentity;
use spew_mesh::paths::Home;
use spew_mesh::settings::Env;
use spew_mesh::spew_api::SystemClock;
use spew_mesh::store::MessageState;
use spew_mesh::wire::{MessageKind, SessionKind};

use iroh::EndpointId;

const WAIT: Duration = Duration::from_secs(10);

struct Person {
    name: &'static str,
    device: &'static str,
}

const SAM: Person = Person {
    name: "sam",
    device: "sam-mac",
};
const MAYA: Person = Person {
    name: "maya",
    device: "maya-mbp",
};
const LEE: Person = Person {
    name: "lee",
    device: "lee-linux",
};

/// One device: its home, its daemon and a folder to run sessions in.
struct Device {
    tmp: tempfile::TempDir,
    home: Home,
    id: EndpointId,
    daemon: Option<Daemon<MemLinks>>,
}

impl Device {
    fn prepare(p: &Person) -> Device {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home")).unwrap();
        home.ensure().unwrap();
        let (identity, _) = DeviceIdentity::load_or_create(&home, p.device, 1).unwrap();
        Device {
            id: identity.endpoint_id(),
            tmp,
            home,
            daemon: None,
        }
    }

    fn write_settings(&self, person: &Person) {
        let roster = self.tmp.path().join("roster.toml");
        let text = format!(
            "username = \"{}\"\n\n[roster]\nkind = \"file\"\nfile = \"{}\"\n",
            person.name,
            roster.display()
        );
        std::fs::write(self.home.settings_file(), text).unwrap();
    }

    fn write_roster(&self, text: &str) {
        std::fs::write(self.tmp.path().join("roster.toml"), text).unwrap();
    }

    async fn start(&mut self, net: &MemNet) {
        let opts = DaemonOptions {
            home: self.home.clone(),
            user_home: Some(self.tmp.path().to_path_buf()),
            env: Env {
                lan_off: true,
                ..Env::default()
            },
            runtime_dirs: Vec::new(),
            clock: Arc::new(SystemClock),
        };
        let net = net.clone();
        let started = daemon::start(opts, move |id| net.join(id))
            .await
            .unwrap()
            .expect("the lock is free");
        assert_eq!(started.device, self.id);
        self.daemon = Some(started);
    }

    async fn stop(&mut self, net: &MemNet) {
        if let Some(d) = self.daemon.take() {
            d.finish().await;
        }
        net.leave(&self.id);
    }

    fn daemon(&self) -> &Daemon<MemLinks> {
        self.daemon.as_ref().unwrap()
    }

    async fn client(&self) -> Client {
        Client::connect_path(&self.daemon().socket).await.unwrap()
    }

    // A folder for sessions, with a `.spew.json` naming `room` when given.
    fn folder(&self, name: &str, room: Option<&str>) -> String {
        let dir = self.tmp.path().join("work").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(room) = room {
            std::fs::write(
                dir.join(".spew.json"),
                format!("{{\"project\": \"{room}\"}}"),
            )
            .unwrap();
        }
        dir.to_string_lossy().into_owned()
    }
}

fn roster_text(people: &[(&Person, &Device)], rooms: &[(&str, &[&str])]) -> String {
    let mut text = String::new();
    for (p, d) in people {
        text.push_str(&format!(
            "[[person]]\nname = \"{}\"\ndevices = [ {{ id = \"{}\", name = \"{}\" }} ]\n\n",
            p.name, d.id, p.device
        ));
    }
    for (room, members) in rooms {
        let list: Vec<String> = members.iter().map(|m| format!("\"{m}\"")).collect();
        text.push_str(&format!(
            "[[room]]\nname = \"{room}\"\npeople = [{}]\n\n",
            list.join(", ")
        ));
    }
    text
}

/// A session opened through a control connection that stays open.
struct Session {
    client: Client,
    token: String,
    name: String,
}

async fn open(device: &Device, cwd: &str) -> Session {
    let client = device.client().await;
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
        name: opened.name,
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

async fn eventually(what: &str, mut check: impl AsyncFnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

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

/// sam and maya share `team`; maya and lee share `ops`.
async fn team() -> (MemNet, Device, Device, Device) {
    let net = MemNet::new();
    let mut sam = Device::prepare(&SAM);
    let mut maya = Device::prepare(&MAYA);
    let mut lee = Device::prepare(&LEE);
    let text = roster_text(
        &[(&SAM, &sam), (&MAYA, &maya), (&LEE, &lee)],
        &[("team", &["sam", "maya"]), ("ops", &["maya", "lee"])],
    );
    for (p, d) in [(&SAM, &sam), (&MAYA, &maya), (&LEE, &lee)] {
        d.write_settings(p);
        d.write_roster(&text);
    }
    sam.start(&net).await;
    maya.start(&net).await;
    lee.start(&net).await;
    (net, sam, maya, lee)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_sessions_on_one_device_ask_and_answer() {
    let net = MemNet::new();
    let mut sam = Device::prepare(&SAM);
    sam.start(&net).await;
    let api = open(&sam, &sam.folder("api", None)).await;
    let web = open(&sam, &sam.folder("web", None)).await;
    assert_eq!(api.name, "claude-api");
    assert_eq!(web.name, "claude-web");

    let asker = api.client.clone();
    let token = api.token.clone();
    let ask = tokio::spawn(async move {
        asker
            .ask(&AskParams {
                token,
                to: "claude-web".into(),
                text: "which port does the dev server use?".into(),
                card: None,
                wait_seconds: Some(20),
            })
            .await
            .unwrap()
    });
    let mut got = None;
    eventually("the ask arrives", async || {
        let f = fetch(&web).await;
        got = f.messages.into_iter().next();
        got.is_some()
    })
    .await;
    let question = got.unwrap();
    assert_eq!(question.kind, MessageKind::Ask);
    assert!(question.from.ends_with("/claude-api"));
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
    assert_eq!(replied.status, SendStatus::Delivered);
    let result = ask.await.unwrap();
    assert_eq!(result.status, SendStatus::Answered);
    let answer = result.answer.unwrap();
    assert_eq!(answer.body, "5220");
    assert!(answer.text.contains("[mesh "));
    // The answer was shown with the ask's result, so nothing is left unread and no late-answer notice came.
    assert!(fetch(&api).await.messages.is_empty());
    assert_eq!(
        sam.daemon().courier.sent_state(&question.id),
        Some(MessageState::Answered)
    );
    sam.stop(&net).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_ending_with_unread_mail_tells_the_senders() {
    let net = MemNet::new();
    let mut sam = Device::prepare(&SAM);
    sam.start(&net).await;
    let api = open(&sam, &sam.folder("api", None)).await;
    let web = open(&sam, &sam.folder("web", None)).await;
    let sent = api
        .client
        .tell(&TellParams {
            token: api.token.clone(),
            to: "claude-web".into(),
            text: "heads up: the schema changed".into(),
            card: None,
        })
        .await
        .unwrap();
    assert_eq!(sent.status, SendStatus::Delivered);
    web.client.session_close(&web.token).await.unwrap();
    let f = fetch(&api).await;
    assert_eq!(f.messages.len(), 1);
    assert_eq!(f.messages[0].kind, MessageKind::Notice);
    assert_eq!(f.messages[0].from, "mesh");
    assert!(f.messages[0].body.contains(&sent.id));
    assert!(f.text.contains("[mesh-notice"));
    // The ended session's token no longer works, and its name is free for a new session.
    let err = fetch_err(&web).await;
    assert_eq!(err, Reason::NotFound);
    let again = open(&sam, &sam.folder("web", None)).await;
    assert_eq!(again.name, "claude-web");
    sam.stop(&net).await;
}

async fn fetch_err(s: &Session) -> Reason {
    s.client
        .fetch(&FetchParams {
            token: s.token.clone(),
            wait_seconds: None,
            keep_unread: false,
        })
        .await
        .unwrap_err()
        .rpc()
        .map(|e| e.reason)
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn address_errors_say_what_is_wrong() {
    let net = MemNet::new();
    let mut sam = Device::prepare(&SAM);
    sam.start(&net).await;
    let api = open(&sam, &sam.folder("api", None)).await;
    let _web = open(&sam, &sam.folder("web", None)).await;
    let _web2 = open(&sam, &sam.folder("web", None)).await;
    let tell = |to: &str| TellParams {
        token: api.token.clone(),
        to: to.into(),
        text: "hello".into(),
        card: None,
    };
    let reason =
        |r: Result<_, spew_mesh::control::ClientError>| r.unwrap_err().rpc().unwrap().clone();
    let missing = reason(api.client.tell(&tell("claude-nowhere")).await);
    assert_eq!(missing.reason, Reason::NotFound);
    let bad = reason(api.client.tell(&tell("Not An Address!")).await);
    assert_eq!(bad.reason, Reason::Invalid);
    let unheld = reason(api.client.tell(&tell("spew#42")).await);
    assert_eq!(unheld.reason, Reason::NotFound);
    // An empty text is refused before anything is sent.
    let mut empty = tell("claude-web");
    empty.text = "  ".into();
    assert_eq!(
        reason(api.client.tell(&empty).await).reason,
        Reason::Invalid
    );
    // A reply to a message the session never received.
    let stray = reason(
        api.client
            .reply(&ReplyParams {
                token: api.token.clone(),
                id: "01J9ZK00000000000000000XYZ".into(),
                text: "hi".into(),
                wait_seconds: None,
            })
            .await,
    );
    assert_eq!(stray.reason, Reason::NotFound);
    sam.stop(&net).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn devices_deliver_and_receipts_reach_shown_and_answered() {
    let (net, mut sam, mut maya, mut lee) = team().await;
    let api = open(&sam, &sam.folder("api", Some("team"))).await;
    let web = open(&maya, &maya.folder("web", Some("team"))).await;
    eventually("sam sees maya's session", async || {
        sees(&api, "maya@maya-mbp/claude-web").await
    })
    .await;

    // A tell is delivered, then shown when maya reads it.
    let told = api
        .client
        .tell(&TellParams {
            token: api.token.clone(),
            to: "maya/claude-web".into(),
            text: "I am moving the login handler".into(),
            card: Some("team#7".into()),
        })
        .await
        .unwrap();
    assert_eq!(told.status, SendStatus::Delivered, "{}", told.note);
    let f = fetch(&web).await;
    assert_eq!(f.messages.len(), 1);
    assert_eq!(f.messages[0].person, "sam");
    assert_eq!(f.messages[0].room.as_deref(), Some("team"));
    assert!(f.text.contains("a teammate in project team"));
    let courier = &sam.daemon().courier;
    eventually("the shown receipt comes back", async || {
        courier.sent_state(&told.id) == Some(MessageState::Shown)
    })
    .await;

    // An ask that stops waiting, then is answered: answered on sam's side, the answer arrives with a notice.
    let asked = api
        .client
        .ask(&AskParams {
            token: api.token.clone(),
            to: "maya@maya-mbp/claude-web".into(),
            text: "does the new handler keep the session cookie?".into(),
            card: None,
            wait_seconds: Some(1),
        })
        .await
        .unwrap();
    assert_eq!(asked.status, SendStatus::Delivered, "{}", asked.note);
    assert!(asked.note.contains("not seen yet"), "{}", asked.note);
    let f = fetch(&web).await;
    let question = f.messages.iter().find(|m| m.id == asked.id).unwrap();
    eventually("the ask shows as seen", async || {
        courier.sent_state(&asked.id) == Some(MessageState::Shown)
    })
    .await;
    let answer = web
        .client
        .reply(&ReplyParams {
            token: web.token.clone(),
            id: question.id.clone(),
            text: "yes, it is set in the middleware".into(),
            wait_seconds: None,
        })
        .await
        .unwrap();
    assert_eq!(answer.status, SendStatus::Delivered);
    assert_eq!(courier.sent_state(&asked.id), Some(MessageState::Answered));
    let f = fetch(&api).await;
    let kinds: Vec<MessageKind> = f.messages.iter().map(|m| m.kind).collect();
    assert_eq!(kinds, [MessageKind::Answer, MessageKind::Notice]);
    assert_eq!(f.messages[0].body, "yes, it is set in the middleware");

    // An ask that waits gets its answer back in the same call.
    let asker = api.client.clone();
    let token = api.token.clone();
    let pending = tokio::spawn(async move {
        asker
            .ask(&AskParams {
                token,
                to: "maya/claude-web".into(),
                text: "and the CSRF token?".into(),
                card: None,
                wait_seconds: Some(20),
            })
            .await
            .unwrap()
    });
    let mut got = None;
    eventually("the second ask arrives", async || {
        got = fetch(&web)
            .await
            .messages
            .into_iter()
            .find(|m| m.kind == MessageKind::Ask && m.body.contains("CSRF"));
        got.is_some()
    })
    .await;
    web.client
        .reply(&ReplyParams {
            token: web.token.clone(),
            id: got.unwrap().id,
            text: "also kept".into(),
            wait_seconds: None,
        })
        .await
        .unwrap();
    let done = pending.await.unwrap();
    assert_eq!(done.status, SendStatus::Answered);
    assert_eq!(done.answer.unwrap().from, "maya@maya-mbp/claude-web");

    sam.stop(&net).await;
    maya.stop(&net).await;
    lee.stop(&net).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_outbox_waits_out_a_link_that_goes_down() {
    let (net, mut sam, mut maya, mut lee) = team().await;
    let api = open(&sam, &sam.folder("api", Some("team"))).await;
    let web = open(&maya, &maya.folder("web", Some("team"))).await;
    eventually("sam sees maya's session", async || {
        sees(&api, "maya@maya-mbp/claude-web").await
    })
    .await;
    net.cut(sam.id, maya.id);
    eventually("maya shows offline", async || {
        !sees(&api, "maya@maya-mbp/claude-web").await
    })
    .await;
    let told = api
        .client
        .tell(&TellParams {
            token: api.token.clone(),
            to: "maya/claude-web".into(),
            text: "ping me when you are back".into(),
            card: None,
        })
        .await
        .unwrap();
    assert_eq!(told.status, SendStatus::Queued, "{}", told.note);
    assert!(told.reason.as_deref().unwrap_or("").contains("offline"));
    assert!(fetch(&web).await.messages.is_empty());
    let courier = &sam.daemon().courier;
    assert_eq!(courier.sent_state(&told.id), Some(MessageState::Queued));

    net.heal(sam.id, maya.id);
    eventually("the queued message is delivered", async || {
        courier.sent_state(&told.id) == Some(MessageState::Delivered)
    })
    .await;
    let f = fetch(&web).await;
    assert_eq!(f.messages.len(), 1);
    assert_eq!(f.messages[0].body, "ping me when you are back");

    sam.stop(&net).await;
    maya.stop(&net).await;
    lee.stop(&net).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sessions_in_other_rooms_stay_out_of_reach() {
    let (net, mut sam, mut maya, mut lee) = team().await;
    let api = open(&sam, &sam.folder("api", Some("team"))).await;
    let ops_lee = open(&lee, &lee.folder("deploy", Some("ops"))).await;
    let web = open(&maya, &maya.folder("web", Some("team"))).await;
    let infra = open(&maya, &maya.folder("infra", Some("ops"))).await;
    let notes = open(&maya, &maya.folder("notes", None)).await;
    eventually("sam sees maya's team session", async || {
        sees(&api, "maya@maya-mbp/claude-web").await
    })
    .await;
    eventually("lee sees maya's ops session", async || {
        sees(&ops_lee, "maya@maya-mbp/claude-infra").await
    })
    .await;
    // sam sees neither maya's ops session nor her private one; lee doesn't see her team session.
    assert!(!sees(&api, "maya@maya-mbp/claude-infra").await);
    assert!(!sees(&api, "maya@maya-mbp/claude-notes").await);
    assert!(!sees(&ops_lee, "maya@maya-mbp/claude-web").await);
    let err = api
        .client
        .tell(&TellParams {
            token: api.token.clone(),
            to: "maya/claude-infra".into(),
            text: "hello".into(),
            card: None,
        })
        .await
        .unwrap_err();
    assert_eq!(err.rpc().unwrap().reason, Reason::NotFound);

    // A session that moves to another room while a message to it waits: the receiving device refuses it with
    // no-such-session, and the sender hears through a notice.
    net.cut(sam.id, maya.id);
    eventually("maya shows offline", async || {
        !sees(&api, "maya@maya-mbp/claude-web").await
    })
    .await;
    let told = api
        .client
        .tell(&TellParams {
            token: api.token.clone(),
            to: "maya/claude-web".into(),
            text: "are you on the login bug?".into(),
            card: None,
        })
        .await
        .unwrap();
    assert_eq!(told.status, SendStatus::Queued, "{}", told.note);
    web.client
        .session_update(&SessionUpdate {
            token: web.token.clone(),
            cwd: Some(maya.folder("web-ops", Some("ops"))),
            ..SessionUpdate::default()
        })
        .await
        .unwrap();
    net.heal(sam.id, maya.id);
    let courier = &sam.daemon().courier;
    eventually("the message is refused", async || {
        courier.sent_state(&told.id) == Some(MessageState::Refused)
    })
    .await;
    let f = fetch(&api).await;
    assert_eq!(f.messages.len(), 1);
    assert_eq!(f.messages[0].kind, MessageKind::Notice);
    assert!(
        f.messages[0].body.contains("no such session"),
        "{}",
        f.messages[0].body
    );
    assert!(fetch(&web).await.messages.is_empty());
    drop((infra, notes));

    sam.stop(&net).await;
    maya.stop(&net).await;
    lee.stop(&net).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn status_reports_the_roster_and_a_second_daemon_backs_off() {
    let (net, mut sam, mut maya, mut lee) = team().await;
    let client = sam.client().await;
    let status = client.status().await.unwrap();
    assert_eq!(status.roster.provider, "file");
    assert_eq!(status.roster.person, "sam");
    assert_eq!(status.roster.rooms, ["team"]);
    assert_eq!(status.network.network, "local");
    assert_eq!(status.device.name, "sam-mac");
    eventually("maya's device is online", async || {
        sam.client().await.status().await.unwrap().counts.online == 1
    })
    .await;
    // One daemon per home.
    let opts = DaemonOptions {
        home: sam.home.clone(),
        user_home: Some(sam.tmp.path().to_path_buf()),
        env: Env::default(),
        runtime_dirs: Vec::new(),
        clock: Arc::new(SystemClock),
    };
    let net2 = net.clone();
    assert!(
        daemon::start(opts, move |id| net2.join(id))
            .await
            .unwrap()
            .is_none()
    );
    // daemon.shutdown answers, then the daemon stops and its socket goes.
    let socket: PathBuf = sam.daemon().socket.clone();
    client.shutdown().await.unwrap();
    let d = sam.daemon.take().unwrap();
    tokio::time::timeout(WAIT, d.stopped()).await.unwrap();
    d.finish().await;
    assert!(!Path::new(&socket).exists());
    net.leave(&sam.id);
    maya.stop(&net).await;
    lee.stop(&net).await;
}
