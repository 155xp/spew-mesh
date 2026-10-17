//! The spew directory API client: sign-in, device registration and peers (14).
//!
//! Sign-in is OAuth 2.1 for a public client: PKCE (S256) and a loopback redirect (RFC 8252). Tokens live in
//! `auth/spew.json` (0600) and never reach logs. The grant must carry exactly the `mesh` scope; any other token
//! is revoked at once and nothing is stored.

use std::fmt;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::alphabet;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, URL_SAFE_NO_PAD};
use reqwest::{Method, StatusCode, header};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinSet;
use url::{Host, Url};

use crate::address::{DEVICE_RULE, is_device_name, is_person};
use crate::roster::spew::{Peers, PeersFetch};
use crate::roster::{EndpointId, MAX_DISPLAY, Me, RelayUrl, is_display_text};

/// The only scope spew mesh asks for and accepts.
pub const SCOPE: &str = "mesh";
/// `client_name` sent at dynamic client registration.
pub const CLIENT_NAME: &str = "spew mesh";
/// The redirect URI registered for the client; the authorize request adds the listener's port (RFC 8252 7.3).
pub const REGISTERED_REDIRECT_URI: &str = "http://127.0.0.1/callback";
/// The loopback listener gives up after this long (14.1).
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
/// First line of the device proof-of-possession message (14.3).
pub const PROOF_PREFIX: &str = "spew-mesh-device-v1";
/// The home relay is reported to spew at most this often (14.3).
pub const RELAY_REPORT_INTERVAL_MS: u64 = 60_000;

const DISCOVERY_PATH: &str = "/.well-known/oauth-authorization-server";
const ME_PATH: &str = "/api/mesh/v1/me";
const CHALLENGE_PATH: &str = "/api/mesh/v1/devices/challenge";
const DEVICES_PATH: &str = "/api/mesh/v1/devices";
const DEVICE_SELF_PATH: &str = "/api/mesh/v1/devices/self";
const PEERS_PATH: &str = "/api/mesh/v1/peers";

// Refresh this long before the access token's stated expiry, so a request doesn't race it.
const REFRESH_MARGIN_MS: u64 = 30_000;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_TOKEN_LEN: usize = 8192;
const MAX_CALLBACK_HEAD: usize = 8192;
// A browser may open a connection it never uses; don't let one hold a handler for long.
const CALLBACK_READ_TIMEOUT: Duration = Duration::from_secs(10);
// Longer than one refresh in another process can take (HTTP timeout 30 s), so waiting here means a stuck holder.
const AUTH_LOCK_TIMEOUT: Duration = Duration::from_secs(60);

/// base64url that accepts input with or without padding.
const URL_SAFE_LENIENT: GeneralPurpose = GeneralPurpose::new(
    &alphabet::URL_SAFE,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// A source of the current time, so token expiry can be tested.
pub trait Clock: Send + Sync {
    /// Milliseconds since the Unix epoch.
    fn now_ms(&self) -> u64;
}

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }
}

/// Opens the sign-in page. Tests pass one that plays the browser; [`SystemBrowser`] is the real one.
pub trait OpenBrowser: Send + Sync {
    /// Shows `url` to the person.
    fn open(&self, url: &str);
}

impl<F: Fn(&str) + Send + Sync> OpenBrowser for F {
    fn open(&self, url: &str) {
        self(url)
    }
}

/// Prints the sign-in URL, then runs `open` (macOS) or `xdg-open` (Linux). Run it in the CLI process: the
/// daemon's environment has no display variables (6.1).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemBrowser;

impl OpenBrowser for SystemBrowser {
    fn open(&self, url: &str) {
        eprintln!("Open this page to sign in to spew:\n  {url}");
        let program = if cfg!(target_os = "macos") {
            "open"
        } else if cfg!(target_os = "linux") {
            "xdg-open"
        } else {
            return;
        };
        // The URL is already printed, so a missing opener is not an error.
        if let Ok(mut child) = Command::new(program)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

/// Errors from the spew API client.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    /// `[spew] url` (or a URL spew advertised) breaks the https rule or doesn't parse.
    #[error("{0}")]
    BaseUrl(String),
    /// No tokens are stored for this spew.
    #[error("not signed in to spew; run `spew mesh login`")]
    NotSignedIn,
    /// spew refused the tokens and a refresh couldn't fix it, or the refresh token was refused (reused or
    /// expired).
    #[error("signed out of spew ({0}); run `spew mesh login`")]
    SignedOut(String),
    /// This spew has no mesh sign-in, or offered a token that can do more than the mesh API (1.3, 14.1).
    #[error("this spew doesn't support mesh sign-in yet ({0})")]
    MeshUnsupported(String),
    /// The sign-in grant already holds a device with another key (14.3).
    #[error(
        "this sign-in already has a device registered with another key; run `spew mesh logout`, then `spew mesh login` again"
    )]
    GrantHasDevice,
    /// The browser part of sign-in failed or timed out.
    #[error("sign-in failed: {0}")]
    Login(String),
    /// A device named in `devices revoke` doesn't match exactly one device.
    #[error("{0}")]
    Device(String),
    /// spew answered with an error.
    #[error("spew answered {status}: {code}: {message}")]
    Server {
        /// HTTP status.
        status: u16,
        /// spew's error code, or the HTTP reason.
        code: String,
        /// spew's message.
        message: String,
    },
    /// spew's answer doesn't have the expected shape.
    #[error("unexpected answer from spew: {0}")]
    InvalidResponse(String),
    /// spew couldn't be reached.
    #[error("can't reach spew: {0}")]
    Transport(String),
    /// `auth/spew.json` can't be read, parsed or written.
    #[error("{path}: {message}")]
    AuthFile {
        /// The auth file.
        path: PathBuf,
        /// What went wrong.
        message: String,
    },
}

impl ApiError {
    /// Whether this error means the person must sign in again.
    pub fn is_signed_out(&self) -> bool {
        matches!(self, Self::SignedOut(_) | Self::NotSignedIn)
    }
}

fn transport(e: reqwest::Error) -> ApiError {
    ApiError::Transport(e.to_string())
}

/// `[spew] url`, checked and without a trailing slash. It must be `https`, except `http` on `127.0.0.1` or
/// `localhost` for tests (11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseUrl(String);

impl BaseUrl {
    /// Parses and checks a base URL.
    pub fn parse(s: &str) -> Result<Self, ApiError> {
        let url = Url::parse(s.trim())
            .map_err(|e| ApiError::BaseUrl(format!("[spew] url {s:?} isn't a URL: {e}")))?;
        check_transport(&url)?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(ApiError::BaseUrl(format!(
                "[spew] url {s:?} must not carry a user name or password"
            )));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(ApiError::BaseUrl(format!(
                "[spew] url {s:?} must not have a query or fragment"
            )));
        }
        Ok(Self(url.as_str().trim_end_matches('/').to_string()))
    }

    /// The URL as used in requests and in the device proof (14.3), without a trailing slash.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn join(&self, path: &str) -> String {
        format!("{}{path}", self.0)
    }
}

impl fmt::Display for BaseUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Checks the transport rule for any URL the client sends tokens or codes to: `https`, or `http` only on
/// `127.0.0.1` or `localhost`.
pub fn check_transport(url: &Url) -> Result<(), ApiError> {
    let loopback = match url.host() {
        Some(Host::Domain(d)) => d == "localhost",
        Some(Host::Ipv4(ip)) => ip == Ipv4Addr::LOCALHOST,
        _ => false,
    };
    match url.scheme() {
        "https" if url.host().is_some() => Ok(()),
        "http" if loopback => Ok(()),
        _ => Err(ApiError::BaseUrl(format!(
            "{url} must use https (http only on 127.0.0.1 or localhost)"
        ))),
    }
}

