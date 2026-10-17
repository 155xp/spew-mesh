//! `spew mesh mcp-server` driven over its stdio against a real daemon on a temp home (8.6): both handshakes, the
//! tool list, every tool, progress on a waiting ask, the Codex `_meta` binding, OpenCode's `_mesh_session`
//! argument, reconnecting after a crash, a stop that holds until a tool call, and closing sessions on exit.

mod agents_support;

use std::time::{Duration, Instant};

use agents_support::{Mesh, context_of, is_error, my_pid, open_session, text};
use serde_json::{Value, json};
use spew_mesh::control::protocol::{AskParams, FetchParams, ReplyParams, TellParams};
use spew_mesh::wire::{MessageKind, Reach};

fn meta_2026(client: &str) -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": client, "version": "2.1.302"},
        "io.modelcontextprotocol/clientCapabilities": {}
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn initialize_is_answered_at_once_and_the_session_opens_in_the_background() {
    let mesh = Mesh::new();
    let mut mcp = mesh.mcp(
        "api",
        &[
            ("SPEW_MESH_CLIENT", "claude"),
            ("CLAUDE_CODE_SESSION_ID", "5f0c2a9e-0001"),
        ],
    );
    // No daemon runs yet: the answer must not wait for one.
    let begun = Instant::now();
    let init = mcp.initialize("2025-11-25", "claude-code");
    assert!(
        begun.elapsed() < Duration::from_secs(3),
        "{:?}",
        begun.elapsed()
    );
    let r = &init["result"];
    assert_eq!(r["protocolVersion"], "2025-11-25");
    assert_eq!(r["serverInfo"]["name"], "spew-mesh");
    assert!(r["capabilities"]["tools"].is_object(), "{r}");
    assert!(
        r["instructions"]
            .as_str()
            .unwrap()
            .starts_with("spew mesh links this session")
    );

    let list = mcp.request("tools/list", json!({}));
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["who", "ask", "tell", "reply", "messages"]);
    assert_eq!(tools[0]["annotations"]["readOnlyHint"], true);
    assert_eq!(tools[1]["annotations"]["openWorldHint"], true);
    assert_eq!(tools[4]["annotations"]["destructiveHint"], false);
    assert!(!list.to_string().contains("_mesh_session"));

    // The session shows before any tool call (6.4), started by the server itself.
    mesh.wait_for_sessions("the server's session", |rows| {
        rows.iter()
            .any(|r| r.name == "claude-api" && r.reach == Reach::Inbox)
    })
    .await;

    let who = mcp.call("who", json!({}));
    assert!(!is_error(&who), "{who}");
    let t = text(&who);
    assert!(
        t.starts_with("You are sam@") && t.contains("/claude-api."),
        "{t}"
    );
    assert!(t.contains("No other agent sessions are online."), "{t}");
    assert!(who["structuredContent"]["rooms"].is_array(), "{who}");
}

#[tokio::test(flavor = "multi_thread")]
async fn server_discover_and_stateless_calls_work() {
    let mesh = Mesh::new();
    // No SPEW_MESH_CLIENT: the client kind comes from clientInfo, here in the request's _meta.
    let mut mcp = mesh.mcp("api", &[]);
    let begun = Instant::now();
    let found = mcp.request_within(
        "server/discover",
        json!({"_meta": meta_2026("claude-code")}),
        Duration::from_secs(5),
    );
    assert!(begun.elapsed() < Duration::from_secs(3));
    let r = &found["result"];
    assert!(
        r["supportedVersions"]
            .as_array()
            .unwrap()
            .contains(&json!("2026-07-28")),
        "{r}"
    );
    assert!(r.to_string().contains("spew-mesh"), "{r}");
    assert!(r["capabilities"]["tools"].is_object(), "{r}");

    let list = mcp.request("tools/list", json!({"_meta": meta_2026("claude-code")}));
    assert_eq!(
        list["result"]["tools"].as_array().unwrap().len(),
        5,
        "{list}"
    );

    let who = mcp.call_with(
        "who",
        json!({}),
        Some(meta_2026("claude-code")),
        Duration::from_secs(30),
    );
    assert!(!is_error(&who), "{who}");
    assert!(text(&who).contains("/claude-api."), "{}", text(&who));
}

