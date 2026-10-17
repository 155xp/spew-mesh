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

/// The file an edit of `path` changes: the file a link at `path` points to (a dotfiles setup, 8), else `path`.
/// A link to a file that can't be reached is an error, so setup never replaces a link with a file.
pub fn edit_target(path: &Path) -> Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => fs::canonicalize(path).with_context(|| {
            format!(
                "{} is a link to a file that can't be reached; left it alone",
                path.display()
            )
        }),
        Ok(_) => Ok(path.to_path_buf()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(e) => Err(e).with_context(|| format!("checking {}", path.display())),
    }
}

/// What an applied edit did to the file system, for the manifest.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Written {
    /// The file that was edited when the path is a link to it (8); `None` when the path itself was edited.
    pub target: Option<PathBuf>,
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

/// Called right before the rename with the editor's value and what the edit is about to do. An error stops the
/// edit with the file unchanged; setup records its manifest here, so a change is never on disk unrecorded.
pub type Commit<'a, T> = &'a mut dyn FnMut(&T, &Written) -> Result<()>;

/// Applies `editor` to the file (to its target when it is a link) and replaces it atomically, re-reading and
/// re-applying when another writer changed it in the meantime (up to [`MAX_RETRIES`] times). The editor gets the
/// current text (`None` when the file is missing) and returns the new text (`None` for no change) plus whatever it
/// wants to hand back; the value from the attempt that was written is returned. `commit` runs right before the
/// rename; when the edit doesn't happen, folders made for it and a backup made by it are removed again.
pub fn edit_file<T>(
    path: &Path,
    make_backup: bool,
    editor: Editor<T>,
    commit: Commit<T>,
    before_rename: BeforeRename,
) -> Result<(T, Option<Written>)> {
    let mut made_dirs = Vec::new();
    let res = edit_attempts(
        path,
        make_backup,
        editor,
        commit,
        before_rename,
        &mut made_dirs,
    );
    if !matches!(res, Ok((_, Some(_)))) {
        remove_empty_dirs(&made_dirs);
    }
    res
}

fn edit_attempts<T>(
    path: &Path,
    make_backup: bool,
    editor: Editor<T>,
    commit: Commit<T>,
    before_rename: BeforeRename,
    made_dirs: &mut Vec<PathBuf>,
) -> Result<(T, Option<Written>)> {
    let mut attempt = 0;
    loop {
        let target = edit_target(path)?;
        let (current, stamp) = read_stamped(&target)?;
        let (new_text, value) = editor(current.as_deref())?;
        let Some(new_text) = new_text else {
            return Ok((value, None));
        };
        let mut written = Written {
            target: (target != path).then(|| target.clone()),
            ..Written::default()
        };
        if !stamp.exists {
            made_dirs.extend(ensure_dirs(&target)?);
            written.created_file = true;
            written.created_dirs = made_dirs.clone();
        }
        let job = Replace {
            path,
            target: &target,
            bytes: new_text.as_bytes(),
            mode: if stamp.exists { stamp.mode } else { 0o600 },
            stamp: &stamp,
            original: current
                .as_deref()
                .filter(|_| make_backup)
                .map(str::as_bytes),
        };
        let done = replace_if_unchanged(
            &job,
            &mut written,
            &mut |w| commit(&value, w),
            attempt,
            before_rename,
        )?;
        match done {
            true => return Ok((value, Some(written))),
            false if attempt < MAX_RETRIES => attempt += 1,
            false => bail!(
                "{} kept changing while setup edited it (another program writes it); left it alone",
                path.display()
            ),
        }
    }
}

/// One replacement: the new bytes for `target` (which is `path`, or the file a link at `path` points to).
struct Replace<'a> {
    path: &'a Path,
    target: &'a Path,
    bytes: &'a [u8],
    mode: u32,
    stamp: &'a Stamp,
    /// The text being replaced, when it is backed up before setup's first edit of the file.
    original: Option<&'a [u8]>,
}

