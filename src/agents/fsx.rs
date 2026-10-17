//! Careful file edits for agent configs: one-time 0600 backups, atomic replace with fsync, and a re-check
//! against concurrent writers right before the rename (8).

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

/// How many times an edit is read and applied again after a concurrent write (8).
pub const MAX_RETRIES: u32 = 3;

/// Suffix of the one-time backup next to an edited file (8).
pub const BACKUP_SUFFIX: &str = ".spew-mesh-backup";

/// What a file looked like when it was read, to notice another writer before the rename.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stamp {
    exists: bool,
    mtime: Option<SystemTime>,
    size: u64,
    dev: u64,
    ino: u64,
    mode: u32,
}

impl Stamp {
    /// Reads the stamp of `path`; a missing file is a stamp too.
    pub fn of(path: &Path) -> io::Result<Stamp> {
        match fs::metadata(path) {
            Ok(m) => Ok(Stamp {
                exists: true,
                mtime: m.modified().ok(),
                size: m.len(),
                dev: m.dev(),
                ino: m.ino(),
                mode: m.mode() & 0o7777,
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Stamp {
                exists: false,
                mtime: None,
                size: 0,
                dev: 0,
                ino: 0,
                mode: 0,
            }),
            Err(e) => Err(e),
        }
    }

    /// Whether the file existed.
    pub fn exists(&self) -> bool {
        self.exists
    }
}

/// Reads a file's text with a stamp taken around the read, so the two agree. `None` when the file is missing.
pub fn read_stamped(path: &Path) -> Result<(Option<String>, Stamp)> {
    for _ in 0..5 {
        let before = Stamp::of(path).with_context(|| format!("checking {}", path.display()))?;
        if !before.exists {
            return Ok((None, before));
        }
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let after = Stamp::of(path).with_context(|| format!("checking {}", path.display()))?;
        if before == after {
            let text = String::from_utf8(bytes)
                .map_err(|_| anyhow::anyhow!("{} isn't UTF-8 text", path.display()))?;
            return Ok((Some(text), after));
        }
    }
    bail!("{} keeps changing while it is read", path.display())
}

/// What an applied edit did to the file system, for the manifest.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Written {
    /// The backup holding the file as it was before setup's first edit, when the file existed.
    pub backup: Option<PathBuf>,
    /// The file didn't exist and was created.
    pub created_file: bool,
    /// Folders created for the file, outermost first.
    pub created_dirs: Vec<PathBuf>,
}

/// A hook called right before each rename, given the attempt number. Tests use it to play a concurrent writer.
pub type BeforeRename<'a> = &'a mut dyn FnMut(&Path, u32);

/// Turns a file's current text (`None` when missing) into its new text (`None` for no change) plus a value.
pub type Editor<'a, T> = &'a mut dyn FnMut(Option<&str>) -> Result<(Option<String>, T)>;

/// Applies `editor` to the file and replaces it atomically, re-reading and re-applying when another writer
/// changed it in the meantime (up to [`MAX_RETRIES`] times). The editor gets the current text (`None` when the file
/// is missing) and returns the new text (`None` for no change) plus whatever it wants to hand back; the value from
/// the attempt that was written is returned.
pub fn edit_file<T>(
    path: &Path,
    make_backup: bool,
    editor: Editor<T>,
    before_rename: BeforeRename,
) -> Result<(T, Option<Written>)> {
    let mut attempt = 0;
    loop {
        let (current, stamp) = read_stamped(path)?;
        let (new_text, extra) = editor(current.as_deref())?;
        let Some(new_text) = new_text else {
            return Ok((extra, None));
        };
        let mut written = Written::default();
        if !stamp.exists {
            written.created_dirs = ensure_dirs(path)?;
            written.created_file = true;
        } else if make_backup {
            written.backup = Some(ensure_backup(
                path,
                current.as_deref().unwrap_or("").as_bytes(),
            )?);
        }
        let mode = if stamp.exists { stamp.mode } else { 0o600 };
        match replace_if_unchanged(
            path,
            new_text.as_bytes(),
            mode,
            &stamp,
            attempt,
            before_rename,
        )? {
            true => return Ok((extra, Some(written))),
            false if attempt < MAX_RETRIES => attempt += 1,
            false => bail!(
                "{} kept changing while setup edited it (another program writes it); left it alone",
                path.display()
            ),
        }
    }
}

