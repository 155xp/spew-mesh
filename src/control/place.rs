//! Where the control socket lives and how the daemon claims it (6.1): the socket path, the lock, stale socket
//! removal, binding 0600 under a strict umask, `run/socket`, uid checks, and starting a daemon from a client.

use std::ffi::{OsStr, OsString};
use std::fs::{File, TryLockError};
use std::io::{self, Write};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use crate::paths::{
    Home, LOG_ROTATE_BYTES, ensure_private_dir, open_private_append, private_open_options,
    rotate_log, write_atomic,
};

/// The socket path must be shorter than this many bytes to live in the home (6.1); Unix socket paths are
/// limited to about 104 bytes.
pub const MAX_SOCKET_PATH: usize = 100;

const RANDOM_CHARS: usize = 10;
const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// The control socket for a home (6.1): `<home>/run/ipc.sock` when that path is shorter than 100 bytes, else
/// `spew-mesh-<10 random base32 characters>.sock` in the first of `runtime_dirs` that exists. The caller passes
/// `$XDG_RUNTIME_DIR`, `$TMPDIR` and `/tmp` in that order.
pub fn choose_socket_path(home: &Home, runtime_dirs: &[Option<PathBuf>]) -> io::Result<PathBuf> {
    let ipc = home.ipc_socket();
    if ipc.as_os_str().len() < MAX_SOCKET_PATH {
        return Ok(ipc);
    }
    let dir = runtime_dirs
        .iter()
        .flatten()
        .find(|d| d.is_absolute() && d.is_dir())
        .ok_or_else(|| {
            io::Error::other(
                "the home path is too long for a socket and no runtime folder exists ($XDG_RUNTIME_DIR, $TMPDIR, /tmp)",
            )
        })?;
    Ok(dir.join(format!("spew-mesh-{}.sock", random_base32(RANDOM_CHARS)?)))
}

/// The runtime folders 6.1 names, from this process's environment.
pub fn runtime_dirs_from_env() -> Vec<Option<PathBuf>> {
    let var = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    vec![
        var("XDG_RUNTIME_DIR"),
        var("TMPDIR"),
        Some(PathBuf::from("/tmp")),
    ]
}

fn random_base32(n: usize) -> io::Result<String> {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(bytes
        .iter()
        .map(|b| char::from(BASE32[usize::from(b & 31)]))
        .collect())
}

/// The exclusive lock on `<home>/run/lock`, held for the daemon's whole life (6.1).
#[derive(Debug)]
pub struct DaemonLock {
    _file: File,
}

/// Takes the daemon lock and records this process's pid and start time in it. `Ok(None)` when another daemon
/// holds it.
pub fn take_lock(home: &Home, started_at_ms: u64) -> io::Result<Option<DaemonLock>> {
    ensure_private_dir(&home.run_dir()).map_err(io::Error::other)?;
    let path = home.lock_file();
    let mut file = private_open_options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(None),
        Err(TryLockError::Error(e)) => return Err(e),
    }
    file.set_len(0)?;
    writeln!(file, "{} {started_at_ms}", std::process::id())?;
    file.flush()?;
    Ok(Some(DaemonLock { _file: file }))
}

/// Whether no daemon holds the lock for this home.
pub fn lock_is_free(home: &Home) -> bool {
    let Ok(file) = private_open_options().read(true).open(home.lock_file()) else {
        return true;
    };
    // The probe's lock is released when `file` drops.
    !matches!(file.try_lock(), Err(TryLockError::WouldBlock))
}

