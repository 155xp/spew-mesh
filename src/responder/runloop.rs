//! The serve loop (9): a responder session on the daemon, kept open across lost connections and daemon restarts,
//! taking one question at a time and replying with the answer or the failure line.
//!
//! A keeper task owns the connection: it connects, opens the session with a stable `client_session` (so a
//! re-open re-attaches the same session, 6.4 rule 3), and opens it again whenever the connection drops. A
//! reconnect starts a missing daemon only while `<home>/run/stopped` is absent (6.1), so `daemon stop` holds. The loop
//! itself only takes, runs and replies, always with the newest open session. A reply that couldn't go out because
//! the connection dropped is sent again once the session is back.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::{Notify, watch};

use super::limits::{Clock, GateSettings};
use super::memory::ConversationStore;
use super::process::ProcessRunner;
use super::{Responder, RunFailure, ServeClient};
use crate::control::client::{Client, ClientError, ConnectOptions, reconnect_delay};
use crate::control::protocol::{
    Reason, ReplyParams, SendStatus, SessionOpen, SessionUpdate, TakeParams, Taken,
};
use crate::paths::Home;
use crate::wire::{ActivityState, SessionKind};

/// How long each `responder.take` waits for a question before asking again.
pub const TAKE_WAIT: Duration = Duration::from_secs(60);
/// How long a stopping responder waits for its last reply and `session.close`.
pub const CLOSE_WAIT: Duration = Duration::from_secs(5);
/// The `client_name` a responder gives in `session.open`.
pub const CLIENT_NAME: &str = "spew-mesh-serve";

/// What the loop needs besides the responder.
#[derive(Clone, Debug)]
pub struct LoopConfig {
    /// The spew mesh home whose daemon it talks to.
    pub home: Home,
    /// How it (re)connects.
    pub connect: ConnectOptions,
    /// The `session.open` it sends, the same each time.
    pub open: SessionOpen,
    /// How long each `responder.take` waits.
    pub take_wait: Duration,
    /// How long a stop waits for the last reply and the close.
    pub close_wait: Duration,
}

impl LoopConfig {
    /// A responder's loop: `session.open` with kind `responder`, a fresh `client_session` that stays the same
    /// for this process, and its `[serve]` limits for the daemon.
    pub fn new(
        home: Home,
        client: ServeClient,
        cwd: String,
        name: Option<String>,
        spew_project: Option<String>,
        limits: GateSettings,
    ) -> Self {
        LoopConfig {
            home,
            connect: ConnectOptions::responder(),
            open: SessionOpen {
                client: client.as_str().to_string(),
                client_session: Some(format!("serve-{}", ulid::Ulid::new())),
                client_pid: None,
                cwd,
                client_name: Some(CLIENT_NAME.to_string()),
                kind: SessionKind::Responder,
                name,
                spew_project,
                serve: Some(limits),
            },
            take_wait: TAKE_WAIT,
            close_wait: CLOSE_WAIT,
        }
    }
}

/// What happened, for the person watching serve and for the log.
#[derive(Clone, Debug, PartialEq)]
pub enum ServeEvent {
    /// The responder session is open; `again` after a reconnect.
    Ready {
        /// Its full address.
        address: String,
        /// False the first time.
        again: bool,
    },
    /// The daemon connection dropped; the keeper reconnects.
    Lost,
    /// A reconnect or re-open failed and will be tried again.
    Retrying {
        /// Why it failed.
        error: String,
    },
    /// A question was taken and a run starts.
    Question {
        /// Its message id.
        id: String,
        /// The asker, `person/name` as the daemon gave it.
        from: String,
    },
    /// The run answered.
    Answered {
        /// The question's id.
        id: String,
        /// The cost the client reported.
        cost_usd: Option<f64>,
    },
    /// The run failed; the failure line is the reply.
    Failed {
        /// The question's id.
        id: String,
        /// The failure.
        reason: String,
    },
    /// The reply went out.
    Replied {
        /// The question's id.
        id: String,
        /// Where the reply is.
        status: SendStatus,
    },
    /// The reply could not be sent and was dropped.
    NotSent {
        /// The question's id.
        id: String,
        /// Why.
        error: String,
    },
}

