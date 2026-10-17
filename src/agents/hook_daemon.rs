//! The hook's daemon calls (6.1, 6.2, 8.1): [`SocketHookDaemon`] implements [`HookDaemon`] over one short-lived
//! blocking connection. It connects within the hook deadline, starts a missing daemon only for `start` and
//! `prompt` events (without waiting for it), and never replaces a daemon of another version.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail};

use super::Client;
use super::hook::{HookAnswer, HookCall, HookDaemon, MeshEvent};
use crate::control::blocking::BlockingClient;
use crate::control::client::HOOK_DEADLINE;
use crate::control::protocol::{Ack, Empty, method};
use crate::paths::Home;

/// The hook's connection to the daemon, made at its first call.
#[derive(Debug)]
pub struct SocketHookDaemon {
    home: Home,
    exe: Option<PathBuf>,
    conn: Option<BlockingClient>,
}

impl SocketHookDaemon {
    /// A client for the daemon of `home`. A missing daemon is started with `exe`, or this process's own
    /// executable when `None` (6.1).
    pub fn new(home: Home, exe: Option<PathBuf>) -> SocketHookDaemon {
        SocketHookDaemon {
            home,
            exe,
            conn: None,
        }
    }

    // The connection, made within `limit` (at most the 1 s hook deadline) when there is none yet.
    fn connect(&mut self, limit: Duration, start_missing: bool) -> anyhow::Result<&BlockingClient> {
        if self.conn.is_none() {
            let conn = BlockingClient::connect(
                &self.home,
                limit.min(HOOK_DEADLINE),
                start_missing,
                self.exe.clone(),
            )?;
            self.conn = Some(conn);
        }
        self.conn
            .as_ref()
            .context("the daemon connection went away")
    }
}

/// Only the events that begin work start a missing daemon (6.1); the others just exit.
fn starts_daemon(event: MeshEvent) -> bool {
    matches!(event, MeshEvent::Start | MeshEvent::Prompt)
}

fn left(start: Instant, limit: Duration) -> anyhow::Result<Duration> {
    match limit.checked_sub(start.elapsed()) {
        Some(d) if !d.is_zero() => Ok(d),
        _ => bail!("the hook ran out of time"),
    }
}

impl HookDaemon for SocketHookDaemon {
    fn event(&mut self, call: &HookCall, limit: Duration) -> anyhow::Result<HookAnswer> {
        let start = Instant::now();
        let conn = self.connect(limit, starts_daemon(call.event))?;
        Ok(conn.call(method::HOOK_EVENT, call, left(start, limit)?)?)
    }

    fn send(&mut self, call: &HookCall, limit: Duration) -> anyhow::Result<()> {
        let start = Instant::now();
        let conn = self.connect(limit, starts_daemon(call.event))?;
        conn.send(method::HOOK_EVENT, call, left(start, limit)?)?;
        Ok(())
    }

    fn ack(
        &mut self,
        client: Client,
        client_session: &str,
        ids: &[String],
        limit: Duration,
    ) -> anyhow::Result<()> {
        let start = Instant::now();
        // The answer being acknowledged came over this connection, so a missing one is never started here.
        let conn = self.connect(limit, false)?;
        let params = Ack {
            token: None,
            client: Some(client.slug().to_string()),
            client_session: Some(client_session.to_string()),
            ids: ids.to_vec(),
        };
        let _: Empty = conn.call(method::MESSAGES_ACK, &params, left(start, limit)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_start_and_prompt_start_a_daemon() {
        assert!(starts_daemon(MeshEvent::Start));
        assert!(starts_daemon(MeshEvent::Prompt));
        assert!(!starts_daemon(MeshEvent::Tool));
        assert!(!starts_daemon(MeshEvent::Stop));
        assert!(!starts_daemon(MeshEvent::End));
    }

    #[test]
    fn no_daemon_is_an_error_and_a_stop_starts_none() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home")).unwrap();
        let marker = tmp.path().join("started");
        let fake = tmp.path().join("fake-spew");
        std::fs::write(&fake, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        crate::paths::set_mode(&fake, 0o700).unwrap();
        let mut d = SocketHookDaemon::new(home, Some(fake));
        let call = HookCall {
            client: Client::Claude,
            event: MeshEvent::Stop,
            client_session: "s1".into(),
            client_pid: None,
            cwd: "/w".into(),
            show: false,
            facts: Default::default(),
        };
        let t = Instant::now();
        assert!(d.event(&call, Duration::from_secs(5)).is_err());
        assert!(
            t.elapsed() < Duration::from_millis(1500),
            "{:?}",
            t.elapsed()
        );
        std::thread::sleep(Duration::from_millis(200));
        assert!(!marker.exists(), "a stop hook must not start the daemon");
        assert!(
            d.ack(Client::Claude, "s1", &["m".into()], Duration::from_secs(1))
                .is_err()
        );
        assert!(left(Instant::now(), Duration::ZERO).is_err());
    }
}