/// Writes `bytes` to a 0600 temp file next to `path`, gives it `mode`, fsyncs it and renames it over `path`, but
/// only when `path` still matches `stamp`. Returns false (and leaves `path` alone) when it doesn't.
fn replace_if_unchanged(
    path: &Path,
    bytes: &[u8],
    mode: u32,
    stamp: &Stamp,
    attempt: u32,
    before_rename: BeforeRename,
) -> Result<bool> {
    let dir = parent_of(path)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".spew-mesh-")
        .suffix(".tmp")
        .permissions(Permissions::from_mode(0o600))
        .tempfile_in(dir)
        .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
    tmp.write_all(bytes)
        .with_context(|| format!("writing a temporary file for {}", path.display()))?;
    tmp.as_file()
        .set_permissions(Permissions::from_mode(mode))
        .with_context(|| {
            format!(
                "setting the mode of the temporary file for {}",
                path.display()
            )
        })?;
    tmp.as_file()
        .sync_all()
        .with_context(|| format!("syncing the temporary file for {}", path.display()))?;
    before_rename(path, attempt);
    let now = Stamp::of(path).with_context(|| format!("checking {}", path.display()))?;
    if &now != stamp {
        return Ok(false);
    }
    tmp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("replacing {}", path.display()))?;
    sync_dir(dir);
    Ok(true)
}

/// Saves the one-time backup of `path` before another program (an agent's own CLI) edits it. `None` when the
/// file doesn't exist.
pub fn backup_once(path: &Path) -> Result<Option<PathBuf>> {
    match read_stamped(path)? {
        (Some(text), _) => Ok(Some(ensure_backup(path, text.as_bytes())?)),
        (None, _) => Ok(None),
    }
}

/// Deletes `path` when its text is still `expected` (a file setup created and nothing else changed since).
pub fn remove_if_unchanged(path: &Path, expected: &str) -> Result<bool> {
    let (text, stamp) = read_stamped(path)?;
    if text.as_deref() != Some(expected) {
        return Ok(false);
    }
    if Stamp::of(path).with_context(|| format!("checking {}", path.display()))? != stamp {
        return Ok(false);
    }
    fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    if let Some(dir) = path.parent() {
        sync_dir(dir);
    }
    Ok(true)
}

/// Writes the one-time backup of `path` holding `original`, unless a backup is already there.
fn ensure_backup(path: &Path, original: &[u8]) -> Result<PathBuf> {
    let backup = backup_path(path);
    if fs::symlink_metadata(&backup).is_ok() {
        return Ok(backup);
    }
    let dir = parent_of(path)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".spew-mesh-")
        .suffix(".tmp")
        .permissions(Permissions::from_mode(0o600))
        .tempfile_in(dir)
        .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
    tmp.write_all(original)
        .with_context(|| format!("writing the backup of {}", path.display()))?;
    tmp.as_file()
        .sync_all()
        .with_context(|| format!("syncing the backup of {}", path.display()))?;
    // persist_noclobber: never replace a backup another run made in the meantime.
    match tmp.persist_noclobber(&backup) {
        Ok(_) => {}
        Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.error).with_context(|| format!("saving {}", backup.display())),
    }
    sync_dir(dir);
    Ok(backup)
}

/// `<file>.spew-mesh-backup`.
pub fn backup_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(BACKUP_SUFFIX);
    PathBuf::from(s)
}

/// Creates the missing parent folders of `path` with mode 0700, returning the ones it created, outermost first.
pub fn ensure_dirs(path: &Path) -> Result<Vec<PathBuf>> {
    let dir = parent_of(path)?;
    let mut missing = Vec::new();
    let mut cur = Some(dir);
    while let Some(d) = cur {
        if d.as_os_str().is_empty() || d.exists() {
            break;
        }
        missing.push(d.to_path_buf());
        cur = d.parent();
    }
    missing.reverse();
    for d in &missing {
        match fs::DirBuilder::new().mode(0o700).create(d) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("creating {}", d.display())),
        }
    }
    Ok(missing)
}

/// Removes the given folders, innermost first, while they are empty.
pub fn remove_empty_dirs(dirs: &[PathBuf]) {
    for d in dirs.iter().rev() {
        if fs::remove_dir(d).is_err() {
            break;
        }
    }
}

/// Writes a whole file atomically with mode 0600, creating its folders 0700 (used for the manifest).
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure_dirs(path)?;
    let dir = parent_of(path)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".spew-mesh-")
        .suffix(".tmp")
        .permissions(Permissions::from_mode(0o600))
        .tempfile_in(dir)
        .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
    tmp.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    tmp.as_file()
        .sync_all()
        .with_context(|| format!("syncing {}", path.display()))?;
    tmp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("replacing {}", path.display()))?;
    sync_dir(dir);
    Ok(())
}

/// Reads a small file that must be private to this user, refusing anything group or world writable.
pub fn read_private(path: &Path) -> Result<Option<String>> {
    let mut f = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
    };
    let meta = f
        .metadata()
        .with_context(|| format!("checking {}", path.display()))?;
    if meta.mode() & 0o022 != 0 {
        bail!(
            "{} is writable by other users; fix its mode (chmod 600) and try again",
            path.display()
        );
    }
    let mut s = String::new();
    f.read_to_string(&mut s)
        .with_context(|| format!("reading {}", path.display()))?;
    Ok(Some(s))
}