/// The HTTP client used for spew: timeouts, no redirects (a redirect could carry a token elsewhere), and the
/// process-wide ring crypto provider.
pub fn http_client() -> Result<reqwest::Client, ApiError> {
    // reqwest is built without a provider; main installs ring first, and this covers any caller that didn't.
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("spew-mesh/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| ApiError::Transport(format!("can't build the HTTP client: {e}")))
}

/// This computer's platform as sent at device registration (`macos`, `linux`).
pub fn platform() -> &'static str {
    std::env::consts::OS
}

/// Whether a token response's `scope` is exactly `{mesh}` as a set of space-separated values (14.1).
pub fn scope_is_mesh(scope: &str) -> bool {
    let mut values = scope.split(' ').filter(|v| !v.is_empty()).peekable();
    values.peek().is_some() && values.all(|v| v == SCOPE)
}

/// A PKCE verifier and its S256 challenge.
#[derive(Clone)]
pub struct Pkce {
    /// The verifier, sent with the token request.
    pub verifier: String,
    /// `BASE64URL(SHA256(verifier))`, sent with the authorize request.
    pub challenge: String,
}

impl Pkce {
    /// A fresh verifier from 32 random bytes.
    pub fn generate() -> Result<Self, ApiError> {
        let verifier = random_b64(32)?;
        let challenge = pkce_challenge(&verifier);
        Ok(Self {
            verifier,
            challenge,
        })
    }
}

impl fmt::Debug for Pkce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pkce").finish_non_exhaustive()
    }
}

/// The S256 code challenge for a verifier.
pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn random_b64(n: usize) -> Result<String, ApiError> {
    let mut bytes = vec![0u8; n];
    getrandom::fill(&mut bytes)
        .map_err(|e| ApiError::Login(format!("no randomness available: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// The exact bytes a device signs to prove it holds its key (14.3).
pub fn proof_message(base: &BaseUrl, public_key: &EndpointId, nonce: &str) -> String {
    format!("{PROOF_PREFIX}\n{}\n{public_key}\n{nonce}", base.as_str())
}

/// What `auth/spew.json` holds: the spew it belongs to, the client id, and the tokens while signed in.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredAuth {
    url: String,
    client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display: Option<String>,
}

impl fmt::Debug for StoredAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredAuth")
            .field("url", &self.url)
            .field("username", &self.username)
            .field("signed_in", &self.access_token.is_some())
            .finish_non_exhaustive()
    }
}

impl StoredAuth {
    fn client_only(url: &str, client_id: &str) -> Self {
        Self {
            url: url.to_string(),
            client_id: client_id.to_string(),
            access_token: None,
            refresh_token: None,
            expires_at: None,
            scope: None,
            username: None,
            display: None,
        }
    }

    fn me(&self) -> Option<Me> {
        let username = self.username.clone()?;
        Some(Me {
            display: self.display.clone().unwrap_or_else(|| username.clone()),
            username,
        })
    }

    fn signed_out(&self) -> Self {
        Self {
            access_token: None,
            refresh_token: None,
            expires_at: None,
            scope: None,
            ..self.clone()
        }
    }
}

/// Reads and writes `auth/spew.json`, always mode 0600 inside a 0700 folder, replaced atomically.
#[derive(Debug, Clone)]
pub struct AuthStore {
    path: PathBuf,
}

impl AuthStore {
    /// The store at `path` (normally `<home>/auth/spew.json`).
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// The file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn err(&self, message: impl fmt::Display) -> ApiError {
        ApiError::AuthFile {
            path: self.path.clone(),
            message: message.to_string(),
        }
    }

    /// The stored auth, or `None` when there is no file.
    pub fn load(&self) -> Result<Option<StoredAuth>, ApiError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(self.err(e)),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = std::fs::metadata(&self.path)
                && meta.permissions().mode() & 0o077 != 0
            {
                std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))
                    .map_err(|e| self.err(format!("can't make the file private: {e}")))?;
            }
        }
        serde_json::from_str(&text).map(Some).map_err(|e| {
            self.err(format!(
                "can't parse: {e}; run `spew mesh login` to replace it"
            ))
        })
    }

    /// Writes the auth atomically: a 0600 temporary file in the same folder, fsync, rename.
    pub fn save(&self, auth: &StoredAuth) -> Result<(), ApiError> {
        let json = serde_json::to_vec_pretty(auth).map_err(|e| self.err(e))?;
        write_private(&self.path, &json).map_err(|e| self.err(e))
    }

    /// The advisory lock file next to the auth file (`auth/spew.lock`).
    pub fn lock_path(&self) -> PathBuf {
        self.path.with_extension("lock")
    }

    /// Blocks until this process holds the exclusive advisory lock on [`AuthStore::lock_path`]. Dropping the
    /// file releases it.
    fn lock_blocking(lock_path: &Path) -> std::io::Result<std::fs::File> {
        if let Some(dir) = lock_path.parent().filter(|d| !d.as_os_str().is_empty()) {
            create_private_dir(dir)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(lock_path)?;
        file.lock()?;
        Ok(file)
    }

    /// Deletes the file if it exists.
    pub fn remove(&self) -> Result<(), ApiError> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(self.err(e)),
        }
    }
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    create_private_dir(dir)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("auth.json");
    let tmp = dir.join(format!(".{name}.{}.tmp", random_b64(6).unwrap_or_default()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Whether this device is signed in, per the auth file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthStatus {
    /// No sign-in for this spew.
    NotSignedIn,
    /// Tokens are stored.
    SignedIn(Me),
    /// Signed in before, but spew refused the tokens; the person must sign in again.
    SignedOut(Me),
}

/// A spew project the grant covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    /// Project key.
    pub key: String,
    /// Project name.
    #[serde(default)]
    pub name: String,
}

/// The user part of `GET /api/mesh/v1/me`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeUser {
    /// spew username.
    pub username: String,
    /// Display name.
    #[serde(default)]
    pub name: Option<String>,
}

/// The device part of `GET /api/mesh/v1/me`: the device bound to this grant, if any.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeDevice {
    /// Device name.
    #[serde(default)]
    pub name: Option<String>,
    /// Endpoint id.
    #[serde(default)]
    pub public_key: Option<String>,
}

/// `GET /api/mesh/v1/me` (14.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeResponse {
    /// The signed-in person.
    pub user: MeUser,
    /// The device bound to this grant.
    #[serde(default)]
    pub device: Option<MeDevice>,
    /// Projects chosen at consent.
    #[serde(default)]
    pub projects: Vec<Project>,
}

impl MeResponse {
    /// The person as a roster [`Me`], after checking the username.
    pub fn to_me(&self) -> Result<Me, ApiError> {
        let username = &self.user.username;
        if !is_person(username) {
            return Err(ApiError::InvalidResponse(format!(
                "username {:?} doesn't fit spew mesh names",
                clean(username)
            )));
        }
        let display = self
            .user
            .name
            .clone()
            .filter(|n| is_display_text(n, MAX_DISPLAY))
            .unwrap_or_else(|| username.clone());
        Ok(Me {
            username: username.clone(),
            display,
        })
    }
}

/// The result of a completed sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedIn {
    /// Who signed in.
    pub me: Me,
    /// Projects chosen at consent.
    pub projects: Vec<Project>,
}

/// A device as returned by registration (14.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredDevice {
    /// Device name.
    pub name: String,
    /// Endpoint id.
    #[serde(with = "crate::roster::endpoint_hex")]
    pub public_key: EndpointId,
    /// When it was first registered, in ms.
    #[serde(default)]
    pub created_at: Option<u64>,
}

