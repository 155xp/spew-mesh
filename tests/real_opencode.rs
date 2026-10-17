//! Opt-in checks against the real OpenCode (8.4, 15): setup's MCP server and plugin, the plugin's events
//! (`chat.message`, `tool.execute.before` and `.after`, subagents), `OPENCODE_PID`, progress against the
//! tool timeout, and the serve agent's read limits and fallback warning. Run with
//! `SPEW_MESH_REAL_CLI=1 cargo test --test real_opencode`; `opencode` must be on `PATH`.

mod real_cli_support;

use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use real_cli_support::{
    Reply, Request, Sandbox, call, http_json, say, tap_messages, token, try_http_json,
};
use serde_json::{Value, json};
use spew_mesh::responder::opencode;
use spew_mesh::wire::Reach;

const LIMIT: Duration = Duration::from_secs(180);

/// The variables that keep OpenCode off the network: no models.dev fetch, no default plugins or LSP
/// downloads, no update check, and npm pointed at the mock (OpenCode installs its plugin package at start).
fn oc_env(sb: &Sandbox) -> Vec<(OsString, OsString)> {
    sb.env_with(&[
        ("OPENCODE_DISABLE_DEFAULT_PLUGINS", "1".into()),
        ("OPENCODE_DISABLE_MODELS_FETCH", "1".into()),
        ("OPENCODE_DISABLE_AUTOUPDATE", "1".into()),
        ("OPENCODE_DISABLE_LSP_DOWNLOAD", "1".into()),
        ("npm_config_registry", format!("{}/npm/", sb.mock.url())),
    ])
}

fn config_path(sb: &Sandbox) -> std::path::PathBuf {
    sb.path("xdg/config").join("opencode").join("opencode.json")
}

/// Adds the mock as the person's provider and default model to `opencode.json`.
fn add_mock_provider(sb: &Sandbox) {
    let path = config_path(sb);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut config: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| json!({}));
    config["provider"] = json!({"mock": {
        "npm": "@ai-sdk/openai-compatible",
        "name": "Mock",
        "options": {"baseURL": format!("{}/v1", sb.mock.url()), "apiKey": "test-key-not-real"},
        "models": {"mock-model": {"name": "Mock Model", "tool_call": true}}
    }});
    config["model"] = json!("mock/mock-model");
    config["small_model"] = json!("mock/mock-model");
    std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
}

fn read_config(sb: &Sandbox) -> Value {
    serde_json::from_str(&std::fs::read_to_string(config_path(sb)).unwrap()).unwrap()
}

fn write_config(sb: &Sandbox, config: &Value) {
    std::fs::write(
        config_path(sb),
        serde_json::to_string_pretty(config).unwrap(),
    )
    .unwrap();
}

/// `opencode serve` on a free port, answering once its API does.
struct Server {
    child: real_cli_support::LineChild,
    port: u16,
}

impl Server {
    fn start(sb: &Sandbox, cli: &Path, cwd: &Path) -> Server {
        let port = real_cli_support::free_port();
        let port_arg = port.to_string();
        let cmd = sb.command(
            cli,
            &["serve", "--hostname", "127.0.0.1", "--port", &port_arg],
            cwd,
            &oc_env(sb),
        );
        let child = real_cli_support::LineChild::spawn(cmd, &sb.path("serve.stderr"));
        real_cli_support::wait_until("opencode serve", Duration::from_secs(60), || {
            try_http_json(port, "GET", "/session", None, Duration::from_secs(5))
                .is_ok_and(|(code, _)| code == 200)
        });
        Server { child, port }
    }

    fn get(&self, path: &str) -> Value {
        let (code, body) = http_json(self.port, "GET", path, None, LIMIT);
        assert_eq!(code, 200, "GET {path}: {body}");
        body
    }

    fn new_session(&self) -> String {
        let (code, body) = http_json(self.port, "POST", "/session", Some(&json!({})), LIMIT);
        assert_eq!(code, 200, "{body}");
        body["id"].as_str().expect("a session id").to_string()
    }

