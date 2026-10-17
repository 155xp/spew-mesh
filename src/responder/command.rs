//! `spew mesh serve` (9, 10): the checks a responder needs, then the serve loop until Ctrl-C.
//!
//! A check that fails prints the way `doctor` does: what is wrong, then a `fix:` line. The loop's events and the
//! raw output of failed runs go to `logs/mesh.log` (9); the terminal gets a line per question.

use std::fmt::Display;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use super::env::HostEnv;
use super::limits::{GateSettings, SystemClock};
use super::memory::StoreConversations;
use super::process::SystemRunner;
use super::runloop::{self, LoopConfig, Report, ServeEvent};
use super::{Responder, ServeClient, ServeConfig, ServeError};
use crate::address::is_session_name;
use crate::control::client::{Client, ConnectOptions};
use crate::control::protocol::SendStatus;
use crate::paths::{Home, open_private_append, user_home_from_env};
use crate::settings::{Env, Settings};
use crate::text::sanitize_line;

/// The arguments of `spew mesh serve`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServeOptions {
    /// claude, codex or opencode.
    pub client: String,
    /// `--name`.
    pub name: Option<String>,
    /// `--cwd`.
    pub cwd: Option<PathBuf>,
    /// `--allow-edits`.
    pub allow_edits: bool,
    /// `--timeout`, in seconds.
    pub timeout: Option<u64>,
    /// `--model`.
    pub model: Option<String>,
}

const DOCTOR: &str = "`spew mesh doctor` shows what is wrong";

// A check that failed: what is wrong and what to do about it.
struct Problem {
    what: String,
    fix: String,
}

impl Problem {
    fn new(what: impl Display, fix: impl Into<String>) -> Self {
        Problem {
            what: what.to_string(),
            fix: fix.into(),
        }
    }
}

impl From<ServeError> for Problem {
    fn from(e: ServeError) -> Self {
        Problem::new(&e, e.fix())
    }
}

/// Runs `spew mesh serve`; `home` is `--home`.
pub fn run(home: Option<&Path>, opts: &ServeOptions) -> ExitCode {
    match start(home, opts) {
        Ok(code) => code,
        Err(p) => {
            let mut err = std::io::stderr().lock();
            let _ = writeln!(err, "spew mesh serve: {}", p.what);
            let _ = writeln!(err, "fix: {}", p.fix);
            ExitCode::FAILURE
        }
    }
}

fn start(home: Option<&Path>, opts: &ServeOptions) -> Result<ExitCode, Problem> {
    let client: ServeClient = opts.client.parse().map_err(|e: ServeError| {
        Problem::new(e, "run `spew mesh serve claude`, `codex` or `opencode`")
    })?;
    if let Some(name) = &opts.name
        && !is_session_name(name)
    {
        return Err(Problem::new(
            format!("{name:?} is not a session name"),
            "use lowercase letters, digits and single hyphens, at most 48",
        ));
    }
    let home = Home::from_env(home).map_err(|e| Problem::new(e, "pass --home DIR"))?;
    let env = Env::from_process().map_err(|e| Problem::new(e, "fix or unset that variable"))?;
    let settings = Settings::load(&home, user_home_from_env().as_deref())
        .map_err(|e| Problem::new(e, "fix settings.toml; `spew mesh doctor` checks it"))?;
    let cwd = match &opts.cwd {
        Some(dir) => dir.clone(),
        None => std::env::current_dir().map_err(|e| {
            Problem::new(
                format!("can't read the current folder: {e}"),
                "pass --cwd DIR",
            )
        })?,
    };
    let mut cfg = ServeConfig::new(client, cwd, home.serve_run_dir(), &settings.serve);
    cfg.allow_edits = opts.allow_edits;
    cfg.model = opts.model.clone();
    if let Some(seconds) = opts.timeout {
        cfg.timeout = Duration::from_secs(seconds);
    }
    // Responder::start checks again; a bad flag shouldn't start a daemon first.
    cfg.validate()?;
    home.ensure()
        .map_err(|e| Problem::new(e, "check that the spew mesh home is yours and writable"))?;
    init_logging(&home, &env)?;
    let limits = GateSettings::from(&settings.serve);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Problem::new(format!("can't start the async runtime: {e}"), DOCTOR))?;
    runtime.block_on(serve(home, env, cfg, opts.name.clone(), limits))
}

