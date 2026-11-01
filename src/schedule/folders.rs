//! The folder each spew project's cards run in on this computer.
//!
//! The daemon already knows where agent sessions work and which project their `.spew.json` names. Each project's
//! folders are remembered in the store (the folder holding the `.spew.json`, with when a session last worked
//! there), so they outlive the sessions. Folders are also found by looking for `.spew.json` files a few levels under
//! the home folder, so a project scheduled before any agent worked in it here still has somewhere to run.
//! `[schedule.folders]` in settings.toml (`spew mesh schedule folder`) wins over both.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::{FolderSource, ProjectFolderInfo};
use crate::settings::{SPEW_JSON, find_spew_json, read_spew_json};
use crate::store::{Store, StoreError};

/// Remembers the project folder of every agent session whose room came from a `.spew.json`.
pub fn learn(store: &Store) -> Result<(), StoreError> {
    for s in store.session_folders()? {
        let Some(file) = find_spew_json(Path::new(&s.cwd)) else {
            continue;
        };
        if read_spew_json(&file).ok().as_deref() != Some(s.project.as_str()) {
            continue;
        }
        let Some(folder) = file.parent().and_then(Path::to_str) else {
            continue;
        };
        store.put_project_folder(&s.project, folder, s.last_seen_at)?;
    }
    Ok(())
}

/// How deep under the home folder `discover` looks: `~/Code/spew` is 2, `~/Code/org/spew` 3.
pub const DISCOVER_DEPTH: usize = 3;
/// At most this many folders are opened per look, so a huge home folder can't stall the daemon.
pub const DISCOVER_MAX_DIRS: usize = 20_000;
// Folders never worth opening: hidden ones, build output, dependencies and the big media folders.
const SKIP: [&str; 14] = [
    "node_modules",
    "target",
    "dist",
    "build",
    "vendor",
    "Library",
    "Applications",
    "Pictures",
    "Music",
    "Movies",
    "Videos",
    "Downloads",
    "snap",
    "go",
];

