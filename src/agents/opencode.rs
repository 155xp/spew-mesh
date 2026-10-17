//! OpenCode wiring (8.4): the `mcp.mesh` entry and send approvals in `opencode.json` (only when it is plain JSON),
//! the generated `plugins/spew-mesh.js`, and the skill when Claude Code's doesn't cover it.

use std::path::PathBuf;

use super::Client;
use super::hook::OPENCODE_KILL_SECS;
use super::json::Json;
use super::plan::{FilePlan, Format, Want};
use super::setup::{ClientPaths, Wiring};
use super::skill::{SKILL_DIR, SKILL_MD};

/// A tool call times out after this many milliseconds; progress restarts the clock (8.4).
pub const MCP_TIMEOUT_MS: i64 = 960_000;

/// The tools the person approves before each send (8.4). Never pre-approved.
pub const ASK_FIRST: [&str; 3] = ["mesh_ask", "mesh_tell", "mesh_reply"];

/// The `mcp.mesh` entry.
pub fn mcp_entry(w: &Wiring) -> Json {
    let mut command = vec![w.spew_str()];
    command.extend(w.args(&["mcp-server"]));
    Json::obj([
        ("type", Json::str("local")),
        ("command", Json::strings(&command)),
        (
            "environment",
            Json::obj([("SPEW_MESH_CLIENT", Json::str("opencode"))]),
        ),
        ("enabled", Json::Bool(true)),
        ("timeout", Json::int(MCP_TIMEOUT_MS)),
    ])
}

/// The config file setup works with: `opencode.json`, or an existing `opencode.jsonc` it never rewrites.
pub fn config_file(paths: &ClientPaths) -> (PathBuf, bool) {
    let json = paths.opencode_dir.join("opencode.json");
    let jsonc = paths.opencode_dir.join("opencode.jsonc");
    if !json.exists() && jsonc.exists() {
        (jsonc, true)
    } else {
        (json, false)
    }
}

/// `opencode.json`: the MCP server and the send approvals, plus `$schema` when setup creates the file.
pub fn config_plan(w: &Wiring, paths: &ClientPaths) -> FilePlan {
    let (file, never_write) = config_file(paths);
    let mut wants = vec![Want::set(
        "opencode.mcp",
        &["mcp", "mesh"],
        mcp_entry(w),
        "the mesh MCP server",
    )];
    let note = format!(
        "{}: \"permission\" is a single value, so setup left it alone and OpenCode won't ask before mesh_ask, \
         mesh_tell or mesh_reply send. Make it an object to have it ask.",
        file.display()
    );
    for tool in ASK_FIRST {
        let mut want = Want::set(
            format!("opencode.permission.{tool}"),
            &["permission", tool],
            Json::str("ask"),
            format!("asking before {tool}"),
        );
        want.blocked_note = Some(note.clone());
        wants.push(want);
    }
    FilePlan {
        client: Client::Opencode,
        file,
        format: Format::Json,
        wants,
        on_create: vec![Want::set(
            "opencode.schema",
            &["$schema"],
            Json::str("https://opencode.ai/config.json"),
            "the config schema",
        )],
        never_write,
    }
}

/// `<config folder>/plugins/spew-mesh.js`.
pub fn plugin_plan(w: &Wiring, paths: &ClientPaths) -> FilePlan {
    FilePlan {
        client: Client::Opencode,
        file: paths.opencode_dir.join("plugins").join("spew-mesh.js"),
        format: Format::Text,
        wants: vec![Want::file(
            "opencode.plugin",
            plugin_js(w),
            "the spew-mesh plugin",
        )],
        on_create: Vec::new(),
        never_write: false,
    }
}

/// `<config folder>/skills/spew-mesh/SKILL.md`, for when Claude Code's skill folder doesn't hold it.
pub fn skill_plan(paths: &ClientPaths) -> FilePlan {
    FilePlan {
        client: Client::Opencode,
        file: paths
            .opencode_dir
            .join("skills")
            .join(SKILL_DIR)
            .join("SKILL.md"),
        format: Format::Text,
        wants: vec![Want::file(
            "opencode.skill",
            SKILL_MD,
            "the spew-mesh skill",
        )],
        on_create: Vec::new(),
        never_write: false,
    }
}

