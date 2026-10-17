//! One device, two agents wired the way setup wires them (8.2, 8.3): a Claude Code session with its hooks and
//! its MCP server, and a Codex thread with its hooks and its MCP server. Each asks the other through the mesh
//! tools, sees the question through its hooks, and answers with the reply tool.

mod agents_support;

use std::time::{Duration, Instant};

use agents_support::{Mesh, context_of, is_error, my_pid, text};
use serde_json::{Value, json};
use spew_mesh::wire::Reach;

const CLAUDE_SESSION: &str = "5f0c2a9e-1111-4c3b-9d2e-000000000001";
const CODEX_THREAD: &str = "01a1c0de-0000-7000-8000-00000000abcd";

// The id in a block header `[mesh <id> | ask | ...`.
fn ask_id(context: &str) -> String {
    let start = context.find("[mesh ").expect("a message block") + "[mesh ".len();
    let rest = &context[start..];
    let id = &rest[..rest.find(' ').unwrap()];
    assert!(rest[id.len()..].starts_with(" | ask"), "{context}");
    id.to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hooked_session_and_an_mcp_session_ask_and_answer_each_other() {
    let mesh = Mesh::new();
    // The MCP servers are children of this test, so their parent pid is ours: it stands for Claude Code's.
    let pid = my_pid();
    let claude_env = [
        ("CLAUDE_PID", pid.as_str()),
        ("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION),
    ];
    let cc = |event: &str, extra: Value| {
        let mut p = json!({"session_id": CLAUDE_SESSION, "cwd": mesh.work_str("api"), "hook_event_name": event});
        for (k, v) in extra.as_object().unwrap() {
            p[k] = v.clone();
        }
        p
    };
    let cx = |event: &str, extra: Value| {
        let mut p = json!({"session_id": CODEX_THREAD, "cwd": mesh.work_str("web"), "hook_event_name": event});
        for (k, v) in extra.as_object().unwrap() {
            p[k] = v.clone();
        }
        p
    };

    // Claude Code starts: its MCP server first (it starts the daemon), then its SessionStart hook.
    let mut claude = mesh.mcp(
        "api",
        &[
            ("SPEW_MESH_CLIENT", "claude"),
            ("CLAUDE_CODE_SESSION_ID", CLAUDE_SESSION),
        ],
    );
    claude.initialize("2025-11-25", "claude-code");
    assert!(text(&claude.call("who", json!({}))).contains("/claude-api."));
    let start = mesh.hook(
        "claude",
        "SessionStart",
        &cc("SessionStart", json!({"source": "startup"})),
        &claude_env,
    );
    assert!(context_of(&start).unwrap().contains("sam/claude-api"));

    // Codex starts a thread: its MCP server in the thread's folder, then the thread's first hooks.
    let mut codex = mesh.mcp("web", &[("SPEW_MESH_CLIENT", "codex")]);
    codex.initialize("2025-06-18", "codex-mcp-client");
    mesh.wait_for_sessions("codex's session", |rows| {
        rows.iter().any(|r| r.name == "codex-web")
    })
    .await;
    let start = mesh.hook(
        "codex",
        "SessionStart",
        &cx("SessionStart", json!({"source": "startup"})),
        &[],
    );
    assert!(context_of(&start).unwrap().contains("sam/codex-web"));
    // Both hooks found the sessions the MCP servers opened: one row each, both hooked.
    let rows = mesh.sessions().await;
    assert_eq!(rows.len(), 2, "{rows:#?}");
    assert!(rows.iter().all(|r| r.reach == Reach::Hooked), "{rows:#?}");

    // Both are working on a turn.
    mesh.hook(
        "claude",
        "UserPromptSubmit",
        &cc("UserPromptSubmit", json!({"prompt": "fix login"})),
        &claude_env,
    );
    mesh.hook(
        "codex",
        "UserPromptSubmit",
        &cx("UserPromptSubmit", json!({"prompt": "build the page"})),
        &[],
    );

    // Codex asks Claude Code through its mesh tool.
    let codex_meta = json!({"sessionId": CODEX_THREAD, "threadId": CODEX_THREAD});
    let pending = codex.start_call(
        "ask",
        json!({"to": "claude-api", "text": "which port does the api listen on?", "wait_seconds": 60}),
        Some(codex_meta.clone()),
    );
    // Claude Code's next tool step shows the question.
    let begun = Instant::now();
    let shown = loop {
        let out = mesh.hook(
            "claude",
            "PostToolUse",
            &cc("PostToolUse", json!({"tool_name": "Bash", "tool_input": {"command": "ls"}, "tool_response": "ok"})),
            &claude_env,
        );
        if let Some(ctx) = context_of(&out) {
            break ctx;
        }
        assert!(
            begun.elapsed() < Duration::from_secs(10),
            "the question never showed"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(shown.contains("| ask | from sam/codex-web"), "{shown}");
    assert!(
        shown.contains("which port does the api listen on?"),
        "{shown}"
    );
    let id = ask_id(&shown);

    // Claude Code answers with its reply tool; Codex's ask returns the answer.
    let replied = claude.call(
        "reply",
        json!({"id": id, "text": "5220, see config/dev.toml"}),
    );
    assert!(!is_error(&replied), "{replied}");
    let r = codex.response(&pending, Duration::from_secs(30));
    let result = &r["result"];
    assert!(!is_error(result), "{r}");
    assert_eq!(result["structuredContent"]["status"], "answered");
    assert!(
        text(result).contains("5220, see config/dev.toml"),
        "{}",
        text(result)
    );
    assert!(
        text(result).contains("| answer | from sam/claude-api"),
        "{}",
        text(result)
    );

    // Now Claude Code asks Codex. Codex sees it at its next prompt.
    let pending = claude.start_call(
        "ask",
        json!({"to": "codex-web", "text": "is the page using the new endpoint?", "wait_seconds": 60}),
        None,
    );
    let begun = Instant::now();
    let shown = loop {
        let out = mesh.hook(
            "codex",
            "UserPromptSubmit",
            &cx("UserPromptSubmit", json!({"prompt": "continue"})),
            &[],
        );
        if let Some(ctx) = context_of(&out) {
            break ctx;
        }
        assert!(
            begun.elapsed() < Duration::from_secs(10),
            "the question never showed"
        );
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(shown.contains("| ask | from sam/claude-api"), "{shown}");
    let id = ask_id(&shown);
    let replied = codex.call_with(
        "reply",
        json!({"id": id, "text": "yes, since this morning"}),
        Some(codex_meta),
        Duration::from_secs(30),
    );
    assert!(!is_error(&replied), "{replied}");
    let r = claude.response(&pending, Duration::from_secs(30));
    let result = &r["result"];
    assert_eq!(result["structuredContent"]["status"], "answered", "{r}");
    assert!(text(result).contains("yes, since this morning"));

    // Nothing is left unread or unanswered on either side.
    for (mcp, meta) in [
        (&mut claude, None),
        (&mut codex, Some(json!({"sessionId": CODEX_THREAD}))),
    ] {
        let m = mcp.call_with("messages", json!({}), meta, Duration::from_secs(30));
        assert_eq!(text(&m), "No messages from other agents.", "{m}");
    }
}