/// Writes the new bytes to a 0600 temp file next to the target, gives it the mode, fsyncs it and renames it over
/// the target, but only when the target still matches the stamp (and the link still points to it). Returns false,
/// leaving the target alone, when it doesn't. The backup is written only once the check has passed, so it holds
/// exactly the text this edit replaces; one this call made is removed again when the rename doesn't happen.
fn replace_if_unchanged(
    job: &Replace,
    written: &mut Written,
    commit: &mut dyn FnMut(&Written) -> Result<()>,
    attempt: u32,
    before_rename: BeforeRename,
) -> Result<bool> {
    let (path, target) = (job.path, job.target);
    let dir = parent_of(target)?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".spew-mesh-")
        .suffix(".tmp")
        .permissions(Permissions::from_mode(0o600))
        .tempfile_in(dir)
        .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
    tmp.write_all(job.bytes)
        .with_context(|| format!("writing a temporary file for {}", path.display()))?;
    tmp.as_file()
        .set_permissions(Permissions::from_mode(job.mode))
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
    let unchanged = || -> Result<bool> {
        let now = Stamp::of(target).with_context(|| format!("checking {}", target.display()))?;
        Ok(now == *job.stamp && edit_target(path)? == target)
    };
    if !unchanged()? {
        return Ok(false);
    }
    let mut fresh = None;
    if let Some(original) = job.original {
        let backup = ensure_backup(target, original)?;
        if backup.created {
            fresh = Some(backup.path.clone());
        }
        written.backup = Some(backup.path);
    }
    let finish = || -> Result<bool> {
        commit(written)?;
        // Checked again: the backup and the commit took a moment.
        if !unchanged()? {
            return Ok(false);
        }
        tmp.persist(target)
            .map_err(|e| e.error)
            .with_context(|| format!("replacing {}", target.display()))?;
        Ok(true)
    };
    let done = finish();
    if !matches!(done, Ok(true))
        && let Some(b) = &fresh
    {
        let _ = fs::remove_file(b);
    }
    if matches!(done, Ok(true)) {
        sync_dir(dir);
    }
    done
}

/// A one-time backup, and whether this call wrote it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Backup {
    pub path: PathBuf,
    pub created: bool,
}