    /// Sends a prompt and waits for the reply.
    fn prompt(port: u16, session: &str, text: &str) -> Value {
        let body = json!({"parts": [{"type": "text", "text": text}]});
        let (code, reply) = http_json(
            port,
            "POST",
            &format!("/session/{session}/message"),
            Some(&body),
            LIMIT,
        );
        assert_eq!(code, 200, "{reply}");
        reply
    }
}

#[test]
fn plugin_events_reach_the_model() {
    let Some(cli) = real_cli_support::cli("plugin_events_reach_the_model", "opencode") else {
        return;
    };
    let markers = [
        "PROMPT-ONE",
        "PROMPT-TWO",
        "PROMPT-THREE",
        "PROMPT-FOUR",
        "SUBAGENT-TASK",
        "PROMPT-FIVE",
    ];
    let sb = Sandbox::new(&cli, move |r: &Request| {
        if !r.has_tools() {
            return say("Mock title");
        }
        let pause = |reply: Reply| Reply::After(Duration::from_secs(4), Box::new(reply));
        match r.turn(&markers) {
            Some((0, 0)) => call("mesh_who", json!({})),
            Some((2, 0)) => pause(call("glob", json!({"pattern": "*.md"}))),
            Some((3, 0)) => call(
                "task",
                json!({"description": "look around", "prompt": "SUBAGENT-TASK look around",
                       "subagent_type": "general"}),
            ),
            Some((4, 0)) => pause(call("mesh_who", json!({}))),
            Some((4, _)) => say("sub done"),
            _ => say("done"),
        }
    });
    let work = sb.work("cli");
    sb.setup("opencode", &oc_env(&sb));
    add_mock_provider(&sb);

    // setup wrote the MCP server and the plugin.
    let spew = env!("CARGO_BIN_EXE_spew");
    let mut config = read_config(&sb);
    let command = config["mcp"]["mesh"]["command"].clone();
    assert_eq!(command[0], spew, "{config}");
    assert!(command.to_string().contains("mcp-server"), "{command}");
    let plugin = sb.path("xdg/config").join("opencode/plugins/spew-mesh.js");
    assert!(
        std::fs::read_to_string(&plugin).unwrap().contains(spew),
        "the plugin names spew"
    );

    // A tap in front of the mesh server records what OpenCode sends it.
    let log = sb.path("tap.jsonl");
    let mut tapped = vec![json!(sb.tap(&log))];
    tapped.extend(command.as_array().cloned().unwrap_or_default());
    config["mcp"]["mesh"]["command"] = json!(tapped);
    write_config(&sb, &config);

    let server = Server::start(&sb, &cli, &work);
    let port = server.port;
    let root = server.new_session();
    Server::prompt(port, &root, "PROMPT-ONE hello");

    // The plugin loaded: its session.created hook opened a hooked session keyed on the OpenCode session.
    let rows = sb.wait_for_sessions("a hooked OpenCode session", |rows| {
        rows.iter().any(|r| {
            r.client == "opencode"
                && r.client_session.as_deref() == Some(root.as_str())
                && r.reach == Reach::Hooked
        })
    });
    let me = rows
        .iter()
        .find(|r| r.client_session.as_deref() == Some(root.as_str()))
        .unwrap();

    // tool.execute.before added `_mesh_session`; OpenCode introduced itself as `opencode`.
    let sent = tap_messages(&log);
    let init = sent.iter().find(|m| m["method"] == "initialize").unwrap();
    assert_eq!(init["params"]["clientInfo"]["name"], "opencode");
    let who = sent
        .iter()
        .find(|m| m["method"] == "tools/call" && m["params"]["name"] == "who")
        .expect("a who call");
    assert_eq!(
        who["params"]["arguments"]["_mesh_session"],
        json!(root),
        "{who}"
    );
    assert!(
        sb.mock
            .requests_for("PROMPT-ONE")
            .iter()
            .flat_map(Request::tool_results)
            .any(|t| real_cli_support::is_who(&t) && t.contains(&me.name)),
        "the who result for this session never reached the model"
    );

    // The MCP server runs with OPENCODE_PID naming the OpenCode process.
    let servers = sb.mcp_servers();
    assert_eq!(servers.len(), 1, "{servers:?}");
    let oc_pid: u32 = servers[0]
        .env_value("OPENCODE_PID")
        .expect("OPENCODE_PID in the MCP server's environment")
        .parse()
        .unwrap();
    assert_eq!(servers[0].env_value("OPENCODE").as_deref(), Some("1"));
    assert!(
        real_cli_support::descends_from(oc_pid, server.child.pid()),
        "OPENCODE_PID {oc_pid} isn't the opencode process"
    );

    // chat.message: a peer message goes in as one more part of the person's message, and the model sees it.
    let note1 = token("PEER-NOTE");
    sb.tell(&me.name, &note1);
    Server::prompt(port, &root, "PROMPT-TWO next");
    assert!(
        sb.mock
            .requests_for("PROMPT-TWO")
            .iter()
            .any(|r| r.contains(&note1)),
        "the chat.message part never reached the model"
    );
    let messages = server.get(&format!("/session/{root}/message"));
    let mine = messages
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["parts"].to_string().contains("PROMPT-TWO"))
        .expect("the PROMPT-TWO message");
    let parts = mine["parts"].as_array().unwrap();
    let typed = parts
        .iter()
        .position(|p| p["text"].as_str().is_some_and(|t| t.contains("PROMPT-TWO")))
        .unwrap();
    let added = parts
        .iter()
        .position(|p| p["text"].as_str().is_some_and(|t| t.contains(&note1)))
        .expect("the appended part was saved");
    assert!(added > typed, "{parts:#?}");
    let typed_id = parts[typed]["id"].as_str().unwrap();
    assert_eq!(parts[added]["id"], json!(format!("{typed_id}mesh")));
    assert_eq!(parts[added]["messageID"], parts[typed]["messageID"]);
    eprintln!(
        "chat.message part ids: {typed_id} then {}",
        parts[added]["id"]
    );

    // tool.execute.after: a message that arrives while a tool runs is appended to the tool's output.
    let note2 = token("PEER-NOTE");
    let busy = {
        let root = root.clone();
        std::thread::spawn(move || Server::prompt(port, &root, "PROMPT-THREE list files"))
    };
    sb.mock.wait_for("PROMPT-THREE", LIMIT, |r| {
        r.has_tools() && r.turn(&["PROMPT-THREE"]).is_some()
    });
    sb.tell(&me.name, &note2);
    busy.join().unwrap();
    let three: Vec<Request> = sb
        .mock
        .requests()
        .into_iter()
        .filter(|r| r.has_tools() && r.turn(&markers).map(|t| t.0) == Some(2))
        .collect();
    assert!(
        !three[0].contains(&note2),
        "the note arrived before the prompt"
    );
    assert!(
        three
            .iter()
            .flat_map(Request::tool_results)
            .any(|t| t.contains(&note2)),
        "the tool.execute.after text never reached the model"
    );

    // A subagent's events are ignored: no mesh session of its own, no peer message in its turn (one that
    // arrives meanwhile waits for the root session), and its mesh calls carry the root session id.
    let note3 = token("PEER-NOTE");
    let busy = {
        let root = root.clone();
        std::thread::spawn(move || Server::prompt(port, &root, "PROMPT-FOUR use a subagent"))
    };
    sb.mock.wait_for("the subagent's prompt", LIMIT, |r| {
        r.has_tools() && r.turn(&["SUBAGENT-TASK"]).is_some()
    });
    sb.tell(&me.name, &note3);
    busy.join().unwrap();
    let sessions = server.get("/session");
    let child = sessions
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["parentID"] == json!(root))
        .and_then(|s| s["id"].as_str())
        .expect("a subagent session")
        .to_string();
    let in_turn =
        |i: usize| move |r: &Request| r.has_tools() && r.turn(&markers).map(|t| t.0) == Some(i);
    let sub: Vec<Request> = sb.mock.requests().into_iter().filter(in_turn(4)).collect();
    assert!(sub.len() >= 2, "the subagent ran its tool step");
    assert!(
        !sub.iter().any(|r| r.contains(&note3)),
        "the subagent saw a peer message"
    );
    let calls: Vec<Value> = tap_messages(&log)
        .into_iter()
        .filter(|m| m["method"] == "tools/call" && m["params"]["name"] == "who")
        .collect();
    assert_eq!(calls.len(), 2, "the root's and the subagent's who calls");
    assert_eq!(
        calls[1]["params"]["arguments"]["_mesh_session"],
        json!(root)
    );
    assert!(
        !sb.live_sessions()
            .iter()
            .any(|r| r.client_session.as_deref() == Some(child.as_str())),
        "the subagent got a mesh session of its own"
    );
    // The root gets the message: after the task step if the 30 s tool-step limit allows, else at its next
    // prompt.
    Server::prompt(port, &root, "PROMPT-FIVE thanks");
    assert!(
        sb.mock
            .requests()
            .iter()
            .filter(|r| in_turn(3)(r) || in_turn(5)(r))
            .any(|r| r.contains(&note3)),
        "the root session never got the message"
    );

    drop(server);
    sb.assert_stayed_local();
}

