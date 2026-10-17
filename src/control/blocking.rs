//! A blocking control client for hooks (6.1): one short-lived connection with a hard deadline, no upgrades, and
//! for `start` and `prompt` hooks a missing daemon started in the background without waiting for it.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::client::{Client, ClientError};
use super::place::{lock_is_free, read_socket_path, spawn_daemon};
use crate::paths::Home;

/// A blocking connection to the daemon. Each call takes its own time limit.
pub struct BlockingClient {
    rt: tokio::runtime::Runtime,
    client: Client,
}

impl std::fmt::Debug for BlockingClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockingClient")
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

fn runtime() -> Result<tokio::runtime::Runtime, ClientError> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

fn timed_out(what: &str) -> ClientError {
    ClientError::Protocol(format!("{what} took longer than the hook may wait"))
}

impl BlockingClient {
    /// Connects to a running daemon within `limit`. When none answers and `start_missing` is set (the `start`
    /// and `prompt` hooks), starts one with `exe` (this process's own executable when `None`) without waiting,
    /// and still returns [`ClientError::NotRunning`]: the hook prints nothing this time (6.1).
    pub fn connect(
        home: &Home,
        limit: Duration,
        start_missing: bool,
        exe: Option<PathBuf>,
    ) -> Result<BlockingClient, ClientError> {
        let deadline = Instant::now() + limit;
        let rt = runtime()?;
        let found = read_socket_path(home).map(|path| {
            rt.block_on(async {
                tokio::time::timeout_at(deadline.into(), Client::connect_path(&path)).await
            })
        });
        match found {
            Some(Ok(Ok(client))) => return Ok(BlockingClient { rt, client }),
            Some(Ok(Err(ClientError::OtherUser(p)))) => return Err(ClientError::OtherUser(p)),
            _ => {}
        }
        if start_missing && lock_is_free(home) {
            let exe = match exe {
                Some(exe) => exe,
                None => std::env::current_exe()?,
            };
            spawn_daemon(home, &exe)?;
        }
        Err(ClientError::NotRunning)
    }

    /// Sends one request and waits up to `limit` for the answer.
    pub fn call<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: &P,
        limit: Duration,
    ) -> Result<R, ClientError> {
        self.rt
            .block_on(async { tokio::time::timeout(limit, self.client.call(method, params)).await })
            .map_err(|_| timed_out(method))?
    }

    /// Writes one request without waiting for its answer, taking at most `limit`.
    pub fn send<P: Serialize>(
        &self,
        method: &str,
        params: &P,
        limit: Duration,
    ) -> Result<(), ClientError> {
        let params = serde_json::to_value(params)
            .map_err(|e| ClientError::Protocol(format!("could not encode params: {e}")))?;
        self.rt
            .block_on(async {
                tokio::time::timeout(limit, self.client.send_value(method, params)).await
            })
            .map_err(|_| timed_out(method))?
    }

    /// Sends one request with untyped params and result.
    pub fn call_value(
        &self,
        method: &str,
        params: Value,
        limit: Duration,
    ) -> Result<Value, ClientError> {
        self.rt
            .block_on(async {
                tokio::time::timeout(limit, self.client.call_value(method, params)).await
            })
            .map_err(|_| timed_out(method))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_daemon_is_started_without_waiting() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home")).unwrap();
        let marker = tmp.path().join("started");
        let fake = tmp.path().join("fake-spew");
        std::fs::write(
            &fake,
            format!("#!/bin/sh\ntouch '{}'\nsleep 2\n", marker.display()),
        )
        .unwrap();
        crate::paths::set_mode(&fake, 0o700).unwrap();
        let begun = Instant::now();
        let err = BlockingClient::connect(&home, Duration::from_secs(1), true, Some(fake.clone()))
            .unwrap_err();
        assert!(matches!(err, ClientError::NotRunning));
        assert!(begun.elapsed() < Duration::from_millis(900));
        for _ in 0..50 {
            if marker.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(marker.exists(), "the daemon was never started");

        // Other hooks never start one.
        std::fs::remove_file(&marker).unwrap();
        let err =
            BlockingClient::connect(&home, Duration::from_secs(1), false, Some(fake)).unwrap_err();
        assert!(matches!(err, ClientError::NotRunning));
        std::thread::sleep(Duration::from_millis(200));
        assert!(!marker.exists());
    }
}
