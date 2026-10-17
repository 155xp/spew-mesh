//! `spew mesh hook <client> <event>` with payloads on stdin, against a daemon on a temp home (8.1 to 8.5): what
//! each client's hooks print, that shown messages are acknowledged, card capture, and which events start a
//! missing daemon.

mod agents_support;

use std::time::{Duration, Instant};

use agents_support::{Mesh, context_of, my_pid, open_session};
use serde_json::{Value, json};
use spew_mesh::control::protocol::{AskParams, SessionRow, TellParams};
use spew_mesh::wire::{ActivityState, Reach};

const CC_SESSION: &str = "5f0c2a9e-1111-4c3b-9d2e-000000000001";

fn cc(mesh: &Mesh, event: &str, extra: Value) -> Value {
    let mut p = json!({
        "session_id": CC_SESSION,
        "transcript_path": "/home/sam/.claude/projects/x.jsonl",
        "cwd": mesh.work_str("api"),
        "hook_event_name": event,
    });
    if let (Some(p), Some(extra)) = (p.as_object_mut(), extra.as_object()) {
        for (k, v) in extra {
            p.insert(k.clone(), v.clone());
        }
    }
    p
}

fn row<'a>(rows: &'a [SessionRow], name: &str) -> Option<&'a SessionRow> {
    rows.iter().find(|r| r.name == name)
}