/// Runs `opencode run --format json` with `prompt` on stdin; returns the run and the first tool event.
fn run_with_timeout(sb: &Sandbox, cli: &Path, work: &Path, timeout_ms: u64) -> (Duration, Value) {
    let mut config = read_config(sb);
    config["mcp"]["mesh"]["timeout"] = json!(timeout_ms);
    write_config(sb, &config);
    let start = Instant::now();
    let out = sb.run(
        cli,
        ["run", "--format", "json"].map(Into::into).to_vec(),
        work,
        oc_env(sb),
        &format!("PROMPT-WAIT {timeout_ms}"),
        LIMIT,
    );
    let took = start.elapsed();
    assert!(out.success(), "{}", out.stderr_text());
    let tool = out
        .stdout_text()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|e| e["type"] == "tool_use")
        .expect("a tool_use event");
    (took, tool["part"]["state"].clone())
}

#[test]
fn progress_resets_the_tool_timeout() {
    let Some(cli) = real_cli_support::cli("progress_resets_the_tool_timeout", "opencode") else {
        return;
    };
    let sb = Sandbox::new(&cli, |r: &Request| {
        if !r.has_tools() {
            return say("Mock title");
        }
        match r.turn(&["PROMPT-WAIT"]) {
            Some((0, 0)) => call("mesh_messages", json!({"wait_seconds": 40})),
            _ => say("waited"),
        }
    });
    let work = sb.work("cli");
    sb.setup("opencode", &oc_env(&sb));
    add_mock_provider(&sb);

    // 20 s timeout, a 40 s wait with progress every 15 s: the call finishes.
    let (took, state) = run_with_timeout(&sb, &cli, &work, 20_000);
    eprintln!("20 s timeout: {} after {took:?}", state["status"]);
    assert_eq!(state["status"], "completed", "{state}");
    assert!(took >= Duration::from_secs(38), "{took:?}");

    // 10 s timeout, shorter than the progress interval: the call fails, so the timeout really applies.
    let (took, state) = run_with_timeout(&sb, &cli, &work, 10_000);
    eprintln!(
        "10 s timeout: {} after {took:?}: {}",
        state["status"], state["error"]
    );
    assert_eq!(state["status"], "error", "{state}");
    sb.assert_stayed_local();
}

