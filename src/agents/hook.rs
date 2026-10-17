//! `spew mesh hook <client> <event>`: read one hook payload, pass its facts to the daemon, print what the client
//! expects (8.1 to 8.4). It must never break the agent: on any problem it prints nothing and exits 0.

use std::io::{Read, Write};
use std::panic::AssertUnwindSafe;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Client;
use super::cards::first_card;

/// A mesh hook event (8.1, 6.2 `hook.event`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MeshEvent {
    Start,
    Prompt,
    Tool,
    Stop,
    End,
}

/// How one client event is wired and handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HookSpec {
    /// The client's own event name, as in `spew mesh hook <client> <event>`.
    pub event: &'static str,
    pub mesh: MeshEvent,
    /// The matcher setup writes, if any.
    pub matcher: Option<&'static str>,
    /// The timeout setup writes (OpenCode: when the plugin kills the hook), in seconds.
    pub timeout_secs: u64,
    /// Codex runs the handler in the background (`"async": true`) and ignores its output.
    pub background: bool,
    /// The hook can put text in front of the agent (6.2 `show`).
    pub show: bool,
}

const fn spec(
    event: &'static str,
    mesh: MeshEvent,
    matcher: Option<&'static str>,
    timeout_secs: u64,
    background: bool,
    show: bool,
) -> HookSpec {
    HookSpec {
        event,
        mesh,
        matcher,
        timeout_secs,
        background,
        show,
    }
}

/// Claude Code's hooks (8.2).
pub const CLAUDE_HOOKS: [HookSpec; 5] = [
    spec("SessionStart", MeshEvent::Start, None, 10, false, true),
    spec("UserPromptSubmit", MeshEvent::Prompt, None, 8, false, true),
    spec("PostToolUse", MeshEvent::Tool, Some("*"), 8, false, true),
    spec("Stop", MeshEvent::Stop, None, 8, false, false),
    spec("SessionEnd", MeshEvent::End, None, 2, false, false),
];

/// Codex's hooks (8.3). `PostToolUse` covers only spew's tools, in the background.
pub const CODEX_HOOKS: [HookSpec; 5] = [
    spec("SessionStart", MeshEvent::Start, None, 10, false, true),
    spec("UserPromptSubmit", MeshEvent::Prompt, None, 8, false, true),
    spec("Stop", MeshEvent::Stop, None, 8, false, false),
    spec("SessionEnd", MeshEvent::End, None, 2, false, false),
    spec(
        "PostToolUse",
        MeshEvent::Tool,
        Some("^mcp__spew__"),
        8,
        true,
        false,
    ),
];

/// How long the OpenCode plugin lets a hook run before killing it (8.4).
pub const OPENCODE_KILL_SECS: u64 = 8;

/// The OpenCode plugin's events (8.4). OpenCode can't show text when a session is created.
pub const OPENCODE_EVENTS: [HookSpec; 5] = [
    spec(
        "session.created",
        MeshEvent::Start,
        None,
        OPENCODE_KILL_SECS,
        false,
        false,
    ),
    spec(
        "chat.message",
        MeshEvent::Prompt,
        None,
        OPENCODE_KILL_SECS,
        false,
        true,
    ),
    spec(
        "tool.execute.after",
        MeshEvent::Tool,
        None,
        OPENCODE_KILL_SECS,
        false,
        true,
    ),
    spec(
        "session.idle",
        MeshEvent::Stop,
        None,
        OPENCODE_KILL_SECS,
        false,
        false,
    ),
    spec(
        "session.deleted",
        MeshEvent::End,
        None,
        OPENCODE_KILL_SECS,
        false,
        false,
    ),
];

/// The wiring of a client's events.
pub fn hook_specs(client: Client) -> &'static [HookSpec] {
    match client {
        Client::Claude => &CLAUDE_HOOKS,
        Client::Codex => &CODEX_HOOKS,
        Client::Opencode => &OPENCODE_EVENTS,
    }
}

/// The spec for one client event name.
pub fn hook_spec(client: Client, event: &str) -> Option<&'static HookSpec> {
    hook_specs(client).iter().find(|s| s.event == event)
}

/// Fields of a spew tool call that card capture reads (8.5). Nothing else is kept.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpewArgs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
}