/// Where events go.
pub type Report = Arc<dyn Fn(&ServeEvent) + Send + Sync>;

/// Why the loop gave up. Only the first connection and the first `session.open` end it; after that it keeps
/// reconnecting.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LoopError {
    /// The daemon couldn't be reached or started.
    #[error("{0}")]
    Connect(String),
    /// The daemon refused the responder session.
    #[error("the daemon refused the responder session: {0}")]
    Open(String),
    /// The connection keeper stopped.
    #[error("the connection to the daemon stopped")]
    Keeper,
}

// One open responder session on one connection.
struct Live {
    client: Client,
    token: String,
    // Set when the loop found the session gone, so it waits for the keeper's next one.
    stale: AtomicBool,
}

impl Live {
    fn usable(&self) -> bool {
        !self.client.is_closed() && !self.stale.load(Ordering::Relaxed)
    }
}

enum Link {
    Connecting,
    Up(Arc<Live>),
    Down,
    Failed(LoopError),
}

// Aborts the keeper when the loop returns.
struct Keeper(tokio::task::JoinHandle<()>);

impl Drop for Keeper {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// A reply waiting for a connection.
struct Pending {
    id: String,
    text: String,
}

/// Runs the serve loop until `stop` turns true (or its sender is dropped), then closes the session. `first` is
/// an already open connection to use first. Returns an error only when the daemon can't be reached or refuses
/// the session at the start.
pub async fn serve<R, M, C>(
    responder: &mut Responder<R, M, C>,
    cfg: LoopConfig,
    first: Option<Client>,
    mut stop: watch::Receiver<bool>,
    report: Report,
) -> Result<(), LoopError>
where
    R: ProcessRunner,
    M: ConversationStore,
    C: Clock,
{
    let cfg = Arc::new(cfg);
    let (tx, mut link) = watch::channel(Link::Connecting);
    let reopen = Arc::new(Notify::new());
    let _keeper = Keeper(tokio::spawn(keep(
        cfg.clone(),
        first,
        tx,
        reopen.clone(),
        report.clone(),
    )));
    let mut pending: Option<Pending> = None;
    let mut errors: u32 = 0;
    loop {
        let live = tokio::select! {
            biased;
            _ = stopped(&mut stop) => {
                finish(current(&link), pending, &cfg, &report).await;
                return Ok(());
            }
            up = next_up(&mut link) => up?,
        };

        if let Some(p) = pending.take() {
            let params = ReplyParams {
                token: live.token.clone(),
                id: p.id.clone(),
                text: p.text.clone(),
                wait_seconds: None,
            };
            let call = live.client.reply(&params);
            tokio::pin!(call);
            let (sent, stopping) = tokio::select! {
                biased;
                sent = &mut call => (Some(sent), false),
                _ = stopped(&mut stop) => {
                    (tokio::time::timeout(cfg.close_wait, &mut call).await.ok(), true)
                }
            };
            match sent {
                Some(Ok(r)) => report(&ServeEvent::Replied {
                    id: p.id,
                    status: r.status,
                }),
                Some(Err(e)) if already_replied(&e) => report(&ServeEvent::Replied {
                    id: p.id,
                    status: SendStatus::Queued,
                }),
                Some(Err(e)) if lost(&e) && !stopping => {
                    pending = Some(p);
                    drop_live(&live, &reopen).await;
                    continue;
                }
                Some(Err(e)) => {
                    report(&ServeEvent::NotSent {
                        id: p.id,
                        error: e.to_string(),
                    });
                    // The daemon sets a responder idle on its reply; with none, say so here.
                    let _ = live
                        .client
                        .session_update(&SessionUpdate {
                            token: live.token.clone(),
                            state: Some(ActivityState::Idle),
                            ..SessionUpdate::default()
                        })
                        .await;
                }
                None => report(&ServeEvent::NotSent {
                    id: p.id,
                    error: "the responder stopped before the reply went out".into(),
                }),
            }
            if stopping {
                close(&live, cfg.close_wait).await;
                return Ok(());
            }
        }

        let params = TakeParams {
            token: live.token.clone(),
            wait_seconds: cfg.take_wait.as_secs().max(1),
        };
        let taken = tokio::select! {
            biased;
            _ = stopped(&mut stop) => {
                close(&live, cfg.close_wait).await;
                return Ok(());
            }
            taken = live.client.take(&params) => taken,
        };
        let view = match taken {
            Ok(Taken { question: None }) => {
                errors = 0;
                continue;
            }
            Ok(Taken {
                question: Some(view),
            }) => {
                errors = 0;
                view
            }
            Err(e) if lost(&e) => {
                drop_live(&live, &reopen).await;
                continue;
            }
            Err(ClientError::Rpc(e)) if e.reason == Reason::NotFound => {
                // The daemon ended the session (or restarted and forgot it): open it again.
                live.stale.store(true, Ordering::Relaxed);
                reopen.notify_one();
                continue;
            }
            Err(e) => {
                report(&ServeEvent::Retrying {
                    error: e.to_string(),
                });
                let wait = reconnect_delay(errors);
                errors = errors.saturating_add(1);
                tokio::select! {
                    biased;
                    _ = stopped(&mut stop) => {
                        close(&live, cfg.close_wait).await;
                        return Ok(());
                    }
                    _ = tokio::time::sleep(wait) => continue,
                }
            }
        };

        let id = view.id.clone();
        let from = format!("{}/{}", view.from.person, view.from.name);
        let Some(question) = view.into_question() else {
            // Only asks and tells reach a responder; anything else gets no run and no reply.
            tracing::warn!(question = %id, "responder.take handed over a message that is not an ask or tell");
            continue;
        };
        report(&ServeEvent::Question {
            id: id.clone(),
            from,
        });
        let outcome = tokio::select! {
            biased;
            _ = stopped(&mut stop) => None,
            outcome = responder.answer(&question) => Some(outcome),
        };
        let text = match outcome {
            Some(Ok(answer)) if !answer.text.trim().is_empty() => {
                report(&ServeEvent::Answered {
                    id: id.clone(),
                    cost_usd: answer.cost_usd,
                });
                answer.text
            }
            Some(Ok(_)) => {
                let failure = RunFailure::new("the run gave an empty answer");
                report(&ServeEvent::Failed {
                    id: id.clone(),
                    reason: failure.reason.clone(),
                });
                failure.reply_text()
            }
            Some(Err(failure)) => {
                report(&ServeEvent::Failed {
                    id: id.clone(),
                    reason: failure.reason.clone(),
                });
                failure.reply_text()
            }
            None => {
                // Dropping the run killed its process group; the asker still gets a line instead of a wait
                // that runs out.
                let failure = RunFailure::new("the responder was stopped before the run finished");
                report(&ServeEvent::Failed {
                    id: id.clone(),
                    reason: failure.reason.clone(),
                });
                let last = Pending {
                    id,
                    text: failure.reply_text(),
                };
                finish(current(&link), Some(last), &cfg, &report).await;
                return Ok(());
            }
        };
        pending = Some(Pending { id, text });
    }
}

// A stopping loop's last steps: the reply still owed, if any, then `session.close`, each within `close_wait`.
async fn finish(live: Option<Arc<Live>>, last: Option<Pending>, cfg: &LoopConfig, report: &Report) {
    let Some(live) = live else {
        if let Some(p) = last {
            report(&ServeEvent::NotSent {
                id: p.id,
                error: "the responder stopped while the daemon was away".into(),
            });
        }
        return;
    };
    if let Some(p) = last {
        let params = ReplyParams {
            token: live.token.clone(),
            id: p.id.clone(),
            text: p.text,
            wait_seconds: None,
        };
        let event = match tokio::time::timeout(cfg.close_wait, live.client.reply(&params)).await {
            Ok(Ok(r)) => ServeEvent::Replied {
                id: p.id,
                status: r.status,
            },
            Ok(Err(e)) if already_replied(&e) => ServeEvent::Replied {
                id: p.id,
                status: SendStatus::Queued,
            },
            Ok(Err(e)) => ServeEvent::NotSent {
                id: p.id,
                error: e.to_string(),
            },
            Err(_) => ServeEvent::NotSent {
                id: p.id,
                error: "the daemon did not answer in time".into(),
            },
        };
        report(&event);
    }
    close(&live, cfg.close_wait).await;
}

// Resolves once a stop is asked for; a dropped sender counts as one.
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|s| *s).await;
}