/// One of the caller's devices from `GET /api/mesh/v1/devices` (14.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInfo {
    /// Device name.
    pub name: String,
    /// Endpoint id.
    #[serde(with = "crate::roster::endpoint_hex")]
    pub public_key: EndpointId,
    /// Platform it registered with.
    #[serde(default)]
    pub platform: Option<String>,
    /// Its home relay as last reported, for display.
    #[serde(default)]
    pub relay: Option<String>,
    /// When it was registered, in ms.
    #[serde(default)]
    pub created_at: Option<u64>,
    /// When spew last heard from it, in ms.
    #[serde(default)]
    pub last_seen_at: Option<u64>,
    /// Whether it is the calling device.
    #[serde(default, rename = "self")]
    pub is_self: bool,
}

/// Picks the device `query` names (a device name, a full endpoint id or its 10-character short form) for
/// `devices revoke`. The calling device is refused: `logout` is how it leaves.
pub fn resolve_device<'a>(
    devices: &'a [DeviceInfo],
    query: &str,
) -> Result<&'a DeviceInfo, ApiError> {
    let q = query.trim().to_ascii_lowercase();
    let matches: Vec<&DeviceInfo> = devices
        .iter()
        .filter(|d| {
            let id = d.public_key.to_string();
            d.name.to_ascii_lowercase() == q || id == q || (q.len() == 10 && id.starts_with(&q))
        })
        .collect();
    match matches.as_slice() {
        [] => Err(ApiError::Device(format!(
            "no device of yours is named {:?}; `spew mesh devices` lists them",
            clean(query)
        ))),
        [one] if one.is_self => Err(ApiError::Device(format!(
            "{} is this device; use `spew mesh logout` to remove it",
            clean(&one.name)
        ))),
        [one] => Ok(one),
        many => Err(ApiError::Device(format!(
            "{:?} matches several devices: {}; use the endpoint id",
            clean(query),
            many.iter()
                .map(|d| format!("{} ({})", clean(&d.name), d.public_key.fmt_short()))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// What `logout` managed to do. Local tokens are always removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogoutReport {
    /// Whether there was a sign-in to undo.
    pub was_signed_in: bool,
    /// Whether spew unregistered this device.
    pub device_unregistered: bool,
    /// Whether spew revoked the refresh token.
    pub token_revoked: bool,
}

/// Reports the home relay to spew when it changes, at most once a minute (14.3).
#[derive(Debug, Clone, Default)]
pub struct RelayReporter {
    sent: Option<Option<String>>,
    last_sent_at: Option<u64>,
}

impl RelayReporter {
    /// Whether `relay` should be sent now.
    pub fn due(&self, relay: Option<&RelayUrl>, now_ms: u64) -> bool {
        let value = relay.map(|r| r.to_string());
        if self.sent.as_ref() == Some(&value) {
            return false;
        }
        self.last_sent_at
            .is_none_or(|t| now_ms.saturating_sub(t) >= RELAY_REPORT_INTERVAL_MS)
    }

    /// Records that spew accepted `relay`.
    pub fn sent(&mut self, relay: Option<&RelayUrl>, now_ms: u64) {
        self.sent = Some(relay.map(|r| r.to_string()));
        self.last_sent_at = Some(now_ms);
    }
}

#[derive(Debug, Clone, Deserialize)]
struct RawMetadata {
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    registration_endpoint: Option<String>,
    #[serde(default)]
    revocation_endpoint: Option<String>,
    #[serde(default)]
    scopes_supported: Option<Vec<String>>,
    #[serde(default)]
    code_challenge_methods_supported: Option<Vec<String>>,
}

/// The parts of the OAuth server metadata (RFC 8414) the client uses.
#[derive(Debug, Clone)]
struct Metadata {
    authorization_endpoint: Url,
    token_endpoint: Url,
    registration_endpoint: Option<Url>,
    revocation_endpoint: Option<Url>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Deserialize)]
struct RegistrationResponse {
    client_id: String,
}

#[derive(Deserialize)]
struct ChallengeResponse {
    nonce: String,
}

// 14.3 says `{"devices": [...]}`; a bare array is the earlier shape, still accepted.
#[derive(Deserialize)]
#[serde(untagged)]
enum DeviceList {
    Wrapped { devices: Vec<DeviceInfo> },
    Bare(Vec<DeviceInfo>),
}

#[derive(Deserialize)]
struct ErrorBody {
    error: ErrorField,
    #[serde(default)]
    error_description: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ErrorField {
    // spew's own shape (14): {"error": {"code", "message"}}
    Detail {
        code: String,
        #[serde(default)]
        message: String,
    },
    // OAuth's shape (RFC 6749 5.2) and the short form `{"error": "grant-has-device"}` (14.3)
    Code(String),
}

/// Keeps server-provided text to one short printable line before it reaches a terminal or log.
fn clean(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(200)
        .collect();
    if s.chars().count() > 200 {
        out.push('…');
    }
    out
}

/// Client for one spew. Cheap to clone. Token refreshes never overlap, within this process or across processes
/// sharing the auth file.
#[derive(Clone)]
pub struct SpewApi {
    inner: Arc<Inner>,
}

struct Inner {
    base: BaseUrl,
    http: reqwest::Client,
    store: AuthStore,
    clock: Arc<dyn Clock>,
    metadata: Mutex<Option<Metadata>>,
    // With the file lock (`auth/spew.lock`), held across every read-refresh-write of the tokens: refresh tokens
    // rotate, so two overlapping refreshes (two tasks, or the CLI and the daemon) would make spew see a reuse
    // and end the grant.
    auth_lock: Mutex<()>,
}

/// Both auth locks: the in-process one, then the advisory file lock other processes honor.
struct AuthLock<'a> {
    _file: std::fs::File,
    _local: tokio::sync::MutexGuard<'a, ()>,
}

impl fmt::Debug for SpewApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpewApi")
            .field("base", &self.inner.base)
            .field("auth", &self.inner.store.path)
            .finish_non_exhaustive()
    }
}

enum Body<'a> {
    None,
    Json(&'a serde_json::Value),
}

impl SpewApi {
    /// A client for `base` that keeps its tokens at `auth_path`.
    pub fn new(base: BaseUrl, auth_path: PathBuf, clock: Arc<dyn Clock>) -> Result<Self, ApiError> {
        Ok(Self::with_http(base, auth_path, clock, http_client()?))
    }

