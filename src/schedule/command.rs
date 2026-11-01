//! The command line, environment and summary of one scheduled run.
//!
//! Scheduled runs are the person's own requests (spew only hands a device runs its own person created), so they
//! run with the person's normal agent settings and MCP servers, unlike serve, which answers other people
//! read-only. Each CLI may edit files in the project folder and can't stop to ask for approval: Claude Code in
//! `acceptEdits` mode with prompts denied, Codex in its `workspace-write` sandbox with approvals off. Nothing
//! skips permission checks. The prompt goes on stdin, so it never shows in the process list.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use super::AgentKind;
use crate::control::place::env_allowed;
use crate::responder::env::HostEnv;
use crate::responder::process::{Exit, RunOutput, RunSpec};
use crate::settings::{CLIENT_ENV, PROJECT_ENV};

/// How much of a run's end goes to spew as its summary, in characters.
pub const SUMMARY_CHARS: usize = 2000;

/// How much of a run's stdout is kept; the rest scrolls away.
pub const STDOUT_TAIL: usize = 1 << 20;

/// Variables a run keeps besides the daemon's own allowed set ([`env_allowed`]): where each CLI keeps its config
/// and sign-in when it isn't the default.
pub const KEEP_EXTRA: &[&str] = &[
    "CODEX_HOME",
    "CLAUDE_CONFIG_DIR",
    "OPENCODE_CONFIG",
    "OPENCODE_CONFIG_DIR",
];

/// The environment runs and detection start from: the daemon's allowed variables (HOME, PATH, USER, locale,
/// proxies, XDG folders) plus [`KEEP_EXTRA`], so each CLI finds its own sign-in. `SPEW_MESH_CLIENT` is left out:
/// it names whichever agent started the daemon.
pub fn base_env(process: &HostEnv) -> HostEnv {
    process.filtered(|k| {
        k != OsStr::new(CLIENT_ENV)
            && (env_allowed(k) || k.to_str().is_some_and(|k| KEEP_EXTRA.contains(&k)))
    })
}

/// True when `model` can go on a command line as one value: no leading `-`, no spaces or shell-looking bytes.
pub fn is_model_id(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 200
        && !model.starts_with('-')
        && model.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'.' | b'_' | b'-' | b':' | b'/' | b'@' | b'+' | b'[' | b']'
                )
        })
}

/// True when `effort` is a plain effort level such as `xhigh`.
pub fn is_effort(effort: &str) -> bool {
    !effort.is_empty() && effort.len() <= 32 && effort.bytes().all(|b| b.is_ascii_lowercase())
}

/// Why a run's model or effort can't be used, if so. An empty effort means the CLI's default.
pub fn check_choice(agent: AgentKind, model: &str, effort: &str) -> Result<(), String> {
    if !is_model_id(model) {
        return Err(format!(
            "{model:?} is not a model id {} takes",
            agent.label()
        ));
    }
    if !effort.is_empty() && !is_effort(effort) {
        return Err(format!("{effort:?} is not an effort level"));
    }
    Ok(())
}

/// The arguments after the program. The prompt goes on stdin.
pub fn args(agent: AgentKind, model: &str, effort: &str, folder: &Path) -> Vec<OsString> {
    let mut v: Vec<OsString> = Vec::new();
    let mut push = |s: &str| v.push(OsString::from(s));
    match agent {
        AgentKind::Claude => {
            push("-p");
            push("--model");
            push(model);
            if !effort.is_empty() {
                push("--effort");
                push(effort);
            }
            for s in [
                "--output-format",
                "json",
                "--permission-mode",
                "acceptEdits",
                // Nobody is there to answer a prompt: anything outside acceptEdits and the person's allow rules
                // is denied at once instead of waiting.
                "--permission-prompts",
                "none",
            ] {
                push(s);
            }
        }
        AgentKind::Codex => {
            for s in ["exec", "--json", "--skip-git-repo-check", "--model", model] {
                push(s);
            }
            if !effort.is_empty() {
                push("-c");
                push(&format!("model_reasoning_effort=\"{effort}\""));
            }
            for s in [
                "-c",
                "approval_policy=\"never\"",
                "--sandbox",
                "workspace-write",
                "-C",
            ] {
                push(s);
            }
            v.push(folder.as_os_str().to_owned());
            // `-` reads the prompt from stdin.
            v.push("-".into());
        }
        AgentKind::Opencode => {
            // OpenCode reports no effort levels, so a run never gets one.
            for s in ["run", "--model", model] {
                push(s);
            }
        }
    }
    v
}