fn stdout(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn only_start_and_prompt_hooks_start_a_missing_daemon() {
    let mesh = Mesh::new();
    let socket = mesh.home().join("run").join("socket");
    let env = [("CLAUDE_PID", "4242")];
    for event in ["Stop", "SessionEnd", "PostToolUse"] {
        let out = mesh.hook(
            "claude",
            event,
            &cc(
                &mesh,
                event,
                json!({"reason": "other", "tool_name": "Read"}),
            ),
            &env,
        );
        assert!(
            out.status.success() && out.stdout.is_empty() && out.stderr.is_empty(),
            "{event}: {out:?}"
        );
    }
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !socket.exists(),
        "only start and prompt hooks start the daemon"
    );

    // A start hook with no daemon starts one and prints nothing this time, without waiting for it (6.1).
    let begun = Instant::now();
    let out = mesh.hook(
        "claude",
        "SessionStart",
        &cc(&mesh, "SessionStart", json!({"source": "startup"})),
        &env,
    );
    assert!(
        begun.elapsed() < Duration::from_secs(3),
        "{:?}",
        begun.elapsed()
    );
    assert!(out.status.success() && out.stdout.is_empty(), "{out:?}");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let status = mesh.run(&["daemon", "status"]);
        if status.status.success() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the start hook never started the daemon"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = mesh.hook(
        "claude",
        "SessionStart",
        &cc(&mesh, "SessionStart", json!({"source": "startup"})),
        &env,
    );
    let ctx = context_of(&out).unwrap();
    assert!(
        ctx.starts_with("spew mesh: this session is sam/claude-api on "),
        "{ctx}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn claude_code_hooks_show_messages_and_acknowledge_them() {
    let mesh = Mesh::new();
    mesh.start_daemon();
    let pid = my_pid();
    let env = [("CLAUDE_PID", pid.as_str())];

    let start = mesh.hook(
        "claude",
        "SessionStart",
        &cc(&mesh, "SessionStart", json!({"source": "startup"})),
        &env,
    );
    let v: Value = serde_json::from_slice(&start.stdout).unwrap();
    assert_eq!(v.as_object().unwrap().len(), 1, "exactly one key: {v}");
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
    assert!(
        v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("Messages from other agents will show up here")
    );
    let rows = mesh.sessions().await;
    let me = row(&rows, "claude-api").unwrap();
    assert_eq!((me.reach, me.state), (Reach::Hooked, ActivityState::Idle));

    // A prompt with nothing to show prints nothing and marks the session busy.
    let prompt = mesh.hook(
        "claude",
        "UserPromptSubmit",
        &cc(&mesh, "UserPromptSubmit", json!({"prompt": "fix it"})),
        &env,
    );
    assert!(
        prompt.status.success() && prompt.stdout.is_empty(),
        "{prompt:?}"
    );
    assert_eq!(
        row(&mesh.sessions().await, "claude-api").unwrap().state,
        ActivityState::Busy
    );

    let control = mesh.control().await;
    let (_, token) = open_session(&control, "gemini", &mesh.work_str("web")).await;
    let told = control
        .tell(&TellParams {
            token: token.clone(),
            to: "claude-api".into(),
            text: "heads up: main is red".into(),
            card: None,
        })
        .await
        .unwrap();
    let asked = control
        .ask(&AskParams {
            token: token.clone(),
            to: "claude-api".into(),
            text: "which port?".into(),
            card: Some("spew#42".into()),
            wait_seconds: Some(0),
        })
        .await
        .unwrap();

    // The next tool step shows both blocks (7.8) and acknowledges them once its output is written (7.3).
    let tool = mesh.hook(
        "claude",
        "PostToolUse",
        &cc(
            &mesh,
            "PostToolUse",
            json!({"tool_name": "Read", "tool_input": {"file_path": "x"}, "tool_response": "ok"}),
        ),
        &env,
    );
    let v: Value = serde_json::from_slice(&tool.stdout).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolUse");
    let ctx = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        ctx.contains(&format!("[mesh {} | tell | from sam/gemini-web", told.id)),
        "{ctx}"
    );
    assert!(
        ctx.contains(&format!("[mesh {} | ask | from sam/gemini-web", asked.id)),
        "{ctx}"
    );
    assert!(
        ctx.contains("heads up: main is red") && ctx.contains("card spew#42"),
        "{ctx}"
    );
    assert!(
        ctx.contains("Answer an ask with the mesh reply tool and its id."),
        "{ctx}"
    );

    // Shown and acknowledged: the next prompt doesn't repeat them, it mentions the unanswered ask once (8.1).
    let prompt = mesh.hook(
        "claude",
        "UserPromptSubmit",
        &cc(&mesh, "UserPromptSubmit", json!({"prompt": "next"})),
        &env,
    );
    let ctx = context_of(&prompt).unwrap();
    assert!(!ctx.contains("heads up"), "{ctx}");
    assert!(
        ctx.contains("1 question from other agents is still waiting for an answer"),
        "{ctx}"
    );
    let prompt = mesh.hook(
        "claude",
        "UserPromptSubmit",
        &cc(&mesh, "UserPromptSubmit", json!({"prompt": "again"})),
        &env,
    );
    assert!(prompt.stdout.is_empty(), "{}", stdout(&prompt));

    // Stop prints nothing, ever, and marks the session idle.
    let stop = mesh.hook(
        "claude",
        "Stop",
        &cc(&mesh, "Stop", json!({"stop_hook_active": false})),
        &env,
    );
    assert!(stop.status.success() && stop.stdout.is_empty());
    assert_eq!(
        row(&mesh.sessions().await, "claude-api").unwrap().state,
        ActivityState::Idle
    );

    // spew tool calls set cards and activity (8.5); a subagent's are ignored.
    let claim = mesh.hook(
        "claude",
        "PostToolUse",
        &cc(
            &mesh,
            "PostToolUse",
            json!({
                "tool_name": "mcp__spew__claim",
                "tool_input": {"target": "spew#7", "note": "Fixing login"},
                "tool_response": [{"type": "text", "text": "{\"claimed\":true}"}]
            }),
        ),
        &env,
    );
    assert!(claim.status.success());
    let status = mesh.hook(
        "claude",
        "PostToolUse",
        &cc(
            &mesh,
            "PostToolUse",
            json!({
                "tool_name": "mcp__spew__status",
                "tool_input": {"text": "Fixing the login redirect", "project": "spew"},
                "tool_response": "ok"
            }),
        ),
        &env,
    );
    assert!(status.status.success());
    let sub = mesh.hook(
        "claude",
        "PostToolUse",
        &cc(
            &mesh,
            "PostToolUse",
            json!({
                "agent_id": "agent_7",
                "tool_name": "mcp__spew__claim",
                "tool_input": {"target": "spew#8"},
                "tool_response": []
            }),
        ),
        &env,
    );
    assert!(sub.status.success() && sub.stdout.is_empty());
    let rows = mesh.sessions().await;
    let me = row(&rows, "claude-api").unwrap();
    assert_eq!(me.cards, ["spew#7"]);
    assert_eq!(me.activity.as_deref(), Some("Fixing the login redirect"));

    // A clear keeps the mesh session; any other end closes it.
    let clear = mesh.hook(
        "claude",
        "SessionEnd",
        &cc(&mesh, "SessionEnd", json!({"reason": "clear"})),
        &env,
    );
    assert!(clear.status.success() && clear.stdout.is_empty());
    assert!(row(&mesh.sessions().await, "claude-api").is_some());
    let begun = Instant::now();
    let end = mesh.hook(
        "claude",
        "SessionEnd",
        &cc(&mesh, "SessionEnd", json!({"reason": "prompt_input_exit"})),
        &env,
    );
    assert!(
        begun.elapsed() < Duration::from_secs(2),
        "SessionEnd took {:?}",
        begun.elapsed()
    );
    assert!(end.status.success() && end.stdout.is_empty());
    mesh.wait_for_sessions("the session to end", |rows| {
        row(rows, "claude-api").is_none()
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn codex_hooks_print_codex_shapes_and_capture_cards_in_the_background() {
    let mesh = Mesh::new();
    mesh.start_daemon();
    let thread = "01a1c0de-0000-7000-8000-00000000abcd";
    let cx = |event: &str, extra: Value| {
        let mut p =
            json!({"session_id": thread, "cwd": mesh.work_str("web"), "hook_event_name": event});
        for (k, v) in extra.as_object().unwrap() {
            p[k] = v.clone();
        }
        p
    };
    let start = mesh.hook(
        "codex",
        "SessionStart",
        &cx("SessionStart", json!({"source": "startup", "model": "m"})),
        &[],
    );
    let v: Value = serde_json::from_slice(&start.stdout).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "SessionStart");
    assert!(
        v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("sam/codex-web")
    );

    // PostToolUse runs in the background for spew tools only: it prints nothing and captures the card.
    let tool = mesh.hook(
        "codex",
        "PostToolUse",
        &cx(
            "PostToolUse",
            json!({
                "tool_name": "mcp__spew__claim",
                "tool_input": {"target": "next"},
                "tool_response": {"content": [{"type": "text", "text": "claimed web#12"}]}
            }),
        ),
        &[],
    );
    assert!(tool.status.success() && tool.stdout.is_empty());
    assert_eq!(
        row(&mesh.sessions().await, "codex-web").unwrap().cards,
        ["web#12"]
    );

    let control = mesh.control().await;
    let (_, token) = open_session(&control, "gemini", &mesh.work_str("api")).await;
    control
        .tell(&TellParams {
            token,
            to: "codex-web".into(),
            text: "deploy is at 3".into(),
            card: None,
        })
        .await
        .unwrap();
    // Even with a message waiting, the background hook shows nothing (it sends show: false).
    let tool = mesh.hook(
        "codex",
        "PostToolUse",
        &cx("PostToolUse", json!({"tool_name": "mcp__spew__status", "tool_input": {"text": "x"}, "tool_response": "ok"})),
        &[],
    );
    assert!(tool.stdout.is_empty());
    let prompt = mesh.hook(
        "codex",
        "UserPromptSubmit",
        &cx("UserPromptSubmit", json!({"prompt": "go"})),
        &[],
    );
    let v: Value = serde_json::from_slice(&prompt.stdout).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
    assert!(
        v["hookSpecificOutput"]["additionalContext"]
            .as_str()
            .unwrap()
            .contains("deploy is at 3")
    );
    let again = mesh.hook(
        "codex",
        "UserPromptSubmit",
        &cx("UserPromptSubmit", json!({"prompt": "go"})),
        &[],
    );
    assert!(again.stdout.is_empty(), "acknowledged: {}", stdout(&again));

    let stop = mesh.hook("codex", "Stop", &cx("Stop", json!({})), &[]);
    assert!(stop.stdout.is_empty());
    let end = mesh.hook(
        "codex",
        "SessionEnd",
        &cx("SessionEnd", json!({"reason": "other"})),
        &[],
    );
    assert!(end.status.success() && end.stdout.is_empty());
    mesh.wait_for_sessions("the thread to end", |rows| row(rows, "codex-web").is_none())
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn opencode_plugin_events_follow_the_table() {
    let mesh = Mesh::new();
    mesh.start_daemon();
    let pid: u64 = my_pid().parse().unwrap();
    let oc = |extra: Value| {
        let mut p =
            json!({"session_id": "ses_3f9a0000000001", "cwd": mesh.work_str("cli"), "pid": pid});
        for (k, v) in extra.as_object().unwrap() {
            p[k] = v.clone();
        }
        p
    };
    // OpenCode can't show text when a session is created: nothing is printed, but the session opens.
    let created = mesh.hook("opencode", "session.created", &oc(json!({})), &[]);
    assert!(created.status.success() && created.stdout.is_empty());
    let rows = mesh.sessions().await;
    assert_eq!(row(&rows, "opencode-cli").unwrap().reach, Reach::Hooked);

    let control = mesh.control().await;
    let (_, token) = open_session(&control, "gemini", &mesh.work_str("api")).await;
    control
        .tell(&TellParams {
            token,
            to: "opencode-cli".into(),
            text: "the schema changed".into(),
            card: None,
        })
        .await
        .unwrap();
    let msg = mesh.hook("opencode", "chat.message", &oc(json!({})), &[]);
    let v: Value = serde_json::from_slice(&msg.stdout).unwrap();
    assert_eq!(v.as_object().unwrap().len(), 1, "{v}");
    assert!(
        v["context"]
            .as_str()
            .unwrap()
            .contains("the schema changed"),
        "{v}"
    );
    let again = mesh.hook("opencode", "chat.message", &oc(json!({})), &[]);
    assert!(again.stdout.is_empty(), "acknowledged: {}", stdout(&again));

    let tool = mesh.hook(
        "opencode",
        "tool.execute.after",
        &oc(json!({"tool_name": "spew_claim", "tool_input": {"target": "mesh#9"}, "tool_response": "ok"})),
        &[],
    );
    assert!(tool.status.success() && tool.stdout.is_empty());
    assert_eq!(
        row(&mesh.sessions().await, "opencode-cli").unwrap().cards,
        ["mesh#9"]
    );

    let idle = mesh.hook("opencode", "session.idle", &oc(json!({})), &[]);
    assert!(idle.stdout.is_empty());
    assert_eq!(
        row(&mesh.sessions().await, "opencode-cli").unwrap().state,
        ActivityState::Idle
    );
    let deleted = mesh.hook("opencode", "session.deleted", &oc(json!({})), &[]);
    assert!(deleted.status.success() && deleted.stdout.is_empty());
    mesh.wait_for_sessions("the session to end", |rows| {
        row(rows, "opencode-cli").is_none()
    })
    .await;
}