    /// Like [`SpewApi::new`] with a given HTTP client (tests use one that ignores proxy variables).
    pub fn with_http(
        base: BaseUrl,
        auth_path: PathBuf,
        clock: Arc<dyn Clock>,
        http: reqwest::Client,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                base,
                http,
                store: AuthStore::new(auth_path),
                clock,
                metadata: Mutex::new(None),
                auth_lock: Mutex::new(()),
            }),
        }
    }

    /// The spew this client talks to.
    pub fn base(&self) -> &BaseUrl {
        &self.inner.base
    }

    /// The current time from this client's clock, in ms.
    pub fn now_ms(&self) -> u64 {
        self.inner.clock.now_ms()
    }

    /// The stored auth if it belongs to this spew.
    fn load_matching(&self) -> Result<Option<StoredAuth>, ApiError> {
        Ok(self
            .inner
            .store
            .load()?
            .filter(|a| a.url == self.inner.base.as_str()))
    }

    /// Takes both auth locks; hold the result across a read, refresh and write of the tokens.
    async fn lock_auth(&self) -> Result<AuthLock<'_>, ApiError> {
        let local = self.inner.auth_lock.lock().await;
        let path = self.inner.store.lock_path();
        let store = &self.inner.store;
        let file = tokio::time::timeout(
            AUTH_LOCK_TIMEOUT,
            tokio::task::spawn_blocking(move || AuthStore::lock_blocking(&path)),
        )
        .await
        .map_err(|_| store.err("another spew mesh process has held the auth lock for a minute"))?
        .map_err(|e| store.err(format!("can't take the auth lock: {e}")))?
        .map_err(|e| store.err(format!("can't take the auth lock: {e}")))?;
        Ok(AuthLock {
            _file: file,
            _local: local,
        })
    }

    /// Whether this device is signed in to this spew, from the auth file alone.
    pub fn auth_status(&self) -> Result<AuthStatus, ApiError> {
        let Some(auth) = self.load_matching()? else {
            return Ok(AuthStatus::NotSignedIn);
        };
        Ok(match (auth.me(), auth.access_token.is_some()) {
            (Some(me), true) => AuthStatus::SignedIn(me),
            (Some(me), false) => AuthStatus::SignedOut(me),
            (None, _) => AuthStatus::NotSignedIn,
        })
    }

    async fn metadata(&self) -> Result<Metadata, ApiError> {
        let mut cached = self.inner.metadata.lock().await;
        if let Some(m) = cached.as_ref() {
            return Ok(m.clone());
        }
        let resp = self
            .inner
            .http
            .get(self.inner.base.join(DISCOVERY_PATH))
            .header(header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(transport)?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Err(ApiError::MeshUnsupported("no OAuth server metadata".into()));
        }
        if !resp.status().is_success() {
            return Err(error_from(resp).await);
        }
        let raw: RawMetadata = read_json(resp).await?;
        if let Some(scopes) = &raw.scopes_supported
            && !scopes.iter().any(|s| s == SCOPE)
        {
            return Err(ApiError::MeshUnsupported(
                "the mesh scope isn't offered".into(),
            ));
        }
        if let Some(methods) = &raw.code_challenge_methods_supported
            && !methods.iter().any(|m| m == "S256")
        {
            return Err(ApiError::MeshUnsupported("PKCE S256 isn't offered".into()));
        }
        let endpoint = |s: &str| -> Result<Url, ApiError> {
            let url = Url::parse(s).map_err(|e| {
                ApiError::InvalidResponse(format!("endpoint {:?} isn't a URL: {e}", clean(s)))
            })?;
            check_transport(&url)?;
            Ok(url)
        };
        let meta = Metadata {
            authorization_endpoint: endpoint(&raw.authorization_endpoint)?,
            token_endpoint: endpoint(&raw.token_endpoint)?,
            registration_endpoint: raw
                .registration_endpoint
                .as_deref()
                .map(endpoint)
                .transpose()?,
            revocation_endpoint: raw
                .revocation_endpoint
                .as_deref()
                .map(endpoint)
                .transpose()?,
        };
        *cached = Some(meta.clone());
        Ok(meta)
    }

    async fn register_client(&self, meta: &Metadata) -> Result<String, ApiError> {
        let endpoint = meta
            .registration_endpoint
            .clone()
            .ok_or_else(|| ApiError::MeshUnsupported("no dynamic client registration".into()))?;
        let body = serde_json::json!({
            "client_name": CLIENT_NAME,
            "redirect_uris": [REGISTERED_REDIRECT_URI],
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
        });
        let resp = self
            .inner
            .http
            .post(endpoint)
            .header(header::ACCEPT, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(transport)?;
        if !resp.status().is_success() {
            return Err(error_from(resp).await);
        }
        let reg: RegistrationResponse = read_json(resp).await?;
        if reg.client_id.is_empty()
            || reg.client_id.len() > 512
            || reg.client_id.chars().any(char::is_control)
        {
            return Err(ApiError::InvalidResponse("bad client_id".into()));
        }
        Ok(reg.client_id)
    }

    /// Starts sign-in: discovery, client registration (reusing a cached client id), the loopback listener on
    /// `127.0.0.1` with a random port, PKCE and state. Show [`LoginFlow::authorize_url`] to the person, then
    /// await [`LoginFlow::complete`].
    pub async fn start_login(&self) -> Result<LoginFlow, ApiError> {
        let meta = self.metadata().await?;
        let client_id = {
            let _lock = self.lock_auth().await?;
            // A broken auth file is replaced by signing in, so it doesn't block this.
            match self.load_matching().ok().flatten() {
                Some(auth) => auth.client_id,
                None => {
                    let id = self.register_client(&meta).await?;
                    // Cached at once so an abandoned sign-in doesn't register another client next time. Tokens
                    // are written only when sign-in completes.
                    self.inner
                        .store
                        .save(&StoredAuth::client_only(self.inner.base.as_str(), &id))?;
                    id
                }
            }
        };
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|e| ApiError::Login(format!("can't listen on 127.0.0.1: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| ApiError::Login(format!("can't read the listener's port: {e}")))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}/callback");
        let pkce = Pkce::generate()?;
        let state = random_b64(24)?;
        let mut url = meta.authorization_endpoint.clone();
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("scope", SCOPE);
        Ok(LoginFlow {
            api: self.clone(),
            meta,
            client_id,
            listener,
            redirect_uri,
            state,
            verifier: pkce.verifier,
            url: url.to_string(),
        })
    }

    /// Signs in: starts the flow, opens the browser and waits up to `timeout` for the callback.
    pub async fn login(
        &self,
        browser: &dyn OpenBrowser,
        timeout: Duration,
    ) -> Result<SignedIn, ApiError> {
        let flow = self.start_login().await?;
        browser.open(flow.authorize_url());
        flow.complete(timeout).await
    }

    async fn post_form(
        &self,
        url: &Url,
        pairs: &[(&str, &str)],
    ) -> Result<reqwest::Response, ApiError> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish();
        self.inner
            .http
            .post(url.clone())
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::ACCEPT, "application/json")
            .body(body)
            .send()
            .await
            .map_err(transport)
    }

    /// Revokes a token at the revocation endpoint (RFC 7009).
    async fn revoke_token(
        &self,
        meta: &Metadata,
        client_id: &str,
        token: &str,
        hint: &str,
    ) -> Result<(), ApiError> {
        let endpoint = meta
            .revocation_endpoint
            .as_ref()
            .ok_or_else(|| ApiError::InvalidResponse("spew has no revocation endpoint".into()))?;
        let resp = self
            .post_form(
                endpoint,
                &[
                    ("token", token),
                    ("token_type_hint", hint),
                    ("client_id", client_id),
                ],
            )
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(error_from(resp).await)
        }
    }

    async fn revoke_pair(&self, meta: &Metadata, client_id: &str, tokens: &TokenResponse) {
        // Best effort: the caller is already failing, and a token we don't store can't be used by us anyway.
        if let Some(refresh) = &tokens.refresh_token {
            let _ = self
                .revoke_token(meta, client_id, refresh, "refresh_token")
                .await;
        }
        let _ = self
            .revoke_token(meta, client_id, &tokens.access_token, "access_token")
            .await;
    }

    /// Checks a token response: a bearer token of sane size whose scope is exactly `mesh`. A refresh response
    /// may leave `scope` out, which means unchanged (RFC 6749 5.1); the first grant must state it.
    fn check_tokens(tokens: &TokenResponse, first_grant: bool) -> Result<(), ApiError> {
        if tokens.access_token.is_empty() || tokens.access_token.len() > MAX_TOKEN_LEN {
            return Err(ApiError::InvalidResponse("bad access_token".into()));
        }
        if tokens
            .refresh_token
            .as_ref()
            .is_some_and(|t| t.is_empty() || t.len() > MAX_TOKEN_LEN)
        {
            return Err(ApiError::InvalidResponse("bad refresh_token".into()));
        }
        if !tokens
            .token_type
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case("bearer"))
        {
            return Err(ApiError::InvalidResponse(
                "the token isn't a bearer token".into(),
            ));
        }
        match tokens.scope.as_deref() {
            Some(scope) if scope_is_mesh(scope) => Ok(()),
            None if !first_grant => Ok(()),
            Some(scope) => Err(ApiError::MeshUnsupported(format!(
                "it granted scope {:?} instead of exactly \"mesh\"",
                clean(scope)
            ))),
            None => Err(ApiError::MeshUnsupported(
                "it granted no scope instead of exactly \"mesh\"".into(),
            )),
        }
    }

    fn expiry(&self, tokens: &TokenResponse) -> Option<u64> {
        tokens
            .expires_in
            .map(|s| self.now_ms().saturating_add(s.saturating_mul(1000)))
    }

    /// Refreshes with the stored refresh token, under the auth lock. Saves the rotated pair and returns the new
    /// access token. A refused refresh clears the stored tokens and means signed out.
    async fn refresh_locked(&self, auth: StoredAuth) -> Result<String, ApiError> {
        let Some(refresh) = auth.refresh_token.clone() else {
            self.inner.store.save(&auth.signed_out())?;
            return Err(ApiError::SignedOut(
                "the token expired and there is no refresh token".into(),
            ));
        };
        let meta = self.metadata().await?;
        let resp = self
            .post_form(
                &meta.token_endpoint,
                &[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", &refresh),
                    ("client_id", &auth.client_id),
                ],
            )
            .await?;
        let status = resp.status();
        if status == StatusCode::BAD_REQUEST || status == StatusCode::UNAUTHORIZED {
            let err = error_from(resp).await;
            self.inner.store.save(&auth.signed_out())?;
            let code = match err {
                ApiError::Server { code, .. } => code,
                other => other.to_string(),
            };
            return Err(ApiError::SignedOut(format!(
                "spew refused the refresh: {code}"
            )));
        }
        if !status.is_success() {
            return Err(error_from(resp).await);
        }
        let tokens: TokenResponse = read_json(resp).await?;
        if let Err(e) = Self::check_tokens(&tokens, false) {
            self.revoke_pair(&meta, &auth.client_id, &tokens).await;
            self.inner.store.save(&auth.signed_out())?;
            return Err(e);
        }
        let updated = StoredAuth {
            access_token: Some(tokens.access_token.clone()),
            // Rotation sends a new refresh token; a server that doesn't rotate keeps the old one valid.
            refresh_token: tokens.refresh_token.clone().or(Some(refresh)),
            expires_at: self.expiry(&tokens),
            scope: tokens.scope.clone().or(auth.scope.clone()),
            ..auth
        };
        self.inner.store.save(&updated)?;
        Ok(tokens.access_token)
    }

    /// The access token to use now, refreshed first when it is about to expire.
    async fn current_token(&self) -> Result<String, ApiError> {
        let _lock = self.lock_auth().await?;
        let auth = self.load_matching()?.ok_or(ApiError::NotSignedIn)?;
        let Some(access) = auth.access_token.clone() else {
            return Err(if auth.username.is_some() {
                ApiError::SignedOut("spew refused the stored sign-in".into())
            } else {
                ApiError::NotSignedIn
            });
        };
        if auth
            .expires_at
            .is_some_and(|t| self.now_ms().saturating_add(REFRESH_MARGIN_MS) >= t)
        {
            return self.refresh_locked(auth).await;
        }
        Ok(access)
    }

    /// The access token to retry with after `used` got a 401: a newer one if another request already
    /// refreshed, else a fresh one.
    async fn token_after_401(&self, used: &str) -> Result<String, ApiError> {
        let _lock = self.lock_auth().await?;
        let auth = self.load_matching()?.ok_or(ApiError::NotSignedIn)?;
        match auth.access_token.as_deref() {
            Some(current) if current != used => Ok(current.to_string()),
            _ => self.refresh_locked(auth).await,
        }
    }

    async fn send_once(
        &self,
        method: &Method,
        url: &str,
        body: &Body<'_>,
        if_none_match: Option<&str>,
        token: &str,
    ) -> Result<reqwest::Response, ApiError> {
        let mut req = self
            .inner
            .http
            .request(method.clone(), url)
            .bearer_auth(token)
            .header(header::ACCEPT, "application/json");
        if let Body::Json(v) = body {
            req = req.json(v);
        }
        if let Some(tag) = if_none_match {
            req = req.header(header::IF_NONE_MATCH, tag);
        }
        req.send().await.map_err(transport)
    }

    /// Sends an authorized request. A 401 gets one refresh and one retry; a second 401 means signed out (4.2).
    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Body<'_>,
        if_none_match: Option<&str>,
    ) -> Result<reqwest::Response, ApiError> {
        let url = self.inner.base.join(path);
        let token = self.current_token().await?;
        let resp = self
            .send_once(&method, &url, &body, if_none_match, &token)
            .await?;
        if resp.status() != StatusCode::UNAUTHORIZED {
            return Ok(resp);
        }
        let token = self.token_after_401(&token).await?;
        let resp = self
            .send_once(&method, &url, &body, if_none_match, &token)
            .await?;
        if resp.status() == StatusCode::UNAUTHORIZED {
            return Err(ApiError::SignedOut(
                "spew refused the token even after a refresh".into(),
            ));
        }
        Ok(resp)
    }

    async fn me_with(&self, token: &str) -> Result<MeResponse, ApiError> {
        let resp = self
            .send_once(
                &Method::GET,
                &self.inner.base.join(ME_PATH),
                &Body::None,
                None,
                token,
            )
            .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Err(ApiError::MeshUnsupported("no /api/mesh/v1/me".into()));
        }
        if !resp.status().is_success() {
            return Err(error_from(resp).await);
        }
        read_json(resp).await
    }

    /// `GET /api/mesh/v1/me` (14.2).
    pub async fn me(&self) -> Result<MeResponse, ApiError> {
        let resp = self.send(Method::GET, ME_PATH, Body::None, None).await?;
        if !resp.status().is_success() {
            return Err(error_from(resp).await);
        }
        read_json(resp).await
    }

    /// Registers this device's key with proof of possession (14.3): asks for a nonce, signs
    /// `spew-mesh-device-v1\n<url>\n<public_key>\n<nonce>` with the device key and sends it with the name and
    /// platform.
    pub async fn register_device(
        &self,
        key: &iroh::SecretKey,
        name: &str,
        platform: &str,
    ) -> Result<RegisteredDevice, ApiError> {
        if !is_device_name(name) {
            return Err(ApiError::Device(format!(
                "device name {:?} must be {DEVICE_RULE}",
                clean(name)
            )));
        }
        if platform.is_empty()
            || platform.len() > 16
            || !platform
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        {
            return Err(ApiError::Device(format!(
                "bad platform {:?}",
                clean(platform)
            )));
        }
        let empty = serde_json::json!({});
        let resp = self
            .send(Method::POST, CHALLENGE_PATH, Body::Json(&empty), None)
            .await?;
        if !resp.status().is_success() {
            return Err(error_from(resp).await);
        }
        let challenge: ChallengeResponse = read_json(resp).await?;
        // The nonce goes into a signed message, so it must be exactly what 14.3 says and nothing else.
        let nonce_ok = challenge.nonce.len() <= 64
            && URL_SAFE_LENIENT
                .decode(challenge.nonce.as_bytes())
                .is_ok_and(|b| b.len() == 32);
        if !nonce_ok {
            return Err(ApiError::InvalidResponse(
                "the nonce isn't base64url of 32 bytes".into(),
            ));
        }
        let public_key = key.public();
        let message = proof_message(&self.inner.base, &public_key, &challenge.nonce);
        let signature = URL_SAFE_NO_PAD.encode(key.sign(message.as_bytes()).to_bytes());
        let body = serde_json::json!({
            "public_key": public_key.to_string(),
            "name": name,
            "platform": platform,
            "nonce": challenge.nonce,
            "signature": signature,
        });
        let resp = self
            .send(Method::POST, DEVICES_PATH, Body::Json(&body), None)
            .await?;
        if !resp.status().is_success() {
            return Err(match error_from(resp).await {
                ApiError::Server {
                    status: 409, code, ..
                } if code == "grant-has-device" => ApiError::GrantHasDevice,
                other => other,
            });
        }
        let device: RegisteredDevice = read_json(resp).await?;
        if device.public_key != public_key {
            return Err(ApiError::InvalidResponse(
                "spew registered a different key".into(),
            ));
        }
        Ok(device)
    }

    /// `PATCH /api/mesh/v1/devices/self`: the home relay (or none) and optionally a new name (14.3).
    pub async fn update_self(
        &self,
        relay: Option<&RelayUrl>,
        name: Option<&str>,
    ) -> Result<(), ApiError> {
        let mut body = serde_json::json!({ "relay": relay.map(|r| r.to_string()) });
        if let Some(name) = name {
            if !is_device_name(name) {
                return Err(ApiError::Device(format!(
                    "bad device name {:?}",
                    clean(name)
                )));
            }
            body["name"] = serde_json::Value::String(name.to_string());
        }
        let resp = self
            .send(Method::PATCH, DEVICE_SELF_PATH, Body::Json(&body), None)
            .await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(error_from(resp).await)
        }
    }

    /// `DELETE /api/mesh/v1/devices/self`. A device spew no longer knows counts as unregistered.
    pub async fn unregister_self(&self) -> Result<(), ApiError> {
        let resp = self
            .send(Method::DELETE, DEVICE_SELF_PATH, Body::None, None)
            .await?;
        if resp.status().is_success() || resp.status() == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(error_from(resp).await)
        }
    }

    /// `GET /api/mesh/v1/devices`: the caller's own devices (14.3).
    pub async fn list_devices(&self) -> Result<Vec<DeviceInfo>, ApiError> {
        let resp = self
            .send(Method::GET, DEVICES_PATH, Body::None, None)
            .await?;
        if !resp.status().is_success() {
            return Err(error_from(resp).await);
        }
        Ok(match read_json::<DeviceList>(resp).await? {
            DeviceList::Bare(d) | DeviceList::Wrapped { devices: d } => d,
        })
    }

    /// `DELETE /api/mesh/v1/devices/<public_key>`: revokes one of the caller's devices (14.3).
    pub async fn revoke_device(&self, public_key: &EndpointId) -> Result<(), ApiError> {
        let path = format!("{DEVICES_PATH}/{public_key}");
        let resp = self.send(Method::DELETE, &path, Body::None, None).await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(error_from(resp).await)
        }
    }

    /// `devices revoke <name>`: finds the device by name or endpoint id and revokes it.
    pub async fn revoke_device_named(&self, query: &str) -> Result<DeviceInfo, ApiError> {
        let devices = self.list_devices().await?;
        let device = resolve_device(&devices, query)?.clone();
        self.revoke_device(&device.public_key).await?;
        Ok(device)
    }

    /// `GET /api/mesh/v1/peers` with `If-None-Match` when an `ETag` is given (14.4).
    pub async fn fetch_peers(&self, if_none_match: Option<&str>) -> Result<PeersFetch, ApiError> {
        let resp = self
            .send(Method::GET, PEERS_PATH, Body::None, if_none_match)
            .await?;
        match resp.status() {
            StatusCode::NOT_MODIFIED if if_none_match.is_some() => Ok(PeersFetch::NotModified),
            StatusCode::NOT_MODIFIED => Err(ApiError::InvalidResponse(
                "304 without If-None-Match".into(),
            )),
            s if s.is_success() => {
                let etag = resp
                    .headers()
                    .get(header::ETAG)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let peers: Peers = read_json(resp).await?;
                Ok(PeersFetch::Modified { etag, peers })
            }
            _ => Err(error_from(resp).await),
        }
    }

    /// Signs out: unregisters this device, revokes the refresh token, and removes the tokens from the auth
    /// file (keeping the client id) whatever spew answered.
    pub async fn logout(&self) -> Result<LogoutReport, ApiError> {
        let mut report = LogoutReport {
            was_signed_in: false,
            device_unregistered: false,
            token_revoked: false,
        };
        let auth = match self.load_matching() {
            Ok(Some(a)) => a,
            Ok(None) => return Ok(report),
            // Nothing usable in a broken file; removing it is the sign-out.
            Err(_) => {
                self.inner.store.remove()?;
                return Ok(report);
            }
        };
        report.was_signed_in = auth.access_token.is_some();
        if report.was_signed_in {
            report.device_unregistered = self.unregister_self().await.is_ok();
        }
        let _lock = self.lock_auth().await?;
        // unregister_self may have refreshed, so read the latest refresh token.
        let auth = self.load_matching().ok().flatten().unwrap_or(auth);
        if let Some(refresh) = &auth.refresh_token
            && let Ok(meta) = self.metadata().await
        {
            report.token_revoked = self
                .revoke_token(&meta, &auth.client_id, refresh, "refresh_token")
                .await
                .is_ok();
        }
        self.inner.store.save(&StoredAuth {
            username: None,
            display: None,
            ..auth.signed_out()
        })?;
        Ok(report)
    }
}