/// Remembers every folder up to `DISCOVER_DEPTH` under `root` holding a `.spew.json` that names a project. Found
/// folders are remembered as never used (time 0), so one an agent session actually worked in wins.
pub fn discover(store: &Store, root: &Path) -> Result<usize, StoreError> {
    let mut found = 0;
    let mut opened = 0;
    let mut level = vec![root.to_path_buf()];
    for depth in 0..=DISCOVER_DEPTH {
        let mut next = Vec::new();
        for dir in level {
            if opened >= DISCOVER_MAX_DIRS {
                return Ok(found);
            }
            opened += 1;
            if depth > 0 {
                let file = dir.join(SPEW_JSON);
                if let (Ok(project), Some(folder)) = (read_spew_json(&file), dir.to_str()) {
                    store.put_project_folder(&project, folder, 0)?;
                    found += 1;
                    // a project's own folders aren't searched for more projects
                    continue;
                }
            }
            if depth == DISCOVER_DEPTH {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in entries.flatten() {
                let name = e.file_name();
                let Some(name) = name.to_str() else { continue };
                // symlinks aren't followed, so a loop can't trap the walk
                if name.starts_with('.')
                    || SKIP.contains(&name)
                    || !e.file_type().is_ok_and(|t| t.is_dir())
                {
                    continue;
                }
                next.push(e.path());
            }
        }
        level = next;
    }
    Ok(found)
}

// Whether `folder` still holds a `.spew.json` naming `project`.
fn still_names(folder: &Path, project: &str) -> bool {
    read_spew_json(&folder.join(SPEW_JSON)).ok().as_deref() == Some(project)
}

/// The folder for each project, by project key: the settings override when its folder exists, else the most
/// recently used remembered folder whose `.spew.json` still names the project.
pub fn resolve(
    overrides: &BTreeMap<String, PathBuf>,
    store: &Store,
) -> Result<Vec<ProjectFolderInfo>, StoreError> {
    let mut out: BTreeMap<String, ProjectFolderInfo> = BTreeMap::new();
    for (project, folder) in overrides {
        if folder.is_dir()
            && let Some(text) = folder.to_str()
        {
            out.insert(
                project.clone(),
                ProjectFolderInfo {
                    project: project.clone(),
                    folder: text.to_string(),
                    source: FolderSource::Settings,
                },
            );
        }
    }
    for f in store.project_folders()? {
        if out.contains_key(&f.project) || !still_names(Path::new(&f.folder), &f.project) {
            continue;
        }
        out.insert(
            f.project.clone(),
            ProjectFolderInfo {
                project: f.project,
                folder: f.folder,
                source: if f.last_used_at == 0 {
                    FolderSource::Found
                } else {
                    FolderSource::Sessions
                },
            },
        );
    }
    Ok(out.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SessionRecord;

    fn session(id: &str, cwd: &Path, room: &str, source: &str, seen: u64) -> SessionRecord {
        SessionRecord {
            id: id.into(),
            name: id.into(),
            client: "claude".into(),
            kind: crate::wire::SessionKind::Live,
            client_session: None,
            client_pid: None,
            client_pid_start: None,
            token_hash: format!("hash-{id}"),
            cwd: Some(cwd.to_str().unwrap().into()),
            repo: None,
            branch: None,
            place: None,
            room: Some(room.into()),
            room_source: Some(source.into()),
            reach: crate::wire::Reach::Inbox,
            state: crate::wire::ActivityState::Idle,
            state_since: seen,
            activity: None,
            activity_project: None,
            opened_at: seen,
            last_seen_at: seen,
            ended_at: Some(seen),
        }
    }

    #[test]
    fn discover_finds_projects_under_home_and_skips_noise() {
        let dir = tempfile::tempdir().unwrap();
        let put = |rel: &str, project: &str| {
            let d = dir.path().join(rel);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(SPEW_JSON), format!(r#"{{"project": "{project}"}}"#)).unwrap();
        };
        put("Code/spew", "spew");
        put("Code/org/web", "web");
        put("Code/spew/packages/inner", "inner"); // inside a project: not searched
        put("Code/org/web/node_modules/x", "dep");
        put(".config/hidden", "hidden");
        put("a/b/c/too-deep", "deep");
        let store = Store::open_in_memory().unwrap();
        assert_eq!(discover(&store, dir.path()).unwrap(), 2);
        let found = resolve(&BTreeMap::new(), &store).unwrap();
        let names: Vec<_> = found
            .iter()
            .map(|f| (f.project.as_str(), f.source))
            .collect();
        assert_eq!(
            names,
            [("spew", FolderSource::Found), ("web", FolderSource::Found)]
        );
        // a folder an agent session worked in wins over a found one for the same project
        let other = dir.path().join("work/spew");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join(SPEW_JSON), r#"{"project": "spew"}"#).unwrap();
        store
            .put_project_folder("spew", other.to_str().unwrap(), 5)
            .unwrap();
        let spew = resolve(&BTreeMap::new(), &store)
            .unwrap()
            .into_iter()
            .find(|f| f.project == "spew")
            .unwrap();
        assert_eq!(
            (spew.folder.as_str(), spew.source),
            (other.to_str().unwrap(), FolderSource::Sessions)
        );
    }

    #[test]
    fn folders_come_from_sessions_and_settings_win() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old-spew");
        let new = dir.path().join("spew");
        let web = dir.path().join("web");
        let elsewhere = dir.path().join("elsewhere");
        for d in [&old, &new, &web, &elsewhere] {
            std::fs::create_dir_all(d.join("src")).unwrap();
        }
        std::fs::write(old.join(".spew.json"), r#"{"project":"spew"}"#).unwrap();
        std::fs::write(new.join(".spew.json"), r#"{"project":"spew"}"#).unwrap();
        std::fs::write(web.join(".spew.json"), r#"{"project":"web"}"#).unwrap();
        let store = Store::open_in_memory().unwrap();
        store
            .put_session(&session("a", &old.join("src"), "spew", "spew-json", 100))
            .unwrap();
        store
            .put_session(&session("b", &new.join("src"), "spew", "spew-json", 200))
            .unwrap();
        store
            .put_session(&session("c", &web, "web", "spew-json", 50))
            .unwrap();
        // A room from SPEW_PROJECT says nothing about the folder.
        store
            .put_session(&session("d", &elsewhere, "ops", "env", 300))
            .unwrap();
        learn(&store).unwrap();
        let found = resolve(&BTreeMap::new(), &store).unwrap();
        let pairs: Vec<(&str, &str)> = found
            .iter()
            .map(|f| (f.project.as_str(), f.folder.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("spew", new.to_str().unwrap()),
                ("web", web.to_str().unwrap())
            ]
        );

        // The newest folder stops naming the project: the older one takes over.
        std::fs::write(new.join(".spew.json"), r#"{"project":"other"}"#).unwrap();
        let found = resolve(&BTreeMap::new(), &store).unwrap();
        assert_eq!(found[0].folder, old.to_str().unwrap());

        let overrides = BTreeMap::from([
            ("spew".to_string(), elsewhere.clone()),
            ("gone".to_string(), dir.path().join("missing")),
        ]);
        let found = resolve(&overrides, &store).unwrap();
        assert_eq!(found[0].project, "spew");
        assert_eq!(found[0].folder, elsewhere.to_str().unwrap());
        assert_eq!(found[0].source, FolderSource::Settings);
        assert_eq!(found.len(), 2);
    }
}
