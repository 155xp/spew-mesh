//! The scheduler loop in the daemon: report capabilities, poll for due runs, run them, report how they ended.
//!
//! It acts only while `[schedule] enabled` (the default) and signed in to spew. Every minute it reads
//! `/runs/due`; for each run whose agent is installed here and whose project has a folder here, it takes the run
//! (`start`; a 409 means another computer has it), runs the CLI in the folder and posts `finish` with the end of
//! the output and the exit status. One run at a time per agent kind. A run past its deadline is left alone: spew
//! marks it missed. Capabilities go to spew at start, every 10 minutes and whenever they change.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{Notify, mpsc};

use super::command::{RunInput, check_choice, spec, summary};
use super::detect::Detect;
use super::{
    AgentCapability, AgentKind, DeviceCapabilities, DueRun, FinishStatus, ProjectFolderInfo,
    RunFinish, ScheduleStatus, SeenRun, folders,
};
use crate::paths::Home;
use crate::responder::env::HostEnv;
use crate::responder::process::{Exit, ProcessRunner};
use crate::settings::{ScheduleSettings, Settings};
use crate::spew_api::{ApiError, AuthStatus, Clock, RunStart, SpewApi};
use crate::store::{ScheduleRunRecord, Store, StoreError};
use crate::text::sanitize_line;

/// How often `/runs/due` is read.
pub const POLL_EVERY: Duration = Duration::from_secs(60);
/// How often the agents are detected again, and capabilities sent even when nothing changed.
pub const REPORT_EVERY_MS: u64 = 10 * 60 * 1000;
/// How many past runs `spew mesh schedule` shows.
pub const RECENT_RUNS: usize = 10;

const NOTE_CHARS: usize = 160;

/// What the scheduler needs besides spew, the store and the runner.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// The spew mesh home, for settings.toml (re-read every minute, so `schedule on|off` and folder overrides
    /// apply without a restart).
    pub home: Home,
    /// The person's home folder, for `~` in settings.
    pub user_home: Option<PathBuf>,
    /// The environment runs start from (see [`super::command::base_env`]).
    pub env: HostEnv,
}

/// What the control socket holds: the scheduler's status and a way to wake it.
#[derive(Clone, Debug)]
pub struct SchedulerHandle {
    status: Arc<Mutex<ScheduleStatus>>,
    kick: Arc<Notify>,
    look: Arc<AtomicBool>,
}

