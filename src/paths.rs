//! The home folder and the files in it (11): where each file lives, private modes, and atomic writes.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The variable that overrides the default home (11).
pub const HOME_ENV: &str = "SPEW_MESH_DIR";
/// Mode of the home and every folder in it.
pub const DIR_MODE: u32 = 0o700;
/// Mode of every file in the home.
pub const FILE_MODE: u32 = 0o600;

/// A filesystem step that failed, with the path it was working on.
#[derive(Debug, thiserror::Error)]
pub enum PathsError {
    /// Neither `--home`, `SPEW_MESH_DIR` nor `HOME` gives a home.
    #[error("no home folder for spew mesh: set HOME or SPEW_MESH_DIR, or pass --home")]
    NoHome,
    /// An I/O step failed.
    #[error("could not {action} {path}: {error}", path = .path.display())]
    Io {
        /// What was being done.
        action: &'static str,
        /// On which path.
        path: PathBuf,
        /// The OS error.
        error: io::Error,
    },
}

impl PathsError {
    fn io(action: &'static str, path: &Path) -> impl FnOnce(io::Error) -> PathsError {
        let path = path.to_path_buf();
        move |error| PathsError::Io {
            action,
            path,
            error,
        }
    }
}

/// The person's home folder from `HOME`, when set and not empty.
pub fn user_home_from_env() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Replaces a leading `~` or `~/` with the person's home folder.
pub fn expand_tilde(path: &str, user_home: Option<&Path>) -> PathBuf {
    match (path, user_home) {
        ("~", Some(home)) => home.to_path_buf(),
        (p, Some(home)) if p.starts_with("~/") => home.join(&p[2..]),
        (p, _) => PathBuf::from(p),
    }
}

/// The spew mesh home folder and the layout inside it (11).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Home {
    root: PathBuf,
}

impl Home {
    /// The home at `root`, made absolute against the current directory.
    pub fn at(root: impl AsRef<Path>) -> Result<Home, PathsError> {
        let root = root.as_ref();
        let root = std::path::absolute(root).map_err(PathsError::io("resolve", root))?;
        Ok(Home { root })
    }

    /// The home from `--home`, else `SPEW_MESH_DIR`, else `<user home>/.spew/mesh` (10, 11). Empty values count as
    /// unset.
    pub fn resolve(
        flag: Option<&Path>,
        env_dir: Option<&OsStr>,
        user_home: Option<&Path>,
    ) -> Result<Home, PathsError> {
        if let Some(flag) = flag.filter(|f| !f.as_os_str().is_empty()) {
            return Home::at(flag);
        }
        if let Some(dir) = env_dir.filter(|d| !d.is_empty()) {
            return Home::at(dir);
        }
        match user_home {
            Some(home) => Home::at(Home::default_root(home)),
            None => Err(PathsError::NoHome),
        }
    }

    /// The home for this process: `--home`, then the environment.
    pub fn from_env(flag: Option<&Path>) -> Result<Home, PathsError> {
        let env_dir = std::env::var_os(HOME_ENV);
        Home::resolve(flag, env_dir.as_deref(), user_home_from_env().as_deref())
    }

    /// `~/.spew/mesh` for a given user home.
    pub fn default_root(user_home: &Path) -> PathBuf {
        user_home.join(".spew").join("mesh")
    }

    /// Whether this is the default home, so setup need not write `--home` into agent configs (10).
    pub fn is_default(&self, user_home: Option<&Path>) -> bool {
        user_home.is_some_and(|home| {
            std::path::absolute(Home::default_root(home)).is_ok_and(|default| default == self.root)
        })
    }

    /// The home folder itself.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `settings.toml`.
    pub fn settings_file(&self) -> PathBuf {
        self.root.join("settings.toml")
    }

    /// `identity/`.
    pub fn identity_dir(&self) -> PathBuf {
        self.root.join("identity")
    }

    /// `identity/device.json` (4.1).
    pub fn device_file(&self) -> PathBuf {
        self.identity_dir().join("device.json")
    }

    /// `identity/device.lock`, held while the device file is read or created.
    pub fn device_lock(&self) -> PathBuf {
        self.identity_dir().join("device.lock")
    }

    /// `store.sqlite3` (11.1).
    pub fn store_file(&self) -> PathBuf {
        self.root.join("store.sqlite3")
    }

    /// `run/`.
    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    /// `run/lock`: the daemon's lock (6.1).
    pub fn lock_file(&self) -> PathBuf {
        self.run_dir().join("lock")
    }

    /// `run/socket`: the path of the control socket in use (6.1).
    pub fn socket_file(&self) -> PathBuf {
        self.run_dir().join("socket")
    }

    /// `run/ipc.sock`: the preferred control socket (6.1).
    pub fn ipc_socket(&self) -> PathBuf {
        self.run_dir().join("ipc.sock")
    }

