//! The device key and name (4.1).
//!
//! `identity/device.json` holds the ed25519 secret key the daemon gives iroh as its endpoint secret. It is created
//! once, never replaced silently, and read and written under an exclusive lock on `identity/device.lock` so two processes starting at
//! once can't create two keys.

use std::fmt;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use iroh::{EndpointId, SecretKey};
use serde::{Deserialize, Serialize};

use crate::address::{DEVICE_RULE, MAX_DEVICE, is_device_name, slugify};
use crate::paths::{
    FILE_MODE, Home, PathsError, ensure_private_dir, mode_of, private_open_options, set_mode,
    write_atomic,
};

/// Length of the short endpoint id (4.1).
pub const SHORT_ID_LEN: usize = 10;

#[derive(Serialize, Deserialize)]
struct DeviceFile {
    secret_key: String,
    name: String,
    created_at: u64,
}

/// This device's key and name.
#[derive(Clone)]
pub struct DeviceIdentity {
    secret_key: SecretKey,
    name: String,
    created_at: u64,
}

impl fmt::Debug for DeviceIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceIdentity")
            .field("endpoint_id", &self.endpoint_id_hex())
            .field("name", &self.name)
            .field("created_at", &self.created_at)
            .finish_non_exhaustive()
    }
}

/// Why the device identity can't be used.
#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    /// A filesystem step failed; the file was left alone.
    #[error(transparent)]
    Paths(#[from] PathsError),
    /// The lock couldn't be taken.
    #[error("could not lock {}: {error}", path.display())]
    Lock {
        /// The lock file.
        path: PathBuf,
        /// The OS error.
        error: io::Error,
    },
    /// The system random source failed.
    #[error("could not generate a device key: {0}")]
    Random(String),
    /// The file doesn't parse as a device identity. It is left exactly as it is: the daemon never moves or
    /// replaces it (4.1).
    #[error(
        "{} could not be read ({reason}). The daemon will not replace it. To start over with a new device key, \
         move the file away yourself (the next start creates a new key), sign in again with `spew mesh login`, \
         then revoke the old device with `spew mesh devices revoke <name>`.",
        path.display()
    )]
    Corrupt {
        /// The device file.
        path: PathBuf,
        /// What was wrong.
        reason: String,
    },
    /// The file parses but holds a name that isn't a valid device name. The key is fine, so the file stays.
    #[error(
        "{} holds the device name \"{name}\", which is not a valid device name ({rule}); fix it there or run \
         `spew mesh setup --device-name NAME`",
        path.display(),
        rule = DEVICE_RULE
    )]
    BadName {
        /// The device file.
        path: PathBuf,
        /// The name in it.
        name: String,
    },
    /// The device file couldn't be encoded.
    #[error("could not encode the device file: {0}")]
    Encode(String),
    /// A new name isn't a valid device name.
    #[error("\"{0}\" is not a valid device name: use {rule}", rule = DEVICE_RULE)]
    InvalidName(String),
}

// Takes `identity/device.lock`. Every access is exclusive: even a load may tighten the file's mode, and loads are
// rare.
fn lock(home: &Home) -> Result<File, IdentityError> {
    ensure_private_dir(&home.identity_dir())?;
    let path = home.device_lock();
    let file = private_open_options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|error| IdentityError::Lock {
            path: path.clone(),
            error,
        })?;
    file.lock()
        .map_err(|error| IdentityError::Lock { path, error })?;
    Ok(file)
}

impl DeviceIdentity {
    /// Reads the device file; `Ok(None)` when there is none yet. A file that doesn't parse is left in place and
    /// reported as [`IdentityError::Corrupt`] (4.1).
    pub fn load(home: &Home) -> Result<Option<DeviceIdentity>, IdentityError> {
        let _guard = lock(home)?;
        read_device_file(&home.device_file())
    }

