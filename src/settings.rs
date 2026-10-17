//! settings.toml (11): loading, fail-closed validation, defaults, environment overrides, and a session's room.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};

use serde::Deserialize;
use url::Url;

use crate::address::{DEVICE_RULE, PERSON_RULE, is_client, is_device_name, is_person, is_room};
use crate::paths::{Home, expand_tilde};

/// The default spew server (11, 14).
pub const DEFAULT_SPEW_URL: &str = "https://mcp.spew.lol";
/// The variable that turns mDNS off when `0` (11).
pub const LAN_ENV: &str = "SPEW_MESH_LAN";
/// The variable holding the tracing filter (11).
pub const LOG_ENV: &str = "SPEW_MESH_LOG";
/// The variable setup puts in MCP configs to name the client (8.6).
pub const CLIENT_ENV: &str = "SPEW_MESH_CLIENT";
/// The variable spew's agent tools read for the project (11).
pub const PROJECT_ENV: &str = "SPEW_PROJECT";
/// The file in a repo that names its spew project (11).
pub const SPEW_JSON: &str = ".spew.json";

const SPEW_JSON_MAX_BYTES: u64 = 64 * 1024;

/// Which roster provider to use (4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RosterKind {
    /// This device only.
    #[default]
    Local,
    /// A roster file.
    File,
    /// spew sign-in.
    Spew,
}

/// How peers are found (5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkMode {
    /// n0's public relays and address lookup.
    #[default]
    N0,
    /// Your own relays only.
    Own,
    /// No relays: mDNS and roster hints only.
    Local,
}

impl NetworkMode {
    /// The mode as written in settings.
    pub fn as_str(self) -> &'static str {
        match self {
            NetworkMode::N0 => "n0",
            NetworkMode::Own => "own",
            NetworkMode::Local => "local",
        }
    }
}

/// Who may ask a responder (9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServeFrom {
    /// Anyone admitted who shares the responder's room.
    #[default]
    Room,
    /// Only your own devices.
    Own,
}

/// A secret setting value. `Debug` never prints it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// The value, for the one place that sends it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

/// `[roster]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterSettings {
    /// The provider.
    pub kind: RosterKind,
    /// The roster file, absolute.
    pub file: PathBuf,
    /// Room for sessions whose repo has no `.spew.json`.
    pub default_room: Option<String>,
}

/// `[spew]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpewSettings {
    /// The spew server; https, or http on 127.0.0.1 or localhost.
    pub url: Url,
    /// Roster refresh period.
    pub refresh_seconds: u64,
    /// How long your own devices from a stale roster stay admitted.
    pub max_stale_seconds: u64,
}

/// `[network]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkSettings {
    /// How peers are found.
    pub mode: NetworkMode,
    /// Address to bind.
    pub bind: IpAddr,
    /// Port to bind; 0 is any.
    pub port: u16,
    /// mDNS on the LAN; `SPEW_MESH_LAN=0` turns it off.
    pub mdns: bool,
    /// Relay URLs for mode `own`.
    pub relays: Vec<Url>,
    /// Bearer token for those relays.
    pub relay_token: Option<Secret>,
}

/// `[presence]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresenceSettings {
    /// Send the branch to teammates (rooms from `.spew.json` only).
    pub branch: bool,
    /// Send the folder name to teammates (same rule).
    pub place: bool,
}

/// `[serve]`.
#[derive(Debug, Clone, PartialEq)]
pub struct ServeSettings {
    /// Run timeout.
    pub timeout_seconds: u64,
    /// Turn cap for Claude Code runs (`--max-turns`).
    pub max_turns: u32,
    /// Who may ask.
    pub from: ServeFrom,
    /// Runs per asking person per hour.
    pub max_runs_per_person_per_hour: u32,
    /// Runs per hour in all.
    pub max_runs_per_hour: u32,
    /// Dollar cap per Claude Code run.
    pub max_budget_usd: f64,
}