/// A sign-in in progress: the listener is bound and the authorize URL is ready.
pub struct LoginFlow {
    api: SpewApi,
    meta: Metadata,
    client_id: String,
    listener: TcpListener,
    redirect_uri: String,
    state: String,
    verifier: String,
    url: String,
}

impl fmt::Debug for LoginFlow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoginFlow")
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

impl LoginFlow {
    /// The page the person opens to sign in.
    pub fn authorize_url(&self) -> &str {
        &self.url
    }

    /// The loopback redirect URI, `http://127.0.0.1:<port>/callback`.
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Waits up to `timeout` for the callback, exchanges the code, checks the scope (revoking anything but
    /// exactly `mesh`), reads who signed in and stores the tokens.
    pub async fn complete(self, timeout: Duration) -> Result<SignedIn, ApiError> {
        let code = wait_for_callback(self.listener, &self.state, timeout).await?;
        let api = &self.api;
        let resp = api
            .post_form(
                &self.meta.token_endpoint,
                &[
                    ("grant_type", "authorization_code"),
                    ("code", &code),
                    ("redirect_uri", &self.redirect_uri),
                    ("code_verifier", &self.verifier),
                    ("client_id", &self.client_id),
                ],
            )
            .await?;
        if !resp.status().is_success() {
            return Err(match error_from(resp).await {
                ApiError::Server { code, message, .. } => {
                    ApiError::Login(format!("spew refused the code: {code} {message}"))
                }
                other => other,
            });
        }
        let tokens: TokenResponse = read_json(resp).await?;
        if let Err(e) = SpewApi::check_tokens(&tokens, true) {
            api.revoke_pair(&self.meta, &self.client_id, &tokens).await;
            return Err(e);
        }
        let me_resp = match api.me_with(&tokens.access_token).await {
            Ok(m) => m,
            Err(e) => {
                api.revoke_pair(&self.meta, &self.client_id, &tokens).await;
                return Err(e);
            }
        };
        let me = match me_resp.to_me() {
            Ok(me) => me,
            Err(e) => {
                api.revoke_pair(&self.meta, &self.client_id, &tokens).await;
                return Err(e);
            }
        };
        let auth = StoredAuth {
            url: api.inner.base.as_str().to_string(),
            client_id: self.client_id.clone(),
            access_token: Some(tokens.access_token.clone()),
            refresh_token: tokens.refresh_token.clone(),
            expires_at: api.expiry(&tokens),
            scope: tokens.scope.clone(),
            username: Some(me.username.clone()),
            display: Some(me.display.clone()),
        };
        {
            let _lock = api.lock_auth().await?;
            api.inner.store.save(&auth)?;
        }
        Ok(SignedIn {
            me,
            projects: me_resp.projects,
        })
    }
}

/// What one request to the loopback listener said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Callback {
    /// The authorization code.
    Code(String),
    /// spew sent an OAuth error (the person declined).
    Denied(String),
}

