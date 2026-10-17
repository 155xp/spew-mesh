//! The whole flow across two devices (C2, C5 to C8, 15): sam-mac and maya-mbp are child-process daemons on real
//! iroh links in `local` mode on 127.0.0.1 (the harness is in `net_support`), sharing the room `team`. Agents
//! reach them the way setup wires them: `spew mesh mcp-server` over stdio, `spew mesh hook claude <event>` with
//! Claude Code payloads on stdin, and `spew mesh serve claude` with a fake `claude` on `PATH`. Nothing leaves the
//! machine and no real agent runs.

mod agents_support;
mod net_support;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use agents_support::{Mcp, context_of, is_error, my_pid, text};
use net_support::{Node, Roster, WAIT};
use serde_json::{Value, json};

const MAYA_SESSION: &str = "7c1d2e3f-2222-4a5b-8c9d-000000000002";
const TEAM: &[(&str, &[&str])] = &[("team", &["sam", "maya"])];
const MAYA: &str = "maya@maya-mbp/claude-web";

/// sam-mac and maya-mbp, both running and linked, sharing `team`.
async fn two_devices() -> (Roster, Node, Node) {
    let roster = Roster::new();
    let mut sam = Node::prepare("sam", "sam-mac", &roster.path());
    let mut maya = Node::prepare("maya", "maya-mbp", &roster.path());
    roster.write(&[(&sam, None), (&maya, None)], TEAM);
    sam.start().await;
    maya.start().await;
    roster.publish(&[&sam, &maya], TEAM).await;
    net_support::eventually("sam and maya link", async || {
        sam.sees_online(&maya).await && maya.sees_online(&sam).await
    })
    .await;
    (roster, sam, maya)
}

/// `spew mesh mcp-server` on `node`, in `folder`, with extra environment, after its `initialize` handshake.
fn mcp(node: &Node, folder: &str, env: &[(&str, &str)], client_info: &str) -> Mcp {
    let mut cmd = node.command(&["mcp-server"]);
    cmd.current_dir(folder);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut server = Mcp::spawn(cmd);
    let init = server.initialize("2025-11-25", client_info);
    assert_eq!(init["result"]["serverInfo"]["name"], "spew-mesh", "{init}");
    server
}

/// An MCP-only agent on sam's device, such as Gemini CLI with the server added by hand (8.6): an `inbox` session.
fn sams_gemini(sam: &Node) -> Mcp {
    let folder = sam.folder("cli", "team");
    mcp(
        sam,
        &folder,
        &[("SPEW_MESH_CLIENT", "gemini")],
        "gemini-cli",
    )
}

/// Maya's Claude Code session in her `web` folder: its MCP server, as setup wires it, and its hooks. The test
/// process stands for Claude Code: it is the MCP server's parent and its pid goes in `CLAUDE_PID`.
struct Claude<'a> {
    node: &'a Node,
    cwd: String,
    server: Mcp,
}