/// Removes a socket file left by a daemon that is gone. Only a socket is removed; anything else at the path is
/// an error, so a mistake never deletes a person's file.
pub fn remove_stale_socket(path: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_socket() => std::fs::remove_file(path),
        Ok(_) => Err(io::Error::other(format!(
            "{} exists and is not a socket; move it away",
            path.display()
        ))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// How long the probe of a recorded socket waits to connect.
const PROBE_WAIT: Duration = Duration::from_secs(1);

/// Removes the socket an earlier daemon of this home recorded in `run/socket`, when it is still there, is a
/// socket of this user and refuses a connection. Call it holding the lock; a daemon that crashed with a socket
/// outside the home then leaves nothing behind for long. A home copied from another carries that home's
/// `run/socket`, so a socket something still answers on belongs to a live daemon and stays.
pub async fn remove_previous_socket(home: &Home) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let Some(path) = read_socket_path(home) else {
        return Ok(());
    };
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_socket() && meta.uid() == current_uid() => {
            if refuses_connections(&path).await {
                std::fs::remove_file(&path)
            } else {
                tracing::info!(socket = %path.display(), "left a socket another daemon still answers on");
                Ok(())
            }
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

// Only a refused connect proves nobody listens; a connect that succeeds, hangs or fails otherwise keeps the socket.
async fn refuses_connections(path: &Path) -> bool {
    match tokio::time::timeout(PROBE_WAIT, tokio::net::UnixStream::connect(path)).await {
        Ok(Err(e)) => e.kind() == io::ErrorKind::ConnectionRefused,
        Ok(Ok(_)) | Err(_) => false,
    }
}

/// The socket path a running daemon recorded in `run/socket`.
pub fn read_socket_path(home: &Home) -> Option<PathBuf> {
    let text = std::fs::read_to_string(home.socket_file()).ok()?;
    let line = text.lines().next()?.trim();
    (!line.is_empty() && Path::new(line).is_absolute()).then(|| PathBuf::from(line))
}

/// Records the socket path in `run/socket` (0600).
pub fn write_socket_path(home: &Home, socket: &Path) -> io::Result<()> {
    let mut text = socket.as_os_str().as_encoded_bytes().to_vec();
    text.push(b'\n');
    write_atomic(&home.socket_file(), &text).map_err(io::Error::other)
}

// umask is per process; this keeps two binds in one process (tests) from restoring each other's mask.
static UMASK: Mutex<()> = Mutex::new(());

#[allow(unsafe_code)]
fn set_umask(mask: libc::mode_t) -> libc::mode_t {
    // SAFETY: umask has no preconditions, can't fail and touches only the process's file mode mask.
    unsafe { libc::umask(mask) }
}

/// Binds the control socket under umask 177, so it is created 0600 (6.1 asks for at least 077). No chmod
/// follows: in a shared runtime folder a chmod by path could be raced onto another file.
pub fn bind_private(path: &Path) -> io::Result<tokio::net::UnixListener> {
    let _guard = UMASK.lock().unwrap_or_else(|e| e.into_inner());
    let old = set_umask(0o177);
    let bound = tokio::net::UnixListener::bind(path);
    set_umask(old);
    bound
}

/// This process's effective user id.
#[allow(unsafe_code)]
pub fn current_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and can't fail.
    unsafe { libc::geteuid() }
}

/// Whether the other end of a connection runs as this user (6.1).
pub fn same_user(stream: &tokio::net::UnixStream) -> io::Result<bool> {
    Ok(stream.peer_cred()?.uid() == current_uid())
}

/// Whether a variable may pass into the daemon's environment (6.1): never an agent's tokens.
pub fn env_allowed(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    const EXACT: [&str; 16] = [
        "HOME",
        "PATH",
        "USER",
        "LOGNAME",
        "SHELL",
        "LANG",
        "TZ",
        "TMPDIR",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "NO_PROXY",
        "https_proxy",
        "http_proxy",
        "no_proxy",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ];
    EXACT.contains(&name)
        || name.starts_with("LC_")
        || name.starts_with("XDG_")
        || name.starts_with("SPEW_MESH_")
}

/// The daemon's environment: the allowed variables from `vars`.
pub fn daemon_env(
    vars: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(OsString, OsString)> {
    vars.into_iter().filter(|(k, _)| env_allowed(k)).collect()
}

/// Starts `spew mesh --home <home> daemon run` from `exe` in a new session, its output appended to
/// `<home>/logs/mesh.log` and its environment limited to [`env_allowed`] (6.1). The child is reaped by a thread
/// so it never lingers as a zombie of a long-lived client.
pub fn spawn_daemon(home: &Home, exe: &Path) -> io::Result<()> {
    home.ensure().map_err(io::Error::other)?;
    let log = home.log_file();
    // Rotation happens here because the daemon's output is this file: renaming it later would leave the daemon
    // writing to mesh.log.1.
    rotate_log(&log, LOG_ROTATE_BYTES).map_err(io::Error::other)?;
    let out = open_private_append(&log).map_err(io::Error::other)?;
    let err = out.try_clone()?;
    let mut command = Command::new(exe);
    command
        .arg("mesh")
        .arg("--home")
        .arg(home.root())
        .arg("daemon")
        .arg("run")
        .env_clear()
        .envs(daemon_env(std::env::vars_os()))
        .current_dir(home.root())
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    new_session(&mut command);
    let mut child = command.spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[allow(unsafe_code)]
fn new_session(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure runs between fork and exec and calls only setsid, which is async-signal-safe.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_homes_use_the_home_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("h")).unwrap();
        let path = choose_socket_path(&home, &[]).unwrap();
        assert_eq!(path, home.ipc_socket());
    }

    #[test]
    fn long_homes_use_a_runtime_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let long = tmp.path().join("x".repeat(120));
        let home = Home::at(&long).unwrap();
        let runtime = tmp.path().join("rt");
        std::fs::create_dir(&runtime).unwrap();
        let path = choose_socket_path(
            &home,
            &[
                None,
                Some(tmp.path().join("missing")),
                Some(runtime.clone()),
            ],
        )
        .unwrap();
        assert_eq!(path.parent().unwrap(), runtime);
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("spew-mesh-") && name.ends_with(".sock"));
        let middle = &name["spew-mesh-".len()..name.len() - ".sock".len()];
        assert_eq!(middle.len(), 10);
        assert!(middle.bytes().all(|b| BASE32.contains(&b)));
        assert!(choose_socket_path(&home, &[None]).is_err());
    }

    #[test]
    fn the_lock_is_exclusive() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path()).unwrap();
        assert!(lock_is_free(&home));
        let lock = take_lock(&home, 5).unwrap().expect("first lock");
        assert!(!lock_is_free(&home));
        assert!(take_lock(&home, 6).unwrap().is_none());
        let text = std::fs::read_to_string(home.lock_file()).unwrap();
        assert!(text.starts_with(&std::process::id().to_string()));
        drop(lock);
        assert!(lock_is_free(&home));
        assert!(take_lock(&home, 7).unwrap().is_some());
    }

    #[tokio::test]
    async fn bind_is_private_and_stale_sockets_go() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("ipc.sock");
        let listener = bind_private(&path).unwrap();
        assert_eq!(crate::paths::mode_of(&path).unwrap(), Some(0o600));
        drop(listener);
        assert!(path.exists());
        remove_stale_socket(&path).unwrap();
        assert!(!path.exists());
        remove_stale_socket(&path).unwrap();
        let file = tmp.path().join("plain");
        std::fs::write(&file, "keep").unwrap();
        assert!(remove_stale_socket(&file).is_err());
        assert!(file.exists());
    }

    #[tokio::test]
    async fn a_crashed_daemons_socket_is_removed_and_nothing_else() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("h")).unwrap();
        home.ensure().unwrap();
        remove_previous_socket(&home).await.unwrap();
        let old = tmp.path().join("spew-mesh-old.sock");
        drop(bind_private(&old).unwrap());
        write_socket_path(&home, &old).unwrap();
        remove_previous_socket(&home).await.unwrap();
        assert!(!old.exists());
        // A file that took the recorded path is left alone.
        std::fs::write(&old, "keep").unwrap();
        remove_previous_socket(&home).await.unwrap();
        assert!(old.exists());
    }

    #[tokio::test]
    async fn a_socket_another_daemon_answers_on_stays() {
        // A copied home names the other home's socket in run/socket; that daemon is still listening.
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("copy")).unwrap();
        home.ensure().unwrap();
        let live = tmp.path().join("spew-mesh-live.sock");
        let listener = bind_private(&live).unwrap();
        write_socket_path(&home, &live).unwrap();
        remove_previous_socket(&home).await.unwrap();
        assert!(live.exists());
        assert!(tokio::net::UnixStream::connect(&live).await.is_ok());
        drop(listener);
        remove_previous_socket(&home).await.unwrap();
        assert!(!live.exists());
    }

    #[tokio::test]
    async fn peers_are_the_same_user() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("s.sock");
        let listener = bind_private(&path).unwrap();
        let client = tokio::net::UnixStream::connect(&path).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        assert!(same_user(&client).unwrap());
        assert!(same_user(&server).unwrap());
    }

    #[test]
    fn socket_path_file_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path()).unwrap();
        home.ensure().unwrap();
        assert_eq!(read_socket_path(&home), None);
        let sock = home.ipc_socket();
        write_socket_path(&home, &sock).unwrap();
        assert_eq!(read_socket_path(&home), Some(sock));
        assert_eq!(
            crate::paths::mode_of(&home.socket_file()).unwrap(),
            Some(0o600)
        );
    }

    #[test]
    fn the_daemon_env_drops_tokens() {
        let vars = [
            ("HOME", "/h"),
            ("PATH", "/bin"),
            ("LC_ALL", "C"),
            ("XDG_RUNTIME_DIR", "/run/u"),
            ("SPEW_MESH_DIR", "/m"),
            ("https_proxy", "http://proxy.example.com"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "secret"),
            ("OPENAI_API_KEY", "secret"),
            ("SPEW_TOKEN", "secret"),
            ("GITHUB_TOKEN", "secret"),
        ];
        let kept: Vec<String> = daemon_env(
            vars.iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v))),
        )
        .into_iter()
        .map(|(k, _)| k.into_string().unwrap())
        .collect();
        assert_eq!(
            kept,
            [
                "HOME",
                "PATH",
                "LC_ALL",
                "XDG_RUNTIME_DIR",
                "SPEW_MESH_DIR",
                "https_proxy"
            ]
        );
    }
}