    /// `run/serve/`: Codex run files (8.3).
    pub fn serve_run_dir(&self) -> PathBuf {
        self.run_dir().join("serve")
    }

    /// `logs/`.
    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// `logs/mesh.log`.
    pub fn log_file(&self) -> PathBuf {
        self.logs_dir().join("mesh.log")
    }

    /// `auth/`.
    pub fn auth_dir(&self) -> PathBuf {
        self.root.join("auth")
    }

    /// `auth/spew.json` (4.2).
    pub fn spew_auth_file(&self) -> PathBuf {
        self.auth_dir().join("spew.json")
    }

    /// `agents/`.
    pub fn agents_dir(&self) -> PathBuf {
        self.root.join("agents")
    }

    /// `agents/manifest.json` (8).
    pub fn manifest_file(&self) -> PathBuf {
        self.agents_dir().join("manifest.json")
    }

    /// `roster.toml`, the default file roster (4.2).
    pub fn roster_file(&self) -> PathBuf {
        self.root.join("roster.toml")
    }

    /// Creates the home and its folders, each 0700, and tightens any that exist with a wider mode.
    pub fn ensure(&self) -> Result<(), PathsError> {
        for dir in [
            self.root.clone(),
            self.identity_dir(),
            self.run_dir(),
            self.logs_dir(),
            self.auth_dir(),
            self.agents_dir(),
        ] {
            ensure_private_dir(&dir)?;
        }
        Ok(())
    }
}

/// Creates `dir` and missing parents with mode 0700, and sets `dir` itself to 0700. Fails when `dir` exists but
/// isn't a folder, or can't be changed (such as one owned by another user).
pub fn ensure_private_dir(dir: &Path) -> Result<(), PathsError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, DIR_MODE);
    builder.create(dir).map_err(PathsError::io("create", dir))?;
    let meta = fs::metadata(dir).map_err(PathsError::io("read", dir))?;
    if !meta.is_dir() {
        return Err(PathsError::Io {
            action: "use as a folder",
            path: dir.to_path_buf(),
            error: io::Error::other("not a folder"),
        });
    }
    set_mode(dir, DIR_MODE).map_err(PathsError::io("set the mode of", dir))
}

/// Sets a path's permission bits.
pub fn set_mode(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// A path's permission bits, or `None` off Unix.
pub fn mode_of(path: &Path) -> io::Result<Option<u32>> {
    let meta = fs::metadata(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(Some(meta.permissions().mode() & 0o7777))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        Ok(None)
    }
}

/// Open options that create files with mode 0600.
pub fn private_open_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, FILE_MODE);
    options
}

/// Opens `path` for appending, creating it 0600 when missing.
pub fn open_private_append(path: &Path) -> Result<File, PathsError> {
    private_open_options()
        .create(true)
        .append(true)
        .open(path)
        .map_err(PathsError::io("open", path))
}

/// Size over which `logs/mesh.log` moves to `mesh.log.1` at daemon start (11).
pub const LOG_ROTATE_BYTES: u64 = 10 * 1024 * 1024;

/// Moves `log` to `<log>.1`, replacing an older one, when it holds more than `max_bytes` (11). Returns whether it
/// moved.
pub fn rotate_log(log: &Path, max_bytes: u64) -> Result<bool, PathsError> {
    let len = match fs::metadata(log) {
        Ok(meta) => meta.len(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(PathsError::io("read", log)(error)),
    };
    if len <= max_bytes {
        return Ok(false);
    }
    let mut rotated = log.as_os_str().to_owned();
    rotated.push(".1");
    fs::rename(log, &rotated).map_err(PathsError::io("rotate", log))?;
    Ok(true)
}

/// Writes `bytes` to `path` atomically with mode 0600: a temporary file in the same folder, fsync, rename, and
/// fsync of the folder.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), PathsError> {
    write_atomic_mode(path, bytes, FILE_MODE)
}