impl<'a> Claude<'a> {
    fn start(node: &'a Node) -> Claude<'a> {
        let cwd = node.folder("web", "team");
        let server = mcp(
            node,
            &cwd,
            &[
                ("SPEW_MESH_CLIENT", "claude"),
                ("CLAUDE_CODE_SESSION_ID", MAYA_SESSION),
            ],
            "claude-code",
        );
        let claude = Claude { node, cwd, server };
        let start = claude.hook("SessionStart", json!({"source": "startup"}));
        let ctx = context_of(&start).unwrap_or_default();
        assert!(
            ctx.contains("this session is maya/claude-web on maya-mbp (project team)"),
            "{start:?}"
        );
        claude
    }

    /// Runs `spew mesh hook claude <event>` with a Claude Code payload on stdin.
    fn hook(&self, event: &str, extra: Value) -> Output {
        let mut payload =
            json!({"session_id": MAYA_SESSION, "cwd": self.cwd, "hook_event_name": event});
        for (k, v) in extra.as_object().unwrap() {
            payload[k] = v.clone();
        }
        let mut child = self
            .node
            .command(&["hook", "claude", event])
            .env("CLAUDE_PID", my_pid())
            .current_dir(&self.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{event}: {out:?}");
        out
    }

    fn prompt(&self, words: &str) -> Option<String> {
        context_of(&self.hook("UserPromptSubmit", json!({"prompt": words})))
    }

    fn tool_step(&self) -> Option<String> {
        context_of(&self.hook(
            "PostToolUse",
            json!({"tool_name": "Bash", "tool_input": {"command": "ls"}, "tool_response": "ok"}),
        ))
    }

    /// Calls one of spew's own MCP tools, as far as the PostToolUse hook sees it (8.5).
    fn spew_tool(&self, tool: &str, input: Value, response: &str) {
        let out = self.hook(
            "PostToolUse",
            json!({
                "tool_name": format!("mcp__spew__{tool}"),
                "tool_input": input,
                "tool_response": [{"type": "text", "text": response}]
            }),
        );
        assert!(out.stdout.is_empty(), "{out:?}");
    }

    /// Repeats a hook until it shows something, as the agent's next steps would.
    fn next_shown(&self, what: &str, mut step: impl FnMut(&Self) -> Option<String>) -> String {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(ctx) = step(self) {
                return ctx;
            }
            assert!(Instant::now() < deadline, "{what} never showed");
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

/// The session rows of a `who` result.
fn who_rows(who: &Value) -> Vec<Value> {
    let mut rows = Vec::new();
    for room in who["structuredContent"]["rooms"]
        .as_array()
        .into_iter()
        .flatten()
    {
        for person in room["people"].as_array().into_iter().flatten() {
            rows.extend(person["sessions"].as_array().into_iter().flatten().cloned());
        }
    }
    rows
}

/// Calls `who` until the row for `address` satisfies `want`, and returns that row.
fn wait_for_row(server: &mut Mcp, address: &str, want: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + WAIT;
    loop {
        let who = server.call("who", json!({}));
        assert!(!is_error(&who), "{who}");
        if let Some(row) = who_rows(&who)
            .into_iter()
            .find(|r| r["address"] == address && r["online"] == true)
            && want(&row)
        {
            return row;
        }
        assert!(
            Instant::now() < deadline,
            "{address} never showed as wanted: {}",
            text(&who)
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The id in a block header `[mesh <id> | <kind> | ...`, checking the kind.
fn block_id(context: &str, kind: &str) -> String {
    let start = context.find("[mesh ").expect("a message block") + "[mesh ".len();
    let rest = &context[start..];
    let id = &rest[..rest.find(' ').unwrap()];
    assert!(
        rest[id.len()..].starts_with(&format!(" | {kind}")),
        "{context}"
    );
    id.to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_mcp_session_and_a_hooked_session_on_two_devices_ask_tell_and_answer() {
    let (_roster, sam, maya) = two_devices().await;
    let mut maya_claude = Claude::start(&maya);
    let mut gemini = sams_gemini(&sam);

    // Maya's MCP server and hooks are one hooked session; sam's MCP-only agent is an inbox session (6.4, 7.3).
    let who = maya_claude.server.call("who", json!({}));
    assert!(
        text(&who).starts_with(&format!("You are {MAYA}.")),
        "{}",
        text(&who)
    );
    assert!(maya_claude.prompt("fix the login page").is_none());
    let row = wait_for_row(&mut gemini, MAYA, |r| r["state"] == "busy");
    assert_eq!(row["reach"], "hooked", "{row}");
    assert_eq!(row["client"], "claude", "{row}");
    let mine = wait_for_row(&mut maya_claude.server, "sam@sam-mac/gemini-cli", |_| true);
    assert_eq!(mine["reach"], "inbox", "{mine}");

    // sam asks; the question shows in maya's next prompt hook (8.1), marked as a teammate's (7.8).
    let asked = gemini.start_call(
        "ask",
        json!({"to": "maya/claude-web", "text": "which port does the dev server use?", "wait_seconds": 120}),
        Some(json!({"progressToken": "ask-1"})),
    );
    let shown = maya_claude.next_shown("sam's question", |c| c.prompt("keep going"));
    assert!(
        shown.contains("| ask | from sam/gemini-cli | device sam-mac | client gemini"),
        "{shown}"
    );
    assert!(
        shown.contains("which port does the dev server use?"),
        "{shown}"
    );
    assert!(
        shown.contains("(sam's agents, a teammate in project team)"),
        "{shown}"
    );
    let id = block_id(&shown, "ask");

    // Maya answers through her own MCP server; sam's waiting ask returns the answer.
    let replied = maya_claude.server.call(
        "reply",
        json!({"id": id, "text": "5220, see config/dev.toml"}),
    );
    assert!(!is_error(&replied), "{replied}");
    assert_eq!(
        replied["structuredContent"]["status"], "delivered",
        "{replied}"
    );
    let r = gemini.response(&asked, WAIT);
    let result = &r["result"];
    assert!(!is_error(result), "{r}");
    assert_eq!(result["structuredContent"]["status"], "answered", "{r}");
    assert_eq!(
        result["structuredContent"]["answer"]["body"],
        "5220, see config/dev.toml"
    );
    assert_eq!(result["structuredContent"]["answer"]["from"], MAYA);
    assert!(
        text(result).contains("| answer | from maya/claude-web | device maya-mbp"),
        "{}",
        text(result)
    );

    // A tell reaches the busy session at its next step, not only at the next prompt (7.3).
    let told = gemini.call(
        "tell",
        json!({"to": "maya/claude-web", "text": "heads up: main is red after the merge"}),
    );
    assert_eq!(told["structuredContent"]["status"], "delivered", "{told}");
    let shown = maya_claude.next_shown("sam's tell", Claude::tool_step);
    assert_eq!(
        block_id(&shown, "tell"),
        told["structuredContent"]["id"].as_str().unwrap()
    );
    assert!(
        shown.contains("heads up: main is red after the merge"),
        "{shown}"
    );
    // Shown and acknowledged: the next prompt doesn't show it again.
    assert!(maya_claude.prompt("ok").is_none());

    // Maya tells sam's MCP-only agent; it gets the message from the messages tool, and other results say so.
    let told = maya_claude.server.call(
        "tell",
        json!({"to": "sam/gemini-cli", "text": "fixed, rebase when you can"}),
    );
    assert_eq!(told["structuredContent"]["status"], "delivered", "{told}");
    let who = gemini.call("who", json!({"match": "maya"}));
    assert!(
        text(&who).ends_with("spew mesh: 1 message waiting; the messages tool shows them."),
        "{}",
        text(&who)
    );
    let msgs = gemini.call("messages", json!({}));
    let t = text(&msgs);
    assert_eq!(
        block_id(&t, "tell"),
        told["structuredContent"]["id"].as_str().unwrap()
    );
    assert!(
        t.contains("| tell | from maya/claude-web | device maya-mbp"),
        "{t}"
    );
    assert!(t.contains("fixed, rebase when you can"), "{t}");
    assert!(
        t.contains("(maya's agents, a teammate in project team)"),
        "{t}"
    );
    let again = gemini.call("messages", json!({}));
    assert_eq!(text(&again), "No messages from other agents.");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_card_ask_reaches_the_session_holding_the_card_on_another_device() {
    let (_roster, sam, maya) = two_devices().await;
    let mut maya_claude = Claude::start(&maya);
    let mut gemini = sams_gemini(&sam);

    // Maya's agent claims team#7 through spew's MCP server; the PostToolUse hook captures the card (8.5).
    assert!(maya_claude.prompt("pick up the login card").is_none());
    maya_claude.spew_tool(
        "claim",
        json!({"target": "team#7", "note": "Fixing the login redirect"}),
        "{\"claimed\":true}",
    );
    maya_claude.spew_tool(
        "status",
        json!({"text": "Fixing the login redirect", "project": "team"}),
        "ok",
    );
    // sam's device sees the card and the activity, which belong to the room they share (5.6).
    let row = wait_for_row(&mut gemini, MAYA, |r| {
        r["cards"] == json!(["team#7"]) && r["state"] == "busy"
    });
    assert_eq!(row["activity"], "Fixing the login redirect", "{row}");

    // A card nobody holds fails with the words the spec gives (2.2).
    let nobody = gemini.call("ask", json!({"to": "team#8", "text": "who has this?"}));
    assert!(is_error(&nobody), "{nobody}");
    assert!(
        text(&nobody).contains("nobody holds team#8 right now"),
        "{}",
        text(&nobody)
    );

    // `ask team#7` reaches the holder.
    let asked = gemini.start_call(
        "ask",
        json!({"to": "team#7", "text": "is the redirect fix going into today's release?", "wait_seconds": 120}),
        None,
    );
    let shown = maya_claude.next_shown("the card ask", |c| c.prompt("continue"));
    assert!(shown.contains("| ask | from sam/gemini-cli"), "{shown}");
    assert!(
        shown.contains("is the redirect fix going into today's release?"),
        "{shown}"
    );
    let id = block_id(&shown, "ask");
    let replied = maya_claude.server.call(
        "reply",
        json!({"id": id, "text": "yes, it merges this afternoon"}),
    );
    assert!(!is_error(&replied), "{replied}");
    let r = gemini.response(&asked, WAIT);
    let result = &r["result"];
    assert_eq!(result["structuredContent"]["status"], "answered", "{r}");
    assert_eq!(result["structuredContent"]["to"], MAYA, "{r}");
    assert_eq!(
        result["structuredContent"]["answer"]["body"],
        "yes, it merges this afternoon"
    );

    // Handing the card to review drops it from presence (8.5).
    maya_claude.spew_tool(
        "issue_update",
        json!({"issue": "team#7", "status": "review"}),
        "ok",
    );
    wait_for_row(&mut gemini, MAYA, |r| r["cards"] == json!([]));
}

/// A fake `claude -p` for serve: records argv and stdin of call N as `<rec>/argv.N` and `stdin.N` and answers
/// "answer N" in the session it was given (`--session-id` or `--resume`).
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
printf '{"type":"result","subtype":"success","is_error":false,"result":"answer %s","session_id":"%s","total_cost_usd":0.01}\n' "$n" "$sid"
"#;

/// `spew mesh serve claude` on a device, its stdout read line by line; killed when dropped.
struct Serve {
    child: Child,
    lines: mpsc::Receiver<String>,
    seen: Vec<String>,
    rec: PathBuf,
}

impl Serve {
    fn start(node: &Node, folder: &str) -> Serve {
        let exe = node.bin().join("claude");
        std::fs::write(&exe, FAKE_CLAUDE).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let rec = node.tmp.path().join("rec");
        std::fs::create_dir_all(&rec).unwrap();
        let mut child = node
            .command(&["serve", "claude", "--cwd", folder])
            .env("FAKE_REC", &rec)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
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
            rec,
        }
    }

    fn wait_for(&mut self, needle: &str) -> String {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(line) = self.seen.iter().find(|l| l.contains(needle)) {
                return line.clone();
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(left) {
                Ok(line) => self.seen.push(line),
                Err(_) => panic!("serve never printed {needle:?}:\n{}", self.seen.join("\n")),
            }
        }
    }

    fn recorded(&self, name: &str, n: usize) -> String {
        std::fs::read_to_string(self.rec.join(format!("{name}.{n}")))
            .unwrap_or_else(|e| panic!("{name}.{n}: {e}"))
    }

    // The value after `flag` in call N's argv.
    fn flag(&self, n: usize, flag: &str) -> Option<String> {
        let argv = self.recorded("argv", n);
        let mut args = argv.lines();
        args.by_ref().find(|a| *a == flag)?;
        args.next().map(str::to_string)
    }

    fn interrupt(&mut self) -> Option<i32> {
        let pid = libc::pid_t::try_from(self.child.id()).unwrap();
        // SAFETY: kill(2) takes plain integers and touches no memory of ours; the pid is our own child's.
        #[allow(unsafe_code)]
        let r = unsafe { libc::kill(pid, libc::SIGINT) };
        assert_eq!(r, 0);
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn serve_on_another_device_answers_a_teammate_and_keeps_the_conversation() {
    let (_roster, sam, maya) = two_devices().await;
    let folder = maya.folder("api", "team");
    let mut serve = Serve::start(&maya, &folder);
    let intro = serve.wait_for("Serving claude in");
    assert!(intro.contains("read-only"), "{intro}");
    let mut gemini = sams_gemini(&sam);

    // The responder shows on sam's device as a serve session (9).
    let row = wait_for_row(&mut gemini, "maya@maya-mbp/claude", |r| {
        r["state"] == "idle"
    });
    assert_eq!(row["kind"], "responder", "{row}");
    assert_eq!(row["reach"], "serve", "{row}");

    let asked = gemini.call_with(
        "ask",
        json!({"to": "maya/claude", "text": "where is the retry logic?", "wait_seconds": 120}),
        None,
        WAIT * 2,
    );
    assert!(!is_error(&asked), "{asked}");
    let first = &asked["structuredContent"];
    assert_eq!(first["status"], "answered", "{asked}");
    assert_eq!(first["answer"]["body"], "answer 1");
    assert_eq!(first["answer"]["from"], "maya@maya-mbp/claude");
    let session = serve.flag(1, "--session-id").expect("a new Claude session");
    let prompt = serve.recorded("stdin", 1);
    assert!(prompt.contains("where is the retry logic?"), "{prompt}");
    assert!(
        prompt.contains("sam/gemini-cli on sam-mac (sam's agent, a teammate in project team)"),
        "{prompt}"
    );
    serve.wait_for("question from sam/gemini-cli");

    // A follow-up in the same conversation resumes the same Claude session (7.2, 9).
    let answer_id = first["answer"]["id"].as_str().unwrap();
    let followed = gemini.call_with(
        "reply",
        json!({"id": answer_id, "text": "and how long between tries?", "wait_seconds": 120}),
        None,
        WAIT * 2,
    );
    assert!(!is_error(&followed), "{followed}");
    let second = &followed["structuredContent"];
    assert_eq!(second["status"], "answered", "{followed}");
    assert_eq!(second["answer"]["body"], "answer 2");
    assert_eq!(second["conversation"], first["conversation"], "{followed}");
    assert_eq!(serve.flag(2, "--resume"), Some(session));
    assert_eq!(serve.flag(2, "--session-id"), None);

    // Ctrl-C closes the responder session, and sam's device sees it go.
    assert_eq!(serve.interrupt(), Some(0));
    serve.wait_for("spew mesh serve: stopped");
    let deadline = Instant::now() + WAIT;
    loop {
        let who = gemini.call("who", json!({}));
        if !who_rows(&who)
            .iter()
            .any(|r| r["address"] == "maya@maya-mbp/claude" && r["online"] == true)
        {
            break;
        }
        assert!(Instant::now() < deadline, "{}", text(&who));
        std::thread::sleep(Duration::from_millis(200));
    }
}