#[test]
fn serve_agent_read_limits_and_fallback_warning() {
    let Some(cli) =
        real_cli_support::cli("serve_agent_read_limits_and_fallback_warning", "opencode")
    else {
        return;
    };
    let steps: Arc<Mutex<Vec<Reply>>> = Arc::default();
    let shared = steps.clone();
    let sb = Sandbox::new(&cli, move |r: &Request| {
        if !r.has_tools() {
            return say("Mock title");
        }
        let steps = shared.lock().unwrap();
        match r.turn(&["PROMPT-SERVE", "PROMPT-FALLBACK"]) {
            Some((0, n)) if n < steps.len() => steps[n].clone(),
            Some((1, _)) => say("fallback answer"),
            _ => say("done"),
        }
    });
    add_mock_provider(&sb);
    let serve = std::fs::canonicalize(sb.work("serve")).unwrap();
    let control = token("CONTROL");
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
        ".git/config",
    ];
    let secrets: Vec<String> = secret_files
        .iter()
        .map(|f| {
            let s = token("SECRET");
            w(f, &format!("value {s}"));
            s
        })
        .collect();
    let outside = sb.path("work").join("outside.txt");
    let outside_secret = token("OUTSIDE");
    std::fs::write(&outside, &outside_secret).unwrap();

    let file = |rel: &str| serve.join(rel).to_string_lossy().into_owned();
    let mut plan = vec![call("read", json!({"filePath": file("notes.txt")}))];
    for f in &secret_files {
        plan.push(call("read", json!({"filePath": file(f)})));
    }
    plan.push(call("read", json!({"filePath": outside.to_string_lossy()})));
    plan.push(call("glob", json!({"pattern": "**/.env*"})));
    plan.push(call("read", json!({"filePath": file("sub")})));
    plan.push(say("serve answer"));
    let n_steps = plan.len();
    *steps.lock().unwrap() = plan;

    let mut env = oc_env(&sb);
    env.extend(
        opencode::env_adds(false)
            .into_iter()
            .map(|(k, v)| (k.into(), v.into())),
    );
    let args = opencode::args(None, None, "sam@sam-mbp/claude-api", &serve);
    let out = sb.run(
        &cli,
        args.clone(),
        &serve,
        env.clone(),
        "PROMPT-SERVE what secrets are here?",
        LIMIT,
    );
    let result = opencode::parse(&out).unwrap_or_else(|f| panic!("{f:?}\n{}", out.stderr_text()));
    assert_eq!(result.answer.trim(), "serve answer");
    assert!(!opencode::names_fallback(&out.stderr_text()));

    let requests = sb.mock.requests_for("PROMPT-SERVE");
    let mut tools = requests[0].tool_names();
    tools.sort();
    eprintln!("serve agent tools: {tools:?}");
    assert!(
        tools
            .iter()
            .all(|t| ["glob", "list", "read"].contains(&t.as_str())),
        "{tools:?}"
    );
    assert!(tools.contains(&"read".to_string()) && tools.contains(&"glob".to_string()));
    let results = requests.last().unwrap().tool_results();
    assert_eq!(results.len(), n_steps - 1, "{results:#?}");
    assert!(
        results[0].contains(&control),
        "read notes.txt: {}",
        results[0]
    );
    for secret in secrets.iter().chain([&outside_secret]) {
        assert!(
            !requests.iter().any(|r| r.contains(secret)),
            "a secret reached the model: {results:#?}"
        );
    }
    eprintln!("read .env answered: {}", results[1].replace('\n', " "));
    eprintln!(
        "glob **/.env* answered: {}",
        results[n_steps - 3].replace('\n', " ")
    );

    // Without the agent in the config, OpenCode falls back to its default agent with a warning that
    // names_fallback recognizes, and the run counts as failed.
    let mut env = oc_env(&sb);
    env.push(("OPENCODE_DISABLE_PROJECT_CONFIG".into(), "1".into()));
    env.push((
        "OPENCODE_CONFIG_CONTENT".into(),
        r#"{"mcp":{"mesh":{"enabled":false}},"permission":{"*":"deny"}}"#.into(),
    ));
    let out = sb.run(&cli, args, &serve, env, "PROMPT-FALLBACK hi", LIMIT);
    let stderr = out.stderr_text();
    let warning = stderr
        .lines()
        .find(|l| l.contains(opencode::AGENT))
        .unwrap_or("");
    eprintln!("fallback warning: {warning}");
    assert!(opencode::names_fallback(&stderr), "{stderr}");
    assert!(opencode::parse(&out).is_err());
    sb.assert_stayed_local();
}