impl SchedulerHandle {
    /// The status as of the scheduler's last step.
    pub fn status(&self) -> ScheduleStatus {
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Asks the scheduler to re-read settings and poll now. Before sign-in it also looks at the agents once, so
    /// `spew mesh schedule` can show them; otherwise a daemon that isn't signed in never runs the CLIs.
    pub fn kick(&self) {
        self.look.store(true, Ordering::Relaxed);
        self.kick.notify_one();
    }
}

// A run that ended, sent from its task back to the loop.
struct Finished {
    id: String,
    agent: AgentKind,
    finish: RunFinish,
}

/// The scheduler. [`Scheduler::run`] is the daemon's loop; tests drive [`Scheduler::tick`] and
/// [`Scheduler::wait_runs`] themselves.
pub struct Scheduler<R, D> {
    api: SpewApi,
    store: Mutex<Store>,
    runner: R,
    detect: D,
    clock: Arc<dyn Clock>,
    cfg: SchedulerConfig,
    settings: ScheduleSettings,
    agents: Option<Vec<AgentCapability>>,
    detected_at: u64,
    sent: Option<(Vec<AgentCapability>, Vec<String>)>,
    sent_at: u64,
    running: HashSet<AgentKind>,
    started: HashSet<String>,
    done_tx: mpsc::UnboundedSender<Finished>,
    done_rx: mpsc::UnboundedReceiver<Finished>,
    handle: SchedulerHandle,
    logged_error: Mutex<Option<String>>,
}

fn idle_status(settings: &ScheduleSettings) -> ScheduleStatus {
    ScheduleStatus {
        enabled: settings.enabled,
        signed_in: false,
        state: "starting".into(),
        capabilities: None,
        reported_at: None,
        polled_at: None,
        last_error: None,
        folders: Vec::new(),
        due: Vec::new(),
        recent: Vec::new(),
        running: Vec::new(),
    }
}

impl<R, D> Scheduler<R, D>
where
    R: ProcessRunner + Clone + 'static,
    D: Detect,
{
    /// A scheduler over `store` (its own connection to the daemon's store).
    pub fn new(
        api: SpewApi,
        store: Store,
        runner: R,
        detect: D,
        clock: Arc<dyn Clock>,
        cfg: SchedulerConfig,
    ) -> Self {
        let settings = Settings::load(&cfg.home, cfg.user_home.as_deref())
            .map(|s| s.schedule)
            .unwrap_or_else(|_| Settings::defaults(&cfg.home).schedule);
        let (done_tx, done_rx) = mpsc::unbounded_channel();
        let handle = SchedulerHandle {
            status: Arc::new(Mutex::new(idle_status(&settings))),
            kick: Arc::new(Notify::new()),
            look: Arc::new(AtomicBool::new(false)),
        };
        Scheduler {
            api,
            store: Mutex::new(store),
            runner,
            detect,
            clock,
            cfg,
            settings,
            agents: None,
            detected_at: 0,
            sent: None,
            sent_at: 0,
            running: HashSet::new(),
            started: HashSet::new(),
            done_tx,
            done_rx,
            handle,
            logged_error: Mutex::new(None),
        }
    }

    /// The handle the control socket keeps.
    pub fn handle(&self) -> SchedulerHandle {
        self.handle.clone()
    }

    fn store(&self) -> std::sync::MutexGuard<'_, Store> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn status(&self) -> std::sync::MutexGuard<'_, ScheduleStatus> {
        self.handle
            .status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    // Records the step's problem for `spew mesh schedule`; the log gets it once, not every minute.
    fn set_error(&self, what: &str, e: &dyn std::fmt::Display) {
        let line = sanitize_line(&format!("{what}: {e}"), 300);
        let mut logged = self
            .logged_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if logged.as_deref() != Some(line.as_str()) {
            tracing::info!(error = %e, "scheduled runs: {what}");
            *logged = Some(line.clone());
        }
        drop(logged);
        self.status().last_error = Some(line);
    }

    /// The daemon's loop: a step every minute, when woken through the handle, and whenever a run ends.
    pub async fn run(mut self) {
        let mut every = tokio::time::interval(POLL_EVERY);
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let kick = self.handle.kick.clone();
        loop {
            tokio::select! {
                _ = every.tick() => self.tick().await,
                _ = kick.notified() => self.tick().await,
                Some(done) = self.done_rx.recv() => self.finished(done).await,
            }
        }
    }

    /// Waits for every run in flight to end and reports each one.
    pub async fn wait_runs(&mut self) {
        while !self.running.is_empty() {
            match self.done_rx.recv().await {
                Some(done) => self.finished(done).await,
                None => return,
            }
        }
    }

    fn reload_settings(&mut self) {
        match Settings::load(&self.cfg.home, self.cfg.user_home.as_deref()) {
            Ok(s) => self.settings = s.schedule,
            // The last good settings stay in force.
            Err(e) => self.set_error("settings.toml can't be read", &e),
        }
    }

    fn folders(&self) -> Vec<ProjectFolderInfo> {
        if let Err(e) = folders::learn(&self.store()) {
            tracing::warn!(error = %e, "scheduled runs: can't learn project folders");
        }
        folders::resolve(&self.settings.folders, &self.store()).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "scheduled runs: can't read project folders");
            Vec::new()
        })
    }

    fn refresh_recent(&self) {
        let recent = self.store().schedule_runs(RECENT_RUNS).unwrap_or_default();
        let mut running: Vec<String> = self.running.iter().map(|k| k.to_string()).collect();
        running.sort();
        let mut status = self.status();
        status.recent = recent;
        status.running = running;
    }

    /// One step: settings, folders, then (when on and signed in) pending reports, capabilities and due runs.
    pub async fn tick(&mut self) {
        self.status().last_error = None;
        self.reload_settings();
        let folders = self.folders();
        let signed_in = matches!(self.api.auth_status(), Ok(AuthStatus::SignedIn(_)));
        {
            let mut status = self.status();
            status.enabled = self.settings.enabled;
            status.signed_in = signed_in;
            status.folders = folders.clone();
            status.state = if !self.settings.enabled {
                "off".into()
            } else if !signed_in {
                "waiting for spew sign-in".into()
            } else {
                "on".into()
            };
        }
        self.refresh_recent();
        let look = self.handle.look.swap(false, Ordering::Relaxed);
        if !self.settings.enabled {
            return;
        }
        if !signed_in {
            if look {
                self.refresh_agents().await;
                self.status().capabilities = Some(self.capabilities(&folders));
            }
            return;
        }
        self.report_unreported().await;
        self.report_capabilities(&folders).await;
        self.poll_due(&folders).await;
        let now = self.clock.now_ms();
        if let Err(e) = self.store().sweep_schedule_runs(now) {
            tracing::warn!(error = %e, "scheduled runs: can't sweep the run history");
        }
        if self.status().last_error.is_none() {
            *self
                .logged_error
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = None;
        }
        self.refresh_recent();
    }

    // Detects the agents when it has never been done or the last time was 10 minutes ago.
    async fn refresh_agents(&mut self) {
        let now = self.clock.now_ms();
        if self.agents.is_none() || now.saturating_sub(self.detected_at) >= REPORT_EVERY_MS {
            let agents = self.detect.detect().await;
            self.agents = Some(agents);
            self.detected_at = now;
        }
    }

    fn capabilities(&self, folders: &[ProjectFolderInfo]) -> DeviceCapabilities {
        DeviceCapabilities {
            agents: self.agents.clone().unwrap_or_default(),
            projects: folders
                .iter()
                .map(|f| f.project.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
            reported_at: self.clock.now_ms(),
        }
    }

    async fn report_capabilities(&mut self, folders: &[ProjectFolderInfo]) {
        self.refresh_agents().await;
        let now = self.clock.now_ms();
        let caps = self.capabilities(folders);
        let agents = caps.agents.clone();
        let projects = caps.projects.clone();
        self.status().capabilities = Some(caps.clone());
        let same = self
            .sent
            .as_ref()
            .is_some_and(|(a, p)| *a == agents && *p == projects);
        if same && now.saturating_sub(self.sent_at) < REPORT_EVERY_MS {
            return;
        }
        match self.api.put_capabilities(&caps).await {
            Ok(()) => {
                if !same {
                    let kinds: Vec<&str> = agents.iter().map(|a| a.kind.as_str()).collect();
                    tracing::info!(agents = ?kinds, projects = ?projects, "told spew what this computer can run");
                }
                self.sent = Some((agents, projects));
                self.sent_at = now;
                self.status().reported_at = Some(now);
            }
            Err(e) => self.set_error("can't send capabilities to spew", &e),
        }
    }

    async fn poll_due(&mut self, folders: &[ProjectFolderInfo]) {
        let raw = match self.api.due_runs().await {
            Ok(raw) => raw,
            Err(e) => return self.set_error("can't read due runs from spew", &e),
        };
        let mut seen = Vec::new();
        for value in raw {
            let run: DueRun = match serde_json::from_value(value) {
                Ok(run) => run,
                Err(e) => {
                    tracing::info!(error = %e, "scheduled runs: skipping a due run spew sent in an unknown shape");
                    continue;
                }
            };
            let note = self.consider(&run, folders).await;
            seen.push(SeenRun {
                id: run.id.clone(),
                issue: sanitize_line(&run.issue, 40),
                title: sanitize_line(&run.title, NOTE_CHARS),
                agent: run.agent.to_string(),
                model: sanitize_line(&run.model, 80),
                effort: sanitize_line(&run.effort, 32),
                run_at: run.run_at,
                deadline: run.deadline,
                note: sanitize_line(&note, NOTE_CHARS),
            });
        }
        let mut status = self.status();
        status.polled_at = Some(self.clock.now_ms());
        status.due = seen;
    }

    // Decides about one due run and starts it when it can; says what happened.
    async fn consider(&mut self, run: &DueRun, folders: &[ProjectFolderInfo]) -> String {
        let label = run.agent.label();
        if self.started.contains(&run.id) {
            return "started here".into();
        }
        if run.deadline <= self.clock.now_ms() {
            return "past its deadline; spew marks it missed".into();
        }
        let installed = self
            .agents
            .as_ref()
            .is_some_and(|a| a.iter().any(|c| c.kind == run.agent));
        let program = self.cfg.env.find_program(run.agent.as_str());
        let (true, Some(program)) = (installed, program) else {
            return format!("{label} isn't installed here");
        };
        let Some(folder) = folders.iter().find(|f| f.project == run.project) else {
            return format!("no folder for project {} here", run.project);
        };
        if self.running.contains(&run.agent) {
            return format!("waiting: a {label} run is going");
        }
        let started = match self.api.start_run::<Value>(&run.id).await {
            Ok(RunStart::Started(v)) => v,
            Ok(RunStart::Taken) => {
                tracing::info!(run = %run.id, "scheduled run taken by another computer");
                return "taken by another computer".into();
            }
            Err(e) => {
                self.set_error("can't start a due run", &e);
                return format!("couldn't take it: {e}");
            }
        };
        // spew's copy at start time is the one to run; the polled one stands in if the answer has another shape.
        let run = serde_json::from_value::<DueRun>(started)
            .ok()
            .filter(|r| r.id == run.id)
            .unwrap_or_else(|| run.clone());
        self.launch(run, program, Path::new(&folder.folder).to_path_buf());
        "started".into()
    }

    fn launch(&mut self, run: DueRun, program: PathBuf, folder: PathBuf) {
        let now = self.clock.now_ms();
        let timeout = Duration::from_secs(self.settings.timeout_minutes * 60);
        let record = ScheduleRunRecord {
            id: run.id.clone(),
            project: run.project.clone(),
            issue: sanitize_line(&run.issue, 40),
            title: sanitize_line(&run.title, NOTE_CHARS),
            agent: run.agent.to_string(),
            model: sanitize_line(&run.model, 200),
            effort: sanitize_line(&run.effort, 32),
            folder: folder.display().to_string(),
            run_at: run.run_at,
            started_at: now,
            finished_at: None,
            status: "running".into(),
            exit_code: None,
            summary: None,
            reported: false,
        };
        if let Err(e) = self.store().put_schedule_run(&record) {
            tracing::warn!(error = %e, "scheduled runs: can't record a run");
        }
        tracing::info!(
            run = %run.id,
            issue = %record.issue,
            agent = %run.agent,
            model = %record.model,
            effort = %record.effort,
            folder = %record.folder,
            timeout_minutes = self.settings.timeout_minutes,
            "starting a scheduled run"
        );
        self.running.insert(run.agent);
        self.started.insert(run.id.clone());
        let runner = self.runner.clone();
        let env = self.cfg.env.clone();
        let done = self.done_tx.clone();
        tokio::spawn(async move {
            let finish = match check_choice(run.agent, &run.model, &run.effort) {
                Err(why) => RunFinish {
                    status: FinishStatus::Failed,
                    summary: format!("Not run: {why}."),
                    exit_code: None,
                },
                Ok(()) => {
                    let input = RunInput {
                        agent: run.agent,
                        program,
                        model: &run.model,
                        effort: &run.effort,
                        prompt: &run.prompt,
                        folder: &folder,
                        project: &run.project,
                        timeout,
                    };
                    match runner.run(spec(&input, &env)).await {
                        Ok(out) => RunFinish {
                            status: if out.success() {
                                FinishStatus::Done
                            } else {
                                FinishStatus::Failed
                            },
                            summary: summary(run.agent, &out, timeout),
                            exit_code: match out.exit {
                                Exit::Code(code) => Some(code),
                                _ => None,
                            },
                        },
                        Err(e) => RunFinish {
                            status: FinishStatus::Failed,
                            summary: format!("{} couldn't start: {e}", run.agent.label()),
                            exit_code: None,
                        },
                    }
                }
            };
            let _ = done.send(Finished {
                id: run.id,
                agent: run.agent,
                finish,
            });
        });
        self.refresh_recent();
    }

    async fn finished(&mut self, done: Finished) {
        self.running.remove(&done.agent);
        let now = self.clock.now_ms();
        if let Err(e) = self.store().finish_schedule_run(
            &done.id,
            done.finish.status.as_str(),
            done.finish.exit_code,
            &done.finish.summary,
            now,
        ) {
            tracing::warn!(error = %e, "scheduled runs: can't record a run's end");
        }
        tracing::info!(
            run = %done.id,
            agent = %done.agent,
            status = done.finish.status.as_str(),
            exit_code = ?done.finish.exit_code,
            "scheduled run ended"
        );
        self.report(&done.id, &done.finish).await;
        self.refresh_recent();
    }

    // Posts a run's end; on success, or when spew no longer has the run, it is marked reported. Otherwise the
    // next step tries again.
    async fn report(&self, id: &str, finish: &RunFinish) {
        match self.api.finish_run(id, finish).await {
            Ok(()) => self.mark_reported(id),
            Err(ApiError::Server { status, .. }) if matches!(status, 400 | 404 | 409 | 410) => {
                tracing::info!(run = %id, status, "spew didn't take the run's end; not trying again");
                self.mark_reported(id);
            }
            Err(e) => self.set_error("can't tell spew how a run ended", &e),
        }
    }

    fn mark_reported(&self, id: &str) {
        if let Err(e) = self.store().mark_schedule_run_reported(id) {
            tracing::warn!(error = %e, "scheduled runs: can't mark a run reported");
        }
    }

    // Ends spew hasn't heard yet, and runs an earlier daemon left running (it stopped mid-run, which killed the
    // run).
    async fn report_unreported(&mut self) {
        let pending: Result<Vec<ScheduleRunRecord>, StoreError> =
            self.store().unreported_schedule_runs();
        let Ok(pending) = pending else {
            return;
        };
        for r in pending {
            if r.status == "running" {
                if self.started.contains(&r.id) {
                    continue;
                }
                let why = "The spew mesh daemon stopped before the run ended.";
                let now = self.clock.now_ms();
                let recorded = self
                    .store()
                    .finish_schedule_run(&r.id, "failed", None, why, now);
                if let Err(e) = recorded {
                    tracing::warn!(error = %e, "scheduled runs: can't record a run's end");
                }
                let finish = RunFinish {
                    status: FinishStatus::Failed,
                    summary: why.into(),
                    exit_code: None,
                };
                self.report(&r.id, &finish).await;
                continue;
            }
            let status = if r.status == "done" {
                FinishStatus::Done
            } else {
                FinishStatus::Failed
            };
            let finish = RunFinish {
                status,
                summary: r.summary.clone().unwrap_or_default(),
                exit_code: r.exit_code,
            };
            self.report(&r.id, &finish).await;
        }
    }
}
