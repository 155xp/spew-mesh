//! Finding agent commands on PATH, running them, and reading their versions (8.2 to 8.4).

use std::ffi::OsString;
use std::fmt;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::Client;

/// The result of a finished command.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunOutput {
    /// The exit code; `None` when a signal ended it.
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl RunOutput {
    /// True for exit code 0.
    pub fn success(&self) -> bool {
        self.status == Some(0)
    }

    /// The first non-empty line of stderr (else stdout), for messages.
    pub fn first_error_line(&self) -> String {
        self.stderr
            .lines()
            .chain(self.stdout.lines())
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("no output")
            .to_string()
    }
}

/// Finds and runs programs. setup and doctor go through this so tests can use fake agent commands.
pub trait Runner {
    /// The absolute path of `program` on PATH, if it is there and executable.
    fn find(&self, program: &str) -> Option<PathBuf>;
    /// Runs `program` with `args`, stdin closed, and waits for it within the runner's time limit.
    fn run(&self, program: &Path, args: &[String]) -> Result<RunOutput>;
}

/// Runs real processes, with PATH and extra environment variables given explicitly.
#[derive(Clone, Debug)]
pub struct SystemRunner {
    path: OsString,
    env: Vec<(OsString, OsString)>,
    timeout: Duration,
}

impl SystemRunner {
    /// A runner searching `path` (a PATH value) that gives each command `timeout`.
    pub fn new(path: impl Into<OsString>, timeout: Duration) -> SystemRunner {
        SystemRunner {
            path: path.into(),
            env: Vec::new(),
            timeout,
        }
    }

    /// A runner using this process's PATH and a 20 s limit per command.
    pub fn from_process_env() -> SystemRunner {
        SystemRunner::new(
            std::env::var_os("PATH").unwrap_or_default(),
            Duration::from_secs(20),
        )
    }

    /// Sets an environment variable for every command run.
    pub fn with_env(
        mut self,
        key: impl Into<OsString>,
        value: impl Into<OsString>,
    ) -> SystemRunner {
        self.env.push((key.into(), value.into()));
        self
    }
}

impl Runner for SystemRunner {
    fn find(&self, program: &str) -> Option<PathBuf> {
        if program.is_empty() || program.contains('/') {
            return None;
        }
        std::env::split_paths(&self.path)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(program))
            .find(|p| {
                std::fs::metadata(p)
                    .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            })
    }

    fn run(&self, program: &Path, args: &[String]) -> Result<RunOutput> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .env("PATH", &self.path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        let mut child =
            spawn_retrying(&mut cmd).with_context(|| format!("running {}", program.display()))?;
        let out = child.stdout.take().map(read_capped);
        let err = child.stderr.take().map(read_capped);
        let start = Instant::now();
        let status = loop {
            match child
                .try_wait()
                .with_context(|| format!("waiting for {}", program.display()))?
            {
                Some(s) => break s,
                None if start.elapsed() >= self.timeout => {
                    let _ = child.kill();
                    let _ = child.wait();
                    bail!(
                        "{} didn't finish within {} s",
                        program.display(),
                        self.timeout.as_secs()
                    );
                }
                None => thread::sleep(Duration::from_millis(10)),
            }
        };
        let join = |h: Option<thread::JoinHandle<String>>| {
            h.and_then(|h| h.join().ok()).unwrap_or_default()
        };
        Ok(RunOutput {
            status: status.code(),
            stdout: join(out),
            stderr: join(err),
        })
    }
}

fn spawn_retrying(cmd: &mut Command) -> std::io::Result<std::process::Child> {
    // A script written just before (as by an updater, or a test) can briefly be "text file busy" (ETXTBSY).
    let mut tries = 0;
    loop {
        match cmd.spawn() {
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && tries < 20 => {
                tries += 1;
                thread::sleep(Duration::from_millis(25));
            }
            other => return other,
        }
    }
}

fn read_capped<R: Read + Send + 'static>(r: R) -> thread::JoinHandle<String> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.take(1 << 20).read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    })
}

/// A `major.minor.patch` version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64, pub u64, pub u64);

impl Version {
    /// Finds the first `N.N.N` in a `--version` line, as `2.1.302 (Claude Code)` or `codex-cli 0.163.0`.
    pub fn find_in(text: &str) -> Option<Version> {
        text.split(|c: char| !(c.is_ascii_digit() || c == '.'))
            .find_map(|tok| {
                let mut parts = tok.split('.');
                let v = Version(
                    parts.next()?.parse().ok()?,
                    parts.next()?.parse().ok()?,
                    parts.next()?.parse().ok()?,
                );
                Some(v)
            })
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// The oldest version of each client spew mesh supports (8.2 to 8.4).
pub fn min_version(client: Client) -> Version {
    match client {
        Client::Claude => Version(2, 1, 259),
        Client::Codex => Version(0, 163, 0),
        Client::Opencode => Version(1, 19, 3),
    }
}

/// What setup found out about a client's command.
#[derive(Clone, Debug, PartialEq)]
pub enum Found {
    /// On PATH, with a supported version.
    Ok { path: PathBuf, version: Version },
    /// On PATH but older than the minimum.
    TooOld { path: PathBuf, version: Version },
    /// On PATH, but `--version` failed or printed no version.
    Unreadable { path: PathBuf, why: String },
    /// Not on PATH.
    Missing,
}

impl Found {
    /// The command's path, when it is on PATH.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Found::Ok { path, .. }
            | Found::TooOld { path, .. }
            | Found::Unreadable { path, .. } => Some(path),
            Found::Missing => None,
        }
    }
}