impl Backup {
    /// Removes the backup when this call wrote it (the edit it was for didn't happen).
    pub fn discard(&self) {
        if self.created {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Saves the one-time backup of `path` before another program (an agent's own CLI) edits it, next to the file a
/// link at `path` points to. `None` when the file doesn't exist.
pub fn backup_once(path: &Path) -> Result<Option<Backup>> {
    let target = edit_target(path)?;
    match read_stamped(&target)? {
        (Some(text), _) => Ok(Some(ensure_backup(&target, text.as_bytes())?)),
        (None, _) => Ok(None),
    }
}

/// Deletes `path` when its text is still `expected` (a file setup created and nothing else changed since). A link
/// is never deleted, nor the file it points to: setup didn't make it.
pub fn remove_if_unchanged(path: &Path, expected: &str) -> Result<bool> {
    if is_link(path) {
        return Ok(false);
    }
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

/// Whether `path` itself is a symbolic link.
pub fn is_link(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
}

/// Writes the one-time backup of `path` holding `original`, unless a backup is already there.
fn ensure_backup(path: &Path, original: &[u8]) -> Result<Backup> {
    let backup = backup_path(path);
    if fs::symlink_metadata(&backup).is_ok() {
        return Ok(Backup {
            path: backup,
            created: false,
        });
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
    let created = match tmp.persist_noclobber(&backup) {
        Ok(_) => true,
        Err(e) if e.error.kind() == io::ErrorKind::AlreadyExists => false,
        Err(e) => return Err(e.error).with_context(|| format!("saving {}", backup.display())),
    };
    sync_dir(dir);
    Ok(Backup {
        path: backup,
        created,
    })
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

/// Removes those of the given folders that are empty, deepest first, so nested ones go before their parents.
pub fn remove_empty_dirs(dirs: &[PathBuf]) {
    let mut sorted: Vec<&PathBuf> = dirs.iter().collect();
    sorted.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    sorted.dedup();
    for d in sorted {
        // Fails, as wanted, when the folder isn't empty or is already gone.
        let _ = fs::remove_dir(d);
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

    /// A commit that records nothing.
    fn no_commit<T>(_: &T, _: &Written) -> Result<()> {
        Ok(())
    }

    fn set(text: &str) -> impl FnMut(Option<&str>) -> Result<(Option<String>, ())> + '_ {
        move |_| Ok((Some(text.to_string()), ()))
    }

    #[test]
    fn edit_keeps_mode_and_backs_up_once() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("settings.json");
        fs::write(&file, "one").unwrap();
        fs::set_permissions(&file, Permissions::from_mode(0o644)).unwrap();

        let (_, w) =
            edit_file(&file, true, &mut set("two"), &mut no_commit, &mut |_, _| {}).unwrap();
        let w = w.unwrap();
        assert_eq!(w.target, None);
        assert_eq!(fs::read_to_string(&file).unwrap(), "two");
        assert_eq!(mode_of(&file), 0o644);
        let backup = w.backup.unwrap();
        assert_eq!(backup, dir.path().join("settings.json.spew-mesh-backup"));
        assert_eq!(fs::read_to_string(&backup).unwrap(), "one");
        assert_eq!(mode_of(&backup), 0o600);

        edit_file(
            &file,
            true,
            &mut set("three"),
            &mut no_commit,
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
    fn a_linked_file_is_edited_at_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        let home = dir.path().join("home");
        fs::create_dir_all(&dotfiles).unwrap();
        fs::create_dir_all(&home).unwrap();
        let target = dotfiles.join("settings.json");
        fs::write(&target, "one").unwrap();
        fs::set_permissions(&target, Permissions::from_mode(0o640)).unwrap();
        let link = home.join("settings.json");
        std::os::unix::fs::symlink("../dotfiles/settings.json", &link).unwrap();

        let mut seen = Vec::new();
        let (_, w) = edit_file(
            &link,
            true,
            &mut set("two"),
            &mut |_: &(), w: &Written| {
                seen.push(w.clone());
                Ok(())
            },
            &mut |_, _| {},
        )
        .unwrap();
        let w = w.unwrap();
        let canonical = fs::canonicalize(&target).unwrap();
        assert_eq!(w.target, Some(canonical.clone()));
        assert_eq!(seen, vec![w.clone()], "committed once, with the same facts");
        assert!(is_link(&link), "the link survives");
        assert_eq!(fs::read_to_string(&target).unwrap(), "two");
        assert_eq!(mode_of(&target), 0o640);
        assert_eq!(w.backup, Some(backup_path(&canonical)));
        assert_eq!(fs::read_to_string(backup_path(&target)).unwrap(), "one");
        let in_home: Vec<_> = fs::read_dir(&home).unwrap().collect();
        assert_eq!(in_home.len(), 1, "no temp file or backup next to the link");

        let b = backup_once(&link).unwrap().unwrap();
        assert_eq!(b.path, backup_path(&canonical));
        assert!(!b.created, "already there");

        fs::write(&target, "two").unwrap();
        assert!(!remove_if_unchanged(&link, "two").unwrap());
        assert!(
            is_link(&link) && target.exists(),
            "neither link nor target is deleted"
        );

        let dangling = home.join("config.toml");
        std::os::unix::fs::symlink("../dotfiles/missing.toml", &dangling).unwrap();
        let err = edit_file(
            &dangling,
            true,
            &mut set("x"),
            &mut no_commit,
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("can't be reached"), "{err}");
        assert!(is_link(&dangling) && !dotfiles.join("missing.toml").exists());
    }

    #[test]
    fn a_failed_commit_leaves_the_file_and_no_backup() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hooks.json");
        fs::write(&file, "one").unwrap();
        let err = edit_file(
            &file,
            true,
            &mut set("two"),
            &mut |_: &(), _: &Written| bail!("the manifest can't be saved"),
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(err.to_string().contains("manifest"), "{err}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "one");
        let left: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(left.len(), 1, "no backup or temp file left");

        let made = dir.path().join("new/dir/SKILL.md");
        edit_file(
            &made,
            true,
            &mut set("x"),
            &mut |_: &(), _: &Written| bail!("no"),
            &mut |_, _| {},
        )
        .unwrap_err();
        assert!(
            !dir.path().join("new").exists(),
            "folders made for it go too"
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
            &mut no_commit,
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
            true,
            &mut |cur| {
                let cur = cur.unwrap_or("").to_string();
                reads.push(cur.clone());
                Ok((Some(format!("{cur}ours\n")), ()))
            },
            &mut no_commit,
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
        assert_eq!(
            fs::read_to_string(backup_path(&file)).unwrap(),
            "a\ntheirs\n",
            "the backup holds the text the edit replaced"
        );
    }

    #[test]
    fn gives_up_after_three_retries_and_leaves_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.txt");
        fs::write(&file, "start").unwrap();
        let mut n = 0;
        let mut commits = 0;
        let err = edit_file(
            &file,
            true,
            &mut set("ours"),
            &mut |_: &(), _: &Written| {
                commits += 1;
                Ok(())
            },
            &mut |p, _| {
                n += 1;
                fs::write(p, "theirs".repeat(n)).unwrap();
            },
        )
        .unwrap_err();
        assert_eq!(n, 4, "the first try and three retries");
        assert_eq!(
            commits, 0,
            "nothing is recorded for an edit that never happens"
        );
        assert!(err.to_string().contains("kept changing"), "{err}");
        assert_eq!(fs::read_to_string(&file).unwrap(), "theirs".repeat(4));
        let leftovers: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(
            leftovers.len(),
            1,
            "no temporary file or backup left behind"
        );
    }

    #[test]
    fn no_change_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.json");
        fs::write(&file, "{}").unwrap();
        let before = Stamp::of(&file).unwrap();
        let (_, w) = edit_file(
            &file,
            true,
            &mut |_| Ok((None, ())),
            &mut no_commit,
            &mut |_, _| {},
        )
        .unwrap();
        assert!(w.is_none());
        assert_eq!(Stamp::of(&file).unwrap(), before);
        assert!(!backup_path(&file).exists());
    }
}
