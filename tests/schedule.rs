//! Scheduled runs (spew#237) against a mock spew: capabilities, due runs, start (with 409), finish, and the runs
//! that are skipped (past the deadline, agent missing, no folder, agent busy).

mod spew_api_mock;

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use spew_api_mock::{MockSpew, TestClock, sign_in_with};
use spew_mesh::paths::Home;
use spew_mesh::responder::env::HostEnv;
use spew_mesh::responder::process::{Exit, ProcessRunner, RunOutput, RunSpec};
use spew_mesh::schedule::detect::Detect;
use spew_mesh::schedule::{AgentCapability, AgentKind, ModelOption, Scheduler, SchedulerConfig};
use spew_mesh::store::Store;

const T0: u64 = 1_800_000_000_000;
const HOUR: u64 = 3_600_000;

#[derive(Clone, Default)]
struct FakeRunner {
    specs: Arc<Mutex<Vec<RunSpec>>>,
}

impl ProcessRunner for FakeRunner {
    fn run(&self, spec: RunSpec) -> impl Future<Output = io::Result<RunOutput>> + Send {
        self.specs.lock().unwrap().push(spec.clone());
        async move {
            let name = spec
                .program
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let (exit, stdout) = match name.as_str() {
                "claude" => (
                    Exit::Code(0),
                    r#"{"type":"result","is_error":false,"result":"Moved spew#1 to review."}"#
                        .to_string(),
                ),
                _ => (Exit::Code(1), "tests failed\n".to_string()),
            };
            Ok(RunOutput {
                exit,
                stdout: stdout.into_bytes(),
                stderr: Vec::new(),
                elapsed: Duration::from_millis(5),
            })
        }
    }
}

struct FixedDetect(Vec<AgentCapability>);

impl Detect for FixedDetect {
    fn detect(&self) -> impl Future<Output = Vec<AgentCapability>> + Send {
        let agents = self.0.clone();
        async move { agents }
    }
}

fn agent(kind: AgentKind, model: &str) -> AgentCapability {
    AgentCapability {
        kind,
        version: "1.0.0".into(),
        plan: Some("Pro".into()),
        models: vec![ModelOption {
            id: model.into(),
            label: model.into(),
            efforts: vec!["low".into(), "high".into()],
            default_effort: Some("high".into()),
        }],
        usage: None,
    }
}

fn due(id: &str, project: &str, agent: &str, deadline: u64) -> Value {
    json!({
        "id": id, "project": project, "issue": format!("{project}#{id}"), "title": format!("card {id}"),
        "agent": agent, "model": if agent == "codex" { "gpt-6.1-sol" } else { "opus" }, "effort": "high",
        "prompt": format!("Pick up {project}#{id}"), "runAt": T0 - HOUR, "deadline": deadline,
    })
}

