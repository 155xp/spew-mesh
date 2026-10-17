//! Opt-in checks against the real Claude Code (8.2, 15): setup's hooks and MCP server, the MCP handshake, the
//! session binding, a peer message reaching the model at the next prompt, and a serve run's deny rules. Run
//! with `SPEW_MESH_REAL_CLI=1 cargo test --test real_claude`; `claude` must be on `PATH`.

mod real_cli_support;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use real_cli_support::{Reply, Request, Sandbox, call, find_key, say, tap_messages, token};
use serde_json::{Value, json};
use spew_mesh::agents::hook::CLAUDE_HOOKS;
use spew_mesh::responder::claude;
use spew_mesh::wire::Reach;

const LIMIT: Duration = Duration::from_secs(120);

/// The variables that point Claude Code at the mock and keep it off the network.
fn claude_env(sb: &Sandbox) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    sb.env_with(&[
        ("ANTHROPIC_BASE_URL", sb.mock.url()),
        ("ANTHROPIC_API_KEY", "test-key-not-real".into()),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1".into()),
        ("DISABLE_TELEMETRY", "1".into()),
        ("DISABLE_AUTOUPDATER", "1".into()),
    ])
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn write_json(path: &Path, v: &Value) {
    std::fs::write(path, serde_json::to_string_pretty(v).unwrap()).unwrap();
}

/// Calls `tool` once after the prompt holding `marker`, then answers `done`.
fn one_call(marker: &'static str, tool: &'static str) -> impl Fn(&Request) -> Reply {
    move |r: &Request| {
        if !r.has_tools() {
            return say("ok");
        }
        match r.turn(&[marker]) {
            Some((0, 0)) => call(tool, json!({})),
            _ => say("done"),
        }
    }
}

#[test]
fn setup_hooks_binding_and_a_peer_message_at_the_next_prompt() {
    let Some(cli) = real_cli_support::cli(
        "setup_hooks_binding_and_a_peer_message_at_the_next_prompt",
        "claude",
    ) else {
        return;
    };
    let script = |r: &Request| {
        if !r.has_tools() {
            return say("ok");
        }
        match r.turn(&["PROMPT-ONE", "PROMPT-TWO"]) {
            Some((0, 0)) => call("mcp__mesh__who", json!({})),
            _ => say("done"),
        }
    };
    let sb = Sandbox::new(&cli, script);
    let work = sb.work("api");
    sb.setup("claude", &claude_env(&sb));

    // setup wrote every hook and the MCP server into the temp config folder.
    let settings_path = sb.path("claude").join("settings.json");
    let mut settings = read_json(&settings_path);
    let spew = env!("CARGO_BIN_EXE_spew");
    for spec in &CLAUDE_HOOKS {
        let groups = settings["hooks"][spec.event].to_string();
        assert!(
            groups.contains(spew) && groups.contains(&format!("\"claude\",\"{}\"", spec.event)),
            "no spew hook for {}: {groups}",
            spec.event
        );
    }
    let server = read_json(&sb.path("claude").join(".claude.json"))["mcpServers"]["mesh"].clone();
    assert_eq!(server["command"], spew);
    assert!(
        server["args"].to_string().contains("mcp-server"),
        "{server}"
    );

    // A second SessionStart hook records CLAUDE_PID, to compare with the MCP server's parent.
    let pid_file = sb.path("claude-pid");
    let record = format!("printf %s \"$CLAUDE_PID\" > '{}'", pid_file.display());
    settings["hooks"]["SessionStart"]
        .as_array_mut()
        .unwrap()
        .push(json!({"hooks": [{"type": "command", "command": record}]}));
    write_json(&settings_path, &settings);

    let args = [
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--allowedTools",
        "mcp__mesh__who",
    ];
    let cmd = sb.command(&cli, &args, &work, &claude_env(&sb));
    let mut claude = real_cli_support::LineChild::spawn(cmd, &sb.path("claude.stderr"));
    let prompt = |text: &str| json!({"type": "user", "message": {"role": "user", "content": text}});
    let is_result = |v: &Value| v["type"] == "result";
    let mut events = Vec::new();

    claude.send_json(&prompt("PROMPT-ONE hello"));
    let first = claude.until("the first result", LIMIT, &mut events, is_result);
    assert_eq!(first["subtype"], "success", "{first}");
    let session_id = events
        .iter()
        .find(|e| e["type"] == "system" && e["subtype"] == "init")
        .and_then(|e| e["session_id"].as_str())
        .expect("an init event with the session id")
        .to_string();

    // The MCP server's parent is the Claude Code process the hooks name in CLAUDE_PID.
    let claude_pid: u32 = std::fs::read_to_string(&pid_file)
        .expect("the SessionStart hook ran")
        .trim()
        .parse()
        .unwrap();
    let servers = sb.mcp_servers();
    assert_eq!(servers.len(), 1, "one mesh MCP server: {servers:?}");
    assert_eq!(
        servers[0].ppid, claude_pid,
        "MCP server parent vs CLAUDE_PID"
    );
    assert!(real_cli_support::descends_from(claude_pid, claude.pid()));

    // Hooks and the MCP server landed on one session, keyed on Claude Code's session id and pid.
    let rows = sb.wait_for_sessions("one hooked Claude session", |rows| {
        rows.len() == 1 && rows[0].reach == Reach::Hooked
    });
    let me = &rows[0];
    assert_eq!(me.client, "claude");
    assert_eq!(me.client_session.as_deref(), Some(session_id.as_str()));
    assert_eq!(me.client_pid, Some(claude_pid));

    // The SessionStart context and the who result reached the model.
    let one = sb.mock.requests_for("PROMPT-ONE");
    assert!(
        one.iter().any(|r| r.contains("spew mesh: this session is")),
        "the SessionStart context never reached the model"
    );
    let who = one
        .iter()
        .flat_map(Request::tool_results)
        .find(|t| real_cli_support::is_who(t))
        .expect("the who result reached the model");
    assert!(who.contains(&me.name), "{who}");

    // A message from another session shows up at the next prompt, not before.
    let note = token("PEER-NOTE");
    sb.tell(&me.name, &note);
    claude.send_json(&prompt("PROMPT-TWO again"));
    let second = claude.until("the second result", LIMIT, &mut events, is_result);
    assert_eq!(second["subtype"], "success", "{second}");
    assert!(
        sb.mock
            .requests_for("PROMPT-TWO")
            .iter()
            .any(|r| r.contains(&note)),
        "the peer message never reached the model"
    );
    assert!(!one.iter().any(|r| r.contains(&note)));

    claude.close_stdin();
    assert!(claude.wait(Duration::from_secs(30)), "claude didn't exit");
    sb.assert_stayed_local();
}

