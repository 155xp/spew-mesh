//! Spawning one headless run: its own session and process group, the prompt on stdin, a timeout that kills
//! the whole group, and a cap on what it may print (9).

use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

/// Stderr kept per run; the rest is read and dropped so the child never blocks on a full pipe.
pub const STDERR_CAP: usize = 64 * 1024;

/// How long the pipes may stay open after the run's main process exits.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// One process to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSpec {
    /// Absolute path of the executable.
    pub program: PathBuf,
    /// Arguments after the program name.
    pub args: Vec<OsString>,
    /// Working directory.
    pub cwd: PathBuf,
    /// The child's whole environment; nothing else is inherited.
    pub env: Vec<(OsString, OsString)>,
    /// Written to stdin, which is then closed.
    pub stdin: Vec<u8>,
    /// After this long the process group is killed.
    pub timeout: Duration,
    /// Printing more than this on stdout kills the process group.
    pub stdout_cap: usize,
}

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// Exited with this status.
    Code(i32),
    /// Killed by this signal (not by us).
    Signal(i32),
    /// Ran past its timeout; the group was killed.
    TimedOut,
    /// Printed more than the stdout cap; the group was killed.
    OutputTooLarge,
}

impl Exit {
    /// A short phrase for messages, as in "codex exited with status 1".
    pub fn describe(self) -> String {
        match self {
            Exit::Code(code) => format!("exited with status {code}"),
            Exit::Signal(sig) => format!("was killed by signal {sig}"),
            Exit::TimedOut => "ran past its timeout".to_string(),
            Exit::OutputTooLarge => "printed more output than allowed".to_string(),
        }
    }
}

/// What a run printed and how it ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunOutput {
    /// How it ended.
    pub exit: Exit,
    /// Stdout, at most the cap.
    pub stdout: Vec<u8>,
    /// The first [`STDERR_CAP`] bytes of stderr.
    pub stderr: Vec<u8>,
    /// Wall time from spawn to exit.
    pub elapsed: Duration,
}

impl RunOutput {
    /// True for exit status 0.
    pub fn success(&self) -> bool {
        self.exit == Exit::Code(0)
    }

