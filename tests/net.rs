//! Two and three daemons on real iroh links (15), as child processes on temp homes in `local` mode on 127.0.0.1
//! (the harness is in `net_support`): presence by room, asks across devices, a tell to a stopped device, a
//! refused stranger, roster removal, and a device that comes back on a new port.

mod net_support;

use std::time::{Duration, Instant};

use net_support::{CLEAN_CLOSE, Node, Roster, Session, eventually, fetch, knows, open, sees, text};
use spew_mesh::control::protocol::{AskParams, ReplyParams, SendResult, SendStatus, TellParams};
use spew_mesh::courier::CONVERSATION_LIMIT;
use spew_mesh::wire::MessageKind;

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
    let port: u16 = maya
        .bound()
        .await
        .rsplit(':')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    maya.pin_port(port);

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

/// How soon a designated dialer must reach a device at its new address once the roster names it; a dial stuck
/// on the old address would give up only after the 15 s dial timeout (5.3).
const REDIAL: Duration = Duration::from_secs(7);

/// A free UDP port on 127.0.0.1 other than `not`.
fn free_port(not: u16) -> u16 {
    loop {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        if port != not {
            return port;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_that_restarts_on_a_new_port_is_dialed_there_at_once() {
    // sam is the designated dialer, so it is sam that has to find maya at her new address.
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
    eventually("sam and maya link", async || sam.sees_online(&maya).await).await;
    let old = maya.bound().await;
    let old_port: u16 = old.rsplit(':').next().unwrap().parse().unwrap();

    // Once maya's link closes, sam's dialer dials her old address at once, and nothing answers there.
    maya.stop().await;
    eventually("sam sees maya offline", async || {
        !sam.sees_online(&maya).await
    })
    .await;
    maya.pin_port(free_port(old_port));
    maya.start().await;
    let new = maya.bound().await;
    assert_ne!(new, old);

    // The new address cancels the dial to the old one, and sam dials again right away.
    let published = Instant::now();
    roster.publish(&[&sam, &maya], rooms).await;
    eventually("sam links to maya at her new address", async || {
        sam.sees_online(&maya).await
    })
    .await;
    assert!(
        published.elapsed() < REDIAL,
        "sam reached maya's new address only after {:?}",
        published.elapsed()
    );
    let link = sam.link_to(&maya).await.unwrap();
    assert_eq!(link.path.as_deref(), Some("direct"), "{link:?}");
}

async fn tell(from: &Session, to: &str, text: &str) -> SendResult {
    from.client
        .tell(&TellParams {
            token: from.token.clone(),
            to: to.into(),
            text: text.into(),
            card: None,
        })
        .await
        .unwrap()
}

async fn reply(from: &Session, id: &str, text: &str) -> SendResult {
    from.client
        .reply(&ReplyParams {
            token: from.token.clone(),
            id: id.into(),
            text: text.into(),
            wait_seconds: None,
        })
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_device_asks_ended_sessions_refuse_and_the_guard_holds_with_links_up() {
    let roster = Roster::new();
    let mut sam = Node::prepare("sam", "sam-mac", &roster.path());
    let mut maya = Node::prepare("maya", "maya-mbp", &roster.path());
    let rooms: &[(&str, &[&str])] = &[("team", &["sam", "maya"])];
    roster.write(&[(&sam, None), (&maya, None)], rooms);
    sam.start().await;
    maya.start().await;
    roster.publish(&[&sam, &maya], rooms).await;
    let api = open(&sam, &sam.folder("api", "team")).await;
    let notes = open(&sam, &sam.folder("notes", "team")).await;
    let web = open(&maya, &maya.folder("web", "team")).await;
    let deploy = open(&maya, &maya.folder("deploy", "team")).await;
    eventually("sam sees maya's sessions", async || {
        sees(&api, "maya@maya-mbp/claude-web").await
            && sees(&api, "maya@maya-mbp/claude-deploy").await
    })
    .await;

    // Ask and answer between two sessions on one device while the device is linked to another (C1).
    let asker = api.client.clone();
    let token = api.token.clone();
    let pending = tokio::spawn(async move {
        asker
            .ask(&AskParams {
                token,
                to: "claude-notes".into(),
                text: "did the migration run?".into(),
                card: None,
                wait_seconds: Some(45),
            })
            .await
            .unwrap()
    });
    let mut got = None;
    eventually("the ask reaches the notes session", async || {
        got = fetch(&notes).await.messages.into_iter().next();
        got.is_some()
    })
    .await;
    let question = got.unwrap();
    assert_eq!(question.kind, MessageKind::Ask);
    let replied = reply(&notes, &question.id, "yes, at 14:00").await;
    assert_eq!(replied.status, SendStatus::Delivered, "{}", replied.note);
    let answered = pending.await.unwrap();
    assert_eq!(answered.status, SendStatus::Answered, "{}", answered.note);
    assert_eq!(answered.answer.unwrap().body, "yes, at 14:00");

    // A session that ends with the message unread refuses it with session-ended, and the sender's session
    // hears so from a notice (6.4, 7.6).
    let told = tell(&api, "maya/claude-web", "the schema changed").await;
    assert_eq!(told.status, SendStatus::Delivered, "{}", told.note);
    web.client.session_close(&web.token).await.unwrap();
    let mut notices = Vec::new();
    eventually("sam hears the tell was refused", async || {
        notices.extend(fetch(&api).await.messages);
        !notices.is_empty()
    })
    .await;
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert_eq!(notices[0].kind, MessageKind::Notice);
    assert!(
        notices[0].body.contains(&told.id)
            && notices[0]
                .body
                .contains("that session ended before it saw the message"),
        "{}",
        notices[0].body
    );

    // Two agents on two devices replying to each other: the conversation guard stops the one message too many
    // (5.5, 7.6).
    let first = tell(&api, "maya/claude-deploy", "can you look at the login bug?").await;
    assert_eq!(first.status, SendStatus::Delivered, "{}", first.note);
    let mut last = first.id;
    for n in 1..CONVERSATION_LIMIT {
        let from = if n % 2 == 1 { &deploy } else { &api };
        let r = reply(from, &last, "more on that").await;
        assert_eq!(r.status, SendStatus::Delivered, "message {n}: {}", r.note);
        last = r.id;
    }
    let from = if CONVERSATION_LIMIT % 2 == 1 {
        &deploy
    } else {
        &api
    };
    let stopped = reply(from, &last, "and another thing").await;
    assert_eq!(stopped.status, SendStatus::Refused, "{}", stopped.note);
    assert_eq!(stopped.code.as_deref(), Some("conversation-limit"));
    assert!(
        stopped.note.contains("Pause and check with your person"),
        "{}",
        stopped.note
    );
}