    /// Reads the device file, or creates it with a new key and `name` when there is none. Returns the identity
    /// and whether it was created now. A file that doesn't parse is never replaced: this fails with
    /// [`IdentityError::Corrupt`] for as long as it is there (4.1).
    pub fn load_or_create(
        home: &Home,
        name: &str,
        now_ms: u64,
    ) -> Result<(DeviceIdentity, bool), IdentityError> {
        if !is_device_name(name) {
            return Err(IdentityError::InvalidName(name.to_string()));
        }
        let _guard = lock(home)?;
        let path = home.device_file();
        if let Some(identity) = read_device_file(&path)? {
            return Ok((identity, false));
        }
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| IdentityError::Random(e.to_string()))?;
        let identity = DeviceIdentity {
            secret_key: SecretKey::from_bytes(&bytes),
            name: name.to_string(),
            created_at: now_ms,
        };
        bytes.fill(0);
        identity.write(&path)?;
        Ok((identity, true))
    }

    /// Changes the device name in the device file and returns what the file now holds (`setup --device-name`,
    /// 2). The file is read again under the lock and only its name changes, so a key written since this process
    /// last read it is kept. It also repairs a file whose name is invalid. `Ok(None)` when there is no device file
    /// yet; a file that doesn't parse fails with [`IdentityError::Corrupt`] and stays as it is.
    pub fn rename(home: &Home, name: &str) -> Result<Option<DeviceIdentity>, IdentityError> {
        if !is_device_name(name) {
            return Err(IdentityError::InvalidName(name.to_string()));
        }
        let _guard = lock(home)?;
        let path = home.device_file();
        let Some(file) = read_parsed(&path)? else {
            return Ok(None);
        };
        let renamed = DeviceIdentity {
            secret_key: file.0,
            name: name.to_string(),
            created_at: file.2,
        };
        if file.1 != name {
            renamed.write(&path)?;
        }
        Ok(Some(renamed))
    }

    fn write(&self, path: &Path) -> Result<(), IdentityError> {
        let file = DeviceFile {
            secret_key: STANDARD.encode(self.secret_key.to_bytes()),
            name: self.name.clone(),
            created_at: self.created_at,
        };
        let mut json =
            serde_json::to_vec_pretty(&file).map_err(|e| IdentityError::Encode(e.to_string()))?;
        json.push(b'\n');
        let written = write_atomic(path, &json);
        json.fill(0);
        Ok(written?)
    }

    /// The key iroh uses as the endpoint secret.
    pub fn secret_key(&self) -> &SecretKey {
        &self.secret_key
    }

    /// The endpoint id: the public key.
    pub fn endpoint_id(&self) -> EndpointId {
        self.secret_key.public()
    }

    /// The endpoint id as iroh prints it: 64 lowercase hex characters (4.1).
    pub fn endpoint_id_hex(&self) -> String {
        self.endpoint_id().to_string()
    }

    /// The first 10 characters of the endpoint id (4.1).
    pub fn short_id(&self) -> String {
        short_id(&self.endpoint_id())
    }

    /// The device name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// When the key was created, in milliseconds.
    pub fn created_at(&self) -> u64 {
        self.created_at
    }
}

fn read_device_file(path: &Path) -> Result<Option<DeviceIdentity>, IdentityError> {
    let Some((secret_key, name, created_at)) = read_parsed(path)? else {
        return Ok(None);
    };
    if !is_device_name(&name) {
        return Err(IdentityError::BadName {
            path: path.to_path_buf(),
            name,
        });
    }
    Ok(Some(DeviceIdentity {
        secret_key,
        name,
        created_at,
    }))
}

// Reads and parses the device file without checking the name, tightening its mode when others could read it. The
// caller holds the lock.
fn read_parsed(path: &Path) -> Result<Option<(SecretKey, String, u64)>, IdentityError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(PathsError::Io {
                action: "read",
                path: path.to_path_buf(),
                error,
            }
            .into());
        }
    };
    let file = parse_device_file(&bytes).map_err(|reason| IdentityError::Corrupt {
        path: path.to_path_buf(),
        reason,
    })?;
    // A key file others could read may already be copied; tightening it is all that can be done here.
    if let Ok(Some(mode)) = mode_of(path)
        && mode & 0o077 != 0
    {
        tracing::warn!(path = %path.display(), mode = format!("{mode:o}"), "device key file was not private; setting 0600");
        set_mode(path, FILE_MODE).map_err(|error| PathsError::Io {
            action: "set the mode of",
            path: path.to_path_buf(),
            error,
        })?;
    }
    Ok(Some(file))
}