/// Looks for the client's command and reads `<command> --version`.
pub fn detect(runner: &dyn Runner, client: Client) -> Found {
    let Some(path) = runner.find(client.slug()) else {
        return Found::Missing;
    };
    match runner.run(&path, &["--version".to_string()]) {
        Ok(out) if out.success() => match Version::find_in(&out.stdout) {
            Some(version) if version >= min_version(client) => Found::Ok { path, version },
            Some(version) => Found::TooOld { path, version },
            None => Found::Unreadable {
                path,
                why: format!("it printed no version ({})", out.first_error_line()),
            },
        },
        Ok(out) => Found::Unreadable {
            path,
            why: format!(
                "`{} --version` failed: {}",
                client.slug(),
                out.first_error_line()
            ),
        },
        Err(e) => Found::Unreadable {
            path,
            why: format!("{e:#}"),
        },
    }
}

/// The line setup and doctor print for a client that is too old.
pub fn too_old_line(client: Client, version: Version) -> String {
    format!(
        "{client} {version} is too old: spew mesh needs {} or newer. Skipped; update {client} and run setup again.",
        min_version(client)
    )
}

#[cfg(test)]
pub(crate) mod testing {
    //! Fake agent commands: small shell scripts that log their arguments and print canned output.

    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    /// Writes an executable script `name` into `dir` that appends its arguments to `<dir>/<name>.log` and runs
    /// `body` (shell). `"$@"` is the argument list.
    pub fn fake_command(dir: &Path, name: &str, body: &str) {
        let log = dir.join(format!("{name}.log"));
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n{body}\n",
            log.display()
        );
        let path = dir.join(name);
        let tmp = dir.join(format!(".{name}.tmp"));
        fs::write(&tmp, script).unwrap();
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)).unwrap();
        fs::rename(&tmp, &path).unwrap();
    }

    /// The logged argument lines of a fake command.
    pub fn calls(dir: &Path, name: &str) -> Vec<String> {
        fs::read_to_string(dir.join(format!("{name}.log")))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn reads_versions_from_each_client_format() {
        assert_eq!(
            Version::find_in("2.1.302 (Claude Code)"),
            Some(Version(2, 1, 302))
        );
        assert_eq!(
            Version::find_in("codex-cli 0.163.0\n"),
            Some(Version(0, 163, 0))
        );
        assert_eq!(Version::find_in("1.19.3"), Some(Version(1, 19, 3)));
        assert_eq!(Version::find_in("v1.2"), None);
        assert!(Version(2, 1, 259) < Version(2, 1, 302));
        assert!(Version(0, 99, 0) < Version(0, 163, 0));
    }

    #[test]
    fn detects_fake_commands_and_their_versions() {
        let dir = tempfile::tempdir().unwrap();
        fake_command(dir.path(), "claude", "echo '2.1.302 (Claude Code)'");
        fake_command(dir.path(), "codex", "echo 'codex-cli 0.150.1'");
        fake_command(dir.path(), "opencode", "echo boom >&2; exit 3");
        let r = SystemRunner::new(dir.path().as_os_str(), Duration::from_secs(10));
        let claude = dir.path().join("claude");
        assert_eq!(
            detect(&r, Client::Claude),
            Found::Ok {
                path: claude,
                version: Version(2, 1, 302)
            }
        );
        assert!(matches!(
            detect(&r, Client::Codex),
            Found::TooOld {
                version: Version(0, 150, 1),
                ..
            }
        ));
        match detect(&r, Client::Opencode) {
            Found::Unreadable { why, .. } => assert!(why.contains("boom"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(calls(dir.path(), "claude"), vec!["--version"]);
        let empty = SystemRunner::new("", Duration::from_secs(1));
        assert_eq!(detect(&empty, Client::Claude), Found::Missing);
        assert!(too_old_line(Client::Codex, Version(0, 150, 1)).contains("needs 0.163.0 or newer"));
    }

    #[test]
    fn a_hanging_command_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        fake_command(dir.path(), "claude", "while :; do :; done");
        let r = SystemRunner::new(dir.path().as_os_str(), Duration::from_millis(200));
        let err = r.run(&dir.path().join("claude"), &[]).unwrap_err();
        assert!(err.to_string().contains("didn't finish"), "{err}");
    }
}
