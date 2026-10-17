use super::*;
use crate::agents::hook::Facts;

fn store() -> Store {
    Store::open_in_memory().unwrap()
}

fn no_procs(_: u32) -> Option<u64> {
    Some(1)
}

fn ctx(now: u64) -> Context<'static> {
    Context {
        now,
        default_room: None,
        start_time: &no_procs,
    }
}

fn folder(name: &str) -> (tempfile::TempDir, String) {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let cwd = dir.to_string_lossy().into_owned();
    (tmp, cwd)
}

fn live(client: &str, cwd: &str) -> OpenRequest {
    OpenRequest {
        client: client.into(),
        client_session: None,
        client_pid: None,
        cwd: cwd.into(),
        kind: SessionKind::Live,
        name: None,
        spew_project: None,
        reach: Reach::Inbox,
    }
}

fn hook(client: Client, event: MeshEvent, cs: &str, pid: Option<u32>, cwd: &str) -> HookCall {
    HookCall {
        client,
        event,
        client_session: cs.into(),
        client_pid: pid,
        cwd: cwd.into(),
        show: true,
        facts: Facts::default(),
    }
}

#[test]
fn names_follow_the_place_and_clashes_add_letters() {
    let s = store();
    let mut reg = Registry::default();
    let (_t, cwd) = folder("Spew App");
    let a = reg.open(&s, &live("claude", &cwd), &ctx(1)).unwrap();
    let b = reg.open(&s, &live("claude", &cwd), &ctx(2)).unwrap();
    assert_eq!(a.session.name, "claude-spew-app");
    assert_eq!(b.session.name, "claude-spew-app-b");
    assert!(a.created && b.created);
    assert_ne!(a.token, b.token);
    // Tokens find their session; nothing else does.
    let found = reg
        .session_for_token(&s, a.token.as_deref().unwrap())
        .unwrap();
    assert_eq!(found.id, a.session.id);
    assert_eq!(
        reg.session_for_token(&s, "nope").unwrap_err().reason,
        crate::control::protocol::Reason::NotFound
    );
}

#[test]
fn responders_and_the_terminal() {
    let s = store();
    let mut reg = Registry::default();
    let (_t, cwd) = folder("api");
    let mut req = live("claude", &cwd);
    req.kind = SessionKind::Responder;
    req.reach = Reach::Serve;
    req.client_session = Some("serve-1".into());
    let r = reg.open(&s, &req, &ctx(1)).unwrap();
    assert_eq!(r.session.name, "claude");
    assert_eq!(r.session.state, ActivityState::Idle);
    // Re-opening after a lost connection keeps the session.
    let again = reg.open(&s, &req, &ctx(2)).unwrap();
    assert_eq!(again.session.id, r.session.id);
    assert!(!again.created);

    let mut named = req.clone();
    named.client_session = Some("serve-2".into());
    named.name = Some("reviewer".into());
    assert_eq!(
        reg.open(&s, &named, &ctx(3)).unwrap().session.name,
        "reviewer"
    );

    let mut term = live(TERMINAL_CLIENT, &cwd);
    term.kind = SessionKind::Terminal;
    let t1 = reg.open(&s, &term, &ctx(4)).unwrap();
    let t2 = reg.open(&s, &term, &ctx(5)).unwrap();
    assert_eq!(t1.session.name, TERMINAL_NAME);
    assert_eq!(t1.session.id, t2.session.id);

    let mut bad = live("claude", &cwd);
    bad.kind = SessionKind::Terminal;
    assert!(reg.open(&s, &bad, &ctx(6)).is_err());
    let mut bad_name = live("claude", &cwd);
    bad_name.name = Some("x".into());
    assert!(reg.open(&s, &bad_name, &ctx(6)).is_err());
    assert!(reg.open(&s, &live("mesh", &cwd), &ctx(6)).is_err());
}

#[test]
fn claude_hooks_and_mcp_meet_on_the_process_id() {
    let s = store();
    let mut reg = Registry::default();
    let (_t, cwd) = folder("api");
    let mut mcp = live("claude", &cwd);
    mcp.client_pid = Some(4242);
    mcp.client_session = Some("sess-a".into());
    let opened = reg.open(&s, &mcp, &ctx(1)).unwrap();
    let ev = hook(Client::Claude, MeshEvent::Start, "sess-a", Some(4242), &cwd);
    let hooked = reg.for_hook(&s, &ev, &ctx(2)).unwrap().unwrap();
    assert_eq!(hooked.session.id, opened.session.id);
    assert_eq!(hooked.session.reach, Reach::Hooked);
    // A clear in the same process brings a new session id and keeps the mesh session.
    let cleared = hook(Client::Claude, MeshEvent::Start, "sess-b", Some(4242), &cwd);
    let kept = reg.for_hook(&s, &cleared, &ctx(3)).unwrap().unwrap();
    assert_eq!(kept.session.id, opened.session.id);
    assert_eq!(kept.session.client_session.as_deref(), Some("sess-b"));
    // An end for a session nobody knows opens nothing.
    let stray = hook(Client::Claude, MeshEvent::End, "sess-z", Some(9), &cwd);
    assert!(reg.for_hook(&s, &stray, &ctx(4)).unwrap().is_none());
}