// The newest usable session, waiting for the keeper when there is none.
async fn next_up(link: &mut watch::Receiver<Link>) -> Result<Arc<Live>, LoopError> {
    loop {
        {
            let now = link.borrow_and_update();
            match &*now {
                Link::Up(live) if live.usable() => return Ok(live.clone()),
                Link::Failed(e) => return Err(e.clone()),
                Link::Up(_) | Link::Connecting | Link::Down => {}
            }
        }
        if link.changed().await.is_err() {
            return Err(LoopError::Keeper);
        }
    }
}

fn current(link: &watch::Receiver<Link>) -> Option<Arc<Live>> {
    match &*link.borrow() {
        Link::Up(live) if live.usable() => Some(live.clone()),
        _ => None,
    }
}

async fn close(live: &Live, wait: Duration) {
    match tokio::time::timeout(wait, live.client.session_close(&live.token)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "could not close the responder session"),
        Err(_) => tracing::warn!("the daemon did not close the responder session in time"),
    }
}

// After a call failed for a lost connection: the keeper sees the close and reconnects. A connection that
// failed a call but stays open is dropped from use and the keeper opens the session again.
async fn drop_live(live: &Live, reopen: &Notify) {
    if tokio::time::timeout(Duration::from_secs(1), live.client.closed())
        .await
        .is_err()
    {
        live.stale.store(true, Ordering::Relaxed);
        reopen.notify_one();
    }
}

