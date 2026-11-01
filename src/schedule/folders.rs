//! The folder each spew project's cards run in on this computer.
//!
//! The daemon already knows where agent sessions work and which project their `.spew.json` names. Each project's
//! folders are remembered in the store (the folder holding the `.spew.json`, with when a session last worked
//! there), so they outlive the sessions. `[schedule.folders]` in settings.toml wins over what was learned.

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
                source: FolderSource::Sessions,
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