fn parse_device_file(bytes: &[u8]) -> Result<(SecretKey, String, u64), String> {
    let file: DeviceFile =
        serde_json::from_slice(bytes).map_err(|e| format!("not a device file: {e}"))?;
    let mut raw = STANDARD
        .decode(file.secret_key.as_bytes())
        .map_err(|_| "secret_key is not base64".to_string())?;
    let key = <[u8; 32]>::try_from(raw.as_slice())
        .map(|bytes| SecretKey::from_bytes(&bytes))
        .map_err(|_| format!("secret_key is {} bytes, not 32", raw.len()));
    raw.fill(0);
    Ok((key?, file.name, file.created_at))
}

/// The short form of an endpoint id: its first 10 hex characters (4.1).
pub fn short_id(id: &EndpointId) -> String {
    id.to_string().chars().take(SHORT_ID_LEN).collect()
}

/// Parses an endpoint id written the way iroh prints it, 64 lowercase hex characters, and nothing else.
pub fn parse_endpoint_id(s: &str) -> Option<EndpointId> {
    if s.len() != 64
        || !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    s.parse().ok()
}

/// This computer's platform as presence names it: `macos`, `linux` and so on (5.6).
pub fn platform() -> &'static str {
    std::env::consts::OS
}

/// The platform part of a default device name: `mac` for macOS, else the platform itself (2).
pub fn platform_short(platform: &str) -> &str {
    match platform {
        "macos" => "mac",
        other => other,
    }
}