// Errors that mean the connection is gone, so the same call can go again on the next one.
fn lost(e: &ClientError) -> bool {
    matches!(e, ClientError::Disconnected | ClientError::Io(_))
}

// The daemon took an earlier send of this reply before the connection dropped: `message.reply` refuses a
// responder's second answer to one ask with a conflict. Where it is now is unknown here, hence `Queued`.
fn already_replied(e: &ClientError) -> bool {
    matches!(e, ClientError::Rpc(e) if e.reason == Reason::Conflict)
}

/// `<home>/run/stopped`: while it exists, a reconnecting responder doesn't start a daemon (6.1). `daemon stop`
/// writes it and a daemon that holds the lock removes it.
pub fn stopped_file(home: &Home) -> PathBuf {
    home.run_dir().join("stopped")
}

// The first connect may start a daemon; a reconnect starts one only while `run/stopped` is absent, so a
// `daemon stop` stays stopped and a crashed daemon comes back (6.1, 12).
fn reconnect_options(cfg: &LoopConfig, reconnect: bool) -> ConnectOptions {
    let mut opts = cfg.connect.clone();
    if reconnect && stopped_file(&cfg.home).exists() {
        opts.start = false;
    }
    opts
}

// Keeps one session open: connects, opens, and re-opens after a lost connection or when the loop asks.
async fn keep(
    cfg: Arc<LoopConfig>,
    mut client: Option<Client>,
    link: watch::Sender<Link>,
    reopen: Arc<Notify>,
    report: Report,
) {
    let mut attempt: u32 = 0;
    let mut opened = false;
    loop {
        let c = match client.take().filter(|c| !c.is_closed()) {
            Some(c) => c,
            None => match Client::connect(&cfg.home, &reconnect_options(&cfg, opened)).await {
                Ok(c) => c,
                Err(e) => {
                    // The first connection decides whether serve starts at all; another user's socket is
                    // never retried.
                    if !opened || matches!(e, ClientError::OtherUser(_)) {
                        link.send_replace(Link::Failed(LoopError::Connect(e.to_string())));
                        return;
                    }
                    report(&ServeEvent::Retrying {
                        error: e.to_string(),
                    });
                    tokio::time::sleep(reconnect_delay(attempt)).await;
                    attempt = attempt.saturating_add(1);
                    continue;
                }
            },
        };
        match c.session_open(&cfg.open).await {
            Ok(s) => {
                attempt = 0;
                report(&ServeEvent::Ready {
                    address: s.address,
                    again: opened,
                });
                opened = true;
                link.send_replace(Link::Up(Arc::new(Live {
                    client: c.clone(),
                    token: s.token,
                    stale: AtomicBool::new(false),
                })));
                tokio::select! {
                    _ = c.closed() => {
                        link.send_replace(Link::Down);
                        report(&ServeEvent::Lost);
                    }
                    _ = reopen.notified() => client = Some(c),
                }
            }
            Err(e) if !opened => {
                let err = match e {
                    ClientError::Rpc(e) => LoopError::Open(e.message),
                    other => LoopError::Connect(other.to_string()),
                };
                link.send_replace(Link::Failed(err));
                return;
            }
            Err(e) => {
                report(&ServeEvent::Retrying {
                    error: e.to_string(),
                });
                if !lost(&e) {
                    client = Some(c);
                }
                tokio::time::sleep(reconnect_delay(attempt)).await;
                attempt = attempt.saturating_add(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_loop_config_opens_a_responder_with_its_limits() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path()).unwrap();
        let limits = GateSettings::from(&super::super::ServeSettings::default());
        let a = LoopConfig::new(
            home.clone(),
            ServeClient::Codex,
            "/w/api".into(),
            Some("web-help".into()),
            None,
            limits,
        );
        let b = LoopConfig::new(
            home,
            ServeClient::Codex,
            "/w/api".into(),
            None,
            None,
            limits,
        );
        assert_eq!(a.open.kind, SessionKind::Responder);
        assert_eq!(a.open.client, "codex");
        assert_eq!(a.open.name.as_deref(), Some("web-help"));
        assert_eq!(a.open.serve, Some(limits));
        assert_eq!(a.open.client_pid, None);
        let cs = a.open.client_session.clone().unwrap();
        assert!(cs.starts_with("serve-"), "{cs}");
        assert_ne!(a.open.client_session, b.open.client_session);
    }

    #[test]
    fn a_reconnect_starts_no_daemon_while_run_stopped_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path()).unwrap();
        home.ensure().unwrap();
        let limits = GateSettings::from(&super::super::ServeSettings::default());
        let cfg = LoopConfig::new(
            home.clone(),
            ServeClient::Claude,
            "/w".into(),
            None,
            None,
            limits,
        );
        assert!(reconnect_options(&cfg, false).start);
        assert!(reconnect_options(&cfg, true).start);
        std::fs::write(stopped_file(&home), "").unwrap();
        assert!(
            reconnect_options(&cfg, false).start,
            "the first connect still starts one"
        );
        assert!(!reconnect_options(&cfg, true).start);
        assert!(!reconnect_options(&cfg, true).upgrade);
    }

    #[test]
    fn a_resent_reply_the_daemon_already_has_counts_as_replied() {
        use crate::control::protocol::RpcError;
        assert!(already_replied(&ClientError::Rpc(RpcError::conflict(
            "01J is already answered"
        ))));
        assert!(!already_replied(&ClientError::Rpc(RpcError::not_found(
            "gone"
        ))));
        assert!(!already_replied(&ClientError::Disconnected));
    }

    #[test]
    fn only_a_dropped_connection_is_retried_on_the_next_one() {
        assert!(lost(&ClientError::Disconnected));
        assert!(lost(&ClientError::Io(std::io::Error::other("reset"))));
        assert!(!lost(&ClientError::NotRunning));
        assert!(!lost(&ClientError::Rpc(
            crate::control::protocol::RpcError::not_found("gone")
        )));
    }
}