/// The generated plugin. Each OpenCode event runs `spew mesh hook opencode <event>` with a small JSON payload on
/// stdin and reads at most one `{"context": "..."}` back; every failure is ignored, the child is killed after
/// [`OPENCODE_KILL_SECS`], and the plugin never starts a turn on its own (8.4).
pub fn plugin_js(w: &Wiring) -> String {
    let spew = Json::str(w.spew_str()).to_compact();
    let args = Json::strings(&w.args(&["hook", "opencode"])).to_compact();
    PLUGIN_TEMPLATE
        .replace("__SPEW__", &spew)
        .replace("__ARGS__", &args)
        .replace("__KILL_MS__", &(OPENCODE_KILL_SECS * 1000).to_string())
}

const PLUGIN_TEMPLATE: &str = r#"// spew mesh plugin for OpenCode. Written by `spew mesh setup`, which rewrites it; `spew mesh uninstall` removes it.
// For each event below it runs `spew mesh hook opencode <event>`. It never starts a turn or adds a message itself.
import { spawn } from "node:child_process";

const SPEW = __SPEW__;
const ARGS = __ARGS__;
const KILL_MS = __KILL_MS__;
const MAX_OUTPUT = 1 << 20;

// Runs one hook and resolves to its context text, or null on any failure.
function hook(event, payload) {
  return new Promise((resolve) => {
    let child;
    let out = "";
    let settled = false;
    let timer;
    const finish = (context) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      resolve(context);
    };
    try {
      child = spawn(SPEW, [...ARGS, event], { stdio: ["pipe", "pipe", "ignore"] });
    } catch {
      finish(null);
      return;
    }
    timer = setTimeout(() => {
      try { child.kill("SIGKILL"); } catch {}
      finish(null);
    }, KILL_MS);
    child.on("error", () => finish(null));
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk) => {
      out += chunk;
      if (out.length > MAX_OUTPUT) {
        try { child.kill("SIGKILL"); } catch {}
        finish(null);
      }
    });
    child.on("close", () => {
      try {
        const text = out.trim();
        const reply = text ? JSON.parse(text) : null;
        const context = reply && typeof reply.context === "string" ? reply.context : "";
        finish(context ? context : null);
      } catch {
        finish(null);
      }
    });
    child.stdin.on("error", () => {});
    try { child.stdin.end(JSON.stringify(payload)); } catch {}
  });
}

