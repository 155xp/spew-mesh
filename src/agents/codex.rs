//! Codex wiring (8.3): the `[mcp_servers.mesh]` table in `config.toml` (edited with `toml_edit`, keeping the
//! person's comments and layout) and hook groups appended to `hooks.json`. No skill file and no approval settings.

use super::Client;
use super::hook::{CODEX_HOOKS, HookSpec, MeshEvent};
use super::json::Json;
use super::plan::{FilePlan, Format, Want};
use super::setup::{ClientPaths, Wiring};

/// Longer than the longest `ask` wait (900 s); progress doesn't extend Codex's limit (8.3).
pub const TOOL_TIMEOUT_SEC: i64 = 960;

/// The line setup ends with, and doctor repeats, when Codex is wired (8.3).
pub const TRUST_LINE: &str = "In Codex, run /hooks and trust the spew mesh hooks.";

/// The `[mcp_servers.mesh]` table, as JSON.
pub fn mcp_table(w: &Wiring) -> Json {
    Json::obj([
        ("command", Json::str(w.spew_str())),
        ("args", Json::strings(&w.args(&["mcp-server"]))),
        ("startup_timeout_sec", Json::int(10)),
        ("tool_timeout_sec", Json::int(TOOL_TIMEOUT_SEC)),
        ("required", Json::Bool(false)),
        ("env", Json::obj([("SPEW_MESH_CLIENT", Json::str("codex"))])),
    ])
}

/// `config.toml` with the MCP server.
pub fn config_plan(w: &Wiring, paths: &ClientPaths) -> FilePlan {
    FilePlan {
        client: Client::Codex,
        file: paths.codex_dir.join("config.toml"),
        format: Format::Toml,
        wants: vec![Want::set(
            "codex.mcp",
            &["mcp_servers", "mesh"],
            mcp_table(w),
            "the mesh MCP server",
        )],
        on_create: Vec::new(),
        never_write: false,
    }
}

/// The hook command: Codex runs it through the person's login shell, so it is one string with the path quoted.
pub fn hook_command(w: &Wiring, spec: &HookSpec) -> String {
    w.shell_command(&["hook", "codex", spec.event])
}

/// One matcher group for an event.
pub fn hook_group(w: &Wiring, spec: &HookSpec) -> Json {
    let mut handler = Json::obj([
        ("type", Json::str("command")),
        ("command", Json::str(hook_command(w, spec))),
        ("timeout", Json::int(spec.timeout_secs as i64)),
    ]);
    if spec.background {
        handler.insert("async", Json::Bool(true));
    }
    if spec.show && matches!(spec.mesh, MeshEvent::Start | MeshEvent::Prompt) {
        // No spill to a file: the 6 KiB cap of 7.8 bounds the size instead.
        handler.insert("additionalContextLimit", Json::int(0));
    }
    let hooks = Json::Array(vec![handler]);
    match spec.matcher {
        Some(m) => Json::obj([("matcher", Json::str(m)), ("hooks", hooks)]),
        None => Json::obj([("hooks", hooks)]),
    }
}

/// New groups appended to `hooks.json` (created when missing). Existing groups keep their order.
pub fn hooks_plan(w: &Wiring, paths: &ClientPaths) -> FilePlan {
    FilePlan {
        client: Client::Codex,
        file: paths.codex_dir.join("hooks.json"),
        format: Format::Json,
        wants: CODEX_HOOKS
            .iter()
            .map(|s| {
                Want::append(
                    format!("codex.hook.{}", s.event),
                    &["hooks", s.event],
                    hook_group(w, s),
                    format!("the {} hook", s.event),
                )
            })
            .collect(),
        on_create: Vec::new(),
        never_write: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn hook_groups_follow_the_spec_table() {
        let w = Wiring::new(PathBuf::from("/Users/sam/it's/spew"), None).unwrap();
        assert_eq!(
            hook_group(&w, &CODEX_HOOKS[0]).to_compact(),
            r#"{"hooks":[{"type":"command","command":"'/Users/sam/it'\\''s/spew' mesh hook codex SessionStart","timeout":10,"additionalContextLimit":0}]}"#
        );
        assert_eq!(
            hook_group(&w, &CODEX_HOOKS[4]).to_compact(),
            r#"{"matcher":"^mcp__spew__","hooks":[{"type":"command","command":"'/Users/sam/it'\\''s/spew' mesh hook codex PostToolUse","timeout":8,"async":true}]}"#
        );
        let stop = hook_group(&w, &CODEX_HOOKS[2]).to_compact();
        assert!(
            !stop.contains("additionalContextLimit") && !stop.contains("async"),
            "{stop}"
        );
        let w = Wiring::new(PathBuf::from("/opt/spew"), Some(PathBuf::from("/srv/mesh"))).unwrap();
        assert_eq!(
            hook_command(&w, &CODEX_HOOKS[3]),
            "'/opt/spew' mesh --home '/srv/mesh' hook codex SessionEnd"
        );
    }
}