/// Validated settings (11).
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// Your name in local and file rosters.
    pub username: Option<String>,
    /// This device's name.
    pub device_name: Option<String>,
    /// `[roster]`.
    pub roster: RosterSettings,
    /// `[spew]`.
    pub spew: SpewSettings,
    /// `[network]`.
    pub network: NetworkSettings,
    /// `[presence]`.
    pub presence: PresenceSettings,
    /// `[serve]`.
    pub serve: ServeSettings,
}

/// Why settings or the environment are unusable. The daemon refuses to start on any of these (11).
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    /// The file exists but can't be read.
    #[error("could not read {}: {error}", path.display())]
    Read {
        /// The settings file.
        path: PathBuf,
        /// The OS error.
        error: io::Error,
    },
    /// The file doesn't parse or a value breaks its rule.
    #[error("{} is not valid: {message}", path.display())]
    Invalid {
        /// The settings file.
        path: PathBuf,
        /// What is wrong.
        message: String,
    },
    /// An environment variable has a value it can't take.
    #[error("{name} is not valid: {message}")]
    Env {
        /// The variable.
        name: &'static str,
        /// What is wrong.
        message: String,
    },
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSettings {
    username: Option<String>,
    device_name: Option<String>,
    #[serde(default)]
    roster: RawRoster,
    #[serde(default)]
    spew: RawSpew,
    #[serde(default)]
    network: RawNetwork,
    #[serde(default)]
    presence: RawPresence,
    #[serde(default)]
    serve: RawServe,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRoster {
    kind: Option<RosterKind>,
    file: Option<String>,
    default_room: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSpew {
    url: Option<String>,
    refresh_seconds: Option<u64>,
    max_stale_seconds: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNetwork {
    mode: Option<NetworkMode>,
    bind: Option<String>,
    port: Option<u16>,
    mdns: Option<bool>,
    relays: Option<Vec<String>>,
    relay_token: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPresence {
    branch: Option<bool>,
    place: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServe {
    timeout_seconds: Option<u64>,
    max_turns: Option<u32>,
    from: Option<ServeFrom>,
    max_runs_per_person_per_hour: Option<u32>,
    max_runs_per_hour: Option<u32>,
    max_budget_usd: Option<Number>,
}

// TOML keeps integers and floats apart; `max_budget_usd = 1` should still mean one dollar.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(untagged)]
enum Number {
    Int(i64),
    Float(f64),
}

impl Number {
    fn value(self) -> f64 {
        match self {
            // Budgets are small; i64 to f64 is exact for them.
            Number::Int(i) => i as f64,
            Number::Float(f) => f,
        }
    }
}

fn in_range<T: PartialOrd + fmt::Display + Copy>(
    value: Option<T>,
    default: T,
    min: T,
    max: T,
    key: &str,
) -> Result<T, String> {
    let value = value.unwrap_or(default);
    if value < min || value > max {
        return Err(format!("{key} = {value} is out of range ({min} to {max})"));
    }
    Ok(value)
}

/// Checks a URL the daemon will send credentials to: `https`, or `http` on 127.0.0.1 or localhost only, with no
/// user info, query or fragment.
pub fn check_service_url(text: &str, key: &str) -> Result<Url, String> {
    let url = Url::parse(text).map_err(|e| format!("{key} = \"{text}\" is not a URL: {e}"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!("{key} must not hold a user name or password"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(format!("{key} must not have a query or fragment"));
    }
    let loopback = matches!(url.host_str(), Some("127.0.0.1" | "localhost"));
    match url.scheme() {
        "https" if url.host_str().is_some_and(|h| !h.is_empty()) => Ok(url),
        "http" if loopback => Ok(url),
        _ => Err(format!(
            "{key} = \"{text}\" must be https (http only on 127.0.0.1 or localhost)"
        )),
    }
}

fn optional(
    value: Option<String>,
    valid: impl Fn(&str) -> bool,
    key: &str,
    what: &str,
) -> Result<Option<String>, String> {
    match value {
        None => Ok(None),
        Some(v) if v.is_empty() => Ok(None),
        Some(v) if valid(&v) => Ok(Some(v)),
        Some(v) => Err(format!("{key} = \"{v}\" is not {what}")),
    }
}

impl Settings {
    /// The settings when there is no settings file.
    pub fn defaults(home: &Home) -> Settings {
        Settings {
            username: None,
            device_name: None,
            roster: RosterSettings {
                kind: RosterKind::Local,
                file: home.roster_file(),
                default_room: None,
            },
            spew: SpewSettings {
                url: Url::parse(DEFAULT_SPEW_URL).unwrap_or_else(|_| unreachable!("constant URL")),
                refresh_seconds: 60,
                max_stale_seconds: 86_400,
            },
            network: NetworkSettings {
                mode: NetworkMode::N0,
                bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                port: 0,
                mdns: true,
                relays: Vec::new(),
                relay_token: None,
            },
            presence: PresenceSettings {
                branch: true,
                place: true,
            },
            serve: ServeSettings {
                timeout_seconds: 300,
                max_turns: 20,
                from: ServeFrom::Room,
                max_runs_per_person_per_hour: 10,
                max_runs_per_hour: 30,
                max_budget_usd: 0.50,
            },
        }
    }

    /// Reads `<home>/settings.toml`; a missing file gives the defaults. Environment overrides are separate
    /// ([`Settings::apply_env`]).
    pub fn load(home: &Home, user_home: Option<&Path>) -> Result<Settings, SettingsError> {
        let path = home.settings_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => Settings::parse(&text, &path, home, user_home),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Settings::defaults(home)),
            Err(error) => Err(SettingsError::Read { path, error }),
        }
    }

    /// Parses and validates settings text. `path` is only used in errors. Empty strings count as unset.
    pub fn parse(
        text: &str,
        path: &Path,
        home: &Home,
        user_home: Option<&Path>,
    ) -> Result<Settings, SettingsError> {
        let invalid = |message: String| SettingsError::Invalid {
            path: path.to_path_buf(),
            message,
        };
        let raw: RawSettings =
            toml::from_str(text).map_err(|e| invalid(e.to_string().trim_end().to_string()))?;
        Settings::validate(raw, home, user_home).map_err(invalid)
    }

    fn validate(
        raw: RawSettings,
        home: &Home,
        user_home: Option<&Path>,
    ) -> Result<Settings, String> {
        let defaults = Settings::defaults(home);
        let username = optional(
            raw.username,
            is_person,
            "username",
            &format!("a valid person name ({PERSON_RULE})"),
        )?;
        let device_name = optional(
            raw.device_name,
            is_device_name,
            "device_name",
            &format!("a valid device name ({DEVICE_RULE})"),
        )?;

        let file = match raw.roster.file.filter(|f| !f.is_empty()) {
            None => defaults.roster.file,
            Some(f) => {
                let expanded = expand_tilde(&f, user_home);
                // Only `~` and `~/...` expand; `~maya/x` is an ordinary relative path.
                if (f == "~" || f.starts_with("~/")) && user_home.is_none() {
                    return Err(format!(
                        "[roster] file = \"{f}\" starts with ~ but HOME is not set"
                    ));
                }
                // A relative path means inside the home, never the daemon's working directory.
                home.root().join(expanded)
            }
        };
        let roster = RosterSettings {
            kind: raw.roster.kind.unwrap_or_default(),
            file,
            default_room: optional(
                raw.roster.default_room,
                is_room,
                "[roster] default_room",
                &format!("a valid room id ({PERSON_RULE})"),
            )?,
        };

        let url = match raw.spew.url.filter(|u| !u.is_empty()) {
            Some(u) => check_service_url(&u, "[spew] url")?,
            None => defaults.spew.url,
        };
        let spew = SpewSettings {
            url,
            refresh_seconds: in_range(
                raw.spew.refresh_seconds,
                60,
                10,
                86_400,
                "[spew] refresh_seconds",
            )?,
            max_stale_seconds: in_range(
                raw.spew.max_stale_seconds,
                86_400,
                0,
                2_592_000,
                "[spew] max_stale_seconds",
            )?,
        };

        let mode = raw.network.mode.unwrap_or_default();
        let bind = match raw.network.bind.filter(|b| !b.is_empty()) {
            Some(b) => b
                .parse::<IpAddr>()
                .map_err(|_| format!("[network] bind = \"{b}\" is not an IP address"))?,
            None => defaults.network.bind,
        };
        let relays = raw
            .network
            .relays
            .unwrap_or_default()
            .iter()
            .map(|r| check_service_url(r, "[network] relays"))
            .collect::<Result<Vec<Url>, String>>()?;
        let relay_token = raw
            .network
            .relay_token
            .filter(|t| !t.is_empty())
            .map(Secret);
        // Relays set under another mode would be ignored, and the daemon would quietly use n0 instead of the
        // person's own relays: refuse rather than guess.
        match mode {
            NetworkMode::Own if relays.is_empty() => {
                return Err("[network] mode = \"own\" needs at least one URL in relays".into());
            }
            NetworkMode::N0 | NetworkMode::Local if !relays.is_empty() || relay_token.is_some() => {
                return Err(format!(
                    "[network] relays and relay_token are only for mode = \"own\" (mode is \"{}\")",
                    mode.as_str()
                ));
            }
            _ => {}
        }
        let network = NetworkSettings {
            mode,
            bind,
            port: raw.network.port.unwrap_or(0),
            mdns: raw.network.mdns.unwrap_or(true),
            relays,
            relay_token,
        };

        let presence = PresenceSettings {
            branch: raw.presence.branch.unwrap_or(true),
            place: raw.presence.place.unwrap_or(true),
        };

        let budget = raw
            .serve
            .max_budget_usd
            .map_or(defaults.serve.max_budget_usd, Number::value);
        if !budget.is_finite() || budget <= 0.0 || budget > 1000.0 {
            return Err(format!(
                "[serve] max_budget_usd = {budget} is out of range (above 0, at most 1000)"
            ));
        }
        let serve = ServeSettings {
            timeout_seconds: in_range(
                raw.serve.timeout_seconds,
                300,
                1,
                86_400,
                "[serve] timeout_seconds",
            )?,
            max_turns: in_range(raw.serve.max_turns, 20, 1, 1000, "[serve] max_turns")?,
            from: raw.serve.from.unwrap_or_default(),
            max_runs_per_person_per_hour: in_range(
                raw.serve.max_runs_per_person_per_hour,
                10,
                0,
                10_000,
                "[serve] max_runs_per_person_per_hour",
            )?,
            max_runs_per_hour: in_range(
                raw.serve.max_runs_per_hour,
                30,
                0,
                10_000,
                "[serve] max_runs_per_hour",
            )?,
            max_budget_usd: budget,
        };

        Ok(Settings {
            username,
            device_name,
            roster,
            spew,
            network,
            presence,
            serve,
        })
    }

    /// Applies environment overrides: `SPEW_MESH_LAN=0` turns mDNS off (5.1, 11).
    pub fn apply_env(&mut self, env: &Env) {
        if env.lan_off {
            self.network.mdns = false;
        }
    }
}

/// The spew mesh variables from an environment (11).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Env {
    /// `SPEW_MESH_DIR`, when set and not empty.
    pub mesh_dir: Option<OsString>,
    /// `SPEW_MESH_LAN=0`.
    pub lan_off: bool,
    /// `SPEW_MESH_LOG`.
    pub log: Option<String>,
    /// `SPEW_MESH_CLIENT` as given.
    pub client: Option<String>,
    /// `SPEW_PROJECT`.
    pub spew_project: Option<String>,
}

impl Env {
    /// Reads the variables from name and value pairs. `SPEW_MESH_LAN` must be `0`, `1` or empty.
    pub fn from_vars<I, K, V>(vars: I) -> Result<Env, SettingsError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        let mut env = Env::default();
        for (name, value) in vars {
            let (name, value): (OsString, OsString) = (name.into(), value.into());
            let text = || value.to_str().map(str::to_string).filter(|v| !v.is_empty());
            match name.to_str() {
                Some(crate::paths::HOME_ENV) => {
                    env.mesh_dir = Some(value.clone()).filter(|v| !v.is_empty())
                }
                Some(LAN_ENV) => {
                    env.lan_off = match value.to_str() {
                        Some("0") => true,
                        Some("1" | "") => false,
                        _ => {
                            return Err(SettingsError::Env {
                                name: LAN_ENV,
                                message: "use 0 to turn mDNS off, or 1".into(),
                            });
                        }
                    }
                }
                Some(LOG_ENV) => env.log = text(),
                Some(CLIENT_ENV) => env.client = text(),
                Some(PROJECT_ENV) => env.spew_project = text(),
                _ => {}
            }
        }
        Ok(env)
    }

    /// Reads this process's environment.
    pub fn from_process() -> Result<Env, SettingsError> {
        Env::from_vars(std::env::vars_os())
    }

    /// The client named by `SPEW_MESH_CLIENT` when it is a valid client slug (2), lowercased.
    pub fn client(&self) -> Option<String> {
        self.client
            .as_deref()
            .map(str::to_ascii_lowercase)
            .filter(|c| is_client(c))
    }
}

/// Where a session's room came from (11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomSource {
    /// The `SPEW_PROJECT` variable.
    Env,
    /// The `project` key of this `.spew.json`.
    SpewJson(PathBuf),
    /// `[roster] default_room`.
    DefaultRoom,
    /// Nothing named a room.
    None,
    /// Something named a room but it can't be used; the session stays private.
    Invalid(String),
}

/// A session's room and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomChoice {
    /// The room, or none: visible to your own devices only.
    pub room: Option<String>,
    /// Where it came from.
    pub source: RoomSource,
}

impl RoomChoice {
    /// Whether the room came from the repo's `.spew.json`, the only source that lets branch and place reach
    /// teammates (5.6).
    pub fn from_spew_json(&self) -> bool {
        matches!(self.source, RoomSource::SpewJson(_))
    }
}

/// The nearest `.spew.json` at or above `cwd`.
pub fn find_spew_json(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .map(|dir| dir.join(SPEW_JSON))
        .find(|candidate| candidate.is_file())
}

#[derive(Deserialize)]
struct SpewJson {
    project: Option<String>,
}

fn read_spew_json(path: &Path) -> Result<String, String> {
    let meta =
        std::fs::metadata(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    if meta.len() > SPEW_JSON_MAX_BYTES {
        return Err(format!("{} is too large", path.display()));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let parsed: SpewJson = serde_json::from_str(&text)
        .map_err(|e| format!("{} is not valid JSON: {e}", path.display()))?;
    match parsed.project {
        Some(project) if is_room(&project) => Ok(project),
        Some(project) => Err(format!(
            "{} names project \"{project}\", which is not a project key",
            path.display()
        )),
        None => Err(format!("{} has no project key", path.display())),
    }
}

/// A session's room (11): `SPEW_PROJECT`, else the `project` of the nearest `.spew.json` above `cwd`, else
/// `default_room`, else none. A value that is set but unusable leaves the session private rather than falling
/// through to a room it didn't name.
pub fn resolve_room(
    spew_project: Option<&str>,
    cwd: &Path,
    default_room: Option<&str>,
) -> RoomChoice {
    let private = |why: String| RoomChoice {
        room: None,
        source: RoomSource::Invalid(why),
    };
    if let Some(project) = spew_project.filter(|p| !p.is_empty()) {
        if is_room(project) {
            return RoomChoice {
                room: Some(project.to_string()),
                source: RoomSource::Env,
            };
        }
        return private(format!(
            "{PROJECT_ENV} = \"{project}\" is not a project key"
        ));
    }
    if let Some(path) = find_spew_json(cwd) {
        return match read_spew_json(&path) {
            Ok(project) => RoomChoice {
                room: Some(project),
                source: RoomSource::SpewJson(path),
            },
            Err(why) => private(why),
        };
    }
    match default_room.filter(|r| is_room(r)) {
        Some(room) => RoomChoice {
            room: Some(room.to_string()),
            source: RoomSource::DefaultRoom,
        },
        None => RoomChoice {
            room: None,
            source: RoomSource::None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> Home {
        Home::at("/m").unwrap()
    }

    fn parse(text: &str) -> Result<Settings, SettingsError> {
        Settings::parse(
            text,
            Path::new("settings.toml"),
            &home(),
            Some(Path::new("/home/sam")),
        )
    }

    fn error(text: &str) -> String {
        match parse(text) {
            Err(e) => e.to_string(),
            Ok(s) => panic!("accepted {text:?}: {s:?}"),
        }
    }

    const FULL: &str = r#"
username = "sam"
device_name = "sam-mbp"

[roster]
kind = "file"
file = "~/.spew/mesh/roster.toml"
default_room = "spew"

[spew]
url = "https://mcp.spew.lol"
refresh_seconds = 60
max_stale_seconds = 86400

[network]
mode = "own"
bind = "127.0.0.1"
port = 7461
mdns = false
relays = ["https://relay.example.com"]
relay_token = "placeholder-token"

[presence]
branch = false
place = true

[serve]
timeout_seconds = 120
max_turns = 5
from = "own"
max_runs_per_person_per_hour = 2
max_runs_per_hour = 4
max_budget_usd = 1
"#;

    #[test]
    fn empty_file_is_defaults() {
        assert_eq!(parse("").unwrap(), Settings::defaults(&home()));
        let d = Settings::defaults(&home());
        assert_eq!(d.roster.file, PathBuf::from("/m/roster.toml"));
        assert_eq!(d.spew.url.as_str(), "https://mcp.spew.lol/");
        assert_eq!(d.network.bind.to_string(), "0.0.0.0");
        assert!(d.network.mdns);
        assert!(d.presence.branch && d.presence.place);
        assert_eq!(d.serve.max_budget_usd, 0.5);
    }

    #[test]
    fn every_key_parses() {
        let s = parse(FULL).unwrap();
        assert_eq!(s.username.as_deref(), Some("sam"));
        assert_eq!(s.device_name.as_deref(), Some("sam-mbp"));
        assert_eq!(s.roster.kind, RosterKind::File);
        assert_eq!(
            s.roster.file,
            PathBuf::from("/home/sam/.spew/mesh/roster.toml")
        );
        assert_eq!(s.roster.default_room.as_deref(), Some("spew"));
        assert_eq!(s.network.mode, NetworkMode::Own);
        assert_eq!(s.network.bind.to_string(), "127.0.0.1");
        assert_eq!(s.network.port, 7461);
        assert!(!s.network.mdns);
        assert_eq!(s.network.relays.len(), 1);
        assert_eq!(
            s.network.relay_token.as_ref().map(Secret::expose),
            Some("placeholder-token")
        );
        assert!(!format!("{s:?}").contains("placeholder-token"));
        assert!(!s.presence.branch);
        assert_eq!(s.serve.timeout_seconds, 120);
        assert_eq!(s.serve.max_turns, 5);
        assert_eq!(s.serve.from, ServeFrom::Own);
        assert_eq!(s.serve.max_runs_per_person_per_hour, 2);
        assert_eq!(s.serve.max_runs_per_hour, 4);
        assert_eq!(s.serve.max_budget_usd, 1.0);
    }

    #[test]
    fn empty_strings_are_unset() {
        let s = parse(
            "username = \"\"\ndevice_name = \"\"\n[roster]\nfile = \"\"\ndefault_room = \"\"\n",
        )
        .unwrap();
        assert_eq!(s, Settings::defaults(&home()));
    }

    #[test]
    fn relative_roster_file_is_inside_home() {
        let s = parse("[roster]\nfile = \"team.toml\"\n").unwrap();
        assert_eq!(s.roster.file, PathBuf::from("/m/team.toml"));
        for needs_home in ["~", "~/r.toml"] {
            let no_home = Settings::parse(
                &format!("[roster]\nfile = \"{needs_home}\"\n"),
                Path::new("s"),
                &home(),
                None,
            )
            .unwrap_err();
            assert!(no_home.to_string().contains("HOME is not set"), "{no_home}");
        }
        // Only `~` and `~/...` expand; `~maya/x` is a relative path and never needs HOME.
        let other_user = Settings::parse(
            "[roster]\nfile = \"~maya/x\"\n",
            Path::new("s"),
            &home(),
            None,
        )
        .unwrap();
        assert_eq!(other_user.roster.file, PathBuf::from("/m/~maya/x"));
    }

    #[test]
    fn unknown_keys_fail() {
        assert!(error("usernme = \"sam\"").contains("usernme"));
        assert!(error("[roster]\nkinds = \"file\"").contains("kinds"));
        assert!(error("[spew]\ntoken = \"x\"").contains("token"));
        assert!(error("[network]\nmdns_name = \"x\"").contains("mdns_name"));
        assert!(error("[presence]\ncards = true").contains("cards"));
        assert!(error("[serve]\nmax_runs = 1").contains("max_runs"));
        assert!(error("[extra]\na = 1").contains("extra"));
    }

    #[test]
    fn bad_values_fail() {
        let long_user = format!("username = \"{}\"", "p".repeat(33));
        let long_device = format!("device_name = \"{}\"", "d".repeat(49));
        for (text, needle) in [
            ("username = \"Sam Smith\"", "username"),
            ("username = \"me\"", "not \"me\""),
            (long_user.as_str(), "1 to 32"),
            ("device_name = \"Sam's Mac\"", "1 to 48"),
            (long_device.as_str(), "device_name"),
            ("[roster]\ndefault_room = \"me\"", "default_room"),
            ("[roster]\nkind = \"ldap\"", "ldap"),
            ("[roster]\ndefault_room = \"a b\"", "default_room"),
            ("[spew]\nrefresh_seconds = 1", "refresh_seconds"),
            ("[spew]\nrefresh_seconds = -5", "refresh_seconds"),
            ("[spew]\nmax_stale_seconds = 99999999", "max_stale_seconds"),
            ("[network]\nmode = \"wan\"", "wan"),
            ("[network]\nbind = \"localhost\"", "bind"),
            ("[network]\nport = 70000", "port"),
            ("[network]\nmdns = \"yes\"", "mdns"),
            ("[network]\nmode = \"own\"", "at least one URL"),
            (
                "[network]\nrelays = [\"https://relay.example.com\"]",
                "only for mode",
            ),
            (
                "[network]\nmode = \"local\"\nrelay_token = \"t\"",
                "only for mode",
            ),
            (
                "[network]\nmode = \"own\"\nrelays = [\"http://relay.example.com\"]",
                "must be https",
            ),
            (
                "[network]\nmode = \"own\"\nrelays = [\"not a url\"]",
                "not a URL",
            ),
            ("[presence]\nbranch = 1", "branch"),
            ("[serve]\ntimeout_seconds = 0", "timeout_seconds"),
            ("[serve]\nmax_turns = 0", "max_turns"),
            ("[serve]\nfrom = \"anyone\"", "anyone"),
            ("[serve]\nmax_budget_usd = 0", "max_budget_usd"),
            ("[serve]\nmax_budget_usd = -1.5", "max_budget_usd"),
            ("[serve]\nmax_budget_usd = nan", "max_budget_usd"),
            ("[serve]\nmax_budget_usd = \"1\"", "max_budget_usd"),
            ("username = ", "settings.toml"),
        ] {
            let message = error(text);
            assert!(message.contains(needle), "{text:?}: {message}");
            assert!(
                message.starts_with("settings.toml is not valid"),
                "{message}"
            );
        }
    }

    #[test]
    fn spew_url_rules() {
        for good in [
            "https://mcp.spew.lol",
            "https://spew.example.com/base",
            "http://127.0.0.1:8080",
            "http://localhost:3000",
            "http://LOCALHOST",
        ] {
            assert!(
                parse(&format!("[spew]\nurl = \"{good}\"")).is_ok(),
                "{good}"
            );
        }
        for bad in [
            "http://spew.example.com",
            "http://127.0.0.2",
            "http://[::1]:80",
            "http://localhost.example.com",
            "ftp://example.com",
            "https://sam:pw@example.com",
            "https://example.com/?x=1",
            "https://example.com/#f",
            "file:///etc/passwd",
            "spew.example.com",
        ] {
            let message = error(&format!("[spew]\nurl = \"{bad}\""));
            assert!(message.contains("[spew] url"), "{bad}: {message}");
        }
    }

    #[test]
    fn env_overrides() {
        let env = Env::from_vars([
            ("SPEW_MESH_LAN", "0"),
            ("SPEW_MESH_DIR", "/srv/mesh"),
            ("SPEW_MESH_LOG", "debug"),
            ("SPEW_MESH_CLIENT", "Gemini"),
            ("SPEW_PROJECT", "spew"),
            ("UNRELATED", "x"),
        ])
        .unwrap();
        assert!(env.lan_off);
        assert_eq!(env.mesh_dir, Some(OsString::from("/srv/mesh")));
        assert_eq!(env.log.as_deref(), Some("debug"));
        assert_eq!(env.client().as_deref(), Some("gemini"));
        assert_eq!(env.spew_project.as_deref(), Some("spew"));
        let mut s = parse("").unwrap();
        s.apply_env(&env);
        assert!(!s.network.mdns);

        let on = Env::from_vars([("SPEW_MESH_LAN", "1"), ("SPEW_MESH_DIR", "")]).unwrap();
        assert!(!on.lan_off);
        assert_eq!(on.mesh_dir, None);
        let mut s = parse("").unwrap();
        s.apply_env(&on);
        assert!(s.network.mdns);
        assert!(Env::from_vars([("SPEW_MESH_LAN", "off")]).is_err());
        let bad_client = Env::from_vars([("SPEW_MESH_CLIENT", "not a slug")]).unwrap();
        assert_eq!(bad_client.client(), None);
    }

    #[test]
    fn load_reads_the_home_file() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path()).unwrap();
        let missing = Settings::load(&home, None).unwrap();
        assert_eq!(missing, Settings::defaults(&home));
        std::fs::write(home.settings_file(), "username = \"maya\"\n").unwrap();
        assert_eq!(
            Settings::load(&home, None).unwrap().username.as_deref(),
            Some("maya")
        );
        std::fs::write(home.settings_file(), "username = 5\n").unwrap();
        let err = Settings::load(&home, None).unwrap_err().to_string();
        assert!(err.contains("settings.toml is not valid"), "{err}");
    }

    #[test]
    fn room_order() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let deep = repo.join("src").join("deep");
        std::fs::create_dir_all(&deep).unwrap();
        let outside = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();

        let none = resolve_room(None, &outside, None);
        assert_eq!(
            none,
            RoomChoice {
                room: None,
                source: RoomSource::None
            }
        );
        let default = resolve_room(None, &outside, Some("team"));
        assert_eq!(default.room.as_deref(), Some("team"));
        assert_eq!(default.source, RoomSource::DefaultRoom);
        assert!(!default.from_spew_json());

        std::fs::write(
            repo.join(".spew.json"),
            r#"{"project": "spew", "other": 1}"#,
        )
        .unwrap();
        let found = resolve_room(None, &deep, Some("team"));
        assert_eq!(found.room.as_deref(), Some("spew"));
        assert_eq!(found.source, RoomSource::SpewJson(repo.join(".spew.json")));
        assert!(found.from_spew_json());

        let env = resolve_room(Some("web"), &deep, Some("team"));
        assert_eq!(env.room.as_deref(), Some("web"));
        assert_eq!(env.source, RoomSource::Env);
        assert!(!env.from_spew_json());
        assert_eq!(
            resolve_room(Some(""), &deep, None).room.as_deref(),
            Some("spew")
        );

        let bad_env = resolve_room(Some("Bad Key"), &deep, Some("team"));
        assert_eq!(bad_env.room, None);
        assert!(matches!(bad_env.source, RoomSource::Invalid(_)));
    }

    #[test]
    fn unusable_spew_json_keeps_the_session_private() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join(".spew.json");
        for text in [
            "not json",
            "{}",
            r#"{"project": "a|b"}"#,
            r#"{"project": 5}"#,
        ] {
            std::fs::write(&file, text).unwrap();
            let choice = resolve_room(None, tmp.path(), Some("team"));
            assert_eq!(choice.room, None, "{text}");
            assert!(matches!(choice.source, RoomSource::Invalid(_)), "{text}");
        }
        std::fs::write(&file, " ".repeat(70 * 1024)).unwrap();
        assert!(matches!(
            resolve_room(None, tmp.path(), None).source,
            RoomSource::Invalid(_)
        ));
    }
}
