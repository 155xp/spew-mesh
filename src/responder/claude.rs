//! Claude Code serve runs (8.2): `claude -p` with only the read and search tools, under `--restricted`.

use serde_json::Value;

use super::process::RunOutput;
use super::{RunFailure, RunResult, ServeError, one_line, random_bytes};

/// `--settings` for every run: no hooks, no cross-session messages, and reads of secret files denied.
pub const SETTINGS: &str = concat!(
    r#"{"disableAllHooks":true,"crossSessionInbound":"refuse","permissions":{"deny":["#,
    r#""Read(./.env)","Read(./.env.*)","Read(./**/.env)","Read(./**/.env.*)","Read(./**/*.pem)","#,
    r#""Read(./**/*.key)","Read(./**/id_rsa*)","Read(./**/id_ed25519*)","Read(./.git/config)"]}}"#,
);

/// Variables that describe the parent Claude Code session; dropped from the child environment.
pub const SCRUB: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_BRIDGE_SESSION_ID",
    "CLAUDE_CODE_REMOTE_SESSION_ID",
    "CLAUDE_PID",
    "CLAUDE_PROJECT_DIR",
    "CLAUDE_ENV_FILE",
    "CLAUDE_EFFORT",
];

/// Every variable with this prefix is dropped too.
pub const SCRUB_PREFIX: &str = "CLAUDE_PLUGIN_";

/// Which session a run uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Session<'a> {
    /// A conversation's first run: `--session-id <uuid>`.
    New(&'a str),
    /// A follow-up: `--resume <uuid>`, with every other flag repeated.
    Resume(&'a str),
}

/// The flags that vary per responder.
#[derive(Clone, Debug, PartialEq)]
pub struct Options<'a> {
    /// `--allow-edits`: adds Edit and Write and switches to `acceptEdits`.
    pub allow_edits: bool,
    /// Whether Grep is offered (8.2: dropped if it doesn't honor the deny rules).
    pub grep: bool,
    /// `--max-turns`.
    pub max_turns: u32,
    /// `--max-budget-usd`.
    pub max_budget_usd: f64,
    /// `--model`.
    pub model: Option<&'a str>,
}

/// The arguments after `claude`.
pub fn args(session: Session<'_>, opts: &Options<'_>) -> Vec<String> {
    let mut tools = String::from(if opts.grep {
        "Read,Grep,Glob"
    } else {
        "Read,Glob"
    });
    if opts.allow_edits {
        tools.push_str(",Edit,Write");
    }
    let mode = if opts.allow_edits {
        "acceptEdits"
    } else {
        "dontAsk"
    };
    let (session_flag, id) = match session {
        Session::New(id) => ("--session-id", id),
        Session::Resume(id) => ("--resume", id),
    };
    let mut a: Vec<String> = [
        "-p",
        "--output-format",
        "json",
        session_flag,
        id,
        "--restricted",
        "--tools",
        &tools,
        "--disallowedTools",
        "mcp__*",
        "--strict-mcp-config",
        "--permission-mode",
        mode,
        "--settings",
        SETTINGS,
        "--max-turns",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    a.push(opts.max_turns.to_string());
    a.push("--max-budget-usd".into());
    a.push(opts.max_budget_usd.to_string());
    if let Some(model) = opts.model {
        a.push("--model".into());
        a.push(model.into());
    }
    a.push("--permission-prompts".into());
    a.push("none".into());
    a
}

/// A new random (version 4) UUID for a conversation's first run.
pub fn new_session_id() -> Result<String, ServeError> {
    let mut b = random_bytes(16)?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

/// True for the 8-4-4-4-12 hex form.
pub fn is_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, n)| g.len() == n && g.chars().all(|c| c.is_ascii_hexdigit()))
}

/// True when the run failed because Claude Code has no session with the id it was asked to resume.
pub fn session_missing(out: &RunOutput) -> bool {
    let needle = "No conversation found";
    out.stdout_text().contains(needle) || out.stderr_text().contains(needle)
}