/// Like [`write_atomic`], giving the file `mode` once written. The temporary file is 0600 until then, so a wider
/// mode never applies to a partial file (8).
pub fn write_atomic_mode(path: &Path, bytes: &[u8], mode: u32) -> Result<(), PathsError> {
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let mut builder = tempfile::Builder::new();
    builder.prefix(".spew-mesh-tmp-");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(fs::Permissions::from_mode(FILE_MODE));
    }
    let mut tmp = builder
        .tempfile_in(dir)
        .map_err(PathsError::io("create a temporary file in", dir))?;
    tmp.write_all(bytes)
        .map_err(PathsError::io("write", tmp.path()))?;
    if mode != FILE_MODE {
        set_mode(tmp.path(), mode).map_err(PathsError::io("set the mode of", tmp.path()))?;
    }
    tmp.as_file()
        .sync_all()
        .map_err(PathsError::io("sync", tmp.path()))?;
    tmp.persist(path)
        .map_err(|e| PathsError::io("replace", path)(e.error))?;
    sync_dir(dir).map_err(PathsError::io("sync", dir))
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_order() {
        let user = Path::new("/home/sam");
        let home =
            Home::resolve(Some(Path::new("/a")), Some(OsStr::new("/b")), Some(user)).unwrap();
        assert_eq!(home.root(), Path::new("/a"));
        let home = Home::resolve(None, Some(OsStr::new("/b")), Some(user)).unwrap();
        assert_eq!(home.root(), Path::new("/b"));
        let home = Home::resolve(Some(Path::new("")), Some(OsStr::new("")), Some(user)).unwrap();
        assert_eq!(home.root(), Path::new("/home/sam/.spew/mesh"));
        assert!(home.is_default(Some(user)));
        assert!(!Home::at("/b").unwrap().is_default(Some(user)));
        assert!(!home.is_default(None));
        assert!(matches!(
            Home::resolve(None, None, None),
            Err(PathsError::NoHome)
        ));
        let relative = Home::resolve(Some(Path::new("rel")), None, None).unwrap();
        assert!(relative.root().is_absolute());
    }

    #[test]
    fn layout() {
        let home = Home::at("/m").unwrap();
        let rel = |p: PathBuf| p.strip_prefix("/m").unwrap().to_string_lossy().into_owned();
        assert_eq!(rel(home.settings_file()), "settings.toml");
        assert_eq!(rel(home.device_file()), "identity/device.json");
        assert_eq!(rel(home.store_file()), "store.sqlite3");
        assert_eq!(rel(home.lock_file()), "run/lock");
        assert_eq!(rel(home.socket_file()), "run/socket");
        assert_eq!(rel(home.ipc_socket()), "run/ipc.sock");
        assert_eq!(rel(home.serve_run_dir()), "run/serve");
        assert_eq!(rel(home.log_file()), "logs/mesh.log");
        assert_eq!(rel(home.spew_auth_file()), "auth/spew.json");
        assert_eq!(rel(home.manifest_file()), "agents/manifest.json");
        assert_eq!(rel(home.roster_file()), "roster.toml");
    }

    #[test]
    fn tilde() {
        let user = Path::new("/home/sam");
        assert_eq!(expand_tilde("~", Some(user)), PathBuf::from("/home/sam"));
        assert_eq!(
            expand_tilde("~/x/y", Some(user)),
            PathBuf::from("/home/sam/x/y")
        );
        assert_eq!(
            expand_tilde("~maya/x", Some(user)),
            PathBuf::from("~maya/x")
        );
        assert_eq!(expand_tilde("~/x", None), PathBuf::from("~/x"));
        assert_eq!(expand_tilde("/abs", Some(user)), PathBuf::from("/abs"));
    }

    #[cfg(unix)]
    #[test]
    fn ensure_makes_private_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("deep").join("mesh")).unwrap();
        home.ensure().unwrap();
        for dir in [
            home.root().to_path_buf(),
            home.identity_dir(),
            home.run_dir(),
            home.logs_dir(),
            home.auth_dir(),
            home.agents_dir(),
        ] {
            assert_eq!(mode_of(&dir).unwrap(), Some(DIR_MODE), "{}", dir.display());
        }
        set_mode(home.root(), 0o755).unwrap();
        home.ensure().unwrap();
        assert_eq!(mode_of(home.root()).unwrap(), Some(DIR_MODE));

        let file = tmp.path().join("plain");
        fs::write(&file, b"x").unwrap();
        assert!(ensure_private_dir(&file).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_writes_are_private() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("f.json");
        write_atomic(&path, b"one").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"one");
        assert_eq!(mode_of(&path).unwrap(), Some(FILE_MODE));
        write_atomic(&path, b"two").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        write_atomic_mode(&path, b"three", 0o644).unwrap();
        assert_eq!(mode_of(&path).unwrap(), Some(0o644));
        let leftovers: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(leftovers.len(), 1);
        let missing = tmp.path().join("nope").join("f");
        assert!(write_atomic(&missing, b"x").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn private_append() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("log");
        let mut file = open_private_append(&path).unwrap();
        file.write_all(b"a").unwrap();
        drop(file);
        let mut file = open_private_append(&path).unwrap();
        file.write_all(b"b").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"ab");
        assert_eq!(mode_of(&path).unwrap(), Some(FILE_MODE));
    }

    #[test]
    fn log_rotation() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("mesh.log");
        let old = tmp.path().join("mesh.log.1");
        assert!(!rotate_log(&log, 4).unwrap());
        fs::write(&log, b"1234").unwrap();
        assert!(!rotate_log(&log, 4).unwrap());
        fs::write(&old, b"older").unwrap();
        fs::write(&log, b"12345").unwrap();
        assert!(rotate_log(&log, 4).unwrap());
        assert!(!log.exists());
        assert_eq!(fs::read(&old).unwrap(), b"12345");
    }
}
