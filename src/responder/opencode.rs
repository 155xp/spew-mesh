//! OpenCode serve runs (8.4): `opencode run` with a deny-all agent and no project config.

use std::ffi::OsString;
use std::path::Path;

use serde_json::Value;

use super::process::RunOutput;
use super::{RunFailure, RunResult, is_safe_id, one_line};

/// The agent every run uses, defined in [`config_content`].
pub const AGENT: &str = "spew-mesh-serve";

/// Dropped from the child environment before the serve variables are added: `OPENCODE_PERMISSION` could
/// widen the agent's permissions, and `OPENCODE` and `OPENCODE_PID` describe a parent OpenCode process.
pub const SCRUB: &[&str] = &["OPENCODE_PERMISSION", "OPENCODE", "OPENCODE_PID"];

/// `OPENCODE_CONFIG_CONTENT`, which outranks the person's global config. `--allow-edits` adds an `edit`
/// path map to the agent that still refuses `.git/` and secret files.
pub fn config_content(allow_edits: bool) -> String {
    let edit = if allow_edits {
        concat!(
            r#","edit":{"*":"allow",".git/*":"deny","*.env":"deny","*.env.*":"deny","#,
            r#""*.pem":"deny","*.key":"deny"}"#
        )
    } else {
        ""
    };
    format!(
        concat!(
            r#"{{"mcp":{{"mesh":{{"enabled":false}}}},"permission":{{"*":"deny"}},"#,
            r#""agent":{{"spew-mesh-serve":{{"description":"Answers questions from other agents, read-only","#,
            r#""mode":"primary","permission":{{"*":"deny","read":{{"*":"allow","*.env":"deny","*.env.*":"deny","#,
            r#""*.pem":"deny","*.key":"deny","*id_rsa*":"deny","*id_ed25519*":"deny","*.git/config":"deny"}},"#,
            r#""glob":"allow","list":"allow"{edit}}}}}}}}}"#,
        ),
        edit = edit
    )
}

/// The variables added to the child environment.
pub fn env_adds(allow_edits: bool) -> Vec<(&'static str, String)> {
    vec![
        ("OPENCODE_DISABLE_PROJECT_CONFIG", "1".to_string()),
        ("OPENCODE_CONFIG_CONTENT", config_content(allow_edits)),
    ]
}

/// The arguments after `opencode`. The message goes on stdin (no positional message).
pub fn args(
    session: Option<&str>,
    model: Option<&str>,
    asker_address: &str,
    cwd: &Path,
) -> Vec<OsString> {
    let mut v: Vec<OsString> = ["run", "--pure", "--format", "json", "--agent", AGENT]
        .iter()
        .map(OsString::from)
        .collect();
    if let Some(id) = session {
        v.push("--session".into());
        v.push(id.into());
    }
    if let Some(model) = model {
        v.push("--model".into());
        v.push(model.into());
    }
    v.push("--title".into());
    v.push(format!("spew mesh: {asker_address}").into());
    v.push("--dir".into());
    v.push(cwd.as_os_str().to_owned());
    v
}

/// True when stderr says OpenCode fell back to its default agent because ours wasn't found: a line naming
/// the agent with "not found", "fall back" or "falling back". The wording isn't pinned by a test against
/// OpenCode yet (8.4).
pub fn names_fallback(stderr: &str) -> bool {
    stderr.lines().any(|line| {
        let l = line.to_ascii_lowercase();
        l.contains(AGENT)
            && ["not found", "fall back", "falling back"]
                .iter()
                .any(|w| l.contains(w))
    })
}