/// The `facts` sent with `hook.event` (8.1).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    #[serde(default)]
    pub subagent: bool,
    /// `SPEW_PROJECT` from the hook's environment (11).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spew_project: Option<String>,
    /// The bare spew tool name, only for `tool` events from spew's MCP server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spew_tool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spew_args: Option<SpewArgs>,
    /// The first card ref in the tool's response text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spew_result: Option<String>,
}

/// The params of `hook.event` (6.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookCall {
    pub client: Client,
    pub event: MeshEvent,
    pub client_session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_pid: Option<u32>,
    pub cwd: String,
    pub show: bool,
    pub facts: Facts,
}

/// The daemon's answer to `hook.event` (6.2).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookAnswer {
    pub session: String,
    pub address: String,
    /// Text to put in front of the agent: rendered message blocks (7.8) or the start line (8.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Messages in `context`, acknowledged with `messages.ack` once the output is written and flushed.
    #[serde(default)]
    pub ack_ids: Vec<String>,
}

/// Why a payload leads to no daemon call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// Not an event setup wires for this client.
    UnknownEvent,
    /// Not a JSON object, or the event name in it doesn't match.
    BadPayload,
    /// No session id or working directory.
    MissingField,
    /// From a subagent: subagents aren't mesh sessions (8.2, 8.3).
    Subagent,
    /// A Claude Code clear or resume inside the same process keeps the mesh session (8.2).
    KeepsSession,
}

const MAX_FIELD: usize = 4096;
const MAX_TEXT_ARG: usize = 1024;
const MAX_RESPONSE_SCAN: usize = 64 * 1024;

/// Turns one hook payload into the `hook.event` call, reading `CLAUDE_PID` and `SPEW_PROJECT` through `env`.
pub fn parse(
    client: Client,
    event: &str,
    payload: &[u8],
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<HookCall, Skip> {
    let spec = hook_spec(client, event).ok_or(Skip::UnknownEvent)?;
    let v: Value = serde_json::from_slice(payload).map_err(|_| Skip::BadPayload)?;
    if !v.is_object() {
        return Err(Skip::BadPayload);
    }
    if client != Client::Opencode
        && let Some(name) = v.get("hook_event_name")
        && name.as_str() != Some(event)
    {
        return Err(Skip::BadPayload);
    }
    let subagent = match v.get("agent_id") {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    } || v.get("subagent").and_then(Value::as_bool) == Some(true);
    if subagent {
        return Err(Skip::Subagent);
    }
    if client == Client::Claude
        && spec.mesh == MeshEvent::End
        && matches!(
            v.get("reason").and_then(Value::as_str),
            Some("clear") | Some("resume")
        )
    {
        return Err(Skip::KeepsSession);
    }

    let field = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let client_session = field("session_id")
        .map(str::to_string)
        .or_else(|| match client {
            Client::Claude => env("CLAUDE_CODE_SESSION_ID").filter(|s| !s.trim().is_empty()),
            _ => None,
        })
        .filter(|s| s.len() <= 256)
        .ok_or(Skip::MissingField)?;
    let cwd = field("cwd")
        .filter(|s| s.len() <= MAX_FIELD)
        .ok_or(Skip::MissingField)?
        .to_string();
    let client_pid = match client {
        Client::Claude => env("CLAUDE_PID").and_then(|p| p.trim().parse::<u32>().ok()),
        Client::Codex => None,
        Client::Opencode => v
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|p| u32::try_from(p).ok()),
    }
    .filter(|p| *p > 0);

    let mut facts = Facts {
        subagent: false,
        spew_project: env("SPEW_PROJECT")
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty() && p.len() <= 100),
        ..Facts::default()
    };
    if spec.mesh == MeshEvent::Tool
        && let Some(tool) = v
            .get("tool_name")
            .and_then(Value::as_str)
            .and_then(|t| spew_tool_name(client, t))
    {
        facts.spew_tool = Some(tool.to_string());
        facts.spew_args = Some(spew_args(v.get("tool_input")));
        let mut text = String::new();
        if let Some(r) = v.get("tool_response") {
            response_text(r, &mut text);
        }
        facts.spew_result = first_card(&text);
    }
    Ok(HookCall {
        client,
        event: spec.mesh,
        client_session,
        client_pid,
        cwd,
        show: spec.show,
        facts,
    })
}