#[test]
fn mcp_handshake_client_info_and_tool_call() {
    let Some(cli) = real_cli_support::cli("mcp_handshake_client_info_and_tool_call", "claude")
    else {
        return;
    };
    let sb = Sandbox::new(&cli, one_call("PROMPT-TAP", "mcp__mesh__who"));
    let work = sb.work("api");
    sb.setup("claude", &claude_env(&sb));

    // Put a tap in front of setup's mesh server to record what Claude Code sends it.
    let config_path = sb.path("claude").join(".claude.json");
    let mut config = read_json(&config_path);
    let server = config["mcpServers"]["mesh"].clone();
    let mut tapped = vec![server["command"].clone()];
    tapped.extend(server["args"].as_array().cloned().unwrap_or_default());
    for (run, negotiation) in [("auto", None), ("legacy", Some("legacy"))] {
        let log = sb.path(&format!("tap-{run}.jsonl"));
        config["mcpServers"]["mesh"]["command"] = json!(sb.tap(&log));
        config["mcpServers"]["mesh"]["args"] = json!(tapped);
        write_json(&config_path, &config);
        let mut env = claude_env(&sb);
        if let Some(n) = negotiation {
            env.push(("MCP_PROTOCOL_NEGOTIATION".into(), n.into()));
        }
        let out = sb.run(
            &cli,
            [
                "-p",
                "--output-format",
                "json",
                "--allowedTools",
                "mcp__mesh__who",
            ]
            .map(Into::into)
            .to_vec(),
            &work,
            env,
            &format!("PROMPT-TAP {run}"),
            LIMIT,
        );
        assert!(out.success(), "{run}: {}", out.stderr_text());

        let sent = tap_messages(&log);
        let first = sent.first().expect("Claude Code sent nothing");
        let method = first["method"].as_str().unwrap_or("");
        let info = find_key(first, "clientInfo").expect("clientInfo in the first request");
        eprintln!(
            "{run}: first request {method}, protocol {}, clientInfo {info}",
            find_key(first, "protocolVersion").unwrap_or(&Value::Null)
        );
        match negotiation {
            None => assert!(
                ["server/discover", "initialize"].contains(&method),
                "{first}"
            ),
            Some(_) => assert_eq!(method, "initialize", "{first}"),
        }
        assert_eq!(info["name"], "claude-code", "src/agents/mcp maps this name");
        let tool_call = sent
            .iter()
            .find(|m| m["method"] == "tools/call")
            .expect("a tools/call");
        assert_eq!(tool_call["params"]["name"], "who");
        if method == "server/discover" {
            assert!(
                find_key(tool_call, "clientInfo").is_some(),
                "each request carries clientInfo in _meta"
            );
        }
        let results: Vec<String> = sb
            .mock
            .requests_for("PROMPT-TAP")
            .iter()
            .flat_map(Request::tool_results)
            .collect();
        assert!(
            results.iter().any(|t| real_cli_support::is_who(t)),
            "{run}: the who result never reached the model: {results:?}"
        );
    }
    sb.assert_stayed_local();
}