#[test]
fn codex_threads_join_the_lone_server_session_or_merge_later() {
    let s = store();
    let mut reg = Registry::default();
    let (_t, cwd) = folder("api");
    let first = reg.open(&s, &live("codex", &cwd), &ctx(1)).unwrap();
    let ev = hook(Client::Codex, MeshEvent::Start, "thread-1", None, &cwd);
    let joined = reg.for_hook(&s, &ev, &ctx(2)).unwrap().unwrap();
    assert_eq!(joined.session.id, first.session.id);

    // Two windows in one folder: the hook can't tell, so it opens its own session; the server binds its
    // thread id at its first tool call and the two merge, the older keeping its name.
    let w1 = reg.open(&s, &live("codex", &cwd), &ctx(10)).unwrap();
    let _w2 = reg.open(&s, &live("codex", &cwd), &ctx(11)).unwrap();
    let ev = hook(Client::Codex, MeshEvent::Prompt, "thread-2", None, &cwd);
    let own = reg.for_hook(&s, &ev, &ctx(12)).unwrap().unwrap();
    assert!(own.created);
    let (merged, gone) = reg
        .bind_client_session(&s, &w1.session.id, "thread-2", &ctx(13))
        .unwrap();
    assert_eq!(merged.id, w1.session.id);
    assert_eq!(merged.name, w1.session.name);
    assert_eq!(merged.reach, Reach::Hooked);
    assert_eq!(gone, vec![own.session.id.clone()]);
    assert_eq!(reg.resolve_id(&own.session.id), w1.session.id);
    assert!(reg.ids_of(&w1.session.id).contains(&own.session.id));
    assert!(
        s.session(&own.session.id)
            .unwrap()
            .unwrap()
            .ended_at
            .is_some()
    );
    let by_thread = s.session_by_client("codex", "thread-2").unwrap().unwrap();
    assert_eq!(by_thread.id, w1.session.id);
}

#[test]
fn a_merge_keeps_the_newer_sessions_token_working() {
    let s = store();
    let mut reg = Registry::default();
    let (_t, cwd) = folder("api");
    let hooked = reg
        .for_hook(
            &s,
            &hook(Client::Codex, MeshEvent::Start, "t-9", None, &cwd),
            &ctx(1),
        )
        .unwrap()
        .unwrap();
    let a = reg.open(&s, &live("codex", &cwd), &ctx(2)).unwrap();
    let b = reg.open(&s, &live("codex", &cwd), &ctx(3)).unwrap();
    let _ = b;
    let (survivor, _) = reg
        .bind_client_session(&s, &a.session.id, "t-9", &ctx(4))
        .unwrap();
    assert_eq!(survivor.id, hooked.session.id, "the older session survives");
    let via_token = reg
        .session_for_token(&s, a.token.as_deref().unwrap())
        .unwrap();
    assert_eq!(via_token.id, hooked.session.id);
}

#[test]
fn opencode_servers_without_the_plugin_are_one_session_per_process() {
    let s = store();
    let mut reg = Registry::default();
    let (_t, cwd) = folder("api");
    let mut req = live("opencode", &cwd);
    req.client_pid = Some(77);
    let a = reg.open(&s, &req, &ctx(1)).unwrap();
    let b = reg.open(&s, &req, &ctx(2)).unwrap();
    assert_eq!(a.session.id, b.session.id);
    // Plugin hooks open their own sessions keyed on the OpenCode session id.
    let ev = hook(Client::Opencode, MeshEvent::Start, "ses_1", Some(77), &cwd);
    let hooked = reg.for_hook(&s, &ev, &ctx(3)).unwrap().unwrap();
    assert_ne!(hooked.session.id, a.session.id);
}