/// The default device name `<person>-<platform>`, then `-2`, `-3` while one of your devices has it (2). The host
/// name is never used. `None` only when a thousand names are taken.
pub fn default_device_name(
    person: &str,
    platform: &str,
    taken: impl Fn(&str) -> bool,
) -> Option<String> {
    let joined = format!("{person}-{}", platform_short(platform));
    let base = if is_device_name(&joined) {
        joined
    } else {
        slugify(&joined, MAX_DEVICE)
    };
    let base = if base.is_empty() {
        "device".to_string()
    } else {
        base
    };
    if !taken(&base) {
        return Some(base);
    }
    (2..1000u32).find_map(|n| {
        let suffix = format!("-{n}");
        let stem_len = base.len().min(MAX_DEVICE - suffix.len());
        let candidate = format!("{}{suffix}", base[..stem_len].trim_end_matches('-'));
        (!taken(&candidate)).then_some(candidate)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> (tempfile::TempDir, Home) {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("mesh")).unwrap();
        (tmp, home)
    }

    #[test]
    fn creates_once_then_loads() {
        let (_tmp, home) = home();
        assert!(DeviceIdentity::load(&home).unwrap().is_none());
        let (created, fresh) =
            DeviceIdentity::load_or_create(&home, "sam-mbp", 1_730_000_000_000).unwrap();
        assert!(fresh);
        assert_eq!(created.name(), "sam-mbp");
        assert_eq!(created.created_at(), 1_730_000_000_000);
        let (again, fresh) = DeviceIdentity::load_or_create(&home, "other-name", 5).unwrap();
        assert!(!fresh);
        assert_eq!(again.endpoint_id(), created.endpoint_id());
        assert_eq!(again.name(), "sam-mbp");
        let loaded = DeviceIdentity::load(&home).unwrap().unwrap();
        assert_eq!(
            loaded.secret_key().to_bytes(),
            created.secret_key().to_bytes()
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_and_folder_are_private() {
        let (_tmp, home) = home();
        DeviceIdentity::load_or_create(&home, "sam-mbp", 1).unwrap();
        assert_eq!(mode_of(&home.device_file()).unwrap(), Some(0o600));
        assert_eq!(mode_of(&home.device_lock()).unwrap(), Some(0o600));
        assert_eq!(mode_of(&home.identity_dir()).unwrap(), Some(0o700));
        set_mode(&home.device_file(), 0o644).unwrap();
        DeviceIdentity::load(&home).unwrap().unwrap();
        assert_eq!(mode_of(&home.device_file()).unwrap(), Some(0o600));
    }

    #[test]
    fn file_format() {
        let (_tmp, home) = home();
        let (identity, _) = DeviceIdentity::load_or_create(&home, "sam-mbp", 42).unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.device_file()).unwrap()).unwrap();
        assert_eq!(json["name"], "sam-mbp");
        assert_eq!(json["created_at"], 42);
        let key = STANDARD
            .decode(json["secret_key"].as_str().unwrap())
            .unwrap();
        assert_eq!(key.as_slice(), identity.secret_key().to_bytes().as_slice());
        assert!(!format!("{identity:?}").contains(json["secret_key"].as_str().unwrap()));
    }

    #[test]
    fn ids_print_as_iroh_does() {
        let (_tmp, home) = home();
        let (identity, _) = DeviceIdentity::load_or_create(&home, "sam-mbp", 1).unwrap();
        let hex = identity.endpoint_id_hex();
        assert_eq!(hex.len(), 64);
        assert!(
            hex.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        );
        assert_eq!(identity.short_id(), hex[..10]);
        assert_eq!(parse_endpoint_id(&hex), Some(identity.endpoint_id()));
        assert_eq!(parse_endpoint_id(&hex.to_uppercase()), None);
        assert_eq!(parse_endpoint_id(&hex[..63]), None);
        assert_eq!(parse_endpoint_id(&identity.endpoint_id().to_z32()), None);
    }

    #[test]
    fn corrupt_file_stays_and_keeps_failing() {
        for bad in [
            &b"not json"[..],
            br#"{"secret_key": "!!!", "name": "sam-mbp", "created_at": 1}"#,
            br#"{"secret_key": "AAAA", "name": "sam-mbp", "created_at": 1}"#,
            br#"{"name": "sam-mbp", "created_at": 1}"#,
        ] {
            let (_tmp, home) = home();
            ensure_private_dir(&home.identity_dir()).unwrap();
            std::fs::write(home.device_file(), bad).unwrap();
            let err = DeviceIdentity::load(&home).unwrap_err();
            let message = err.to_string();
            assert!(matches!(err, IdentityError::Corrupt { .. }), "{message}");
            assert!(message.contains("device.json"), "{message}");
            assert!(message.contains("move the file away"), "{message}");
            assert!(message.contains("spew mesh login"), "{message}");
            assert!(message.contains("spew mesh devices revoke"), "{message}");
            // Every later start fails the same way and never touches the file.
            for now in [2, 3] {
                assert!(matches!(
                    DeviceIdentity::load_or_create(&home, "sam-mbp", now),
                    Err(IdentityError::Corrupt { .. })
                ));
                assert_eq!(std::fs::read(home.device_file()).unwrap(), bad);
            }
            assert!(matches!(
                DeviceIdentity::rename(&home, "sam-linux"),
                Err(IdentityError::Corrupt { .. })
            ));
            assert_eq!(std::fs::read(home.device_file()).unwrap(), bad);
            let others: Vec<_> = std::fs::read_dir(home.identity_dir())
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .filter(|n| n != "device.json" && n != "device.lock")
                .collect();
            assert!(others.is_empty(), "{others:?}");
            // Once the person moves it away, the next start creates a new key.
            std::fs::rename(
                home.device_file(),
                home.identity_dir().join("device.json.old"),
            )
            .unwrap();
            let (_, fresh) = DeviceIdentity::load_or_create(&home, "sam-mbp", 4).unwrap();
            assert!(fresh);
        }
    }

    #[test]
    fn bad_name_keeps_the_file() {
        let (_tmp, home) = home();
        DeviceIdentity::load_or_create(&home, "sam-mbp", 1).unwrap();
        let text = std::fs::read_to_string(home.device_file()).unwrap();
        std::fs::write(home.device_file(), text.replace("sam-mbp", "Sam's Mac")).unwrap();
        let err = DeviceIdentity::load(&home).unwrap_err();
        assert!(matches!(err, IdentityError::BadName { .. }), "{err}");
        assert!(err.to_string().contains("1 to 48"), "{err}");
        assert!(home.device_file().exists());
        // Renaming repairs it and keeps the key.
        let id = DeviceIdentity::rename(&home, "sam-mbp").unwrap().unwrap();
        assert_eq!(
            DeviceIdentity::load(&home).unwrap().unwrap().endpoint_id(),
            id.endpoint_id()
        );
    }

    #[test]
    fn unknown_fields_are_kept_readable() {
        let (_tmp, home) = home();
        let (identity, _) = DeviceIdentity::load_or_create(&home, "sam-mbp", 1).unwrap();
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.device_file()).unwrap()).unwrap();
        json["future"] = serde_json::json!(true);
        std::fs::write(home.device_file(), serde_json::to_vec(&json).unwrap()).unwrap();
        assert_eq!(
            DeviceIdentity::load(&home).unwrap().unwrap().endpoint_id(),
            identity.endpoint_id()
        );
    }

    #[test]
    fn rename_changes_only_the_name() {
        let (_tmp, home) = home();
        assert!(
            DeviceIdentity::rename(&home, "sam-linux")
                .unwrap()
                .is_none()
        );
        assert!(!home.device_file().exists());
        let (stale, _) = DeviceIdentity::load_or_create(&home, "sam-mbp", 1).unwrap();
        // Another process replaces the key after this one read it.
        std::fs::remove_file(home.device_file()).unwrap();
        let (current, _) = DeviceIdentity::load_or_create(&home, "sam-mbp", 9).unwrap();
        assert_ne!(stale.endpoint_id(), current.endpoint_id());
        let renamed = DeviceIdentity::rename(&home, "sam.linux").unwrap().unwrap();
        assert_eq!(renamed.name(), "sam.linux");
        assert_eq!(renamed.endpoint_id(), current.endpoint_id());
        assert_eq!(renamed.created_at(), 9);
        let loaded = DeviceIdentity::load(&home).unwrap().unwrap();
        assert_eq!(loaded.name(), "sam.linux");
        assert_eq!(loaded.endpoint_id(), current.endpoint_id());
        assert_eq!(loaded.created_at(), 9);
        let err = DeviceIdentity::rename(&home, "Bad Name").unwrap_err();
        assert!(matches!(err, IdentityError::InvalidName(_)), "{err}");
        assert!(err.to_string().contains("1 to 48"), "{err}");
        assert!(matches!(
            DeviceIdentity::rename(&home, &"d".repeat(49)),
            Err(IdentityError::InvalidName(_))
        ));
        assert!(matches!(
            DeviceIdentity::load_or_create(&home, "", 1),
            Err(IdentityError::InvalidName(_))
        ));
    }

    #[test]
    fn concurrent_creation_makes_one_key() {
        let (_tmp, home) = home();
        let ids: Vec<EndpointId> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        DeviceIdentity::load_or_create(&home, "sam-mbp", 1)
                            .unwrap()
                            .0
                            .endpoint_id()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(ids.windows(2).all(|w| w[0] == w[1]));
    }

    #[test]
    fn default_names() {
        assert_eq!(
            default_device_name("maya", "macos", |_| false).unwrap(),
            "maya-mac"
        );
        assert_eq!(
            default_device_name("maya", "linux", |_| false).unwrap(),
            "maya-linux"
        );
        assert_eq!(
            default_device_name("maya", "linux", |n| n == "maya-linux").unwrap(),
            "maya-linux-2"
        );
        assert_eq!(
            default_device_name("maya", "linux", |n| n == "maya-linux"
                || n == "maya-linux-2")
            .unwrap(),
            "maya-linux-3"
        );
        assert_eq!(
            default_device_name("sam_o.k", "freebsd", |_| false).unwrap(),
            "sam_o.k-freebsd"
        );
        assert_eq!(
            default_device_name("Sam K", "linux", |_| false).unwrap(),
            "sam-k-linux"
        );
        let long = "p".repeat(60);
        let name =
            default_device_name(&long, "linux", |n| n.len() == 48 && !n.ends_with("-2")).unwrap();
        assert_eq!(default_device_name("maya", "linux", |_| true), None);
        assert!(is_device_name(&name), "{name}");
        assert!(name.ends_with("-2"));
        assert_eq!(
            platform_short(platform()),
            if cfg!(target_os = "macos") {
                "mac"
            } else {
                platform()
            }
        );
    }
}