/// The bare tool name when `name` is a call to spew's MCP server (8.5): `mcp__spew__<tool>`, a plugin form
/// `mcp__plugin_spew_<server>__<tool>`, or OpenCode's `spew_<tool>`.
pub fn spew_tool_name(client: Client, name: &str) -> Option<&str> {
    let tool = match client {
        Client::Opencode => name.strip_prefix("spew_")?,
        Client::Claude | Client::Codex => match name.strip_prefix("mcp__spew__") {
            Some(t) => t,
            None => name.strip_prefix("mcp__plugin_spew_")?.split_once("__")?.1,
        },
    };
    let ok = !tool.is_empty()
        && tool.len() <= 64
        && tool
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    ok.then_some(tool)
}

fn spew_args(input: Option<&Value>) -> SpewArgs {
    let s = |k: &str, max: usize| {
        input
            .and_then(|i| i.get(k))
            .and_then(Value::as_str)
            .map(|v| v.chars().take(max).collect::<String>())
    };
    SpewArgs {
        target: s("target", 200),
        issue: s("issue", 200),
        action: s("action", 40),
        status: s("status", 40),
        text: s("text", MAX_TEXT_ARG),
        done: input.and_then(|i| i.get("done")).and_then(Value::as_bool),
        project: s("project", 100),
    }
}

/// Collects the text of a tool response: a string, MCP content blocks (`[{"type":"text","text":...}]`), or an MCP
/// result object (`{"content": [...]}`), up to a size limit.
fn response_text(v: &Value, out: &mut String) {
    if out.len() >= MAX_RESPONSE_SCAN {
        return;
    }
    match v {
        Value::String(s) => {
            let mut cut = s.len().min(MAX_RESPONSE_SCAN - out.len());
            while !s.is_char_boundary(cut) {
                cut -= 1;
            }
            out.push_str(&s[..cut]);
            out.push('\n');
        }
        Value::Array(items) => items.iter().for_each(|i| response_text(i, out)),
        Value::Object(m) => {
            if let Some(c) = m.get("content") {
                response_text(c, out);
            } else if let Some(t) = m.get("text") {
                response_text(t, out);
            }
        }
        _ => {}
    }
}

/// What the client expects on stdout for this answer, or `None` to print nothing.
pub fn render(client: Client, spec: &HookSpec, answer: &HookAnswer) -> Option<String> {
    if !spec.show || matches!(spec.mesh, MeshEvent::Stop | MeshEvent::End) {
        return None;
    }
    let context = answer.context.as_deref().filter(|c| !c.trim().is_empty())?;
    let out = match client {
        Client::Claude | Client::Codex => serde_json::json!({
            "hookSpecificOutput": { "hookEventName": spec.event, "additionalContext": context }
        }),
        Client::Opencode => serde_json::json!({ "context": context }),
    };
    let mut s = serde_json::to_string(&out).ok()?;
    s.push('\n');
    Some(s)
}

/// The daemon calls a hook makes. Part 2 implements this over the control socket (6.1, 6.2): it connects with a
/// short deadline and, for `start` and `prompt`, starts a missing daemon without waiting for it.
pub trait HookDaemon {
    /// Sends `hook.event` and waits up to `limit` for the answer.
    fn event(&mut self, call: &HookCall, limit: Duration) -> anyhow::Result<HookAnswer>;
    /// Sends `hook.event` without waiting for the answer (`end`).
    fn send(&mut self, call: &HookCall, limit: Duration) -> anyhow::Result<()>;
    /// Sends `messages.ack` for the messages the hook's output carried.
    fn ack(
        &mut self,
        client: Client,
        client_session: &str,
        ids: &[String],
        limit: Duration,
    ) -> anyhow::Result<()>;
}

/// Largest payload read from stdin; a bigger one (a huge tool response) is skipped.
pub const MAX_PAYLOAD: usize = 8 << 20;

/// How long a hook may read stdin, and how long its daemon round trip may take, for a hook with this timeout:
/// the whole budget is the timeout minus 1 s, stdin gets at most 1 s, and 1 s is kept for writing output (8.1).
pub fn budget(timeout_secs: u64) -> (Duration, Duration) {
    let total = Duration::from_secs(timeout_secs.saturating_sub(1).max(1));
    let stdin = Duration::from_secs(1).min(total / 2);
    (total, stdin)
}