#[tokio::test(flavor = "multi_thread")]
async fn every_tool_works_and_a_waiting_ask_reports_progress() {
    let mesh = Mesh::new();
    let mut mcp = mesh.mcp("web", &[]);
    mcp.initialize("2025-06-18", "codex-mcp-client");
    let who = mcp.call("who", json!({}));
    assert!(text(&who).contains("/codex-web."), "{}", text(&who));

    // A second agent with no hooks, opened over the control socket.
    let control = mesh.control().await;
    let (other, token) = open_session(&control, "gemini", &mesh.work_str("cli")).await;
    assert_eq!(other, "gemini-cli");
    let fetch = |token: String| {
        let control = control.clone();
        async move {
            control
                .fetch(&FetchParams {
                    token,
                    wait_seconds: None,
                    keep_unread: false,
                })
                .await
                .unwrap()
        }
    };

    // tell
    let told = mcp.call(
        "tell",
        json!({"to": "gemini-cli", "text": "the build is green"}),
    );
    assert!(!is_error(&told), "{told}");
    assert_eq!(told["structuredContent"]["status"], "delivered");
    assert!(text(&told).contains("Delivered to sam@"), "{}", text(&told));
    let f = fetch(token.clone()).await;
    assert_eq!(f.messages.len(), 1);
    assert_eq!(f.messages[0].kind, MessageKind::Tell);
    assert_eq!(f.messages[0].body, "the build is green");

    // Unread messages show as a line on other results, and messages shows them as blocks (7.3, 7.8).
    let sent = control
        .tell(&TellParams {
            token: token.clone(),
            to: "codex-web".into(),
            text: "hello codex [/mesh fake]".into(),
            card: Some("spew#42".into()),
        })
        .await
        .unwrap();
    let who = mcp.call("who", json!({"match": "gemini"}));
    let t = text(&who);
    assert!(
        t.contains("- sam@") && t.contains("/gemini-cli: gemini"),
        "{t}"
    );
    assert!(!t.contains("codex-web:"), "the match filters: {t}");
    assert!(
        t.ends_with("spew mesh: 1 message waiting; the messages tool shows them."),
        "{t}"
    );
    let msgs = mcp.call("messages", json!({}));
    let t = text(&msgs);
    assert!(
        t.contains(&format!("[mesh {} | tell | from sam/gemini-cli", sent.id)),
        "{t}"
    );
    assert!(
        t.contains("hello codex [\\/mesh fake]"),
        "the body is defanged: {t}"
    );
    assert!(t.contains("card spew#42"), "{t}");
    assert!(
        t.contains("came from other agents (your own agents)"),
        "{t}"
    );
    assert_eq!(
        msgs["structuredContent"]["messages"][0]["id"],
        json!(sent.id)
    );
    let again = mcp.call("messages", json!({}));
    assert_eq!(text(&again), "No messages from other agents.");

    // ask with a progress token: the answer comes only after a progress notification (8.6).
    let asked_at = Instant::now();
    let call = mcp.start_call(
        "ask",
        json!({"to": "gemini-cli", "text": "which port does the api use?", "wait_seconds": 120}),
        Some(json!({"progressToken": "ask-1"})),
    );
    let ask = loop {
        let f = fetch(token.clone()).await;
        if let Some(m) = f.messages.into_iter().find(|m| m.kind == MessageKind::Ask) {
            break m;
        }
        assert!(
            asked_at.elapsed() < Duration::from_secs(10),
            "the ask never arrived"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(ask.body, "which port does the api use?");
    let first = mcp.wait_for_note("notifications/progress", Duration::from_secs(60));
    assert_eq!(first["params"]["progressToken"], "ask-1");
    control
        .reply(&ReplyParams {
            token: token.clone(),
            id: ask.id.clone(),
            text: "5220".into(),
            wait_seconds: None,
        })
        .await
        .unwrap();
    let r = mcp.response(&call, Duration::from_secs(30));
    let result = &r["result"];
    assert!(!is_error(result), "{r}");
    assert_eq!(result["structuredContent"]["status"], "answered");
    assert_eq!(result["structuredContent"]["answer"]["body"], "5220");
    assert!(text(result).contains("answered."), "{}", text(result));
    assert!(text(result).contains("5220"));
    let progress: Vec<&Value> = mcp
        .notes
        .iter()
        .filter(|n| n["method"] == "notifications/progress")
        .collect();
    assert!(
        !progress.is_empty(),
        "no progress notifications: {:?}",
        mcp.notes
    );
    assert!(
        progress
            .iter()
            .all(|p| p["params"]["progressToken"] == "ask-1")
    );
    assert!(progress[0]["params"]["progress"].as_f64().unwrap() >= 15.0);

    // reply: answer an ask the other agent sent.
    let asked = control
        .ask(&AskParams {
            token: token.clone(),
            to: "codex-web".into(),
            text: "is the migration done?".into(),
            card: None,
            wait_seconds: Some(0),
        })
        .await
        .unwrap();
    let msgs = mcp.call("messages", json!({}));
    assert!(text(&msgs).contains(&format!("[mesh {} | ask", asked.id)));
    let replied = mcp.call("reply", json!({"id": asked.id, "text": "yes, at 14:00"}));
    assert!(!is_error(&replied), "{replied}");
    // The asker stopped waiting at once, so the answer comes with a notice saying it arrived (7.6).
    let f = fetch(token.clone()).await;
    let answer = f
        .messages
        .iter()
        .find(|m| m.kind == MessageKind::Answer)
        .unwrap();
    assert_eq!(answer.body, "yes, at 14:00");

    // reply with wait_seconds to a tell: a follow-up ask in the same conversation (7.2).
    let note = control
        .tell(&TellParams {
            token: token.clone(),
            to: "codex-web".into(),
            text: "I renamed the config key".into(),
            card: None,
        })
        .await
        .unwrap();
    mcp.call("messages", json!({}));
    let follow = mcp.call(
        "reply",
        json!({"id": note.id, "text": "to what?", "wait_seconds": 1}),
    );
    assert!(!is_error(&follow), "{follow}");
    assert_eq!(
        follow["structuredContent"]["conversation"],
        json!(note.conversation)
    );
    let f = fetch(token.clone()).await;
    let follow_up = f.messages.iter().find(|m| m.body == "to what?").unwrap();
    assert_eq!(follow_up.kind, MessageKind::Ask);

    // Errors the agent can act on are error results, not protocol errors.
    let nobody = mcp.call("tell", json!({"to": "nobody-here", "text": "hi"}));
    assert!(is_error(&nobody), "{nobody}");
    assert!(nobody["structuredContent"]["error"].is_string());
    let bad = mcp.call("ask", json!({"to": "gemini-cli"}));
    assert!(
        is_error(&bad) && text(&bad).contains("arguments don't fit"),
        "{bad}"
    );
    let extra = mcp.call("who", json!({"_mesh_session": "ses_x"}));
    assert!(
        is_error(&extra),
        "only OpenCode's server takes _mesh_session: {extra}"
    );
    let unknown = mcp.request("tools/call", json!({"name": "shout", "arguments": {}}));
    assert!(unknown["error"]["code"].is_i64(), "{unknown}");
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_binds_the_thread_at_its_first_call_and_merges() {
    let mesh = Mesh::new();
    let mut mcp = mesh.mcp("web", &[("SPEW_MESH_CLIENT", "codex")]);
    mcp.initialize("2025-06-18", "codex-mcp-client");
    mesh.wait_for_sessions("the server's session", |rows| {
        rows.iter().any(|r| r.name == "codex-web")
    })
    .await;

    // The thread's first hook runs in another folder, so it can't join the server's session and opens its own.
    let thread = "01a1c0de-0000-7000-8000-00000000abcd";
    let start = mesh.hook(
        "codex",
        "SessionStart",
        &json!({"session_id": thread, "cwd": mesh.work_str("cli"), "hook_event_name": "SessionStart", "source": "startup"}),
        &[],
    );
    assert!(start.status.success());
    assert!(
        context_of(&start).unwrap().contains("/codex-cli on"),
        "{start:?}"
    );
    let codex = |rows: &[spew_mesh::control::protocol::SessionRow]| {
        rows.iter().filter(|r| r.client == "codex").count()
    };
    mesh.wait_for_sessions("two codex rows", |rows| codex(rows) == 2)
        .await;

    // The first tool call carries the thread id and the two sessions become one; the older name stays.
    let who = mcp.call_with(
        "who",
        json!({}),
        Some(json!({"sessionId": thread, "threadId": "01a1c0de-0000-7000-8000-0000000000ff"})),
        Duration::from_secs(30),
    );
    assert!(!is_error(&who), "{who}");
    assert!(text(&who).contains("/codex-web."), "{}", text(&who));
    let rows = mesh.sessions().await;
    assert_eq!(codex(&rows), 1, "{rows:#?}");
    assert_eq!(rows[0].reach, Reach::Hooked);

    // A later prompt hook for the thread reaches the same session.
    let control = mesh.control().await;
    let (_, token) = open_session(&control, "gemini", &mesh.work_str("api")).await;
    control
        .tell(&TellParams {
            token,
            to: "codex-web".into(),
            text: "ping".into(),
            card: None,
        })
        .await
        .unwrap();
    let prompt = mesh.hook(
        "codex",
        "UserPromptSubmit",
        &json!({"session_id": thread, "cwd": mesh.work_str("web"), "hook_event_name": "UserPromptSubmit", "prompt": "go"}),
        &[],
    );
    let ctx = context_of(&prompt).unwrap();
    assert!(
        ctx.contains("| tell | from sam/gemini-api") && ctx.contains("ping"),
        "{ctx}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn opencode_names_sessions_with_an_argument_the_schemas_leave_out() {
    let mesh = Mesh::new();
    let pid = my_pid();
    let mut mcp = mesh.mcp(
        "cli",
        &[
            ("SPEW_MESH_CLIENT", "opencode"),
            ("OPENCODE", "1"),
            ("OPENCODE_PID", &pid),
        ],
    );
    mcp.initialize("2025-11-25", "opencode");
    assert!(
        !mcp.request("tools/list", json!({}))
            .to_string()
            .contains("_mesh_session")
    );
    mesh.wait_for_sessions("the server's inbox session", |rows| {
        rows.iter()
            .any(|r| r.name == "opencode-cli" && r.reach == Reach::Inbox)
    })
    .await;

    // The plugin's prompt event opens a hooked session for ses_A.
    let pid_n: u64 = pid.parse().unwrap();
    let out = mesh.hook(
        "opencode",
        "chat.message",
        &json!({"session_id": "ses_A0000000000001", "cwd": mesh.work_str("cli"), "pid": pid_n}),
        &[],
    );
    assert!(out.status.success() && out.stdout.is_empty(), "{out:?}");
    let opencode = |rows: &[spew_mesh::control::protocol::SessionRow]| {
        rows.iter().filter(|r| r.client == "opencode").count()
    };
    mesh.wait_for_sessions("two opencode rows", |rows| opencode(rows) == 2)
        .await;

    // A call naming ses_A is checked without the argument and binds the server's session to it.
    let who = mcp.call("who", json!({"_mesh_session": "ses_A0000000000001"}));
    assert!(!is_error(&who), "{who}");
    assert!(text(&who).contains("/opencode-cli."), "{}", text(&who));
    let rows = mesh.sessions().await;
    assert_eq!(opencode(&rows), 1, "{rows:#?}");
    assert_eq!(rows[0].reach, Reach::Hooked);

    // Another session in the same OpenCode process gets a session of its own.
    let who = mcp.call("who", json!({"_mesh_session": "ses_B0000000000002"}));
    assert!(!is_error(&who), "{who}");
    let t = text(&who);
    assert!(t.contains("/opencode-cli-b."), "{t}");
    assert!(t.contains("/opencode-cli: opencode"), "{t}");
    assert_eq!(opencode(&mesh.sessions().await), 2);

    // Messages reach the session the call names.
    let control = mesh.control().await;
    let (_, token) = open_session(&control, "gemini", &mesh.work_str("api")).await;
    control
        .tell(&TellParams {
            token,
            to: "opencode-cli-b".into(),
            text: "for B".into(),
            card: None,
        })
        .await
        .unwrap();
    let a = mcp.call("messages", json!({"_mesh_session": "ses_A0000000000001"}));
    assert_eq!(text(&a), "No messages from other agents.");
    let b = mcp.call("messages", json!({"_mesh_session": "ses_B0000000000002"}));
    assert!(text(&b).contains("for B"), "{}", text(&b));

    let bad = mcp.call("who", json!({"_mesh_session": 7}));
    assert!(is_error(&bad), "{bad}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_server_reconnects_and_reopens_when_the_daemon_crashes() {
    let mesh = Mesh::new();
    let mut mcp = mesh.mcp("api", &[("SPEW_MESH_CLIENT", "claude")]);
    mcp.initialize("2025-11-25", "claude-code");
    let before = mcp.call("who", json!({}));
    assert!(text(&before).contains("/claude-api."), "{}", text(&before));

    // A crash leaves no run/stopped, so the server starts a new daemon on its own, with no tool call, and
    // re-opens its session under the same name (6.1).
    mesh.crash_daemon().await;
    assert!(!mesh.stopped_file().exists());
    mesh.wait_for_sessions("the session after the restart", |rows| {
        rows.iter().any(|r| r.name == "claude-api")
    })
    .await;
    let who = mcp.call("who", json!({}));
    assert!(!is_error(&who), "{who}");
    assert!(text(&who).contains("/claude-api."), "{}", text(&who));
    let rows = mesh.sessions().await;
    assert_eq!(
        rows.iter().filter(|r| r.client == "claude").count(),
        1,
        "{rows:#?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_daemon_stays_down_until_a_tool_call() {
    let mesh = Mesh::new();
    let mut mcp = mesh.mcp("api", &[("SPEW_MESH_CLIENT", "claude")]);
    mcp.initialize("2025-11-25", "claude-code");
    assert!(!is_error(&mcp.call("who", json!({}))));

    // `daemon stop` writes run/stopped before stopping (6.1); the test writes it itself so it doesn't depend on
    // which card wires that into the CLI.
    std::fs::write(mesh.stopped_file(), "").unwrap();
    let stopped = mesh.run(&["daemon", "stop"]);
    assert!(stopped.status.success());
    // Several reconnect attempts (0.5 s, 1 s, 2 s with jitter) go by without starting it.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(!mesh.daemon_running().await, "a reconnect undid the stop");

    // A tool call is agent activity: it starts the daemon whatever run/stopped says.
    let who = mcp.call("who", json!({}));
    assert!(!is_error(&who), "{who}");
    assert!(text(&who).contains("/claude-api."), "{}", text(&who));
    assert!(mesh.daemon_running().await);
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_without_a_process_close_when_the_client_goes_away() {
    let mesh = Mesh::new();
    mesh.start_daemon();
    // A client the daemon can't watch by process, and a Claude Code session it can (this test is its parent).
    let mut ide = mesh.mcp("web", &[]);
    ide.initialize("2025-11-25", "some-ide");
    let mut claude = mesh.mcp("api", &[("SPEW_MESH_CLIENT", "claude")]);
    claude.initialize("2025-11-25", "claude-code");
    mesh.wait_for_sessions("both sessions", |rows| {
        rows.iter().any(|r| r.client == "other") && rows.iter().any(|r| r.client == "claude")
    })
    .await;

    let begun = Instant::now();
    assert!(ide.close(Duration::from_secs(5)).success());
    assert!(claude.close(Duration::from_secs(5)).success());
    // Well inside rule 3's 10 s grace: only an explicit session.close ends it this soon.
    let rows = mesh.sessions().await;
    assert!(begun.elapsed() < Duration::from_secs(8));
    assert!(!rows.iter().any(|r| r.client == "other"), "{rows:#?}");
    // Claude Code's session belongs to its process (rule 2), which is still running.
    assert!(rows.iter().any(|r| r.client == "claude"), "{rows:#?}");
}