    /// Stdout as text, with invalid UTF-8 replaced.
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    /// Stderr as text, with invalid UTF-8 replaced.
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// Runs processes. The real one is [`SystemRunner`]; tests substitute their own.
pub trait ProcessRunner: Send + Sync {
    /// Runs one process to completion, timeout or cap. An error means it could not be started.
    fn run(&self, spec: RunSpec) -> impl Future<Output = io::Result<RunOutput>> + Send;
}

/// Spawns real processes.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemRunner;

impl ProcessRunner for SystemRunner {
    fn run(&self, spec: RunSpec) -> impl Future<Output = io::Result<RunOutput>> + Send {
        run_system(spec)
    }
}

/// Kills the run's process group if the run is abandoned before its leader is reaped (the future dropped
/// mid-run). Disarmed once the leader is reaped, so a recycled pid is never signalled.
struct GroupGuard {
    pgid: libc::pid_t,
    armed: bool,
}

impl GroupGuard {
    fn kill(&self) {
        // SAFETY: killpg only sends a signal. The group id is our child's pid, which can't be reused while
        // the child is unreaped.
        unsafe {
            libc::killpg(self.pgid, libc::SIGKILL);
        }
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if self.armed {
            self.kill();
        }
    }
}

async fn run_system(spec: RunSpec) -> io::Result<RunOutput> {
    let start = Instant::now();
    let mut cmd = tokio::process::Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // A new session gives the run its own process group and no controlling terminal, so it can't read
    // from or write to the person's terminal and the whole group can be killed at once.
    // SAFETY: the closure runs between fork and exec and only calls setsid, which is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    let pid = child
        .id()
        .and_then(|id| libc::pid_t::try_from(id).ok())
        .ok_or_else(|| io::Error::other("the child has no pid"))?;
    let mut guard = GroupGuard {
        pgid: pid,
        armed: true,
    };

    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut out_buf = Vec::new();
    let mut err_buf = Vec::new();
    let overflow = AtomicBool::new(false);

    let finished = {
        let input = spec.stdin.as_slice();
        let cap = spec.stdout_cap;
        let out_buf = &mut out_buf;
        let err_buf = &mut err_buf;
        let overflow = &overflow;
        let guard = &guard;
        let child = &mut child;
        let work = async move {
            let write = async move {
                if let Some(mut pipe) = stdin {
                    // A child that exits without reading its stdin gives a broken pipe; that's its business.
                    let _ = pipe.write_all(input).await;
                    let _ = pipe.shutdown().await;
                }
            };
            let read_out = async move {
                if let Some(pipe) = stdout
                    && read_capped(pipe, out_buf, cap).await
                {
                    overflow.store(true, Ordering::SeqCst);
                    guard.kill();
                }
            };
            let read_err = async move {
                if let Some(pipe) = stderr {
                    read_keep_first(pipe, err_buf, STDERR_CAP).await;
                }
            };
            let io = async move {
                tokio::join!(write, read_out, read_err);
            };
            tokio::pin!(io);
            tokio::select! {
                () = &mut io => child.wait().await,
                status = child.wait() => {
                    // The leader is gone, but something it started may still hold the pipes. Let them
                    // drain briefly, then stop the rest of the group. The leader is reaped by now, so
                    // this signal relies on its pid not being reused within the grace period.
                    if tokio::time::timeout(DRAIN_GRACE, &mut io).await.is_err() {
                        guard.kill();
                    }
                    status
                }
            }
        };
        tokio::time::timeout(spec.timeout, work).await
    };

    let exit = match finished {
        Ok(status) => {
            guard.armed = false;
            let status = status?;
            if overflow.load(Ordering::SeqCst) {
                Exit::OutputTooLarge
            } else if let Some(code) = status.code() {
                Exit::Code(code)
            } else {
                Exit::Signal(status.signal().unwrap_or(0))
            }
        }
        Err(_elapsed) => {
            guard.kill();
            // Reap the leader; after SIGKILL this returns promptly.
            let _ = child.wait().await;
            guard.armed = false;
            Exit::TimedOut
        }
    };

    Ok(RunOutput {
        exit,
        stdout: out_buf,
        stderr: err_buf,
        elapsed: start.elapsed(),
    })
}

/// Reads until EOF or until more than `cap` bytes arrived; returns true on overflow.
async fn read_capped<R: AsyncRead + Unpin>(mut r: R, buf: &mut Vec<u8>, cap: usize) -> bool {
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => return false,
            Ok(n) => {
                if buf.len() + n > cap {
                    let room = cap.saturating_sub(buf.len());
                    buf.extend_from_slice(&chunk[..room]);
                    return true;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
        }
    }
}

/// Reads until EOF, keeping the first `cap` bytes.
async fn read_keep_first<R: AsyncRead + Unpin>(mut r: R, buf: &mut Vec<u8>, cap: usize) {
    let mut chunk = [0u8; 8192];
    loop {
        match r.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let room = cap.saturating_sub(buf.len()).min(n);
                buf.extend_from_slice(&chunk[..room]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn spec(program: PathBuf, dir: &Path) -> RunSpec {
        RunSpec {
            program,
            args: vec![],
            cwd: dir.to_path_buf(),
            env: vec![
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("REC".into(), dir.as_os_str().to_owned()),
            ],
            stdin: vec![],
            timeout: Duration::from_secs(10),
            stdout_cap: 1024 * 1024,
        }
    }

    #[tokio::test]
    async fn passes_stdin_args_env_and_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(
            dir.path(),
            "fake",
            r#"cat > "$REC/stdin"; printf '%s\n' "$@" > "$REC/argv"; env > "$REC/env"; pwd > "$REC/pwd"; echo out; echo err >&2; exit 3"#,
        );
        let mut s = spec(program, dir.path());
        s.args = vec!["a b".into(), "--flag".into()];
        s.stdin = b"the prompt\nline two".to_vec();
        let out = SystemRunner.run(s).await.unwrap();
        assert_eq!(out.exit, Exit::Code(3));
        assert_eq!(out.stdout, b"out\n");
        assert_eq!(out.stderr, b"err\n");
        let read = |name: &str| std::fs::read_to_string(dir.path().join(name)).unwrap();
        assert_eq!(read("stdin"), "the prompt\nline two");
        assert_eq!(read("argv"), "a b\n--flag\n");
        let env = read("env");
        let mut names: Vec<&str> = env
            .lines()
            .filter_map(|l| l.split('=').next())
            .filter(|n| *n != "PWD" && *n != "SHLVL" && *n != "_" && *n != "OLDPWD")
            .collect();
        names.sort();
        assert_eq!(names, ["PATH", "REC"]);
        let pwd = std::fs::canonicalize(read("pwd").trim()).unwrap();
        assert_eq!(pwd, std::fs::canonicalize(dir.path()).unwrap());
    }

    #[tokio::test]
    async fn runs_in_its_own_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(
            dir.path(),
            "fake",
            r#"echo $$ > "$REC/pid"; ps -o pgid= -p $$ > "$REC/pgid""#,
        );
        let out = SystemRunner.run(spec(program, dir.path())).await.unwrap();
        assert_eq!(out.exit, Exit::Code(0));
        let pid = std::fs::read_to_string(dir.path().join("pid")).unwrap();
        let pgid = std::fs::read_to_string(dir.path().join("pgid")).unwrap();
        assert_eq!(pid.trim(), pgid.trim());
    }

    #[tokio::test]
    async fn timeout_kills_the_whole_group() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(
            dir.path(),
            "fake",
            r#"sleep 30 & echo $! > "$REC/bg"; echo started; sleep 30"#,
        );
        let mut s = spec(program, dir.path());
        s.timeout = Duration::from_millis(700);
        let started = Instant::now();
        let out = SystemRunner.run(s).await.unwrap();
        assert_eq!(out.exit, Exit::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(out.stdout, b"started\n");
        let bg: i32 = std::fs::read_to_string(dir.path().join("bg"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(wait_gone(bg).await, "background child {bg} survived");
    }

    #[tokio::test]
    async fn a_leftover_child_holding_the_pipes_does_not_stall_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(
            dir.path(),
            "fake",
            r#"sleep 30 & echo $! > "$REC/bg"; echo answer; exit 0"#,
        );
        let started = Instant::now();
        let out = SystemRunner.run(spec(program, dir.path())).await.unwrap();
        assert_eq!(out.exit, Exit::Code(0));
        assert_eq!(out.stdout, b"answer\n");
        assert!(started.elapsed() < Duration::from_secs(8));
        let bg: i32 = std::fs::read_to_string(dir.path().join("bg"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(wait_gone(bg).await, "leftover child {bg} survived");
    }

    #[tokio::test]
    async fn output_over_the_cap_kills_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(
            dir.path(),
            "fake",
            "while :; do echo xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; done",
        );
        let mut s = spec(program, dir.path());
        s.stdout_cap = 10_000;
        let out = SystemRunner.run(s).await.unwrap();
        assert_eq!(out.exit, Exit::OutputTooLarge);
        assert_eq!(out.stdout.len(), 10_000);
    }

    #[tokio::test]
    async fn stderr_is_drained_past_its_cap() {
        let dir = tempfile::tempdir().unwrap();
        let program = script(
            dir.path(),
            "fake",
            "i=0; while [ $i -lt 3000 ]; do echo eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee >&2; i=$((i+1)); done; echo done",
        );
        let out = SystemRunner.run(spec(program, dir.path())).await.unwrap();
        assert_eq!(out.exit, Exit::Code(0));
        assert_eq!(out.stderr.len(), STDERR_CAP);
        assert_eq!(out.stdout, b"done\n");
    }

    #[tokio::test]
    async fn missing_program_is_a_start_error() {
        let dir = tempfile::tempdir().unwrap();
        let s = spec(dir.path().join("nope"), dir.path());
        assert!(SystemRunner.run(s).await.is_err());
    }

    /// True once the process is gone or a zombie nobody reaped yet.
    pub(crate) async fn wait_gone(pid: i32) -> bool {
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
}