#[test]
fn the_four_end_rules() {
    let s = store();
    let mut reg = Registry::default();
    let (_t, cwd) = folder("api");
    // Rule 2: a process that is gone, or whose start time changed.
    let mut with_pid = live("claude", &cwd);
    with_pid.client_pid = Some(10);
    let p = reg.open(&s, &with_pid, &ctx(0)).unwrap();
    // Rule 3: a connection that closed.
    let c = reg.open(&s, &live("other", &cwd), &ctx(0)).unwrap();
    reg.attach(5, &c.session.id);
    // Rule 4: hooks only.
    let h = reg
        .for_hook(
            &s,
            &hook(Client::Codex, MeshEvent::Start, "t", None, &cwd),
            &ctx(0),
        )
        .unwrap()
        .unwrap();
    let live_now = s.live_sessions().unwrap();

    assert!(reg.due(&live_now, 1_000, true, &no_procs).is_empty());
    let changed = |_: u32| Some(2);
    let due = reg.due(&live_now, 1_000, true, &changed);
    assert_eq!(due, vec![(p.session.id.clone(), EndReason::ProcessExited)]);
    assert!(
        reg.due(&live_now, 1_000, false, &changed).is_empty(),
        "pids only every 30 s"
    );

    reg.connection_closed(5, 2_000);
    assert!(
        reg.due(&live_now, 2_000 + DETACH_GRACE_MS - 1, false, &no_procs)
            .is_empty()
    );
    let due = reg.due(&live_now, 2_000 + DETACH_GRACE_MS, false, &no_procs);
    assert_eq!(due, vec![(c.session.id.clone(), EndReason::Detached)]);
    // A new attach within the grace keeps it.
    reg.attach(6, &c.session.id);
    assert!(reg.due(&live_now, 60_000, false, &no_procs).is_empty());

    let due = reg.due(&live_now, SILENT_LIMIT_MS, false, &no_procs);
    assert_eq!(due, vec![(h.session.id.clone(), EndReason::Silent)]);
}

#[test]
fn after_a_restart_connected_sessions_get_a_minute() {
    let s = store();
    let mut reg = Registry::default();
    let (_t, cwd) = folder("api");
    let inbox = reg.open(&s, &live("gemini", &cwd), &ctx(0)).unwrap();
    let mut term = live(TERMINAL_CLIENT, &cwd);
    term.kind = SessionKind::Terminal;
    reg.open(&s, &term, &ctx(0)).unwrap();
    let restarted = Registry::restore(&s, 100_000).unwrap();
    let live_now = s.live_sessions().unwrap();
    assert!(
        restarted
            .due(&live_now, 100_000 + RESTART_GRACE_MS - 1, false, &no_procs)
            .is_empty()
    );
    let due = restarted.due(&live_now, 100_000 + RESTART_GRACE_MS, false, &no_procs);
    assert_eq!(due, vec![(inbox.session.id.clone(), EndReason::Detached)]);
    // Its token still works after the restart.
    assert!(
        restarted
            .session_for_token(&s, inbox.token.as_deref().unwrap())
            .is_ok()
    );
}

#[test]
fn rooms_come_from_spew_json_and_stay_private_when_broken() {
    let s = store();
    let mut reg = Registry::default();
    let (tmp, cwd) = folder("api");
    std::fs::write(tmp.path().join(".spew.json"), r#"{"project":"spew"}"#).unwrap();
    let a = reg.open(&s, &live("claude", &cwd), &ctx(1)).unwrap();
    assert_eq!(a.session.room.as_deref(), Some("spew"));
    assert!(room_from_spew_json(&a.session));
    let mut env = live("codex", &cwd);
    env.spew_project = Some("web".into());
    let b = reg.open(&s, &env, &ctx(2)).unwrap();
    assert_eq!(b.session.room.as_deref(), Some("web"));
    assert!(!room_from_spew_json(&b.session));
    std::fs::write(tmp.path().join(".spew.json"), "not json").unwrap();
    let c = reg.open(&s, &live("opencode", &cwd), &ctx(3)).unwrap();
    assert_eq!(c.session.room, None);
    assert_eq!(c.session.room_source.as_deref(), Some("invalid"));
}

#[test]
fn tool_shows_and_unanswered_lines_are_rationed() {
    let mut reg = Registry::default();
    assert!(reg.tool_show_allowed("s", 0));
    assert!(!reg.tool_show_allowed("s", 29_999));
    assert!(reg.tool_show_allowed("s", 30_000));
    assert!(reg.mention_unanswered("s", &["a".into()]));
    assert!(!reg.mention_unanswered("s", &["a".into()]));
    assert!(reg.mention_unanswered("s", &["a".into(), "b".into()]));
}