/// Reads the JSON result: `result`, `is_error`, `subtype`, `session_id` and `total_cost_usd`.
pub fn parse(out: &RunOutput) -> Result<RunResult, RunFailure> {
    let text = out.stdout_text();
    let Some(result) = find_result(&text) else {
        return Err(RunFailure::new(if out.success() {
            "claude printed no JSON result".to_string()
        } else {
            format!("claude {}", out.exit.describe())
        }));
    };
    let session_id = result
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|s| is_uuid(s))
        .map(str::to_string);
    let cost_usd = result.get("total_cost_usd").and_then(Value::as_f64);
    let is_error = result.get("is_error").and_then(Value::as_bool);
    let subtype = result.get("subtype").and_then(Value::as_str).unwrap_or("");
    let answer = result.get("result").and_then(Value::as_str).unwrap_or("");

    let fail = |reason: String| RunFailure {
        reason: one_line(&reason, 300),
        session_id: session_id.clone(),
        cost_usd,
    };

    if subtype != "success" {
        return Err(fail(subtype_reason(subtype, &result)));
    }
    // A missing is_error is treated as an error: the result can't be trusted to be an answer.
    if is_error != Some(false) {
        let detail = one_line(answer, 200);
        return Err(fail(if detail.is_empty() {
            "claude reported an error".to_string()
        } else {
            format!("claude reported an error: {detail}")
        }));
    }
    if !out.success() {
        return Err(fail(format!("claude {}", out.exit.describe())));
    }
    if answer.trim().is_empty() {
        return Err(fail("claude gave no answer".to_string()));
    }
    Ok(RunResult {
        answer: answer.to_string(),
        session_id,
        cost_usd,
    })
}

fn subtype_reason(subtype: &str, result: &Value) -> String {
    let first_error = result
        .get("errors")
        .and_then(Value::as_array)
        .and_then(|errs| errs.iter().find_map(error_text));
    let base = match subtype {
        "" => "claude's result has no subtype".to_string(),
        "error_max_turns" => "it reached its turn limit (max_turns)".to_string(),
        "error_max_budget_usd" => "it reached its budget (max_budget_usd)".to_string(),
        "error_during_execution" => "claude hit an error while running".to_string(),
        other => format!("claude ended with {other}"),
    };
    match first_error {
        Some(e) => format!("{base}: {}", one_line(&e, 200)),
        None => base,
    }
}

fn error_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(o) => o.get("message").and_then(Value::as_str).map(str::to_string),
        _ => None,
    }
}

/// The result object: the whole stdout, or failing that the last line that is a `"type":"result"` object.
fn find_result(text: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(text.trim())
        && v.is_object()
    {
        return Some(v);
    }
    text.lines().rev().find_map(|line| {
        let v: Value = serde_json::from_str(line.trim()).ok()?;
        (v.get("type").and_then(Value::as_str) == Some("result")).then_some(v)
    })
}

#[cfg(test)]
mod tests {
    use super::super::process::Exit;
    use super::*;
    use std::time::Duration;

    const ID: &str = "3f0c6a52-8d2e-4b7a-9c11-0a1b2c3d4e5f";