/// Handles one hook event end to end. Never fails and never panics outward; the caller exits 0 whatever happens.
pub fn run_hook<R: Read + Send + 'static>(
    client: &str,
    event: &str,
    stdin: R,
    stdout: &mut dyn Write,
    env: &dyn Fn(&str) -> Option<String>,
    daemon: &mut dyn HookDaemon,
) {
    let start = Instant::now();
    let _ = std::panic::catch_unwind(AssertUnwindSafe(|| {
        run_inner(start, client, event, stdin, stdout, env, daemon);
    }));
}

fn run_inner<R: Read + Send + 'static>(
    start: Instant,
    client: &str,
    event: &str,
    stdin: R,
    stdout: &mut dyn Write,
    env: &dyn Fn(&str) -> Option<String>,
    daemon: &mut dyn HookDaemon,
) -> Option<()> {
    let client = Client::from_slug(client)?;
    let spec = hook_spec(client, event)?;
    let (total, stdin_limit) = budget(spec.timeout_secs);
    let deadline = start + total;
    let payload = read_within(stdin, stdin_limit, MAX_PAYLOAD)?;
    let call = parse(client, event, &payload, env).ok()?;
    let left = |reserve: Duration| deadline.checked_duration_since(Instant::now() + reserve);
    if spec.mesh == MeshEvent::End {
        daemon.send(&call, left(Duration::ZERO)?).ok()?;
        return Some(());
    }
    // An 8 s hook: 1 s for stdin, at most 5 s for the daemon, 1 s kept for writing the output (8.1).
    let reserve = if spec.show {
        Duration::from_secs(1)
    } else {
        Duration::ZERO
    };
    let round_trip = total
        .saturating_sub(stdin_limit + reserve)
        .min(left(reserve)?);
    let answer = daemon.event(&call, round_trip).ok()?;
    let text = render(client, spec, &answer)?;
    stdout.write_all(text.as_bytes()).ok()?;
    stdout.flush().ok()?;
    if !answer.ack_ids.is_empty() {
        let _ = daemon.ack(
            client,
            &call.client_session,
            &answer.ack_ids,
            left(Duration::ZERO)?,
        );
    }
    Some(())
}

/// Reads all of `r` within `limit`, or gives up. A reader still blocked is left behind; the hook exits soon after.
fn read_within<R: Read + Send + 'static>(r: R, limit: Duration, cap: usize) -> Option<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let res = r.take(cap as u64 + 1).read_to_end(&mut buf);
        let _ = tx.send(res.map(|_| buf));
    });
    match rx.recv_timeout(limit) {
        Ok(Ok(buf)) if buf.len() <= cap => Some(buf),
        _ => None,
    }
}

