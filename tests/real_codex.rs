//! Opt-in checks against the real Codex (8.3, 15): setup's MCP server and hooks, `_meta` on tool calls, hook
//! output Codex accepts, a peer message at the next prompt, and a serve run: the profile's deny rules, the
//! startup canary check and `codex exec resume <id> -`. Run with
//! `SPEW_MESH_REAL_CLI=1 cargo test --test real_codex`; `codex` (and `node`, for the npm wrapper) must be
//! on `PATH`.

mod real_cli_support;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use real_cli_support::{
    Reply, Request, Sandbox, call, call_in, find_key, say, tap_messages, token,
};
use serde_json::{Value, json};
use spew_mesh::agents::hook::CODEX_HOOKS;
use spew_mesh::responder::codex;
use spew_mesh::responder::process::SystemRunner;
use spew_mesh::wire::Reach;

const LIMIT: Duration = Duration::from_secs(180);

#[test]
fn serve_profile_denies_deep_secrets_with_and_without_edits() {
    let Some(cli) = real_cli_support::cli(
        "serve_profile_denies_deep_secrets_with_and_without_edits",
        "codex",
    ) else {
        return;
    };
    let sb = Sandbox::new(&cli, |_: &Request| say("unused"));
    let work = sb.work("serve");
    std::fs::create_dir_all(work.join(".git/hooks")).unwrap();
    std::fs::write(
        work.join(".git/config"),
        "[core]\nrepositoryformatversion = 0\n",
    )
    .unwrap();
    let deep = work.join("a/b/c/d");
    std::fs::create_dir_all(&deep).unwrap();
    let control = token("CONTROL");
    std::fs::write(work.join("notes.txt"), &control).unwrap();
    let secret = token("SECRET");
    let names = [
        ".env",
        ".env.production",
        "prod.env",
        "server.pem",
        "deploy.key",
        "id_ed25519",
    ];
    for name in names {
        std::fs::write(deep.join(name), &secret).unwrap();
    }
    let real = std::fs::canonicalize(&cli).unwrap();
    let dir = codex::codex_dir(&real, &sb.path("codex"));
    for allow_edits in [false, true] {
        let profile = codex::profile(&dir, allow_edits).unwrap();
        let run = |args| {
            sb.run(
                &cli,
                args,
                &work,
                codex_env(&sb),
                "",
                Duration::from_secs(30),
            )
        };
        let out = run(codex::sandbox_args(
            &work,
            &profile,
            &work.join("notes.txt"),
        ));
        assert!(
            out.success() && out.stdout_text().contains(&control),
            "{out:?}"
        );
        for name in names {
            let file = deep.join(name);
            let out = run(codex::sandbox_args(&work, &profile, &file));
            assert!(!out.success(), "read {name} with allow_edits={allow_edits}");
            assert!(!out.stdout_text().contains(&secret), "{out:?}");
            assert!(!out.stderr_text().contains(&secret), "{out:?}");
            let mut args = codex::sandbox_args(&work, &profile, &file);
            args.truncate(args.len() - 2);
            args.extend([
                OsString::from("sh"),
                OsString::from("-c"),
                OsString::from(format!("printf changed > a/b/c/d/{name}")),
            ]);
            let out = run(args);
            assert!(
                !out.success(),
                "wrote {name} with allow_edits={allow_edits}"
            );
            assert_eq!(std::fs::read_to_string(&file).unwrap(), secret);
        }
    }
    sb.assert_stayed_local();
}

/// The variables Codex needs for the mock provider.
fn codex_env(sb: &Sandbox) -> Vec<(OsString, OsString)> {
    sb.env_with(&[("MOCK_API_KEY", "test-key-not-real".into())])
}

/// The person's own config.toml: the mock as provider, no plugins and no usage metrics (Codex 0.163.0
/// otherwise clones its plugin marketplace from github.com and sends metrics to ab.chatgpt.com).
fn write_user_config(sb: &Sandbox) -> PathBuf {
    let path = sb.path("codex").join("config.toml");
    let text = format!(
        concat!(
            "model = \"mock-model\"\nmodel_provider = \"mock\"\n\n[features]\nplugins = false\n\n",
            "[analytics]\nenabled = false\n\n",
            "[model_providers.mock]\nname = \"mock\"\nbase_url = \"{}/v1\"\nenv_key = \"MOCK_API_KEY\"\n",
            "wire_api = \"responses\"\n"
        ),
        sb.mock.url()
    );
    std::fs::write(&path, text).unwrap();
    path
}