    fn opts() -> Options<'static> {
        Options {
            allow_edits: false,
            grep: true,
            max_turns: 20,
            max_budget_usd: 0.5,
            model: None,
        }
    }

    fn output(code: i32, stdout: &str) -> RunOutput {
        RunOutput {
            exit: Exit::Code(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: vec![],
            elapsed: Duration::from_secs(1),
        }
    }

    #[test]
    fn first_run_argv_is_exact() {
        assert_eq!(
            args(Session::New(ID), &opts()),
            [
                "-p",
                "--output-format",
                "json",
                "--session-id",
                ID,
                "--restricted",
                "--tools",
                "Read,Grep,Glob",
                "--disallowedTools",
                "mcp__*",
                "--strict-mcp-config",
                "--permission-mode",
                "dontAsk",
                "--settings",
                SETTINGS,
                "--max-turns",
                "20",
                "--max-budget-usd",
                "0.5",
                "--permission-prompts",
                "none",
            ]
        );
    }

    #[test]
    fn follow_up_repeats_every_flag() {
        let first = args(Session::New(ID), &opts());
        let resume = args(Session::Resume(ID), &opts());
        assert_eq!(resume[3], "--resume");
        assert_eq!(resume[4], ID);
        assert_eq!(first[5..], resume[5..]);
    }

    #[test]
    fn allow_edits_and_model() {
        let a = args(
            Session::New(ID),
            &Options {
                allow_edits: true,
                model: Some("claude-sonnet-5"),
                max_budget_usd: 1.25,
                max_turns: 8,
                ..opts()
            },
        );
        assert_eq!(a[7], "Read,Grep,Glob,Edit,Write");
        assert_eq!(a[12], "acceptEdits");
        assert_eq!(a[14], SETTINGS, "the deny rules stay with --allow-edits");
        assert_eq!(a[16], "8");
        assert_eq!(a[18], "1.25");
        assert_eq!(
            a[19..],
            ["--model", "claude-sonnet-5", "--permission-prompts", "none"]
        );
    }

    #[test]
    fn without_grep() {
        let a = args(
            Session::New(ID),
            &Options {
                grep: false,
                ..opts()
            },
        );
        assert_eq!(a[7], "Read,Glob");
    }

    #[test]
    fn settings_is_the_spec_json() {
        let v: Value = serde_json::from_str(SETTINGS).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "disableAllHooks": true,
                "crossSessionInbound": "refuse",
                "permissions": {"deny": [
                    "Read(./.env)", "Read(./.env.*)", "Read(./**/.env)", "Read(./**/.env.*)",
                    "Read(./**/*.pem)", "Read(./**/*.key)", "Read(./**/id_rsa*)",
                    "Read(./**/id_ed25519*)", "Read(./.git/config)"
                ]}
            })
        );
    }

    #[test]
    fn session_ids_are_v4_uuids() {
        let a = new_session_id().unwrap();
        let b = new_session_id().unwrap();
        assert!(is_uuid(&a), "{a}");
        assert_ne!(a, b);
        assert_eq!(&a[14..15], "4");
        assert!(!is_uuid("not-a-uuid"));
        assert!(!is_uuid("3f0c6a52-8d2e-4b7a-9c11-0a1b2c3d4e5"));
    }

    #[test]
    fn parses_a_successful_result() {
        let out = output(
            0,
            &format!(
                r#"{{"type":"result","subtype":"success","is_error":false,"result":"The API lives in api/.","session_id":"{ID}","total_cost_usd":0.0123,"num_turns":2}}"#
            ),
        );
        assert_eq!(
            parse(&out),
            Ok(RunResult {
                answer: "The API lives in api/.".into(),
                session_id: Some(ID.into()),
                cost_usd: Some(0.0123),
            })
        );
    }

    #[test]
    fn error_results_fail() {
        let max_turns = output(
            1,
            &format!(
                r#"{{"type":"result","subtype":"error_max_turns","is_error":true,"session_id":"{ID}","total_cost_usd":0.2}}"#
            ),
        );
        let f = parse(&max_turns).unwrap_err();
        assert_eq!(f.reason, "it reached its turn limit (max_turns)");
        assert_eq!(f.session_id.as_deref(), Some(ID));
        assert_eq!(f.cost_usd, Some(0.2));

        let is_error = output(
            0,
            r#"{"type":"result","subtype":"success","is_error":true,"result":"API Error: overloaded\nmore"}"#,
        );
        assert_eq!(
            parse(&is_error).unwrap_err().reason,
            "claude reported an error: API Error: overloaded"
        );

        let during = output(
            1,
            r#"{"type":"result","subtype":"error_during_execution","is_error":true,"errors":["No conversation found with session ID: x"]}"#,
        );
        assert_eq!(
            parse(&during).unwrap_err().reason,
            "claude hit an error while running: No conversation found with session ID: x"
        );
        assert!(session_missing(&during));

        let missing_flag = output(0, r#"{"type":"result","subtype":"success","result":"hi"}"#);
        assert!(parse(&missing_flag).is_err());
    }

    #[test]
    fn non_zero_exit_with_a_success_result_fails() {
        let out = output(
            2,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"hi"}"#,
        );
        assert_eq!(
            parse(&out).unwrap_err().reason,
            "claude exited with status 2"
        );
    }

    #[test]
    fn no_json_or_empty_answer_fails() {
        assert_eq!(
            parse(&output(1, "Error: bad flag\n")).unwrap_err().reason,
            "claude exited with status 1"
        );
        assert_eq!(
            parse(&output(0, "")).unwrap_err().reason,
            "claude printed no JSON result"
        );
        let empty = output(
            0,
            r#"{"type":"result","subtype":"success","is_error":false,"result":"  "}"#,
        );
        assert_eq!(parse(&empty).unwrap_err().reason, "claude gave no answer");
    }

    #[test]
    fn finds_the_result_line_among_others() {
        let out = output(
            0,
            "warning: something\n{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ok\"}\n",
        );
        assert_eq!(parse(&out).unwrap().answer, "ok");
    }
}
