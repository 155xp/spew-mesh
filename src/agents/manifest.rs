//! `agents/manifest.json`: every change setup made, so uninstall reverts exactly those (8, 11).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::Client;
use super::fsx;

/// The manifest format this code reads and writes.
pub const MANIFEST_VERSION: u32 = 1;

/// `<home>/agents/manifest.json`.
pub fn manifest_path(home: &Path) -> PathBuf {
    home.join("agents").join("manifest.json")
}

/// How a change was made, which decides how it is undone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Op {
    /// A JSON object member at `path` (its last element is the key) was set to `value`.
    JsonSet,
    /// `value` was appended to the JSON array at `path`.
    JsonAppend,
    /// A TOML table or key at `path` was set to `value` (given as JSON).
    TomlSet,
    /// A whole file was written; `value` is `{"sha256": "<hex>"}`.
    File,
    /// The MCP server was added with `claude mcp add-json`; `path` and `value` say where it landed in `file`.
    ClaudeCli,
}

/// One recorded change: file, location inside it, the value written, and the backup path (8).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// A stable name for the change, such as `claude.hook.SessionStart`.
    pub id: String,
    pub client: Client,
    pub file: PathBuf,
    pub op: Op,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<String>,
    pub value: serde_json::Value,
    /// Containers (objects, arrays, tables) this change created, so uninstall can remove them when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub created: Vec<Vec<String>>,
    /// The file didn't exist before setup; uninstall deletes it when nothing else is left in it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub created_file: bool,
    /// Folders created for the file, outermost first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub created_dirs: Vec<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<PathBuf>,
}

/// The whole manifest.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    #[serde(default)]
    pub entries: Vec<Entry>,
}

impl Default for Manifest {
    fn default() -> Self {
        Manifest {
            version: MANIFEST_VERSION,
            entries: Vec::new(),
        }
    }
}

impl Manifest {
    /// Loads the manifest; a missing file is an empty manifest, a broken one an error (never silently reset).
    pub fn load(path: &Path) -> Result<Manifest> {
        let Some(text) = fsx::read_private(path)? else {
            return Ok(Manifest::default());
        };
        let m: Manifest = serde_json::from_str(&text).with_context(|| {
            format!(
                "{} doesn't parse; fix it or move it away before running setup again",
                path.display()
            )
        })?;
        if m.version != MANIFEST_VERSION {
            bail!(
                "{} has version {}, this spew understands version {MANIFEST_VERSION}; use a matching spew",
                path.display(),
                m.version
            );
        }
        Ok(m)
    }

    /// Saves the manifest (0600, atomic). An empty manifest removes the file.
    pub fn save(&self, path: &Path) -> Result<()> {
        if self.entries.is_empty() {
            return match fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
            };
        }
        let mut text = serde_json::to_string_pretty(self).context("encoding the manifest")?;
        text.push('\n');
        fsx::write_private(path, text.as_bytes())
    }

    /// The entry for change `id` in `file`.
    pub fn find(&self, id: &str, file: &Path) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id && e.file == file)
    }

    /// Adds an entry, replacing one with the same id and file.
    pub fn upsert(&mut self, entry: Entry) {
        match self
            .entries
            .iter_mut()
            .find(|e| e.id == entry.id && e.file == entry.file)
        {
            Some(slot) => *slot = entry,
            None => self.entries.push(entry),
        }
    }

    /// Drops the entry for change `id` in `file`.
    pub fn remove(&mut self, id: &str, file: &Path) {
        self.entries.retain(|e| !(e.id == id && e.file == file));
    }

    /// The entries of one client.
    pub fn for_client(&self, client: Client) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(move |e| e.client == client)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn entry(id: &str) -> Entry {
        Entry {
            id: id.into(),
            client: Client::Claude,
            file: PathBuf::from("/home/sam/.claude/settings.json"),
            op: Op::JsonAppend,
            path: vec!["hooks".into(), "Stop".into()],
            value: serde_json::json!({"hooks": []}),
            created: vec![],
            created_file: false,
            created_dirs: vec![],
            backup: None,
        }
    }

    #[test]
    fn round_trips_privately_and_upserts() {
        let dir = tempfile::tempdir().unwrap();
        let path = manifest_path(dir.path());
        assert_eq!(Manifest::load(&path).unwrap(), Manifest::default());
        let mut m = Manifest::default();
        m.upsert(entry("a"));
        m.upsert(entry("b"));
        let mut changed = entry("a");
        changed.value = serde_json::json!(1);
        m.upsert(changed.clone());
        assert_eq!(m.entries.len(), 2);
        m.save(&path).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let back = Manifest::load(&path).unwrap();
        assert_eq!(back.find("a", &changed.file), Some(&changed));
        m.entries.clear();
        m.save(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn a_broken_or_newer_manifest_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = manifest_path(dir.path());
        fsx::write_private(&path, b"{not json").unwrap();
        assert!(Manifest::load(&path).is_err());
        fsx::write_private(&path, b"{\"version\": 9, \"entries\": []}").unwrap();
        assert!(
            Manifest::load(&path)
                .unwrap_err()
                .to_string()
                .contains("version 9")
        );
    }
}