/// One scheduled run, ready for a [`crate::responder::process::ProcessRunner`].
#[derive(Debug, Clone)]
pub struct RunInput<'a> {
    /// Which agent.
    pub agent: AgentKind,
    /// Its executable.
    pub program: PathBuf,
    /// The model.
    pub model: &'a str,
    /// The effort, or empty for the CLI's default.
    pub effort: &'a str,
    /// The prompt.
    pub prompt: &'a str,
    /// The project folder.
    pub folder: &'a Path,
    /// The project key, passed as `SPEW_PROJECT`.
    pub project: &'a str,
    /// The run's timeout.
    pub timeout: Duration,
}

/// The process to start for one run: the CLI in the project folder, the base environment minus the variables
/// that describe a parent agent session, `SPEW_PROJECT` set, the prompt on stdin, and stdout kept from its end.
pub fn spec(input: &RunInput<'_>, env: &HostEnv) -> RunSpec {
    RunSpec {
        program: input.program.clone(),
        args: args(input.agent, input.model, input.effort, input.folder),
        cwd: input.folder.to_path_buf(),
        env: env.child_env(
            input.agent.serve_client(),
            &[(PROJECT_ENV, input.project.to_string())],
        ),
        stdin: input.prompt.as_bytes().to_vec(),
        timeout: input.timeout,
        stdout_cap: STDOUT_TAIL,
        keep_tail: true,
    }
}

/// The last `n` characters of `text`, trimmed.
pub fn tail_chars(text: &str, n: usize) -> String {
    let text = text.trim();
    let count = text.chars().count();
    if count <= n {
        return text.to_string();
    }
    let mut out: String = text.chars().skip(count - n + 1).collect();
    out.insert(0, '…');
    out
}

// Drops terminal escapes and control characters except newlines and tabs.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI: ESC [ ... final byte in @..~
            if chars.peek() == Some(&'[') {
                chars.next();
                for d in chars.by_ref() {
                    if ('@'..='~').contains(&d) {
                        break;
                    }
                }
            }
            continue;
        }
        if c == '\n' || c == '\t' || !c.is_control() {
            out.push(c);
        }
    }
    out
}

// Claude Code's `--output-format json` result text.
fn claude_result(stdout: &str) -> Option<String> {
    let v: Value = serde_json::from_str(stdout.trim()).ok()?;
    v.get("result")?.as_str().map(str::to_string)
}

// The last agent message in Codex's `--json` events.
fn codex_result(stdout: &str) -> Option<String> {
    stdout.lines().rev().find_map(|line| {
        if !line.contains("agent_message") {
            return None;
        }
        let v: Value = serde_json::from_str(line).ok()?;
        let item = v.get("item")?;
        (item.get("type")?.as_str()? == "agent_message")
            .then(|| item.get("text")?.as_str().map(str::to_string))
            .flatten()
    })
}