/// How the listener answers one request, and whether it ends the wait.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackReply {
    /// HTTP status to send.
    pub status: u16,
    /// Page text to send.
    pub page: &'static str,
    /// `Some` for the one valid callback.
    pub outcome: Option<Callback>,
}

const PAGE_DONE: &str = "Signed in to spew mesh. You can close this tab.";
const PAGE_DENIED: &str = "spew mesh sign-in was not approved. You can close this tab.";
const PAGE_BAD_STATE: &str = "This sign-in link doesn't match the one spew mesh started. Go back to the terminal and try again.";
const PAGE_BAD_REQUEST: &str = "This request isn't a spew mesh sign-in callback.";
const PAGE_NOT_FOUND: &str = "Not found.";

fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// Decides the answer to one request head received by the loopback listener (14.1). Only a `GET
/// /callback` whose single `state` matches ends the wait; a wrong or missing `state` gets 400.
pub fn classify_callback(head: &str, expected_state: &str) -> CallbackReply {
    let reply = |status, page| CallbackReply {
        status,
        page,
        outcome: None,
    };
    let mut parts = head.lines().next().unwrap_or("").split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return reply(400, PAGE_BAD_REQUEST);
    };
    if method != "GET" {
        return reply(405, PAGE_BAD_REQUEST);
    }
    if !target.starts_with('/') {
        return reply(400, PAGE_BAD_REQUEST);
    }
    let Ok(url) = Url::parse(&format!("http://127.0.0.1{target}")) else {
        return reply(400, PAGE_BAD_REQUEST);
    };
    if url.path() != "/callback" {
        return reply(404, PAGE_NOT_FOUND);
    }
    let pairs: Vec<(String, String)> = url.query_pairs().into_owned().collect();
    let single = |key: &str| -> Result<Option<&str>, ()> {
        let mut values = pairs
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str());
        let first = values.next();
        if values.next().is_some() {
            Err(())
        } else {
            Ok(first)
        }
    };
    let (Ok(state), Ok(code), Ok(error)) = (single("state"), single("code"), single("error"))
    else {
        return reply(400, PAGE_BAD_REQUEST);
    };
    if !state.is_some_and(|s| same_secret(s, expected_state)) {
        return reply(400, PAGE_BAD_STATE);
    }
    if let Some(error) = error {
        return CallbackReply {
            status: 200,
            page: PAGE_DENIED,
            outcome: Some(Callback::Denied(clean(error))),
        };
    }
    match code {
        Some(code) if !code.is_empty() && code.len() <= MAX_TOKEN_LEN => CallbackReply {
            status: 200,
            page: PAGE_DONE,
            outcome: Some(Callback::Code(code.to_string())),
        },
        _ => reply(400, PAGE_BAD_REQUEST),
    }
}