#[test]
fn serve_run_grep_and_glob_honor_the_deny_rules() {
    let Some(cli) = real_cli_support::cli("serve_run_grep_and_glob_honor_the_deny_rules", "claude")
    else {
        return;
    };
    let control = token("CONTROL");
    let secrets: Vec<String> = (0..7).map(|i| token(&format!("SECRET{i}"))).collect();
    let probe = format!("{}|{}", "CONTROL-", "SECRET");

    // The steps name files in the sandbox, so the script reads them once they're filled in.
    let steps: Arc<Mutex<Vec<Reply>>> = Arc::default();
    let shared = steps.clone();
    let script = move |r: &Request| {
        if !r.has_tools() {
            return say("ok");
        }
        let steps = shared.lock().unwrap();
        match r.turn(&["PROMPT-SERVE-1", "PROMPT-SERVE-2"]) {
            Some((0, n)) if n < steps.len() => steps[n].clone(),
            Some((1, _)) => say("follow-up answer"),
            _ => say("done"),
        }
    };
    let sb = Sandbox::new(&cli, script);
    let serve = sb.work("serve");
    let w = |rel: &str, text: &str| {
        let p = serve.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, format!("{text}\n")).unwrap();
    };
    w("notes.txt", &format!("notes {control}"));
    let secret_files = [
        ".env",
        ".env.local",
        "sub/.env",
        "sub/.env.production",
        "sub/server.pem",
        "sub/deploy.key",
        "sub/id_ed25519",
    ];
    for (file, secret) in secret_files.iter().zip(&secrets) {
        w(file, &format!("value {secret}"));
    }
    let git = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&serve)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", sb.path("home"))
        .status()
        .unwrap();
    assert!(git.success());
    let git_secret = token("SECRET-GIT");
    let mut git_config = std::fs::read_to_string(serve.join(".git/config")).unwrap();
    git_config.push_str(&format!("# {git_secret}\n"));
    std::fs::write(serve.join(".git/config"), git_config).unwrap();

    let file = |rel: &str| serve.join(rel).to_string_lossy().into_owned();
    *steps.lock().unwrap() = vec![
        call(
            "Grep",
            json!({"pattern": probe, "path": file(""), "output_mode": "content"}),
        ),
        call(
            "Grep",
            json!({"pattern": probe, "path": file(""), "output_mode": "files_with_matches"}),
        ),
        call("Glob", json!({"pattern": "**/*", "path": file("")})),
        call("Glob", json!({"pattern": "**/.env*", "path": file("")})),
        call("Read", json!({"file_path": file("notes.txt")})),
        call("Read", json!({"file_path": file(".env")})),
        call("Read", json!({"file_path": file("sub/.env.production")})),
        call("Read", json!({"file_path": file("sub/server.pem")})),
        call("Read", json!({"file_path": file(".git/config")})),
        say("serve answer"),
    ];
    let n_steps = steps.lock().unwrap().len();

    let id = claude::new_session_id().unwrap();
    let opts = claude::Options {
        allow_edits: false,
        grep: true,
        max_turns: 20,
        max_budget_usd: 1.0,
        model: None,
    };
    let run = |session: claude::Session<'_>, prompt: &str| {
        sb.run(
            &cli,
            claude::args(session, &opts)
                .into_iter()
                .map(Into::into)
                .collect(),
            &serve,
            claude_env(&sb),
            prompt,
            LIMIT,
        )
    };
    let out = run(claude::Session::New(&id), "PROMPT-SERVE-1 what is here?");
    let result = claude::parse(&out).unwrap_or_else(|f| panic!("{f:?}\n{}", out.stderr_text()));
    assert_eq!(result.answer, "serve answer");
    assert_eq!(result.session_id.as_deref(), Some(id.as_str()));

    let requests = sb.mock.requests_for("PROMPT-SERVE-1");
    let last = requests.last().expect("the serve run reached the mock");
    let results = last.tool_results();
    assert_eq!(results.len(), n_steps - 1, "{results:#?}");
    for secret in secrets.iter().chain([&git_secret]) {
        assert!(
            !requests.iter().any(|r| r.contains(secret)),
            "a secret reached the model: {results:#?}"
        );
    }
    assert!(
        results[0].contains(&control),
        "Grep content: {}",
        results[0]
    );
    assert!(
        results[1].contains("notes.txt"),
        "Grep files: {}",
        results[1]
    );
    assert!(results[2].contains("notes.txt"), "Glob: {}", results[2]);
    assert!(
        results[4].contains(&control),
        "Read notes.txt: {}",
        results[4]
    );
    eprintln!("Glob **/* listed: {}", results[2].replace('\n', " "));
    eprintln!("Glob **/.env* listed: {}", results[3].replace('\n', " "));
    eprintln!("Read .env answered: {}", results[5].replace('\n', " "));

    // A follow-up resumes the same conversation.
    let out = run(claude::Session::Resume(&id), "PROMPT-SERVE-2 and then?");
    let result = claude::parse(&out).unwrap_or_else(|f| panic!("{f:?}\n{}", out.stderr_text()));
    assert_eq!(result.answer, "follow-up answer");
    assert_eq!(result.session_id.as_deref(), Some(id.as_str()));
    assert!(
        sb.mock
            .requests_for("PROMPT-SERVE-2")
            .iter()
            .any(|r| r.contains("PROMPT-SERVE-1")),
        "the follow-up didn't carry the first question"
    );
    sb.assert_stayed_local();
}