/// What spew hears about a finished run: the agent's last words (or the end of its output), with why it failed
/// in front when it did, at most [`SUMMARY_CHARS`] characters.
pub fn summary(agent: AgentKind, out: &RunOutput, timeout: Duration) -> String {
    let stdout = out.stdout_text();
    let body = match agent {
        AgentKind::Claude => claude_result(&stdout),
        AgentKind::Codex => codex_result(&stdout),
        AgentKind::Opencode => None,
    }
    .unwrap_or(stdout);
    let mut body = plain(&body).trim().to_string();
    if body.is_empty() && !out.success() {
        body = plain(&out.stderr_text()).trim().to_string();
    }
    let head = match out.exit {
        Exit::Code(0) => None,
        Exit::TimedOut => Some(format!(
            "{} ran past the {}-minute timeout and was stopped.",
            agent.label(),
            timeout.as_secs() / 60
        )),
        other => Some(format!("{} {}.", agent.label(), other.describe())),
    };
    match head {
        None => tail_chars(&body, SUMMARY_CHARS),
        Some(head) if body.is_empty() => head,
        Some(head) => {
            let room = SUMMARY_CHARS.saturating_sub(head.chars().count() + 1);
            format!("{head}\n{}", tail_chars(&body, room))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(v: &[OsString]) -> Vec<String> {
        v.iter().map(|s| s.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn claude_command_line() {
        let a = args(AgentKind::Claude, "opus", "xhigh", Path::new("/w/spew"));
        assert_eq!(
            strings(&a),
            [
                "-p",
                "--model",
                "opus",
                "--effort",
                "xhigh",
                "--output-format",
                "json",
                "--permission-mode",
                "acceptEdits",
                "--permission-prompts",
                "none"
            ]
        );
        assert!(!strings(&a).iter().any(|s| s.contains("dangerously")));
        let a = args(AgentKind::Claude, "sonnet", "", Path::new("/w"));
        assert!(!strings(&a).contains(&"--effort".to_string()));
    }

    #[test]
    fn codex_command_line() {
        let a = args(
            AgentKind::Codex,
            "gpt-6.1-sol",
            "high",
            Path::new("/w/spew"),
        );
        assert_eq!(
            strings(&a),
            [
                "exec",
                "--json",
                "--skip-git-repo-check",
                "--model",
                "gpt-6.1-sol",
                "-c",
                "model_reasoning_effort=\"high\"",
                "-c",
                "approval_policy=\"never\"",
                "--sandbox",
                "workspace-write",
                "-C",
                "/w/spew",
                "-"
            ]
        );
    }

    #[test]
    fn opencode_command_line() {
        let a = args(
            AgentKind::Opencode,
            "opencode-go/kimi-k3",
            "high",
            Path::new("/w"),
        );
        assert_eq!(strings(&a), ["run", "--model", "opencode-go/kimi-k3"]);
    }

    #[test]
    fn spec_runs_in_the_folder_with_the_prompt_on_stdin_and_a_clean_env() {
        let host = base_env(&HostEnv::new([
            ("HOME", "/home/sam"),
            ("PATH", "/usr/bin"),
            ("CODEX_HOME", "/home/sam/.codex"),
            ("CLAUDECODE", "1"),
            ("CLAUDE_CODE_SESSION_ID", "s"),
            ("ANTHROPIC_API_KEY", "k"),
            ("SPEW_MESH_CLIENT", "codex"),
            ("SPEW_MESH_DIR", "/m"),
            ("XDG_CONFIG_HOME", "/home/sam/.config"),
        ]));
        let input = RunInput {
            agent: AgentKind::Claude,
            program: PathBuf::from("/usr/bin/claude"),
            model: "opus",
            effort: "high",
            prompt: "Pick up spew#42",
            folder: Path::new("/w/spew"),
            project: "spew",
            timeout: Duration::from_secs(7200),
        };
        let s = spec(&input, &host);
        assert_eq!(s.cwd, Path::new("/w/spew"));
        assert_eq!(s.stdin, b"Pick up spew#42");
        assert!(s.keep_tail);
        assert!(!strings(&s.args).iter().any(|a| a.contains("spew#42")));
        let names: Vec<String> = s
            .env
            .iter()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            [
                "HOME",
                "PATH",
                "CODEX_HOME",
                "SPEW_MESH_DIR",
                "XDG_CONFIG_HOME",
                "SPEW_PROJECT"
            ]
        );
    }

    #[test]
    fn choices_are_checked_before_they_reach_a_command_line() {
        assert!(check_choice(AgentKind::Claude, "opus", "max").is_ok());
        assert!(check_choice(AgentKind::Opencode, "opencode-go/kimi-k3", "").is_ok());
        assert!(check_choice(AgentKind::Claude, "claude-fable-5-1[1m]", "").is_ok());
        assert!(check_choice(AgentKind::Claude, "--dangerously-skip-permissions", "").is_err());
        assert!(check_choice(AgentKind::Codex, "gpt 6", "high").is_err());
        assert!(check_choice(AgentKind::Codex, "gpt-6", "high\"; x=\"").is_err());
    }

    fn output(exit: Exit, stdout: &str, stderr: &str) -> RunOutput {
        RunOutput {
            exit,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
            elapsed: Duration::from_secs(1),
        }
    }

    #[test]
    fn summaries_use_the_agents_last_words() {
        let t = Duration::from_secs(7200);
        let claude = r#"{"type":"result","subtype":"success","is_error":false,"result":"Moved spew#42 to review."}"#;
        assert_eq!(
            summary(AgentKind::Claude, &output(Exit::Code(0), claude, ""), t),
            "Moved spew#42 to review."
        );
        let codex = concat!(
            r#"{"type":"thread.started","thread_id":"t"}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"1","type":"agent_message","text":"first"}}"#,
            "\n",
            r#"{"type":"item.completed","item":{"id":"2","type":"agent_message","text":"done, in review"}}"#,
            "\n",
            r#"{"type":"turn.completed","usage":{}}"#,
            "\n"
        );
        assert_eq!(
            summary(AgentKind::Codex, &output(Exit::Code(0), codex, ""), t),
            "done, in review"
        );
        assert_eq!(
            summary(
                AgentKind::Opencode,
                &output(Exit::Code(0), "\u{1b}[1mDone\u{1b}[0m\n", ""),
                t
            ),
            "Done"
        );
        assert_eq!(
            summary(AgentKind::Codex, &output(Exit::TimedOut, "", ""), t),
            "Codex ran past the 120-minute timeout and was stopped."
        );
        assert_eq!(
            summary(
                AgentKind::Claude,
                &output(Exit::Code(1), "", "not logged in"),
                t
            ),
            "Claude Code exited with status 1.\nnot logged in"
        );
        let long = "x".repeat(5000);
        let s = summary(AgentKind::Opencode, &output(Exit::Code(0), &long, ""), t);
        assert_eq!(s.chars().count(), SUMMARY_CHARS);
        assert!(s.starts_with('…'));
    }
}