async fn read_head(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        if buf.len() >= MAX_CALLBACK_HEAD {
            break;
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

async fn handle_callback(mut stream: TcpStream, expected_state: &str) -> Option<Callback> {
    let head = tokio::time::timeout(CALLBACK_READ_TIMEOUT, read_head(&mut stream))
        .await
        .ok()?
        .ok()?;
    let reply = classify_callback(&head, expected_state);
    let reason = match reply.status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Bad Request",
    };
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>spew mesh</title><p>{}</p>\n",
        reply.page
    );
    let response = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        reply.status,
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
    reply.outcome
}

/// Accepts connections on the loopback listener until one valid callback arrives or `timeout` passes.
/// Connections are handled concurrently so an idle browser connection can't hold up the real one.
async fn wait_for_callback(
    listener: TcpListener,
    expected_state: &str,
    timeout: Duration,
) -> Result<String, ApiError> {
    let (tx, mut rx) = mpsc::channel::<Callback>(4);
    let mut handlers = JoinSet::new();
    let state: Arc<str> = Arc::from(expected_state);
    let wait = async {
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue };
                    let tx = tx.clone();
                    let state = state.clone();
                    handlers.spawn(async move {
                        if let Some(outcome) = handle_callback(stream, &state).await {
                            let _ = tx.send(outcome).await;
                        }
                    });
                }
                Some(outcome) = rx.recv() => return outcome,
            }
        }
    };
    let outcome = tokio::time::timeout(timeout, wait).await.map_err(|_| {
        ApiError::Login(format!("no sign-in arrived within {} s", timeout.as_secs()))
    })?;
    handlers.abort_all();
    match outcome {
        Callback::Code(code) => Ok(code),
        Callback::Denied(error) => Err(ApiError::Login(format!("spew said {error}"))),
    }
}