/// Entry point for `spew mesh hook <client> <event>`: real stdin, stdout and environment, silent panics.
/// Always returns success.
pub fn main(client: &str, event: &str, daemon: &mut dyn HookDaemon) -> std::process::ExitCode {
    // A panic message on stderr is harmless to the clients, but a hook should leave no trace on failure.
    std::panic::set_hook(Box::new(|_| {}));
    let env = |k: &str| std::env::var(k).ok();
    let mut out = std::io::stdout().lock();
    run_hook(client, event, std::io::stdin(), &mut out, &env, daemon);
    std::process::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| m.get(k).cloned()
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    // Payloads shaped like the research notes, with invented ids and paths.
    const CC_START: &str = r##"{"session_id":"5f0c2a9e-1111-4c3b-9d2e-000000000001","transcript_path":"/home/sam/.claude/projects/-home-sam-api/5f0c2a9e.jsonl","cwd":"/home/sam/api","hook_event_name":"SessionStart","source":"startup"}"##;
    const CC_PROMPT: &str = r##"{"session_id":"5f0c2a9e-1111-4c3b-9d2e-000000000001","transcript_path":"/x.jsonl","cwd":"/home/sam/api","prompt_id":"0b1e","permission_mode":"default","hook_event_name":"UserPromptSubmit","prompt":"fix the login bug"}"##;
    const CC_TOOL_SPEW: &str = r##"{"session_id":"5f0c2a9e-1111-4c3b-9d2e-000000000001","cwd":"/home/sam/api","hook_event_name":"PostToolUse","tool_name":"mcp__spew__claim","tool_input":{"target":"next","note":"Fixing login","model":"claude-x"},"tool_use_id":"toolu_01","tool_response":[{"type":"text","text":"{\"claimed\":true,\"issue\":{\"ref\":\"spew#42\"}}"}],"duration_ms":40}"##;
    const CC_TOOL_READ: &str = r##"{"session_id":"5f0c2a9e-1111-4c3b-9d2e-000000000001","cwd":"/home/sam/api","hook_event_name":"PostToolUse","tool_name":"Read","tool_input":{"file_path":"/home/sam/api/notes.md"},"tool_response":{"file":{"content":"see spew#9"}}}"##;
    const CC_STOP: &str = r##"{"session_id":"5f0c2a9e-1111-4c3b-9d2e-000000000001","cwd":"/home/sam/api","hook_event_name":"Stop","stop_hook_active":false,"last_assistant_message":"done","background_tasks":[],"session_crons":[]}"##;
    const CC_END_CLEAR: &str = r##"{"session_id":"5f0c2a9e-1111-4c3b-9d2e-000000000001","cwd":"/home/sam/api","hook_event_name":"SessionEnd","reason":"clear"}"##;
    const CC_END_EXIT: &str = r##"{"session_id":"5f0c2a9e-1111-4c3b-9d2e-000000000001","cwd":"/home/sam/api","hook_event_name":"SessionEnd","reason":"prompt_input_exit"}"##;
    const CC_SUBAGENT: &str = r##"{"session_id":"5f0c2a9e-1111-4c3b-9d2e-000000000001","cwd":"/home/sam/api","hook_event_name":"PostToolUse","agent_id":"agent_7","agent_type":"Explore","tool_name":"mcp__spew__status","tool_input":{"text":"x"},"tool_response":[]}"##;
    const CX_START: &str = r##"{"session_id":"01a1c0de-0000-7000-8000-00000000abcd","transcript_path":"/home/maya/.codex/sessions/2026/10/17/rollout-x.jsonl","cwd":"/home/maya/web","hook_event_name":"SessionStart","model":"some-model","permission_mode":"default","source":"startup"}"##;
    const CX_TOOL: &str = r##"{"session_id":"01a1c0de-0000-7000-8000-00000000abcd","turn_id":"01a1c0de-0001","cwd":"/home/maya/web","hook_event_name":"PostToolUse","tool_name":"mcp__spew__issue_update","tool_use_id":"call_1","tool_input":{"issue":"#7","project":"web","status":"review","comment":"done"},"tool_response":{"content":[{"type":"text","text":"moved web#7 to review"}]}}"##;
    const CX_END: &str = r##"{"session_id":"01a1c0de-0000-7000-8000-00000000abcd","transcript_path":null,"cwd":"/home/maya/web","hook_event_name":"SessionEnd","reason":"other"}"##;
    const OC_PROMPT: &str =
        r##"{"session_id":"ses_3f9a0000000001","cwd":"/home/sam/cli","pid":4242}"##;
    const OC_TOOL: &str = r##"{"session_id":"ses_3f9a0000000001","cwd":"/home/sam/cli","pid":4242,"tool_name":"spew_status","tool_input":{"text":"Reviewing mesh#3","project":"mesh"},"tool_response":"ok"}"##;

    #[test]
    fn parses_claude_code_payloads() {
        let env = env_of(&[("CLAUDE_PID", "31337"), ("SPEW_PROJECT", "spew")]);
        let call = parse(Client::Claude, "SessionStart", CC_START.as_bytes(), &env).unwrap();
        assert_eq!(call.event, MeshEvent::Start);
        assert_eq!(call.client_session, "5f0c2a9e-1111-4c3b-9d2e-000000000001");
        assert_eq!(call.client_pid, Some(31337));
        assert_eq!(call.cwd, "/home/sam/api");
        assert!(call.show);
        assert_eq!(
            call.facts,
            Facts {
                spew_project: Some("spew".into()),
                ..Facts::default()
            }
        );

        let p = parse(
            Client::Claude,
            "UserPromptSubmit",
            CC_PROMPT.as_bytes(),
            &env,
        )
        .unwrap();
        assert_eq!(p.event, MeshEvent::Prompt);

        let t = parse(Client::Claude, "PostToolUse", CC_TOOL_SPEW.as_bytes(), &env).unwrap();
        assert_eq!(t.facts.spew_tool.as_deref(), Some("claim"));
        assert_eq!(
            t.facts.spew_args,
            Some(SpewArgs {
                target: Some("next".into()),
                ..SpewArgs::default()
            })
        );
        assert_eq!(t.facts.spew_result.as_deref(), Some("spew#42"));

        let r = parse(Client::Claude, "PostToolUse", CC_TOOL_READ.as_bytes(), &env).unwrap();
        assert_eq!(r.facts.spew_tool, None, "only spew tools carry spew facts");
        assert_eq!(r.facts.spew_result, None);

        let s = parse(Client::Claude, "Stop", CC_STOP.as_bytes(), &env).unwrap();
        assert_eq!((s.event, s.show), (MeshEvent::Stop, false));
        assert_eq!(
            parse(Client::Claude, "SessionEnd", CC_END_CLEAR.as_bytes(), &env),
            Err(Skip::KeepsSession)
        );
        assert_eq!(
            parse(Client::Claude, "SessionEnd", CC_END_EXIT.as_bytes(), &env)
                .unwrap()
                .event,
            MeshEvent::End
        );
        assert_eq!(
            parse(Client::Claude, "PostToolUse", CC_SUBAGENT.as_bytes(), &env),
            Err(Skip::Subagent)
        );
    }

    #[test]
    fn parses_codex_and_opencode_payloads() {
        let c = parse(
            Client::Codex,
            "SessionStart",
            CX_START.as_bytes(),
            &env_of(&[("CLAUDE_PID", "1")]),
        )
        .unwrap();
        assert_eq!(
            c.client_pid, None,
            "Codex sessions are keyed on the thread id only"
        );
        assert_eq!(c.client_session, "01a1c0de-0000-7000-8000-00000000abcd");
        let t = parse(Client::Codex, "PostToolUse", CX_TOOL.as_bytes(), &no_env).unwrap();
        assert!(!t.show, "Codex runs it in the background");
        let args = t.facts.spew_args.unwrap();
        assert_eq!(
            (args.issue.as_deref(), args.status.as_deref()),
            (Some("#7"), Some("review"))
        );
        assert_eq!(t.facts.spew_result.as_deref(), Some("web#7"));
        assert_eq!(
            parse(Client::Codex, "SessionEnd", CX_END.as_bytes(), &no_env)
                .unwrap()
                .event,
            MeshEvent::End
        );

        let p = parse(
            Client::Opencode,
            "chat.message",
            OC_PROMPT.as_bytes(),
            &no_env,
        )
        .unwrap();
        assert_eq!(
            (p.event, p.client_pid, p.show),
            (MeshEvent::Prompt, Some(4242), true)
        );
        let t = parse(
            Client::Opencode,
            "tool.execute.after",
            OC_TOOL.as_bytes(),
            &no_env,
        )
        .unwrap();
        assert_eq!(t.facts.spew_tool.as_deref(), Some("status"));
        assert_eq!(t.facts.spew_args.unwrap().project.as_deref(), Some("mesh"));
        let s = parse(
            Client::Opencode,
            "session.created",
            OC_PROMPT.as_bytes(),
            &no_env,
        )
        .unwrap();
        assert!(!s.show, "OpenCode can't show text at session start");
    }

    #[test]
    fn rejects_bad_payloads() {
        assert_eq!(
            parse(Client::Claude, "Notification", CC_START.as_bytes(), &no_env),
            Err(Skip::UnknownEvent)
        );
        assert_eq!(
            parse(Client::Claude, "Stop", CC_START.as_bytes(), &no_env),
            Err(Skip::BadPayload)
        );
        assert_eq!(
            parse(Client::Claude, "Stop", b"[1]", &no_env),
            Err(Skip::BadPayload)
        );
        assert_eq!(
            parse(Client::Claude, "Stop", b"{\"x\":1", &no_env),
            Err(Skip::BadPayload)
        );
        assert_eq!(
            parse(Client::Codex, "Stop", b"{\"session_id\":\"a\"}", &no_env),
            Err(Skip::MissingField)
        );
        let env = env_of(&[("CLAUDE_CODE_SESSION_ID", "abc"), ("CLAUDE_PID", "nope")]);
        let c = parse(Client::Claude, "Stop", b"{\"cwd\":\"/w\"}", &env).unwrap();
        assert_eq!((c.client_session.as_str(), c.client_pid), ("abc", None));
    }

    #[test]
    fn recognizes_spew_tool_names() {
        assert_eq!(
            spew_tool_name(Client::Claude, "mcp__spew__claim"),
            Some("claim")
        );
        assert_eq!(
            spew_tool_name(Client::Claude, "mcp__plugin_spew_spew__issue_update"),
            Some("issue_update")
        );
        assert_eq!(
            spew_tool_name(Client::Codex, "mcp__spew__status"),
            Some("status")
        );
        assert_eq!(
            spew_tool_name(Client::Opencode, "spew_handoff"),
            Some("handoff")
        );
        assert_eq!(spew_tool_name(Client::Claude, "mcp__mesh__ask"), None);
        assert_eq!(spew_tool_name(Client::Opencode, "mcp__spew__claim"), None);
        assert_eq!(spew_tool_name(Client::Claude, "mcp__spew__"), None);
    }

    #[test]
    fn renders_what_each_client_expects() {
        let a = HookAnswer {
            context: Some("[mesh 01J | ask]\nhi\n[/mesh 01J]".into()),
            ..HookAnswer::default()
        };
        let cc = render(Client::Claude, &CLAUDE_HOOKS[1], &a).unwrap();
        let v: Value = serde_json::from_str(&cc).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit");
        assert_eq!(
            v["hookSpecificOutput"]["additionalContext"],
            "[mesh 01J | ask]\nhi\n[/mesh 01J]"
        );
        assert_eq!(
            v.as_object().unwrap().len(),
            1,
            "exactly one key: Codex rejects unknown ones"
        );
        assert!(cc.ends_with("}\n") && cc.matches('\n').count() == 1);
        let cx: Value =
            serde_json::from_str(&render(Client::Codex, &CODEX_HOOKS[0], &a).unwrap()).unwrap();
        assert_eq!(cx["hookSpecificOutput"]["hookEventName"], "SessionStart");
        assert_eq!(
            render(Client::Codex, &CODEX_HOOKS[4], &a),
            None,
            "background PostToolUse prints nothing"
        );
        assert_eq!(
            render(Client::Claude, &CLAUDE_HOOKS[3], &a),
            None,
            "Stop prints nothing"
        );
        assert_eq!(render(Client::Claude, &CLAUDE_HOOKS[4], &a), None);
        let oc = render(Client::Opencode, &OPENCODE_EVENTS[2], &a).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&oc).unwrap()["context"],
            a.context.clone().unwrap()
        );
        assert_eq!(render(Client::Opencode, &OPENCODE_EVENTS[0], &a), None);
        assert_eq!(
            render(Client::Claude, &CLAUDE_HOOKS[0], &HookAnswer::default()),
            None
        );
    }

    #[test]
    fn budgets_follow_the_timeouts() {
        assert_eq!(budget(8), (Duration::from_secs(7), Duration::from_secs(1)));
        assert_eq!(
            budget(2),
            (Duration::from_secs(1), Duration::from_millis(500))
        );
        assert_eq!(budget(10).0, Duration::from_secs(9));
    }

    #[derive(Default)]
    struct FakeDaemon {
        answer: Option<HookAnswer>,
        fail: bool,
        panic: bool,
        events: Vec<(HookCall, Duration)>,
        sent: Vec<HookCall>,
        acks: Vec<Vec<String>>,
    }

    impl HookDaemon for FakeDaemon {
        fn event(&mut self, call: &HookCall, limit: Duration) -> anyhow::Result<HookAnswer> {
            if self.panic {
                panic!("daemon client bug");
            }
            self.events.push((call.clone(), limit));
            if self.fail {
                anyhow::bail!("no daemon");
            }
            Ok(self.answer.clone().unwrap_or_default())
        }
        fn send(&mut self, call: &HookCall, _: Duration) -> anyhow::Result<()> {
            self.sent.push(call.clone());
            Ok(())
        }
        fn ack(&mut self, _: Client, _: &str, ids: &[String], _: Duration) -> anyhow::Result<()> {
            self.acks.push(ids.to_vec());
            Ok(())
        }
    }

    fn run(client: &str, event: &str, payload: &'static str, d: &mut FakeDaemon) -> String {
        let mut out = Vec::new();
        let env = env_of(&[("CLAUDE_PID", "31337")]);
        run_hook(
            client,
            event,
            io::Cursor::new(payload.as_bytes()),
            &mut out,
            &env,
            d,
        );
        String::from_utf8(out).unwrap()
    }

    fn answer_with(ids: &[&str]) -> HookAnswer {
        HookAnswer {
            session: "01J9ZK0000000000000000000A".into(),
            address: "sam/claude-api".into(),
            context: Some("spew mesh: two messages".into()),
            ack_ids: ids.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn run_prints_context_then_acknowledges() {
        let mut d = FakeDaemon {
            answer: Some(answer_with(&["m1", "m2"])),
            ..FakeDaemon::default()
        };
        let out = run("claude", "UserPromptSubmit", CC_PROMPT, &mut d);
        assert!(out.contains("\"additionalContext\":\"spew mesh: two messages\""));
        assert_eq!(d.acks, vec![vec!["m1".to_string(), "m2".to_string()]]);
        let (_, limit) = &d.events[0];
        assert!(
            *limit <= Duration::from_secs(5) && *limit > Duration::from_secs(4),
            "{limit:?}"
        );
    }

    #[test]
    fn stop_and_end_print_nothing() {
        let mut d = FakeDaemon {
            answer: Some(answer_with(&["m1"])),
            ..FakeDaemon::default()
        };
        assert_eq!(run("claude", "Stop", CC_STOP, &mut d), "");
        assert_eq!(d.events.len(), 1, "Stop still marks the session idle");
        assert!(d.acks.is_empty());
        assert_eq!(run("codex", "SessionEnd", CX_END, &mut d), "");
        assert_eq!(d.sent.len(), 1, "SessionEnd sends without waiting");
        assert_eq!(run("claude", "SessionEnd", CC_END_CLEAR, &mut d), "");
        assert_eq!(d.sent.len(), 1, "a clear keeps the session");
    }

    #[test]
    fn any_failure_prints_nothing() {
        let mut d = FakeDaemon {
            fail: true,
            ..FakeDaemon::default()
        };
        assert_eq!(run("claude", "SessionStart", CC_START, &mut d), "");
        let mut d = FakeDaemon {
            panic: true,
            ..FakeDaemon::default()
        };
        assert_eq!(run("claude", "SessionStart", CC_START, &mut d), "");
        let mut d = FakeDaemon::default();
        assert_eq!(run("claude", "SessionStart", "not json", &mut d), "");
        assert_eq!(run("gemini", "SessionStart", CC_START, &mut d), "");
        assert_eq!(run("claude", "PostToolUse", CC_SUBAGENT, &mut d), "");
        assert!(
            d.events.is_empty(),
            "subagents and junk never reach the daemon"
        );
    }

    #[test]
    fn a_failed_write_is_not_acknowledged() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut d = FakeDaemon {
            answer: Some(answer_with(&["m1"])),
            ..FakeDaemon::default()
        };
        run_hook(
            "claude",
            "SessionStart",
            io::Cursor::new(CC_START.as_bytes()),
            &mut Broken,
            &no_env,
            &mut d,
        );
        assert_eq!(d.events.len(), 1);
        assert!(d.acks.is_empty());
    }

    #[test]
    fn a_stalled_stdin_gives_up_within_the_budget() {
        struct Stall;
        impl Read for Stall {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                thread::sleep(Duration::from_secs(30));
                Ok(0)
            }
        }
        let mut d = FakeDaemon::default();
        let mut out = Vec::new();
        let t = Instant::now();
        run_hook("claude", "SessionEnd", Stall, &mut out, &no_env, &mut d);
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        assert!(out.is_empty() && d.sent.is_empty());
    }
}