/// Reads the JSON events: the session id from the first event, the answer from the text parts after the
/// last tool event. A non-zero exit, an `error` event or the fallback warning fails the run. Only `type`,
/// `sessionID`, `part.type`, `part.text` and `error` are read.
pub fn parse(out: &RunOutput) -> Result<RunResult, RunFailure> {
    let mut session_id: Option<String> = None;
    let mut texts: Vec<String> = Vec::new();
    let mut error: Option<String> = None;
    for line in out.stdout_text().lines() {
        let Ok(event) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if session_id.is_none()
            && let Some(id) = event.get("sessionID").and_then(Value::as_str)
            && is_safe_id(id)
        {
            session_id = Some(id.to_string());
        }
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        let part = event.get("part");
        let part_kind = part
            .and_then(|p| p.get("type"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if kind == "error" {
            error = Some(error_text(&event));
        } else if kind.starts_with("tool") || part_kind == "tool" {
            texts.clear();
        } else if kind == "text"
            && let Some(text) = part.and_then(|p| p.get("text")).and_then(Value::as_str)
        {
            texts.push(text.to_string());
        }
    }
    let fail = |reason: String| RunFailure {
        reason: one_line(&reason, 300),
        session_id: session_id.clone(),
        cost_usd: None,
    };
    if names_fallback(&out.stderr_text()) {
        return Err(fail(format!(
            "OpenCode didn't load the {AGENT} agent and fell back to its default agent"
        )));
    }
    if let Some(e) = error {
        return Err(fail(format!("OpenCode reported an error: {e}")));
    }
    if !out.success() {
        return Err(fail(format!("opencode {}", out.exit.describe())));
    }
    let answer = texts.join("\n").trim().to_string();
    if answer.is_empty() {
        return Err(fail("opencode gave no answer".into()));
    }
    Ok(RunResult {
        answer,
        session_id,
        cost_usd: None,
    })
}

fn error_text(event: &Value) -> String {
    let err = event.get("error");
    let message = err
        .and_then(|e| e.get("data"))
        .and_then(|d| d.get("message"))
        .or_else(|| err.and_then(|e| e.get("message")))
        .or_else(|| err.and_then(|e| e.get("name")))
        .or_else(|| err.filter(|e| e.is_string()))
        .and_then(Value::as_str);
    one_line(message.unwrap_or("an error event"), 200)
}

#[cfg(test)]
mod tests {
    use super::super::process::Exit;
    use super::*;
    use std::time::Duration;

    fn output(code: i32, stdout: &str, stderr: &str) -> RunOutput {
        RunOutput {
            exit: Exit::Code(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
            elapsed: Duration::from_secs(1),
        }
    }

    #[test]
    fn config_is_the_spec_json() {
        let v: Value = serde_json::from_str(&config_content(false)).unwrap();
        let expected = serde_json::json!({
            "mcp": {"mesh": {"enabled": false}},
            "permission": {"*": "deny"},
            "agent": {"spew-mesh-serve": {
                "description": "Answers questions from other agents, read-only",
                "mode": "primary",
                "permission": {
                    "*": "deny",
                    "read": {"*": "allow", "*.env": "deny", "*.env.*": "deny", "*.pem": "deny",
                             "*.key": "deny", "*id_rsa*": "deny", "*id_ed25519*": "deny",
                             "*.git/config": "deny"},
                    "glob": "allow",
                    "list": "allow"
                }
            }}
        });
        assert_eq!(v, expected);
        assert_eq!(
            config_content(false),
            concat!(
                r#"{"mcp":{"mesh":{"enabled":false}},"permission":{"*":"deny"},"agent":{"spew-mesh-serve":"#,
                r#"{"description":"Answers questions from other agents, read-only","mode":"primary","#,
                r#""permission":{"*":"deny","read":{"*":"allow","*.env":"deny","*.env.*":"deny","*.pem":"deny","#,
                r#""*.key":"deny","*id_rsa*":"deny","*id_ed25519*":"deny","*.git/config":"deny"},"#,
                r#""glob":"allow","list":"allow"}}}}"#
            )
        );
    }

    #[test]
    fn allow_edits_adds_the_edit_path_map_to_the_agent_only() {
        let v: Value = serde_json::from_str(&config_content(true)).unwrap();
        let perm = &v["agent"]["spew-mesh-serve"]["permission"];
        assert_eq!(
            perm["edit"],
            serde_json::json!({"*": "allow", ".git/*": "deny", "*.env": "deny", "*.env.*": "deny",
                               "*.pem": "deny", "*.key": "deny"})
        );
        assert_eq!(perm["*"], "deny");
        assert_eq!(perm["read"]["*.env"], "deny");
        assert_eq!(v["permission"], serde_json::json!({"*": "deny"}));
        assert_eq!(
            config_content(true),
            config_content(false).replace(
                r#""list":"allow"}"#,
                r#""list":"allow","edit":{"*":"allow",".git/*":"deny","*.env":"deny","*.env.*":"deny","*.pem":"deny","*.key":"deny"}}"#
            )
        );
    }

    #[test]
    fn env_adds_are_exact() {
        assert_eq!(
            env_adds(false),
            vec![
                ("OPENCODE_DISABLE_PROJECT_CONFIG", "1".to_string()),
                ("OPENCODE_CONFIG_CONTENT", config_content(false)),
            ]
        );
    }

    fn strings(v: &[OsString]) -> Vec<String> {
        v.iter().map(|s| s.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn argv_is_exact() {
        assert_eq!(
            strings(&args(None, None, "maya/claude-api", Path::new("/work/api"))),
            [
                "run",
                "--pure",
                "--format",
                "json",
                "--agent",
                "spew-mesh-serve",
                "--title",
                "spew mesh: maya/claude-api",
                "--dir",
                "/work/api",
            ]
        );
        assert_eq!(
            strings(&args(
                Some("ses_01ABC"),
                Some("acme/m1"),
                "maya/claude-api",
                Path::new("/work/api")
            )),
            [
                "run",
                "--pure",
                "--format",
                "json",
                "--agent",
                "spew-mesh-serve",
                "--session",
                "ses_01ABC",
                "--model",
                "acme/m1",
                "--title",
                "spew mesh: maya/claude-api",
                "--dir",
                "/work/api",
            ]
        );
    }

    const EVENTS: &str = concat!(
        r#"{"type":"step_start","timestamp":1,"sessionID":"ses_01ABC","part":{"type":"step-start"}}"#,
        "\n",
        r#"{"type":"text","timestamp":2,"sessionID":"ses_01ABC","part":{"type":"text","text":"Let me look."}}"#,
        "\n",
        r#"{"type":"tool_use","timestamp":3,"sessionID":"ses_01ABC","part":{"type":"tool","tool":"read","state":{"status":"completed","output":"secret-ish file text"}}}"#,
        "\n",
        r#"{"type":"text","timestamp":4,"sessionID":"ses_01ABC","part":{"type":"text","text":"The handler is in api/x.ts."}}"#,
        "\n",
        r#"{"type":"text","timestamp":5,"sessionID":"ses_01ABC","part":{"type":"text","text":"Line 40."}}"#,
        "\n",
        r#"{"type":"step_finish","timestamp":6,"sessionID":"ses_01ABC","part":{"type":"step-finish"}}"#,
        "\n",
    );

    #[test]
    fn answer_is_the_text_after_the_last_tool() {
        let r = parse(&output(0, EVENTS, "")).unwrap();
        assert_eq!(r.answer, "The handler is in api/x.ts.\nLine 40.");
        assert_eq!(r.session_id.as_deref(), Some("ses_01ABC"));
    }

    #[test]
    fn failures() {
        let with_error = format!(
            "{EVENTS}{}\n",
            r#"{"type":"error","timestamp":7,"sessionID":"ses_01ABC","error":{"name":"APIError","data":{"message":"rate limited"}}}"#
        );
        let f = parse(&output(0, &with_error, "")).unwrap_err();
        assert_eq!(f.reason, "OpenCode reported an error: rate limited");
        assert_eq!(f.session_id.as_deref(), Some("ses_01ABC"));

        assert_eq!(
            parse(&output(1, EVENTS, "")).unwrap_err().reason,
            "opencode exited with status 1"
        );
        let fallback = parse(&output(
            0,
            EVENTS,
            "! agent \"spew-mesh-serve\" not found. Falling back to default agent\n",
        ))
        .unwrap_err();
        assert!(fallback.reason.contains("fell back"), "{}", fallback.reason);
        assert_eq!(
            parse(&output(0, "", "")).unwrap_err().reason,
            "opencode gave no answer"
        );
        let only_tool = r#"{"type":"tool_use","sessionID":"ses_1","part":{"type":"tool"}}"#;
        assert!(parse(&output(0, only_tool, "")).is_err());
    }

    #[test]
    fn unsafe_session_ids_are_ignored() {
        let out = r#"{"type":"text","sessionID":"--model","part":{"type":"text","text":"hi"}}"#;
        assert_eq!(parse(&output(0, out, "")).unwrap().session_id, None);
    }

    #[test]
    fn fallback_detection() {
        assert!(names_fallback(
            "Agent spew-mesh-serve not found, using build"
        ));
        assert!(names_fallback(
            "warning: agent spew-mesh-serve missing, falling back to build"
        ));
        assert!(names_fallback(
            "could not load spew-mesh-serve; fall back to default"
        ));
        assert!(!names_fallback("loading config\n"));
        assert!(!names_fallback("file not found: x.ts"));
        assert!(!names_fallback("agent build not found"));
    }

    #[test]
    fn only_the_listed_fields_are_read() {
        // Text outside `part.text` and a message outside `error` are ignored.
        let out = concat!(
            r#"{"type":"text","sessionID":"ses_1","text":"top-level text"}"#,
            "\n",
            r#"{"type":"text","sessionID":"ses_1","part":{"type":"text","text":"the answer"}}"#,
            "\n",
            r#"{"type":"step_finish","sessionID":"ses_1","message":"not an error"}"#,
            "\n",
        );
        assert_eq!(parse(&output(0, out, "")).unwrap().answer, "the answer");
        let plain = r#"{"type":"error","sessionID":"ses_1","error":"quota exceeded"}"#;
        assert_eq!(
            parse(&output(0, plain, "")).unwrap_err().reason,
            "OpenCode reported an error: quota exceeded"
        );
    }
}