async fn read_body(mut resp: reqwest::Response) -> Result<Vec<u8>, ApiError> {
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(transport)? {
        if out.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(ApiError::InvalidResponse(format!(
                "response larger than {MAX_RESPONSE_BYTES} bytes"
            )));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

async fn read_json<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T, ApiError> {
    let bytes = read_body(resp).await?;
    serde_json::from_slice(&bytes).map_err(|e| ApiError::InvalidResponse(e.to_string()))
}

/// The code and message of an error body: spew's `{"error": {"code", "message"}}`, or OAuth's
/// `{"error": "<code>", "error_description"}` (also the earlier short form of 14.3's 409).
fn parse_error_body(bytes: &[u8]) -> Option<(String, String)> {
    match serde_json::from_slice::<ErrorBody>(bytes).ok()? {
        ErrorBody {
            error: ErrorField::Detail { code, message },
            ..
        } => Some((code, message)),
        ErrorBody {
            error: ErrorField::Code(code),
            error_description,
        } => Some((code, error_description.unwrap_or_default())),
    }
}

/// Turns an error response into [`ApiError::Server`], reading spew's or OAuth's error body when there is one.
async fn error_from(resp: reqwest::Response) -> ApiError {
    let status = resp.status();
    let fallback = status.canonical_reason().unwrap_or("error").to_string();
    let (code, message) = read_body(resp)
        .await
        .ok()
        .and_then(|bytes| parse_error_body(&bytes))
        .unwrap_or((fallback, String::new()));
    ApiError::Server {
        status: status.as_u16(),
        code: clean(&code),
        message: clean(&message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_rule() {
        assert_eq!(
            BaseUrl::parse("https://mcp.spew.lol").unwrap().as_str(),
            "https://mcp.spew.lol"
        );
        assert_eq!(
            BaseUrl::parse("https://mcp.spew.lol/").unwrap().as_str(),
            "https://mcp.spew.lol"
        );
        assert_eq!(
            BaseUrl::parse("https://example.com:443/spew/")
                .unwrap()
                .as_str(),
            "https://example.com/spew"
        );
        assert!(BaseUrl::parse("http://127.0.0.1:8080").is_ok());
        assert!(BaseUrl::parse("http://localhost:8080").is_ok());
        for bad in [
            "http://example.com",
            "http://127.0.0.2",
            "http://[::1]:80",
            "http://localhost.example.com",
            "ftp://example.com",
            "https://user:pw@example.com",
            "https://example.com/?a=b",
            "https://example.com/#x",
            "not a url",
        ] {
            assert!(BaseUrl::parse(bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn scope_must_be_exactly_mesh() {
        assert!(scope_is_mesh("mesh"));
        assert!(scope_is_mesh(" mesh "));
        assert!(scope_is_mesh("mesh mesh"));
        assert!(!scope_is_mesh(""));
        assert!(!scope_is_mesh("mesh issues"));
        assert!(!scope_is_mesh("issues"));
        assert!(!scope_is_mesh("mesh\tissues"));
        assert!(!scope_is_mesh("MESH"));
    }

    #[test]
    fn pkce_s256() {
        // Expected value computed separately: base64url(sha256(verifier)) without padding.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r7wW1gFWFOEjXk"),
            "bwWFMyPfdG9qreDhH2lmftFx_dFeLDalzcT1gb_j68g"
        );
        let p = Pkce::generate().unwrap();
        assert_eq!(p.verifier.len(), 43);
        assert_eq!(p.challenge, pkce_challenge(&p.verifier));
        assert_ne!(Pkce::generate().unwrap().verifier, p.verifier);
    }

    #[test]
    fn callback_classification() {
        let head = |target: &str| format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n");
        let ok = classify_callback(&head("/callback?code=abc&state=s1"), "s1");
        assert_eq!(ok.status, 200);
        assert_eq!(ok.outcome, Some(Callback::Code("abc".into())));
        for (target, status) in [
            ("/callback?code=abc&state=s2", 400),
            ("/callback?code=abc", 400),
            ("/callback?code=abc&state=s1&state=s1", 400),
            ("/callback?state=s1", 400),
            ("/callback?state=s1&code=", 400),
            ("/favicon.ico", 404),
            ("/other?code=abc&state=s1", 404),
        ] {
            let r = classify_callback(&head(target), "s1");
            assert_eq!(r.status, status, "{target}");
            assert_eq!(r.outcome, None, "{target}");
        }
        let denied = classify_callback(&head("/callback?error=access_denied&state=s1"), "s1");
        assert_eq!(
            denied.outcome,
            Some(Callback::Denied("access_denied".into()))
        );
        let post = classify_callback("POST /callback?code=a&state=s1 HTTP/1.1\r\n\r\n", "s1");
        assert_eq!((post.status, post.outcome), (405, None));
        assert_eq!(classify_callback("", "s1").status, 400);
    }

    #[test]
    fn proof_message_format() {
        let base = BaseUrl::parse("https://mcp.spew.lol/").unwrap();
        let key = iroh::SecretKey::from_bytes(&[3; 32]).public();
        assert_eq!(
            proof_message(&base, &key, "bm9uY2U"),
            format!("spew-mesh-device-v1\nhttps://mcp.spew.lol\n{key}\nbm9uY2U")
        );
    }

    #[test]
    fn device_resolution() {
        let key = |s: u8| iroh::SecretKey::from_bytes(&[s; 32]).public();
        let dev = |s: u8, name: &str, is_self: bool| DeviceInfo {
            name: name.into(),
            public_key: key(s),
            platform: None,
            relay: None,
            created_at: None,
            last_seen_at: None,
            is_self,
        };
        let devices = vec![
            dev(1, "sam-mbp", true),
            dev(2, "sam-linux", false),
            dev(3, "old", false),
            dev(4, "old", false),
        ];
        assert_eq!(
            resolve_device(&devices, "sam-linux").unwrap().public_key,
            key(2)
        );
        let short = key(3).to_string()[..10].to_string();
        assert_eq!(resolve_device(&devices, &short).unwrap().public_key, key(3));
        assert!(
            matches!(resolve_device(&devices, "sam-mbp"), Err(ApiError::Device(m)) if m.contains("logout"))
        );
        assert!(
            matches!(resolve_device(&devices, "old"), Err(ApiError::Device(m)) if m.contains("several"))
        );
        assert!(resolve_device(&devices, "nope").is_err());
    }

    #[test]
    fn error_bodies() {
        assert_eq!(
            parse_error_body(br#"{"error": {"code": "grant-has-device", "message": "m"}}"#),
            Some(("grant-has-device".into(), "m".into()))
        );
        assert_eq!(
            parse_error_body(br#"{"error": "grant-has-device"}"#),
            Some(("grant-has-device".into(), String::new()))
        );
        assert_eq!(
            parse_error_body(br#"{"error": "invalid_grant", "error_description": "reused"}"#),
            Some(("invalid_grant".into(), "reused".into()))
        );
        assert_eq!(parse_error_body(b"<html>"), None);
    }

    #[test]
    fn device_lists_bare_or_wrapped() {
        let key = iroh::SecretKey::from_bytes(&[5; 32]).public();
        let one = format!(
            r#"{{"name": "sam-mbp", "public_key": "{key}", "relay": "https://relay.example.com", "self": true}}"#
        );
        for text in [format!("[{one}]"), format!(r#"{{"devices": [{one}]}}"#)] {
            let (DeviceList::Bare(d) | DeviceList::Wrapped { devices: d }) =
                serde_json::from_str(&text).unwrap();
            assert_eq!(d.len(), 1);
            assert!(d[0].is_self);
            assert_eq!(d[0].relay.as_deref(), Some("https://relay.example.com"));
        }
    }

    #[test]
    fn relay_reports_are_throttled() {
        let a: RelayUrl = "https://relay-a.example.com".parse().unwrap();
        let b: RelayUrl = "https://relay-b.example.com".parse().unwrap();
        let mut r = RelayReporter::default();
        assert!(r.due(Some(&a), 0));
        r.sent(Some(&a), 0);
        assert!(!r.due(Some(&a), 120_000), "unchanged");
        assert!(!r.due(Some(&b), 30_000), "too soon");
        assert!(r.due(Some(&b), 60_000));
        assert!(r.due(None, 60_000));
    }

    #[test]
    fn stored_auth_debug_hides_tokens() {
        let auth = StoredAuth {
            url: "https://example.com".into(),
            client_id: "c".into(),
            access_token: Some("secret-access".into()),
            refresh_token: Some("secret-refresh".into()),
            expires_at: None,
            scope: Some("mesh".into()),
            username: Some("sam".into()),
            display: None,
        };
        let shown = format!("{auth:?}");
        assert!(!shown.contains("secret"), "{shown}");
    }

    #[test]
    fn auth_store_is_private_and_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let store = AuthStore::new(dir.path().join("auth").join("spew.json"));
        assert_eq!(store.load().unwrap(), None);
        let auth = StoredAuth {
            url: "https://example.com".into(),
            client_id: "c".into(),
            access_token: None,
            refresh_token: None,
            expires_at: None,
            scope: None,
            username: None,
            display: None,
        };
        store.save(&auth).unwrap();
        assert_eq!(store.load().unwrap(), Some(auth.clone()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(store.path()), 0o600);
            assert_eq!(mode(store.path().parent().unwrap()), 0o700);
            std::fs::set_permissions(store.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
            store.load().unwrap();
            assert_eq!(mode(store.path()), 0o600, "tightened on load");
        }
        let leftovers = std::fs::read_dir(store.path().parent().unwrap())
            .unwrap()
            .count();
        assert_eq!(leftovers, 1, "no temporary files left");
        std::fs::write(store.path(), "{").unwrap();
        assert!(matches!(store.load(), Err(ApiError::AuthFile { .. })));
        store.remove().unwrap();
        store.remove().unwrap();
    }
}