fn parent_of(path: &Path) -> Result<&Path> {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .with_context(|| format!("{} has no parent folder", path.display()))
}

fn sync_dir(dir: &Path) {
    // Best effort: the data itself was synced before the rename.
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

/// Single-quotes `s` for a POSIX shell, writing a single quote inside it as `'\''` (8).
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode_of(p: &Path) -> u32 {
        fs::metadata(p).unwrap().mode() & 0o777
    }

    #[test]
    fn quotes_paths_for_the_shell() {
        assert_eq!(sh_quote("/opt/spew/bin/spew"), "'/opt/spew/bin/spew'");
        assert_eq!(
            sh_quote("/Users/sam/it's here/spew"),
            "'/Users/sam/it'\\''s here/spew'"
        );
    }

    #[test]
    fn edit_keeps_mode_and_backs_up_once() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        fs::write(&file, "one").unwrap();
        fs::set_permissions(&file, Permissions::from_mode(0o644)).unwrap();

        let (_, w) = edit_file(
            &file,
            true,
            &mut |_| Ok((Some("two".to_string()), ())),
            &mut |_, _| {},
        )
        .unwrap();
        let w = w.unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "two");
        assert_eq!(mode_of(&file), 0o644);
        let backup = w.backup.unwrap();
        assert_eq!(backup, dir.path().join("settings.json.spew-mesh-backup"));
        assert_eq!(fs::read_to_string(&backup).unwrap(), "one");
        assert_eq!(mode_of(&backup), 0o600);

        edit_file(
            &file,
            true,
            &mut |_| Ok((Some("three".to_string()), ())),
            &mut |_, _| {},
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(&backup).unwrap(),
            "one",
            "the backup is written only once"
        );
    }

    #[test]
    fn new_files_are_private_and_their_folders_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a/b/SKILL.md");
        let (_, w) = edit_file(
            &file,
            true,
            &mut |cur| Ok((cur.is_none().then(|| "x".into()), ())),
            &mut |_, _| {},
        )
        .unwrap();
        let w = w.unwrap();
        assert!(w.created_file);
        assert_eq!(w.backup, None);
        assert_eq!(
            w.created_dirs,
            vec![dir.path().join("a"), dir.path().join("a/b")]
        );
        assert_eq!(mode_of(&file), 0o600);
        assert_eq!(mode_of(&dir.path().join("a")), 0o700);
        remove_empty_dirs(&w.created_dirs);
        assert!(dir.path().join("a").exists(), "not empty yet");
        fs::remove_file(&file).unwrap();
        remove_empty_dirs(&w.created_dirs);
        assert!(!dir.path().join("a").exists());
    }

    #[test]
    fn a_concurrent_write_is_merged_by_retrying() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.txt");
        fs::write(&file, "a\n").unwrap();
        let mut reads = Vec::new();
        let (_, w) = edit_file(
            &file,
            false,
            &mut |cur| {
                let cur = cur.unwrap_or("").to_string();
                reads.push(cur.clone());
                Ok((Some(format!("{cur}ours\n")), ()))
            },
            &mut |p, attempt| {
                if attempt == 0 {
                    fs::write(p, "a\ntheirs\n").unwrap();
                }
            },
        )
        .unwrap();
        assert!(w.is_some());
        assert_eq!(reads, vec!["a\n".to_string(), "a\ntheirs\n".to_string()]);
        assert_eq!(fs::read_to_string(&file).unwrap(), "a\ntheirs\nours\n");
    }

    #[test]
    fn gives_up_after_three_retries_and_leaves_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.txt");
        fs::write(&file, "start").unwrap();
        let mut n = 0;
        let err = edit_file(
            &file,
            false,
            &mut |_| Ok((Some("ours".into()), ())),
            &mut |p, _| {
                n += 1;
                fs::write(p, "theirs".repeat(n)).unwrap();
            },
        )
        .unwrap_err();
        assert_eq!(n, 4, "the first try and three retries");
        assert!(err.to_string().contains("kept changing"), "{err}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "theirs".repeat(4));
        let leftovers: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "no temporary files left behind");
    }

    #[test]
    fn no_change_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.json");
        fs::write(&file, "{}").unwrap();
        let before = Stamp::of(&file).unwrap();
        let (_, w) = edit_file(&file, true, &mut |_| Ok((None, ())), &mut |_, _| {}).unwrap();
        assert!(w.is_none());
        assert_eq!(Stamp::of(&file).unwrap(), before);
        assert!(!backup_path(&file).exists());
    }
}
