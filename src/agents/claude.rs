//! Claude Code wiring (8.2): hooks and pre-approvals in `settings.json`, the MCP server in `.claude.json`
//! (through `claude mcp add-json` when the CLI is there), and the skill.

use super::Client;
use super::hook::{CLAUDE_HOOKS, HookSpec};
use super::json::Json;
use super::plan::{FilePlan, Format, Want};
use super::setup::{ClientPaths, Wiring};
use super::skill::{SKILL_DIR, SKILL_MD};

/// Manifest id of the MCP server entry.
pub const MCP_ID: &str = "claude.mcp";

/// Tools setup pre-approves. Never `ask`, `tell` or `reply` (8.2, 8.6).
pub const PRE_APPROVED: [&str; 2] = ["mcp__mesh__who", "mcp__mesh__messages"];

/// The MCP server definition (user scope, name `mesh`).
pub fn mcp_definition(w: &Wiring) -> Json {
    Json::obj([
        ("type", Json::str("stdio")),
        ("command", Json::str(w.spew_str())),
        ("args", Json::strings(&w.args(&["mcp-server"]))),
        (
            "env",
            Json::obj([("SPEW_MESH_CLIENT", Json::str("claude"))]),
        ),
        ("alwaysLoad", Json::Bool(true)),
    ])
}

/// `claude mcp add-json --scope user mesh '<definition>'`, as arguments.
pub fn cli_add_args(w: &Wiring) -> Vec<String> {
    ["mcp", "add-json", "--scope", "user", "mesh"]
        .iter()
        .map(|s| s.to_string())
        .chain([mcp_definition(w).to_compact()])
        .collect()
}

/// `claude mcp remove --scope user mesh`, as arguments.
pub fn cli_remove_args() -> Vec<String> {
    ["mcp", "remove", "--scope", "user", "mesh"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// One matcher group holding the exec-form handler for an event.
pub fn hook_group(w: &Wiring, spec: &HookSpec) -> Json {
    let handler = Json::obj([
        ("type", Json::str("command")),
        ("command", Json::str(w.spew_str())),
        (
            "args",
            Json::strings(&w.args(&["hook", "claude", spec.event])),
        ),
        ("timeout", Json::int(spec.timeout_secs as i64)),
    ]);
    let hooks = Json::Array(vec![handler]);
    match spec.matcher {
        Some(m) => Json::obj([("matcher", Json::str(m)), ("hooks", hooks)]),
        None => Json::obj([("hooks", hooks)]),
    }
}

/// Hooks and pre-approvals in `<config dir>/settings.json`.
pub fn settings_plan(w: &Wiring, paths: &ClientPaths) -> FilePlan {
    let mut wants: Vec<Want> = CLAUDE_HOOKS
        .iter()
        .map(|s| {
            Want::append(
                format!("claude.hook.{}", s.event),
                &["hooks", s.event],
                hook_group(w, s),
                format!("the {} hook", s.event),
            )
        })
        .collect();
    for tool in PRE_APPROVED {
        wants.push(Want::append(
            format!("claude.allow.{tool}"),
            &["permissions", "allow"],
            Json::str(tool),
            format!("the pre-approval of {tool}"),
        ));
    }
    FilePlan {
        client: Client::Claude,
        file: paths.claude_dir.join("settings.json"),
        format: Format::Json,
        wants,
        on_create: Vec::new(),
        never_write: false,
    }
}

/// The MCP server written straight into `.claude.json`, for when the `claude` command isn't on PATH.
pub fn mcp_plan(w: &Wiring, paths: &ClientPaths) -> FilePlan {
    let mut want = Want::set(
        MCP_ID,
        &["mcpServers", "mesh"],
        mcp_definition(w),
        "the mesh MCP server",
    );
    // Claude Code may store extra fields of its own next to ours.
    want.subset_ok = true;
    FilePlan {
        client: Client::Claude,
        file: paths.claude_json.clone(),
        format: Format::Json,
        wants: vec![want],
        on_create: Vec::new(),
        never_write: false,
    }
}

/// `<config dir>/skills/spew-mesh/SKILL.md`.
pub fn skill_plan(paths: &ClientPaths) -> FilePlan {
    FilePlan {
        client: Client::Claude,
        file: paths
            .claude_dir
            .join("skills")
            .join(SKILL_DIR)
            .join("SKILL.md"),
        format: Format::Text,
        wants: vec![Want::file("claude.skill", SKILL_MD, "the spew-mesh skill")],
        on_create: Vec::new(),
        never_write: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn handlers_are_exec_form_with_the_spec_timeouts() {
        let w = Wiring::new(PathBuf::from("/opt/spew/bin/spew"), None).unwrap();
        let g = hook_group(&w, &CLAUDE_HOOKS[2]);
        assert_eq!(
            g.to_compact(),
            r#"{"matcher":"*","hooks":[{"type":"command","command":"/opt/spew/bin/spew","args":["mesh","hook","claude","PostToolUse"],"timeout":8}]}"#
        );
        let end = hook_group(&w, &CLAUDE_HOOKS[4]);
        assert_eq!(end.get("matcher"), None);
        assert_eq!(
            end.get("hooks").and_then(|h| h.as_array()).unwrap()[0].get("timeout"),
            Some(&Json::int(2))
        );
    }

    #[test]
    fn the_mcp_definition_matches_the_spec_and_carries_a_custom_home() {
        let w = Wiring::new(PathBuf::from("/opt/spew"), None).unwrap();
        assert_eq!(
            mcp_definition(&w).to_compact(),
            r#"{"type":"stdio","command":"/opt/spew","args":["mesh","mcp-server"],"env":{"SPEW_MESH_CLIENT":"claude"},"alwaysLoad":true}"#
        );
        let w = Wiring::new(
            PathBuf::from("/opt/spew"),
            Some(PathBuf::from("/srv/mesh home")),
        )
        .unwrap();
        assert_eq!(
            mcp_definition(&w).get("args"),
            Some(&Json::strings(&[
                "mesh",
                "--home",
                "/srv/mesh home",
                "mcp-server"
            ]))
        );
        assert_eq!(
            cli_add_args(&w)[..5],
            ["mcp", "add-json", "--scope", "user", "mesh"]
        );
    }
}