fn executable(dir: &Path, name: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

struct Fixture {
    mock: MockSpew,
    runner: FakeRunner,
    scheduler: Scheduler<FakeRunner, FixedDetect>,
    folder: PathBuf,
    bin: PathBuf,
    _dir: tempfile::TempDir,
}

async fn fixture(settings_extra: &str) -> Fixture {
    let mock = MockSpew::start().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = TestClock::at(T0);
    let api = mock.api(dir.path(), clock.clone());
    let (signed, _) = sign_in_with(&api, vec![], Duration::from_secs(10)).await;
    signed.expect("sign-in");

    let folder = dir.path().join("code").join("spew");
    std::fs::create_dir_all(&folder).unwrap();
    let home = Home::at(dir.path().join("mesh")).unwrap();
    home.ensure().unwrap();
    std::fs::write(
        home.settings_file(),
        format!(
            "[schedule]\n{settings_extra}\n[schedule.folders]\nspew = \"{}\"\n",
            folder.display()
        ),
    )
    .unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    executable(&bin, "claude");
    executable(&bin, "codex");
    let env = HostEnv::new([
        ("HOME", dir.path().to_str().unwrap()),
        ("PATH", bin.to_str().unwrap()),
    ]);
    let runner = FakeRunner::default();
    let store = Store::open(&home.store_file()).unwrap();
    let detect = FixedDetect(vec![
        agent(AgentKind::Claude, "opus"),
        agent(AgentKind::Codex, "gpt-6.1-sol"),
    ]);
    let scheduler = Scheduler::new(
        api,
        store,
        runner.clone(),
        detect,
        clock,
        SchedulerConfig {
            home,
            user_home: Some(dir.path().to_path_buf()),
            env,
        },
    );
    Fixture {
        mock,
        runner,
        scheduler,
        folder,
        bin,
        _dir: dir,
    }
}

#[tokio::test]
async fn due_runs_are_started_run_and_finished() {
    let mut f = fixture("").await;
    f.mock.with(|s| {
        s.due = vec![
            due("r3", "spew", "claude", T0 + HOUR),
            due("r1", "spew", "claude", T0 + HOUR),
            due("r2", "spew", "codex", T0 + HOUR),
            due("r4", "spew", "claude", T0 - 1),
            due("r5", "spew", "opencode", T0 + HOUR),
            due("r6", "web", "claude", T0 + HOUR),
            due("r7", "spew", "claude", T0 + HOUR),
        ];
        s.taken = vec!["r3".into()];
    });
    f.scheduler.tick().await;
    f.scheduler.wait_runs().await;

    // Capabilities went first, with the agents and the one project that has a folder here.
    let caps = f.mock.with(|s| s.capabilities.clone());
    assert_eq!(caps.len(), 1);
    assert_eq!(caps[0]["projects"], json!(["spew"]));
    assert_eq!(caps[0]["reportedAt"], json!(T0));
    assert_eq!(caps[0]["agents"][0]["kind"], "claude");
    assert_eq!(caps[0]["agents"][0]["models"][0]["defaultEffort"], "high");
    assert_eq!(caps[0]["agents"][1]["kind"], "codex");

    // r1 and r2 ran; r3 was taken (409); r4 is past its deadline; r5's agent and r6's folder aren't here; r7 waits
    // for the Claude Code run.
    let (started, finished) = f.mock.with(|s| (s.started.clone(), s.finished.clone()));
    assert_eq!(started, ["r1", "r2"]);
    assert_eq!(finished.len(), 2);
    let r1 = &finished.iter().find(|(id, _)| id == "r1").unwrap().1;
    assert_eq!(
        *r1,
        json!({ "status": "done", "summary": "Moved spew#1 to review.", "exitCode": 0 })
    );
    let r2 = &finished.iter().find(|(id, _)| id == "r2").unwrap().1;
    assert_eq!(r2["status"], "failed");
    assert_eq!(r2["exitCode"], 1);
    assert_eq!(r2["summary"], "Codex exited with status 1.\ntests failed");

    let status = f.scheduler.handle().status();
    let note = |id: &str| {
        status
            .due
            .iter()
            .find(|r| r.id == id)
            .map(|r| r.note.clone())
            .unwrap()
    };
    assert_eq!(note("r1"), "started");
    assert_eq!(note("r3"), "taken by another computer");
    assert_eq!(note("r4"), "past its deadline; spew marks it missed");
    assert_eq!(note("r5"), "OpenCode isn't installed here");
    assert_eq!(note("r6"), "no folder for project web here");
    assert_eq!(note("r7"), "waiting: a Claude Code run is going");

    // The command lines: the CLI in the project folder, the prompt on stdin.
    let specs = f.runner.specs.lock().unwrap().clone();
    let claude = specs
        .iter()
        .find(|s| s.program == f.bin.join("claude"))
        .unwrap();
    assert_eq!(claude.cwd, f.folder);
    assert_eq!(claude.stdin, b"Pick up spew#r1");
    let args: Vec<String> = claude
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        [
            "-p",
            "--model",
            "opus",
            "--effort",
            "high",
            "--output-format",
            "json",
            "--permission-mode",
            "acceptEdits",
            "--permission-prompts",
            "none"
        ]
    );
    assert!(
        claude
            .env
            .iter()
            .any(|(k, v)| k == "SPEW_PROJECT" && v == "spew")
    );
    assert_eq!(claude.timeout, Duration::from_secs(120 * 60));
    let codex = specs
        .iter()
        .find(|s| s.program == f.bin.join("codex"))
        .unwrap();
    assert!(codex.args.iter().any(|a| a == "workspace-write"));
    assert!(codex.args.iter().any(|a| a == f.folder.as_os_str()));

    // The next minute: r7 runs now that Claude Code is free. Capabilities didn't change, so they aren't sent again.
    f.scheduler.tick().await;
    f.scheduler.wait_runs().await;
    assert_eq!(f.mock.with(|s| s.started.clone()), ["r1", "r2", "r7"]);
    assert_eq!(f.mock.with(|s| s.capabilities.len()), 1);

    // The local history has every run, reported.
    let recent = f.scheduler.handle().status().recent;
    assert_eq!(recent.len(), 3);
    assert!(recent.iter().all(|r| r.reported));
    assert!(
        recent
            .iter()
            .any(|r| r.id == "r2" && r.status == "failed" && r.exit_code == Some(1))
    );
}

#[tokio::test]
async fn a_finish_spew_missed_is_sent_again() {
    let mut f = fixture("timeout_minutes = 30").await;
    f.mock.with(|s| {
        s.due = vec![due("r1", "spew", "claude", T0 + HOUR)];
        s.fail_finish = true;
    });
    f.scheduler.tick().await;
    f.scheduler.wait_runs().await;
    assert!(f.mock.with(|s| s.finished.is_empty()));
    let status = f.scheduler.handle().status();
    assert!(!status.recent[0].reported);
    assert!(
        status
            .last_error
            .as_deref()
            .unwrap()
            .contains("can't tell spew how a run ended")
    );
    assert_eq!(
        f.runner.specs.lock().unwrap()[0].timeout,
        Duration::from_secs(30 * 60)
    );

    f.mock.with(|s| s.fail_finish = false);
    f.scheduler.tick().await;
    let finished = f.mock.with(|s| s.finished.clone());
    assert_eq!(finished.len(), 1);
    assert_eq!(finished[0].1["status"], "done");
    assert!(f.scheduler.handle().status().recent[0].reported);
}

#[tokio::test]
async fn nothing_happens_when_scheduling_is_off() {
    let mut f = fixture("enabled = false").await;
    f.mock
        .with(|s| s.due = vec![due("r1", "spew", "claude", T0 + HOUR)]);
    f.scheduler.tick().await;
    let status = f.scheduler.handle().status();
    assert!(!status.enabled);
    assert_eq!(status.state, "off");
    // Folders still show, so `spew mesh schedule` can say where runs would go.
    assert_eq!(status.folders.len(), 1);
    f.mock.with(|s| {
        assert!(s.capabilities.is_empty());
        assert!(s.started.is_empty());
        assert_eq!(s.count("GET /api/mesh/v1/runs/due"), 0);
    });
}
