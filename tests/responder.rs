//! serve with fake `claude`, `codex` and `opencode` binaries (15): exact command lines, the environment
//! scrub, the prompt on stdin, conversation follow-ups, failed runs, the timeout killing the process group,
//! and the Codex sandbox self-check.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use spew_mesh::responder::codex::{self, ExecArgs, ModelChoice, Provider};
use spew_mesh::responder::env::HostEnv;
use spew_mesh::responder::limits::ManualClock;
use spew_mesh::responder::memory::{ConversationKey, ConversationStore, MemoryConversations};
use spew_mesh::responder::process::SystemRunner;
use spew_mesh::responder::prompt::{self, PromptContext};
use spew_mesh::responder::{
    Asker, MessageKind, Question, Responder, ServeClient, ServeConfig, ServeError, ServeSettings,
    claude, opencode,
};

/// Records argv, env and stdin of every call as `<rec>/argv.N`, `env.N`, `stdin.N`, then plays back
/// `stdout.N`, `stderr.N` and `exit.N`. `@SESSION@` in stdout becomes the Claude session id from argv.
/// `codex sandbox` cats its file only inside `-C` (or always with FAKE_SANDBOX_LEAK set), and `-o` gets
/// `runfile.N`. With `sleep.N` the call starts a background sleeper and sleeps.
const FAKE: &str = r#"#!/bin/sh
rec="$FAKE_REC"
n=$(cat "$rec/count" 2>/dev/null || echo 0)
n=$((n+1))
echo "$n" > "$rec/count"
for a in "$@"; do printf '%s\n' "$a"; done > "$rec/argv.$n"
env > "$rec/env.$n"
cat > "$rec/stdin.$n"
if [ "$1" = "sandbox" ]; then
  cwd=""; file=""; prev=""
  for a in "$@"; do
    if [ "$prev" = "-C" ]; then cwd="$a"; fi
    file="$a"; prev="$a"
  done
  if [ -n "$FAKE_SANDBOX_LEAK" ]; then cat "$file"; exit 0; fi
  case "$file" in
    "$cwd"/*) cat "$file"; exit 0;;
    *) echo "cat: $file: No such file or directory" >&2; exit 1;;
  esac
fi
sid=""; prev=""
for a in "$@"; do
  case "$prev" in
    --session-id|--resume) sid="$a";;
    -o) [ -f "$rec/runfile.$n" ] && cp "$rec/runfile.$n" "$a";;
  esac
  prev="$a"
done
if [ -f "$rec/sleep.$n" ]; then sleep 30 & echo $! > "$rec/bg.$n"; sleep 30; fi
[ -f "$rec/stderr.$n" ] && cat "$rec/stderr.$n" >&2
[ -f "$rec/stdout.$n" ] && sed "s/@SESSION@/$sid/g" "$rec/stdout.$n"
code=0
[ -f "$rec/exit.$n" ] && code=$(cat "$rec/exit.$n")
exit $code
"#;

struct Fake {
    _tmp: tempfile::TempDir,
    bin: PathBuf,
    rec: PathBuf,
    home: PathBuf,
    codex_home: PathBuf,
    cwd: PathBuf,
    run_dir: PathBuf,
    extra_env: Vec<(String, String)>,
}

impl Fake {
    fn new(program: &str) -> Fake {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let bin = root.join("bin");
        let rec = root.join("rec");
        let home = root.join("home");
        let codex_home = home.join(".codex");
        let cwd = root.join("work").join("api");
        let run_dir = home.join(".spew/mesh/run/serve");
        for d in [&bin, &rec, &home, &codex_home, &cwd] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::create_dir(cwd.join(".git")).unwrap();
        let exe = bin.join(program);
        std::fs::write(&exe, FAKE).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        Fake {
            _tmp: tmp,
            bin,
            rec,
            home,
            codex_home,
            cwd,
            run_dir,
            extra_env: vec![],
        }
    }

    fn host(&self) -> HostEnv {
        let mut vars: Vec<(String, String)> = vec![
            (
                "PATH".into(),
                format!("{}:/usr/bin:/bin", self.bin.display()),
            ),
            ("HOME".into(), self.home.display().to_string()),
            ("CODEX_HOME".into(), self.codex_home.display().to_string()),
            ("FAKE_REC".into(), self.rec.display().to_string()),
            (
                "SPEW_MESH_DIR".into(),
                self.home.join(".spew/mesh").display().to_string(),
            ),
            ("SPEW_MESH_LAN".into(), "0".into()),
        ];
        vars.extend(self.extra_env.iter().cloned());
        HostEnv::new(vars)
    }

    fn config(&self, client: ServeClient) -> ServeConfig {
        ServeConfig::new(client, &self.cwd, &self.run_dir, &ServeSettings::default())
    }

    fn set(&self, name: &str, n: usize, content: &str) {
        std::fs::write(self.rec.join(format!("{name}.{n}")), content).unwrap();
    }

    fn get(&self, name: &str, n: usize) -> String {
        std::fs::read_to_string(self.rec.join(format!("{name}.{n}")))
            .unwrap_or_else(|e| panic!("{name}.{n}: {e}"))
    }

    fn argv(&self, n: usize) -> Vec<String> {
        self.get("argv", n).lines().map(str::to_string).collect()
    }

    fn env(&self, n: usize) -> BTreeMap<String, String> {
        self.get("env", n)
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn calls(&self) -> usize {
        std::fs::read_to_string(self.rec.join("count"))
            .map(|s| s.trim().parse().unwrap())
            .unwrap_or(0)
    }

    async fn start(
        &self,
        cfg: ServeConfig,
    ) -> Result<Responder<SystemRunner, MemoryConversations, ManualClock>, ServeError> {
        Responder::start(
            cfg,
            &self.host(),
            SystemRunner,
            MemoryConversations::new(),
            ManualClock::new(1_000),
        )
        .await
    }
}

fn question(id: &str, conversation: &str, body: &str) -> Question {
    Question {
        id: id.into(),
        conversation: conversation.into(),
        kind: MessageKind::Ask,
        from: Asker {
            person: "maya".into(),
            device: "maya-mbp".into(),
            device_id: "maya-endpoint".into(),
            session: "01J9ZK0000000000000000000A".into(),
            name: "claude-api".into(),
            client: "claude".into(),
            own: false,
            room: Some("spew".into()),
        },
        repo: Some("acme/api".into()),
        card: Some("spew#42".into()),
        body: body.into(),
    }
}

const Q1: &str = "01J9ZK3M5N7P9Q1R3S5T7V9W10";
const Q2: &str = "01J9ZK3M5N7P9Q1R3S5T7V9W11";
const Q3: &str = "01J9ZK3M5N7P9Q1R3S5T7V9W12";
const C1: &str = "01J9ZJ3M5N7P9Q1R3S5T7V9W10";
const C2: &str = "01J9ZJ3M5N7P9Q1R3S5T7V9W11";

fn claude_ok(answer: &str, cost: f64) -> String {
    format!(
        r#"{{"type":"result","subtype":"success","is_error":false,"result":"{answer}","session_id":"@SESSION@","total_cost_usd":{cost}}}"#
    )
}

fn claude_opts() -> claude::Options<'static> {
    claude::Options {
        allow_edits: false,
        grep: true,
        max_turns: 20,
        max_budget_usd: 0.5,
        model: None,
    }
}

#[tokio::test]
async fn claude_runs_with_exact_flags_scrubbed_env_and_follow_ups() {
    let mut fake = Fake::new("claude");
    fake.extra_env = [
        "CLAUDECODE",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDE_CODE_MESSAGING_TOKEN",
        "CLAUDE_PID",
        "CLAUDE_PROJECT_DIR",
        "CLAUDE_PLUGIN_ROOT",
        "CLAUDE_PLUGIN_DATA",
    ]
    .iter()
    .map(|k| (k.to_string(), "parent".to_string()))
    .chain([
        ("CLAUDE_CODE_OAUTH_TOKEN".to_string(), "kept".to_string()),
        ("CLAUDE_CODE_USE_BEDROCK".to_string(), "1".to_string()),
    ])
    .collect();
    fake.set("stdout", 1, &claude_ok("It retries three times.", 0.0123));
    fake.set("stdout", 2, &claude_ok("Backoff doubles.", 0.03));
    fake.set("stdout", 3, &claude_ok("Fresh start.", 0.01));

    let mut r = fake.start(fake.config(ServeClient::Claude)).await.unwrap();
    assert_eq!(fake.calls(), 0, "Claude Code needs no startup check");

    let q1 = question(Q1, C1, "How does retry() work?");
    let a1 = r.answer(&q1).await.unwrap();
    assert_eq!(a1.text, "It retries three times.");
    assert_eq!(a1.cost_usd, Some(0.0123));

    let argv = fake.argv(1);
    let session = argv[4].clone();
    assert!(claude::is_uuid(&session), "{session}");
    assert_eq!(
        argv,
        claude::args(claude::Session::New(&session), &claude_opts())
    );

    let env = fake.env(1);
    for gone in [
        "CLAUDECODE",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDE_CODE_MESSAGING_TOKEN",
        "CLAUDE_PID",
        "CLAUDE_PROJECT_DIR",
        "CLAUDE_PLUGIN_ROOT",
        "CLAUDE_PLUGIN_DATA",
    ] {
        assert!(!env.contains_key(gone), "{gone} reached the run");
    }
    assert_eq!(
        env.get("CLAUDE_CODE_OAUTH_TOKEN").map(String::as_str),
        Some("kept")
    );
    assert_eq!(
        env.get("CLAUDE_CODE_USE_BEDROCK").map(String::as_str),
        Some("1")
    );
    assert!(env.contains_key("FAKE_REC"));

    let expected_prompt = prompt::build(
        &q1,
        &PromptContext {
            allow_edits: false,
            place: "api",
        },
    );
    assert_eq!(fake.get("stdin", 1), expected_prompt);
    assert!(expected_prompt.contains(&format!("[mesh {Q1} | ask | from maya/claude-api")));

    // A follow-up in the same conversation resumes the same session with every flag repeated.
    let a2 = r
        .answer(&question(Q2, C1, "And the backoff?"))
        .await
        .unwrap();
    assert_eq!(a2.text, "Backoff doubles.");
    assert_eq!(
        fake.argv(2),
        claude::args(claude::Session::Resume(&session), &claude_opts())
    );

    // A new conversation starts a new session.
    r.answer(&question(Q3, C2, "Unrelated")).await.unwrap();
    let argv3 = fake.argv(3);
    assert_eq!(argv3[3], "--session-id");
    assert_ne!(argv3[4], session);
    assert_eq!(r.conversations().len(), 2);
}

#[tokio::test]
async fn claude_allow_edits_and_model() {
    let fake = Fake::new("claude");
    fake.set("stdout", 1, &claude_ok("done", 0.1));
    let mut cfg = fake.config(ServeClient::Claude);
    cfg.allow_edits = true;
    cfg.model = Some("claude-sonnet-5".into());
    let mut r = fake.start(cfg).await.unwrap();
    r.answer(&question(Q1, C1, "Fix the typo")).await.unwrap();
    let argv = fake.argv(1);
    assert_eq!(
        argv,
        claude::args(
            claude::Session::New(&argv[4]),
            &claude::Options {
                allow_edits: true,
                model: Some("claude-sonnet-5"),
                ..claude_opts()
            }
        )
    );
    assert!(argv.contains(&"Read,Grep,Glob,Edit,Write".to_string()));
    assert!(argv.contains(&"acceptEdits".to_string()));
    assert!(
        fake.get("stdin", 1)
            .contains("may read, search and edit files in api")
    );
}

#[tokio::test]
async fn claude_failed_runs_answer_with_one_line() {
    let fake = Fake::new("claude");
    fake.set(
        "stdout",
        1,
        r#"{"type":"result","subtype":"error_max_turns","is_error":true,"session_id":"@SESSION@","total_cost_usd":0.4}"#,
    );
    fake.set("exit", 1, "1");
    fake.set("stderr", 2, "Error: something broke\n");
    fake.set("exit", 2, "1");
    let mut r = fake.start(fake.config(ServeClient::Claude)).await.unwrap();

    let f = r.answer(&question(Q1, C1, "q")).await.unwrap_err();
    assert_eq!(
        f.reply_text(),
        "The serve run failed: it reached its turn limit (max_turns)."
    );
    assert_eq!(f.cost_usd, Some(0.4));

    let f = r.answer(&question(Q2, C2, "q")).await.unwrap_err();
    assert_eq!(
        f.reply_text(),
        "The serve run failed: claude exited with status 1."
    );
}

#[tokio::test]
async fn claude_forgets_a_session_it_no_longer_has() {
    let fake = Fake::new("claude");
    fake.set("stdout", 1, &claude_ok("one", 0.0));
    fake.set("stderr", 2, "No conversation found with session ID: x\n");
    fake.set("exit", 2, "1");
    fake.set("stdout", 3, &claude_ok("three", 0.0));
    let mut r = fake.start(fake.config(ServeClient::Claude)).await.unwrap();
    let q = question(Q1, C1, "q");
    r.answer(&q).await.unwrap();
    assert!(r.answer(&question(Q2, C1, "q2")).await.is_err());
    assert_eq!(fake.argv(2)[3], "--resume");
    assert!(
        r.conversations()
            .get(&ConversationKey::of(&q))
            .unwrap()
            .is_none()
    );
    r.answer(&question(Q3, C1, "q3")).await.unwrap();
    assert_eq!(fake.argv(3)[3], "--session-id");
}

#[tokio::test]
async fn timeout_kills_the_run_and_its_children() {
    let fake = Fake::new("claude");
    fake.set("sleep", 1, "");
    let mut cfg = fake.config(ServeClient::Claude);
    cfg.timeout = Duration::from_secs(1);
    let mut r = fake.start(cfg).await.unwrap();
    let started = std::time::Instant::now();
    let f = r.answer(&question(Q1, C1, "q")).await.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(15));
    assert_eq!(
        f.reply_text(),
        "The serve run failed: it ran longer than 1 second and was stopped."
    );
    let bg: i32 = fake.get("bg", 1).trim().parse().unwrap();
    assert!(gone(bg).await, "the run's background child {bg} survived");
    assert_eq!(r.conversations().len(), 0);
}

#[tokio::test]
async fn missing_program_refuses_to_start() {
    let fake = Fake::new("claude");
    let err = fake
        .start(fake.config(ServeClient::Opencode))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(err, ServeError::ProgramNotFound("opencode")),
        "{err}"
    );
}

const THREAD: &str = "0199a3e1-0000-7000-8000-000000000001";
const THREAD2: &str = "0199a3e1-0000-7000-8000-000000000002";

fn codex_events(thread: &str, message: &str) -> String {
    format!(
        "{}\n{}\n{}\n{}\n",
        format_args!(r#"{{"type":"thread.started","thread_id":"{thread}"}}"#),
        r#"{"type":"turn.started"}"#,
        format_args!(
            r#"{{"type":"item.completed","item":{{"id":"item_0","type":"agent_message","text":"{message}"}}}}"#
        ),
        r#"{"type":"turn.completed","usage":{"input_tokens":1,"output_tokens":1}}"#,
    )
}

fn strings(v: &[std::ffi::OsString]) -> Vec<String> {
    v.iter().map(|s| s.to_string_lossy().into_owned()).collect()
}

#[tokio::test]
async fn codex_checks_the_sandbox_then_runs_and_resumes() {
    let mut fake = Fake::new("codex");
    fake.extra_env = vec![
        ("CODEX_THREAD_ID".into(), "parent".into()),
        ("CODEX_SESSION_ID".into(), "parent".into()),
        ("CODEX_TOOL_CALL_ID".into(), "call_1".into()),
        ("OPENAI_API_KEY".into(), "kept".into()),
    ];
    std::fs::write(
        fake.codex_home.join("config.toml"),
        "model = \"gpt-6\"\nmodel_provider = \"acme\"\n\n[model_providers.acme]\nname = \"Acme\"\n\
         base_url = \"https://llm.example.com/v1\"\nenv_key = \"ACME_API_KEY\"\nwire_api = \"responses\"\n",
    )
    .unwrap();
    // Calls 1 and 2 are the sandbox self-check.
    fake.set("runfile", 3, "The answer, from the run file.\n");
    fake.set("stdout", 3, &codex_events(THREAD, "last message"));
    fake.set("stdout", 4, &codex_events(THREAD, "Follow-up answer."));

    let mut r = fake.start(fake.config(ServeClient::Codex)).await.unwrap();
    assert_eq!(fake.calls(), 2);

    let profile = codex::profile(&fake.bin, false).unwrap();
    let inside = fake.argv(1);
    assert_eq!(
        inside[..inside.len() - 1],
        strings(&codex::sandbox_args(&fake.cwd, &profile, Path::new("x")))[..9]
    );
    assert!(Path::new(inside.last().unwrap()).starts_with(&fake.cwd));
    let outside = fake.argv(2);
    assert!(Path::new(outside.last().unwrap()).starts_with(&fake.run_dir));
    // The probe files are gone.
    assert_eq!(
        std::fs::read_dir(&fake.cwd).unwrap().count(),
        1,
        "only .git is left"
    );

    let q1 = question(Q1, C1, "Where is the config loaded?");
    let a = r.answer(&q1).await.unwrap();
    assert_eq!(a.text, "The answer, from the run file.");
    assert_eq!(a.cost_usd, None);

    let argv = fake.argv(3);
    let run_file = PathBuf::from(&argv[argv.len() - 2]);
    assert!(run_file.starts_with(&fake.run_dir));
    assert!(!run_file.exists(), "the run file is deleted after reading");
    let model = ModelChoice {
        model: Some("gpt-6".into()),
        provider: Some(Provider {
            id: "acme".into(),
            table: Some(
                "{ name = \"Acme\", base_url = \"https://llm.example.com/v1\", env_key = \"ACME_API_KEY\", \
                 wire_api = \"responses\" }"
                    .into(),
            ),
        }),
    };
    assert_eq!(
        argv,
        strings(&codex::exec_args(&ExecArgs {
            cwd: &fake.cwd,
            profile: &profile,
            model: &model,
            run_file: &run_file,
            resume: None,
        }))
    );
    assert_eq!(argv.last().map(String::as_str), Some("-"));

    let env = fake.env(3);
    for gone in ["CODEX_THREAD_ID", "CODEX_SESSION_ID", "CODEX_TOOL_CALL_ID"] {
        assert!(!env.contains_key(gone), "{gone} reached the run");
    }
    assert_eq!(env.get("OPENAI_API_KEY").map(String::as_str), Some("kept"));
    assert!(fake.get("stdin", 3).contains("Where is the config loaded?"));

    // The follow-up resumes the thread; every flag comes before `resume`.
    let a = r
        .answer(&question(Q2, C1, "And the defaults?"))
        .await
        .unwrap();
    assert_eq!(a.text, "Follow-up answer.");
    let argv = fake.argv(4);
    let n = argv.len();
    assert_eq!(argv[n - 3..], ["resume", THREAD, "-"]);
    assert_eq!(argv[n - 5], "-o");
    assert!(fake.get("stdin", 4).contains("And the defaults?"));
}

#[tokio::test]
async fn codex_falls_back_when_resume_rejects_the_dash() {
    let fake = Fake::new("codex");
    fake.set("stdout", 3, &codex_events(THREAD, "first"));
    fake.set("stderr", 4, "error: unexpected argument '-' found\n");
    fake.set("exit", 4, "2");
    fake.set("stdout", 5, &codex_events(THREAD, "second"));
    fake.set("stdout", 6, &codex_events(THREAD, "third"));
    let mut r = fake.start(fake.config(ServeClient::Codex)).await.unwrap();
    r.answer(&question(Q1, C1, "one")).await.unwrap();

    let a = r.answer(&question(Q2, C1, "two")).await.unwrap();
    assert_eq!(a.text, "second");
    let n = fake.argv(4).len();
    assert_eq!(fake.argv(4)[n - 3..], ["resume", THREAD, "-"]);
    assert_eq!(
        fake.argv(5)[n - 3..],
        ["resume", THREAD, codex::RESUME_PROMPT]
    );
    assert!(fake.get("stdin", 5).contains("two"));

    // The fallback sticks.
    r.answer(&question(Q3, C1, "three")).await.unwrap();
    assert_eq!(
        fake.argv(6).last().map(String::as_str),
        Some(codex::RESUME_PROMPT)
    );
    assert_eq!(fake.calls(), 6);
}

#[tokio::test]
async fn codex_new_thread_replaces_the_mapping() {
    let fake = Fake::new("codex");
    fake.set("stdout", 3, &codex_events(THREAD, "first"));
    fake.set(
        "stdout",
        4,
        &codex_events(THREAD2, "second, in a new thread"),
    );
    fake.set("stdout", 5, &codex_events(THREAD2, "third"));
    let mut r = fake.start(fake.config(ServeClient::Codex)).await.unwrap();
    let q = question(Q1, C1, "one");
    r.answer(&q).await.unwrap();
    r.answer(&question(Q2, C1, "two")).await.unwrap();
    let stored = r
        .conversations()
        .get(&ConversationKey::of(&q))
        .unwrap()
        .unwrap();
    assert_eq!(stored.session_id, THREAD2);
    r.answer(&question(Q3, C1, "three")).await.unwrap();
    let argv = fake.argv(5);
    assert_eq!(argv[argv.len() - 2], THREAD2);
}

#[tokio::test]
async fn codex_failed_runs() {
    let fake = Fake::new("codex");
    fake.set(
        "stdout",
        3,
        &format!(
            "{}{}\n",
            codex_events(THREAD, "partial"),
            r#"{"type":"turn.failed","error":{"message":"stream disconnected"}}"#
        ),
    );
    fake.set("exit", 3, "1");
    fake.set("exit", 4, "1");
    let mut r = fake.start(fake.config(ServeClient::Codex)).await.unwrap();
    let f = r.answer(&question(Q1, C1, "one")).await.unwrap_err();
    assert_eq!(
        f.reply_text(),
        "The serve run failed: the Codex turn failed: stream disconnected."
    );
    let f = r.answer(&question(Q2, C2, "two")).await.unwrap_err();
    assert_eq!(
        f.reply_text(),
        "The serve run failed: codex exited with status 1."
    );
    assert_eq!(
        std::fs::read_dir(&fake.run_dir).unwrap().count(),
        0,
        "no run files are left behind"
    );
}

#[tokio::test]
async fn codex_refuses_when_the_sandbox_leaks() {
    let mut fake = Fake::new("codex");
    fake.extra_env = vec![("FAKE_SANDBOX_LEAK".into(), "1".into())];
    let err = fake
        .start(fake.config(ServeClient::Codex))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, ServeError::Canary(_)), "{err}");
    assert_eq!(fake.calls(), 2);
}

#[tokio::test]
async fn codex_refuses_a_weakening_project_config() {
    let fake = Fake::new("codex");
    std::fs::create_dir(fake.cwd.join(".codex")).unwrap();
    std::fs::write(
        fake.cwd.join(".codex/config.toml"),
        "sandbox_mode = \"danger-full-access\"\n",
    )
    .unwrap();
    let err = fake
        .start(fake.config(ServeClient::Codex))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(&err, ServeError::ProjectConfig { key, .. } if key == "sandbox_mode"),
        "{err}"
    );
    assert_eq!(fake.calls(), 0, "nothing runs before the config checks");
}

#[tokio::test]
async fn codex_refuses_a_provider_with_inline_headers() {
    let fake = Fake::new("codex");
    std::fs::write(
        fake.codex_home.join("config.toml"),
        "model_provider = \"acme\"\n[model_providers.acme]\nbase_url = \"https://llm.example.com\"\n\
         http_headers = { \"X-Key\" = \"secret\" }\n",
    )
    .unwrap();
    let err = fake
        .start(fake.config(ServeClient::Codex))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, ServeError::ProviderSecret { .. }), "{err}");
    assert_eq!(fake.calls(), 0);
}

#[tokio::test]
async fn codex_allow_edits_makes_the_workspace_writable() {
    let fake = Fake::new("codex");
    fake.set("stdout", 3, &codex_events(THREAD, "edited"));
    let mut cfg = fake.config(ServeClient::Codex);
    cfg.allow_edits = true;
    let mut r = fake.start(cfg).await.unwrap();
    let write_profile = codex::profile(&fake.bin, true).unwrap();
    assert_eq!(
        fake.argv(1)[4],
        format!("permissions.spew-mesh-serve={write_profile}")
    );
    r.answer(&question(Q1, C1, "edit")).await.unwrap();
    assert!(
        fake.argv(3)
            .contains(&format!("permissions.spew-mesh-serve={write_profile}"))
    );
}

fn opencode_events(session: &str, text: &str) -> String {
    format!(
        "{}\n{}\n{}\n",
        format_args!(
            r#"{{"type":"step_start","sessionID":"{session}","part":{{"type":"step-start"}}}}"#
        ),
        format_args!(
            r#"{{"type":"tool_use","sessionID":"{session}","part":{{"type":"tool","tool":"read"}}}}"#
        ),
        format_args!(
            r#"{{"type":"text","sessionID":"{session}","part":{{"type":"text","text":"{text}"}}}}"#
        ),
    )
}

#[tokio::test]
async fn opencode_runs_with_its_config_and_continues_the_session() {
    let mut fake = Fake::new("opencode");
    fake.extra_env = vec![
        (
            "OPENCODE_CONFIG_CONTENT".into(),
            "{\"permission\":\"allow\"}".into(),
        ),
        ("OPENCODE_DISABLE_PROJECT_CONFIG".into(), "0".into()),
    ];
    fake.set(
        "stdout",
        1,
        &opencode_events("ses_01ABC", "It's in api/x.ts."),
    );
    fake.set("stdout", 2, &opencode_events("ses_01ABC", "Line 40."));
    let mut cfg = fake.config(ServeClient::Opencode);
    cfg.model = Some("acme/m1".into());
    let mut r = fake.start(cfg).await.unwrap();

    let a = r.answer(&question(Q1, C1, "Where?")).await.unwrap();
    assert_eq!(a.text, "It's in api/x.ts.");
    assert_eq!(
        fake.argv(1),
        strings(&opencode::args(
            None,
            Some("acme/m1"),
            "maya/claude-api",
            &fake.cwd
        ))
    );
    let env = fake.env(1);
    assert_eq!(
        env.get("OPENCODE_CONFIG_CONTENT"),
        Some(&opencode::config_content(false))
    );
    assert_eq!(
        env.get("OPENCODE_DISABLE_PROJECT_CONFIG")
            .map(String::as_str),
        Some("1")
    );
    assert!(fake.get("stdin", 1).contains("Where?"));

    r.answer(&question(Q2, C1, "Which line?")).await.unwrap();
    assert_eq!(
        fake.argv(2),
        strings(&opencode::args(
            Some("ses_01ABC"),
            Some("acme/m1"),
            "maya/claude-api",
            &fake.cwd
        ))
    );
}

#[tokio::test]
async fn opencode_failed_runs() {
    let fake = Fake::new("opencode");
    fake.set(
        "stdout",
        1,
        &opencode_events("ses_01ABC", "fallback answer"),
    );
    fake.set(
        "stderr",
        1,
        "! agent \"spew-mesh-serve\" not found. Falling back to default agent\n",
    );
    fake.set(
        "stdout",
        2,
        r#"{"type":"error","sessionID":"ses_02","error":{"name":"ProviderAuthError","data":{"message":"no credentials"}}}"#,
    );
    fake.set("exit", 3, "1");
    let mut cfg = fake.config(ServeClient::Opencode);
    cfg.allow_edits = true;
    let mut r = fake.start(cfg).await.unwrap();
    let f = r.answer(&question(Q1, C1, "q")).await.unwrap_err();
    assert!(
        f.reply_text().contains("fell back to its default agent"),
        "{}",
        f.reply_text()
    );
    let f = r.answer(&question(Q2, C2, "q")).await.unwrap_err();
    assert_eq!(
        f.reply_text(),
        "The serve run failed: OpenCode reported an error: no credentials."
    );
    let f = r.answer(&question(Q3, C2, "q")).await.unwrap_err();
    assert_eq!(
        f.reply_text(),
        "The serve run failed: opencode exited with status 1."
    );
    assert_eq!(
        fake.env(1).get("OPENCODE_CONFIG_CONTENT"),
        Some(&opencode::config_content(true))
    );
}

/// True once the process is gone (or a zombie nobody has reaped yet).
async fn gone(pid: i32) -> bool {
    for _ in 0..50 {
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if stat.is_empty() || stat.starts_with('Z') {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}
