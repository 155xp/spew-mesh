//! What a session's working directory says about its repo (2.1, 5.6): the `origin` slug and the branch, read
//! from the `.git` folder without running git.

use std::path::{Path, PathBuf};

use crate::wire::repo_from_origin;

// Git files a session's folder points at are small; anything bigger is not what we look for.
const MAX_GIT_FILE: u64 = 256 * 1024;

/// The repo around a working directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitInfo {
    /// The folder holding `.git`.
    pub root: PathBuf,
    /// `owner/repo` from the `origin` remote, when there is one.
    pub repo: Option<String>,
    /// The checked-out branch; none when detached.
    pub branch: Option<String>,
}

impl GitInfo {
    /// The place part of a session name (2.1): the repo name, else the repo folder's name.
    pub fn place(&self) -> Option<String> {
        match &self.repo {
            Some(repo) => repo.rsplit('/').next().map(str::to_string),
            None => self
                .root
                .file_name()
                .map(|n| n.to_string_lossy().into_owned()),
        }
    }
}

fn read_small(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_GIT_FILE {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// The git dir for a `.git` entry: the folder itself, or where a worktree's `.git` file points.
fn git_dir(dot_git: &Path) -> Option<PathBuf> {
    if dot_git.is_dir() {
        return Some(dot_git.to_path_buf());
    }
    let text = read_small(dot_git)?;
    let target = text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
    let target = Path::new(target);
    Some(if target.is_absolute() {
        target.to_path_buf()
    } else {
        dot_git.parent()?.join(target)
    })
}

/// Reads the repo around `cwd`, or `None` outside a repo.
pub fn git_info(cwd: &Path) -> Option<GitInfo> {
    let root = cwd.ancestors().find(|dir| dir.join(".git").exists())?;
    let dir = git_dir(&root.join(".git"))?;
    let common = match read_small(&dir.join("commondir")) {
        Some(rel) => dir.join(rel.trim()),
        None => dir.clone(),
    };
    let branch = read_small(&dir.join("HEAD")).and_then(|head| branch_from_head(&head));
    let repo = read_small(&common.join("config"))
        .and_then(|config| origin_url(&config))
        .and_then(|url| repo_from_origin(&url));
    Some(GitInfo {
        root: root.to_path_buf(),
        repo,
        branch,
    })
}

/// The branch a `HEAD` file names; none when detached.
pub fn branch_from_head(head: &str) -> Option<String> {
    let name = head.trim().strip_prefix("ref:")?.trim();
    let branch = name.strip_prefix("refs/heads/")?;
    (!branch.is_empty()).then(|| branch.to_string())
}

/// The `url` of `[remote "origin"]` in a git config file.
pub fn origin_url(config: &str) -> Option<String> {
    let mut in_origin = false;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            let header: String = line.chars().filter(|c| !c.is_whitespace()).collect();
            in_origin = header == "[remote\"origin\"]";
            continue;
        }
        if !in_origin {
            continue;
        }
        if let Some((key, value)) = line.split_once('=')
            && key.trim().eq_ignore_ascii_case("url")
        {
            let value = value.trim().trim_matches('"');
            return (!value.is_empty()).then(|| value.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_branch_and_origin() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("api");
        let git = repo.join(".git");
        std::fs::create_dir_all(git.join("refs")).unwrap();
        std::fs::create_dir_all(repo.join("src/deep")).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        std::fs::write(
            git.join("config"),
            "[core]\n\tbare = false\n[remote \"upstream\"]\n\turl = git@example.com:other/thing.git\n\
             [remote \"origin\"]\n\turl = https://sam:token@example.com/acme/api.git\n\tfetch = x\n",
        )
        .unwrap();
        let info = git_info(&repo.join("src/deep")).unwrap();
        assert_eq!(info.root, repo);
        assert_eq!(info.repo.as_deref(), Some("acme/api"));
        assert_eq!(info.branch.as_deref(), Some("feature/x"));
        assert_eq!(info.place().as_deref(), Some("api"));
    }

    #[test]
    fn worktrees_and_detached_heads() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        let wt_git = main.join(".git/worktrees/wt");
        std::fs::create_dir_all(&wt_git).unwrap();
        std::fs::write(
            main.join(".git/config"),
            "[remote \"origin\"]\nurl = example.com:acme/web\n",
        )
        .unwrap();
        std::fs::write(wt_git.join("HEAD"), "0123456789abcdef\n").unwrap();
        std::fs::write(wt_git.join("commondir"), "../..\n").unwrap();
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_git.display())).unwrap();
        let info = git_info(&wt).unwrap();
        assert_eq!(info.repo.as_deref(), Some("acme/web"));
        assert_eq!(info.branch, None);
        assert_eq!(info.place().as_deref(), Some("web"));
    }

    #[test]
    fn a_repo_without_origin_is_named_after_its_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("notes");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main").unwrap();
        let info = git_info(&repo).unwrap();
        assert_eq!(info.repo, None);
        assert_eq!(info.branch.as_deref(), Some("main"));
        assert_eq!(info.place().as_deref(), Some("notes"));
    }
}