async fn serve(
    home: Home,
    env: Env,
    cfg: ServeConfig,
    name: Option<String>,
    limits: GateSettings,
) -> Result<ExitCode, Problem> {
    let client = cfg.client;
    // The checks come before the daemon, which is started only for a responder that can run.
    let mut responder = Responder::start(
        cfg,
        &HostEnv::from_process(),
        SystemRunner,
        StoreConversations::open_later(home.store_file()),
        SystemClock,
    )
    .await?;
    // The first connect is a CLI command's: it starts a missing daemon and replaces an older one (6.1, 9).
    // Reconnects use the loop's responder options.
    let daemon = Client::connect(&home, &ConnectOptions::cli())
        .await
        .map_err(|e| Problem::new(e, DOCTOR))?;
    let folder = responder.config().cwd.clone();
    let cwd = folder.to_str().map(str::to_string).ok_or_else(|| {
        Problem::new(
            format!("{} is not a UTF-8 path", folder.display()),
            "serve a folder whose path is UTF-8",
        )
    })?;
    let mode = if responder.config().allow_edits {
        "file edits allowed"
    } else {
        "read-only"
    };
    let report = reporter(format!("{client} in {cwd}, {mode}"));
    let loop_cfg = LoopConfig::new(home, client, cwd, name, env.spew_project.clone(), limits);
    let (stop_tx, stop_rx) = watch::channel(false);
    tokio::spawn(stop_on_signal(stop_tx));
    match runloop::serve(&mut responder, loop_cfg, Some(daemon), stop_rx, report).await {
        Ok(()) => {
            let _ = writeln!(std::io::stdout(), "spew mesh serve: stopped");
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => Err(Problem::new(e, DOCTOR)),
    }
}

// Turns `stop` true on Ctrl-C, SIGTERM or SIGHUP.
async fn stop_on_signal(stop: watch::Sender<bool>) {
    use tokio::signal::unix::{SignalKind, signal};
    async fn on(kind: SignalKind) {
        match signal(kind) {
            Ok(mut s) => {
                s.recv().await;
            }
            // Without the handler the default action applies, which ends the process anyway.
            Err(_) => std::future::pending::<()>().await,
        }
    }
    tokio::select! {
        _ = on(SignalKind::interrupt()) => {}
        _ = on(SignalKind::terminate()) => {}
        _ = on(SignalKind::hangup()) => {}
    }
    let _ = stop.send(true);
}

// Writes the loop's events to the terminal and the log. Peer text is cut to one clean line (10).
fn reporter(serving: String) -> Report {
    Arc::new(move |event: &ServeEvent| {
        let clean = |s: &str| sanitize_line(s, 300);
        let line = match event {
            ServeEvent::Ready { address, again } => {
                tracing::info!(address = %address, again, "responder session open");
                if *again {
                    Some(format!("Back on the daemon as {}.", clean(address)))
                } else {
                    Some(format!(
                        "Serving {serving} as {}. Ctrl-C stops.",
                        clean(address)
                    ))
                }
            }
            ServeEvent::Lost => {
                tracing::warn!("lost the daemon connection; reconnecting");
                Some("Lost the daemon; reconnecting.".to_string())
            }
            ServeEvent::Retrying { error } => {
                tracing::warn!(error = %error, "the daemon is not back yet");
                None
            }
            ServeEvent::Question { id, from } => {
                tracing::info!(question = %id, from = %from, "question taken");
                Some(format!("{}: question from {}", clean(id), clean(from)))
            }
            ServeEvent::Answered { id, cost_usd } => Some(match cost_usd {
                Some(cost) => format!("{}: answered (${cost:.4})", clean(id)),
                None => format!("{}: answered", clean(id)),
            }),
            ServeEvent::Failed { id, reason } => {
                Some(format!("{}: the run failed: {}", clean(id), clean(reason)))
            }
            ServeEvent::Replied { id, status } => {
                tracing::info!(question = %id, status = ?status, "reply sent");
                match status {
                    SendStatus::Refused | SendStatus::Expired => Some(format!(
                        "{}: the asker's device did not take the reply ({status:?})",
                        clean(id)
                    )),
                    _ => None,
                }
            }
            ServeEvent::NotSent { id, error } => {
                tracing::warn!(question = %id, error = %error, "reply not sent");
                Some(format!(
                    "{}: the reply could not be sent: {}",
                    clean(id),
                    clean(error)
                ))
            }
        };
        if let Some(line) = line {
            let _ = writeln!(std::io::stdout(), "{line}");
        }
    })
}

// The serve process logs to the daemon's log file, where `doctor` and bug reports look (9, 11).
fn init_logging(home: &Home, env: &Env) -> Result<(), Problem> {
    let file = open_private_append(&home.log_file())
        .map_err(|e| Problem::new(e, "check that the spew mesh home is yours and writable"))?;
    let filter = tracing_subscriber::EnvFilter::try_new(env.log.as_deref().unwrap_or("info"))
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .try_init();
    Ok(())
}