fn events(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn thread_id(events: &[Value]) -> String {
    events
        .iter()
        .find(|e| e["type"] == "thread.started")
        .and_then(|e| e["thread_id"].as_str())
        .expect("a thread.started event")
        .to_string()
}

#[test]
fn setup_meta_session_id_and_hook_output() {
    let Some(cli) = real_cli_support::cli("setup_meta_session_id_and_hook_output", "codex") else {
        return;
    };
    let script = |r: &Request| {
        if !r.has_tools() {
            return say("ok");
        }
        match r.turn(&["PROMPT-CX1"]) {
            Some((0, 0)) => call_in("mcp__mesh", "who", json!({})),
            _ => say("codex done"),
        }
    };
    let sb = Sandbox::new(&cli, script);
    let work = sb.work("web");
    let config_path = write_user_config(&sb);
    sb.setup("codex", &codex_env(&sb));

    // setup wrote the MCP server into config.toml and every hook into hooks.json.
    let spew = env!("CARGO_BIN_EXE_spew");
    let mut doc: toml_edit::DocumentMut = std::fs::read_to_string(&config_path)
        .unwrap()
        .parse()
        .unwrap();
    let server = &doc["mcp_servers"]["mesh"];
    assert_eq!(server["command"].as_str(), Some(spew), "{doc}");
    let args: Vec<String> = server["args"]
        .as_array()
        .expect("mcp_servers.mesh.args")
        .iter()
        .filter_map(|a| a.as_str().map(str::to_string))
        .collect();
    assert!(args.iter().any(|a| a == "mcp-server"), "{args:?}");
    let hooks = std::fs::read_to_string(sb.path("codex").join("hooks.json")).unwrap();
    for spec in &CODEX_HOOKS {
        assert!(
            hooks.contains(&format!("\"codex\",\"{}\"", spec.event))
                || hooks.contains(&format!("codex {}", spec.event)),
            "no spew hook for {}: {hooks}",
            spec.event
        );
    }

    // A tap in front of the mesh server records what Codex sends it.
    let log = sb.path("tap.jsonl");
    let mut tapped = toml_edit::Array::new();
    tapped.push(spew);
    for a in &args {
        tapped.push(a.as_str());
    }
    doc["mcp_servers"]["mesh"]["command"] =
        toml_edit::value(sb.tap(&log).to_string_lossy().as_ref());
    doc["mcp_servers"]["mesh"]["args"] = toml_edit::value(tapped);
    std::fs::write(&config_path, doc.to_string()).unwrap();

    let out = sb.run(
        &cli,
        [
            "exec",
            "--json",
            "--skip-git-repo-check",
            "--dangerously-bypass-hook-trust",
            "--disable",
            "plugins",
            "-",
        ]
        .map(Into::into)
        .to_vec(),
        &work,
        codex_env(&sb),
        "PROMPT-CX1 hello",
        LIMIT,
    );
    assert!(out.success(), "{}", out.stderr_text());
    let thread = thread_id(&events(&out.stdout_text()));

    let sent = tap_messages(&log);
    let init = sent
        .iter()
        .find(|m| m["method"] == "initialize")
        .expect("an initialize");
    assert_eq!(
        init["params"]["clientInfo"]["name"], "codex-mcp-client",
        "src/agents/mcp maps this name"
    );
    let tool_call = sent
        .iter()
        .find(|m| m["method"] == "tools/call")
        .expect("a tools/call");
    let meta = &tool_call["params"]["_meta"];
    eprintln!("tools/call _meta: {meta}");
    for key in ["sessionId", "threadId"] {
        assert_eq!(
            find_key(meta, key).and_then(Value::as_str),
            Some(thread.as_str()),
            "_meta.{key}: {meta}"
        );
    }

    // Codex accepted the SessionStart output: its context reached the model. The who result did too.
    let requests = sb.mock.requests_for("PROMPT-CX1");
    assert!(
        requests
            .iter()
            .any(|r| r.contains("spew mesh: this session is")),
        "the SessionStart context never reached the model"
    );
    assert!(
        requests
            .iter()
            .flat_map(Request::tool_results)
            .any(|t| real_cli_support::is_who(&t)),
        "the who result never reached the model"
    );
    sb.assert_stayed_local();
}

/// A `codex app-server` over stdio.
struct AppServer {
    child: real_cli_support::LineChild,
    next: u64,
}

impl AppServer {
    fn start(sb: &Sandbox, cli: &Path, cwd: &Path, stderr: &str) -> AppServer {
        let cmd = sb.command(
            cli,
            &["app-server", "--disable", "plugins"],
            cwd,
            &codex_env(sb),
        );
        let mut s = AppServer {
            child: real_cli_support::LineChild::spawn(cmd, &sb.path(stderr)),
            next: 0,
        };
        s.request(
            "initialize",
            json!({"clientInfo": {"name": "mesh-check", "title": null, "version": "0"}, "capabilities": null}),
        );
        s.child
            .send_json(&json!({"jsonrpc": "2.0", "method": "initialized"}));
        s
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        self.child
            .send_json(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let reply = self.child.until(method, LIMIT, &mut Vec::new(), |m| {
            m["id"] == id && m.get("method").is_none()
        });
        assert!(reply.get("error").is_none(), "{method}: {reply}");
        reply["result"].clone()
    }

    fn turn(&mut self, thread: &str, text: &str) {
        self.request(
            "turn/start",
            json!({"threadId": thread, "input": [{"type": "text", "text": text, "text_elements": []}]}),
        );
        let done = self
            .child
            .until("turn/completed", LIMIT, &mut Vec::new(), |m| {
                m["method"] == "turn/completed"
            });
        let status = find_key(&done["params"], "status").cloned();
        assert_ne!(status, Some(json!("failed")), "{done}");
    }
}

#[test]
fn peer_message_at_the_next_prompt() {
    let Some(cli) = real_cli_support::cli("peer_message_at_the_next_prompt", "codex") else {
        return;
    };
    let sb = Sandbox::new(&cli, |r: &Request| {
        match r.turn(&["PROMPT-CX1", "PROMPT-CX2"]) {
            Some((0, _)) => say("first"),
            Some((1, _)) => say("second"),
            _ => say("ok"),
        }
    });
    let work = sb.work("web");
    let config_path = write_user_config(&sb);
    sb.setup("codex", &codex_env(&sb));

    // Trust the hooks the way Codex's /hooks screen does, so they run outside `exec`.
    {
        let mut app = AppServer::start(&sb, &cli, &work, "app-trust.stderr");
        let listed = app.request("hooks/list", json!({"cwds": [work]}));
        let mut trust = String::new();
        for hook in listed["data"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|d| d["hooks"].as_array().cloned().unwrap_or_default())
        {
            if let (Some(key), Some(hash)) = (hook["key"].as_str(), hook["currentHash"].as_str()) {
                trust.push_str(&format!(
                    "\n[hooks.state.{key:?}]\ntrusted_hash = {hash:?}\n"
                ));
            }
        }
        assert!(!trust.is_empty(), "hooks/list named no hooks: {listed}");
        let mut config = std::fs::read_to_string(&config_path).unwrap();
        config.push_str(&trust);
        std::fs::write(&config_path, config).unwrap();
    }

    let mut app = AppServer::start(&sb, &cli, &work, "app.stderr");
    let thread = app.request("thread/start", json!({"cwd": work}))["thread"]["id"]
        .as_str()
        .expect("a thread id")
        .to_string();
    app.turn(&thread, "PROMPT-CX1 hello");
    let rows = sb.wait_for_sessions("a hooked Codex session", |rows| {
        rows.iter()
            .any(|r| r.client == "codex" && r.reach == Reach::Hooked)
    });
    let me = rows.iter().find(|r| r.client == "codex").unwrap();
    assert_eq!(me.client_session.as_deref(), Some(thread.as_str()));

    let note = token("PEER-NOTE");
    sb.tell(&me.name, &note);
    app.turn(&thread, "PROMPT-CX2 again");
    assert!(
        sb.mock
            .requests_for("PROMPT-CX2")
            .iter()
            .any(|r| r.contains(&note)),
        "the peer message never reached the model"
    );
    assert!(
        !sb.mock
            .requests()
            .iter()
            .filter(|r| r.turn(&["PROMPT-CX1", "PROMPT-CX2"]).map(|t| t.0) == Some(0))
            .any(|r| r.contains(&note)),
        "the peer message showed up before the next prompt"
    );
    drop(app);
    sb.assert_stayed_local();
}

#[test]
fn serve_profile_canaries_and_resume_dash() {
    let Some(cli) = real_cli_support::cli("serve_profile_canaries_and_resume_dash", "codex") else {
        return;
    };
    let steps: Arc<Mutex<Vec<Reply>>> = Arc::default();
    let shared = steps.clone();
    let sb = Sandbox::new(&cli, move |r: &Request| {
        let steps = shared.lock().unwrap();
        match r.turn(&["PROMPT-CXS1", "PROMPT-CXS2"]) {
            Some((0, n)) if n < steps.len() => steps[n].clone(),
            Some((1, _)) => say("second answer"),
            _ => say("ok"),
        }
    });
    let serve = std::fs::canonicalize(sb.work("serve")).unwrap();
    let outside = sb.path("mesh").join("run").join("serve");
    std::fs::create_dir_all(&outside).unwrap();
    let outside = std::fs::canonicalize(outside).unwrap();

    let control = token("CONTROL");
    let w = |rel: &str, text: &str| {
        let p = serve.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, format!("{text}\n")).unwrap();
    };
    w("notes.txt", &format!("notes {control}"));
    let secret_files = [
        ".env",
        "sub/.env.production",
        "sub/prod.env",
        "sub/server.pem",
        "sub/deploy.key",
        "sub/id_ed25519",
        ".git/config",
        "a/b/c/d/.env",
        "a/b/c/d/deploy.key",
    ];
    let secrets: Vec<String> = secret_files
        .iter()
        .map(|f| {
            let s = token("SECRET");
            w(f, &format!("value {s}"));
            s
        })
        .collect();
    let outside_secret = token("OUTSIDE");
    std::fs::write(sb.path("work").join("outside.txt"), &outside_secret).unwrap();

    let shell = |cmd: &str| call("exec_command", json!({"cmd": cmd}));
    *steps.lock().unwrap() = vec![
        shell("cat notes.txt"),
        shell("cat .env"),
        shell("cat sub/.env.production sub/prod.env sub/server.pem sub/deploy.key sub/id_ed25519"),
        shell("cat .git/config"),
        shell("cat ../outside.txt"),
        shell("grep -r SECRET . ; ls -la sub"),
        say("first answer"),
    ];
    let n_steps = steps.lock().unwrap().len();

    let real = std::fs::canonicalize(&cli).unwrap();
    let dir = codex::codex_dir(&real, &sb.path("codex"));
    eprintln!("codex dir: {}", dir.display());
    let profile = codex::profile(&dir, false).unwrap();
    let env = codex_env(&sb);

    // The startup self-check passes through the real `codex sandbox`.
    real_cli_support::block_on(codex::canary_check(
        &SystemRunner,
        &codex::Canary {
            program: &cli,
            cwd: &serve,
            profile: &profile,
            env: &env,
            outside_dir: &outside,
            timeout: Duration::from_secs(60),
        },
    ))
    .unwrap_or_else(|e| panic!("canary check: {e}"));

    let user_config = write_user_config(&sb);
    let model = codex::read_user_config(&user_config, None).unwrap();
    let run = |resume: Option<(&str, codex::ResumeStyle)>, file: &str, prompt: &str| {
        let run_file = outside.join(file);
        let out = sb.run(
            &cli,
            codex::exec_args(&codex::ExecArgs {
                cwd: &serve,
                profile: &profile,
                model: &model,
                run_file: &run_file,
                resume,
            }),
            &serve,
            env.clone(),
            prompt,
            LIMIT,
        );
        let answer = std::fs::read_to_string(&run_file).ok();
        let parsed = codex::parse(&out, answer.as_deref());
        (out, parsed)
    };

    let (out, parsed) = run(None, "r1.txt", "PROMPT-CXS1 what is here?");
    let first = parsed.unwrap_or_else(|f| panic!("{f:?}\n{}", out.stderr_text()));
    assert_eq!(first.answer.trim(), "first answer");
    let thread = first.session_id.clone().expect("a thread id");

    let requests = sb.mock.requests_for("PROMPT-CXS1");
    let results = requests.last().unwrap().tool_results();
    assert_eq!(results.len(), n_steps - 1, "{results:#?}");
    assert!(
        results[0].contains(&control),
        "cat notes.txt: {}",
        results[0]
    );
    for secret in secrets.iter().chain([&outside_secret]) {
        assert!(
            !requests.iter().any(|r| r.contains(secret)),
            "a secret reached the model: {results:#?}"
        );
    }
    eprintln!("cat .env answered: {}", results[1].replace('\n', " "));
    eprintln!("ls sub answered: {}", results[5].replace('\n', " "));

    // A follow-up with `resume <id> -` and the prompt on stdin continues the same thread.
    let (out, parsed) = run(
        Some((&thread, codex::ResumeStyle::Dash)),
        "r2.txt",
        "PROMPT-CXS2 and then?",
    );
    assert!(
        !codex::rejected_resume_dash(&out),
        "Codex rejected `resume <id> -`: {}",
        out.stderr_text()
    );
    let second = parsed.unwrap_or_else(|f| panic!("{f:?}\n{}", out.stderr_text()));
    assert_eq!(second.answer.trim(), "second answer");
    assert_eq!(second.session_id.as_deref(), Some(thread.as_str()));
    assert!(
        sb.mock
            .requests_for("PROMPT-CXS2")
            .iter()
            .any(|r| r.contains("PROMPT-CXS1")),
        "the follow-up didn't carry the first question"
    );
    sb.assert_stayed_local();
}