export const SpewMesh = async ({ directory }) => {
  // Subagent sessions: id -> parent id. Subagents aren't mesh sessions; mesh tools they call act as the root.
  const parents = new Map();
  // Arguments of spew tool calls, by call id, for card capture after the call.
  const spewCalls = new Map();
  const root = (id) => {
    let cur = id;
    for (let i = 0; i < 64 && parents.has(cur); i++) cur = parents.get(cur);
    return cur;
  };
  const base = (id) => ({ session_id: id, cwd: directory, pid: process.pid });

  return {
    event: async ({ event }) => {
      try {
        const props = (event && event.properties) || {};
        if (event.type === "session.created") {
          const info = props.info || {};
          if (!info.id) return;
          if (info.parentID) {
            parents.set(info.id, info.parentID);
            return;
          }
          void hook("session.created", base(info.id));
        } else if (event.type === "session.idle") {
          const id = props.sessionID;
          if (id && !parents.has(id)) void hook("session.idle", base(id));
        } else if (event.type === "session.deleted") {
          const id = (props.info && props.info.id) || props.sessionID;
          if (!id) return;
          if (parents.has(id)) {
            parents.delete(id);
            return;
          }
          void hook("session.deleted", base(id));
        }
      } catch {}
    },

    // The person's message, before it is saved: messages for this session go in as one more text part.
    "chat.message": async (input, output) => {
      try {
        const id = input && input.sessionID;
        if (!id || parents.has(id) || !output || !Array.isArray(output.parts)) return;
        const context = await hook("chat.message", base(id));
        if (!context) return;
        const message = output.message || {};
        const last = output.parts[output.parts.length - 1] || {};
        const part = {
          sessionID: last.sessionID || message.sessionID || id,
          messageID: last.messageID || message.id,
          type: "text",
          text: context,
          synthetic: true,
        };
        if (last.id) part.id = last.id + "mesh";
        output.parts.push(part);
      } catch {}
    },

    "tool.execute.before": async (input, output) => {
      try {
        const tool = String((input && input.tool) || "");
        if (tool.startsWith("mesh_") && input.sessionID && output) {
          if (!output.args || typeof output.args !== "object") output.args = {};
          output.args._mesh_session = root(input.sessionID);
        } else if (tool.startsWith("spew_") && input.callID) {
          if (spewCalls.size > 256) spewCalls.clear();
          spewCalls.set(input.callID, (output && output.args) || {});
        }
      } catch {}
    },

    "tool.execute.after": async (input, output) => {
      try {
        const id = input && input.sessionID;
        const tool = String((input && input.tool) || "");
        const args = spewCalls.get(input && input.callID) || (input && input.args) || {};
        spewCalls.delete(input && input.callID);
        if (!id || parents.has(id)) return;
        const payload = base(id);
        payload.tool_name = tool;
        if (tool.startsWith("spew_")) {
          payload.tool_input = args;
          payload.tool_response = output && typeof output.output === "string" ? output.output : "";
        }
        const context = await hook("tool.execute.after", payload);
        if (context && output && typeof output.output === "string") output.output += "\n\n" + context;
      } catch {}
    },
  };
};
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::runner::testing::fake_command;
    use std::fs;
    use std::path::Path;
    use std::process::Command;
    use std::time::{Duration, Instant};

    /// The `node` on PATH; without one, the plugin tests say they were skipped.
    fn node(test: &str) -> Option<PathBuf> {
        let found = std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|d| d.join("node"))
            .find(|p| p.is_file());
        if found.is_none() {
            eprintln!("{test}: skipped, node isn't on PATH");
        }
        found
    }

    /// The log's lines once it has `n` of them, waiting at most 10 s (fire-and-forget hooks finish on their own).
    fn wait_for_lines(log: &Path, n: usize) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let text = fs::read_to_string(log).unwrap_or_default();
            if text.lines().count() >= n || Instant::now() >= deadline {
                return text;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn the_entry_matches_the_spec() {
        let w = Wiring::new(PathBuf::from("/opt/spew"), None).unwrap();
        assert_eq!(
            mcp_entry(&w).to_compact(),
            r#"{"type":"local","command":["/opt/spew","mesh","mcp-server"],"environment":{"SPEW_MESH_CLIENT":"opencode"},"enabled":true,"timeout":960000}"#
        );
    }

    #[test]
    fn the_plugin_embeds_paths_as_string_literals() {
        let w = Wiring::new(
            PathBuf::from("/opt/a \"b\"/spew"),
            Some(PathBuf::from("/srv/m")),
        )
        .unwrap();
        let js = plugin_js(&w);
        assert!(js.contains(r#"const SPEW = "/opt/a \"b\"/spew";"#), "{js}");
        assert!(js.contains(r#"const ARGS = ["mesh","--home","/srv/m","hook","opencode"];"#));
        assert!(js.contains("const KILL_MS = 8000;"));
        assert!(!js.contains("__"), "every placeholder is filled");
        assert!(!js.contains("session.prompt") && !js.contains("promptAsync"));
    }

    #[test]
    fn the_plugin_passes_node_check() {
        let Some(node) = node("the_plugin_passes_node_check") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let w = Wiring::new(PathBuf::from("/opt/spew"), None).unwrap();
        let file = dir.path().join("spew-mesh.mjs");
        fs::write(&file, plugin_js(&w)).unwrap();
        let out = Command::new(node)
            .arg("--check")
            .arg(&file)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Drives the plugin's hooks in node with a fake `spew` that logs each call and answers with context.
    #[test]
    fn the_plugin_runs_hooks_and_appends_context() {
        let Some(node) = node("the_plugin_runs_hooks_and_appends_context") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls.log");
        fake_command(
            dir.path(),
            "spew",
            &format!(
                "payload=$(cat)\nprintf '%s %s\\n' \"$4\" \"$payload\" >> '{}'\n\
                 case \"$4\" in chat.message|tool.execute.after) printf '{{\"context\":\"MESH %s\"}}\\n' \"$4\";; esac",
                log.display()
            ),
        );
        let w = Wiring::new(dir.path().join("spew"), None).unwrap();
        let plugin = dir.path().join("spew-mesh.mjs");
        fs::write(&plugin, plugin_js(&w)).unwrap();
        let driver = dir.path().join("driver.mjs");
        fs::write(
            &driver,
            r#"
const mod = await import(process.argv[2]);
const p = await mod.SpewMesh({ directory: "/home/sam/cli" });
await p.event({ event: { type: "session.created", properties: { info: { id: "ses_root" } } } });
await p.event({ event: { type: "session.created", properties: { info: { id: "ses_kid", parentID: "ses_root" } } } });
const msg = { message: { id: "msg_1", sessionID: "ses_root" },
  parts: [{ id: "prt_1", sessionID: "ses_root", messageID: "msg_1", type: "text", text: "hi" }] };
await p["chat.message"]({ sessionID: "ses_root" }, msg);
const kidMsg = { message: { id: "msg_2" }, parts: [{ id: "prt_2", type: "text", text: "sub" }] };
await p["chat.message"]({ sessionID: "ses_kid" }, kidMsg);
const ask = { args: { to: "maya", text: "q" } };
await p["tool.execute.before"]({ tool: "mesh_ask", sessionID: "ses_kid", callID: "c1" }, ask);
const claim = { args: { target: "next" } };
await p["tool.execute.before"]({ tool: "spew_claim", sessionID: "ses_root", callID: "c2" }, claim);
const after = { title: "", output: "claimed spew#4", metadata: {} };
await p["tool.execute.after"]({ tool: "spew_claim", sessionID: "ses_root", callID: "c2" }, after);
await p.event({ event: { type: "session.idle", properties: { sessionID: "ses_kid" } } });
await p.event({ event: { type: "session.idle", properties: { sessionID: "ses_root" } } });
console.log(JSON.stringify({ parts: msg.parts, kid: kidMsg.parts.length, args: ask.args, after: after.output }));
"#,
        )
        .unwrap();
        let out = Command::new(node)
            .arg(&driver)
            .arg(&plugin)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["parts"][1]["text"], "MESH chat.message");
        assert_eq!(v["parts"][1]["id"], "prt_1mesh");
        assert_eq!(v["parts"][1]["messageID"], "msg_1");
        assert_eq!(v["kid"], 1, "subagent prompts get nothing");
        assert_eq!(
            v["args"]["_mesh_session"], "ses_root",
            "mesh tools act as the root session"
        );
        assert_eq!(v["after"], "claimed spew#4\n\nMESH tool.execute.after");

        let calls = wait_for_lines(&log, 4);
        let events: Vec<&str> = calls
            .lines()
            .map(|l| l.split(' ').next().unwrap())
            .collect();
        let mut sorted = events.clone();
        sorted.sort();
        assert_eq!(
            sorted,
            vec![
                "chat.message",
                "session.created",
                "session.idle",
                "tool.execute.after"
            ]
        );
        let tool_line = calls
            .lines()
            .find(|l| l.starts_with("tool.execute.after"))
            .unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(tool_line.split_once(' ').unwrap().1).unwrap();
        assert_eq!(payload["session_id"], "ses_root");
        assert_eq!(payload["cwd"], "/home/sam/cli");
        assert_eq!(payload["tool_name"], "spew_claim");
        assert_eq!(payload["tool_input"]["target"], "next");
        assert_eq!(payload["tool_response"], "claimed spew#4");
        assert!(payload["pid"].as_u64().is_some());
    }
}
