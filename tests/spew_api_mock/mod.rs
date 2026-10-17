//! A small spew server for tests: OAuth (discovery, registration, authorize, token, revoke), the mesh API and
//! peers, on a hand-written HTTP/1.1 responder. It plays one user, `sam`, and checks what 14 requires of the
//! client: registration fields, PKCE S256, the exact scope, refresh rotation with reuse detection, and the
//! device proof-of-possession signature.

#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use spew_mesh::spew_api::{BaseUrl, Clock, SpewApi};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// A clock the test moves by hand.
#[derive(Debug, Default)]
pub struct TestClock(AtomicU64);

impl TestClock {
    pub fn at(ms: u64) -> Arc<Self> {
        Arc::new(Self(AtomicU64::new(ms)))
    }
    pub fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }
    pub fn advance_secs(&self, secs: u64) {
        self.0.fetch_add(secs * 1000, Ordering::SeqCst);
    }
}

impl Clock for TestClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

pub struct PendingCode {
    pub client_id: String,
    pub redirect_uri: String,
    pub challenge: String,
}

#[derive(Debug, Clone)]
pub struct Grant {
    pub alive: bool,
    pub device: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Device {
    pub public_key: String,
    pub name: String,
    pub platform: String,
    pub grant: Option<u32>,
    pub relay: Option<String>,
}

pub struct State {
    pub base: String,
    pub granted_scope: Option<String>,
    pub refresh_scope: Option<String>,
    pub expires_in: u64,
    pub no_discovery: bool,
    pub bad_nonce: bool,
    pub reject_all_bearer: bool,
    pub clients: Vec<String>,
    pub authorize_queries: Vec<HashMap<String, String>>,
    pub token_requests: Vec<HashMap<String, String>>,
    pub codes: HashMap<String, PendingCode>,
    pub grants: HashMap<u32, Grant>,
    pub access: HashMap<String, u32>,
    pub refresh: HashMap<String, (u32, bool)>,
    pub revoked: Vec<String>,
    pub nonces: HashMap<String, bool>,
    pub devices: Vec<Device>,
    pub peers: Value,
    pub etag: String,
    pub if_none_match: Vec<Option<String>>,
    pub log: Vec<String>,
    next: u32,
}

impl State {
    fn next(&mut self) -> u32 {
        self.next += 1;
        self.next
    }

    pub fn count(&self, line: &str) -> usize {
        self.log.iter().filter(|l| l.as_str() == line).count()
    }

    /// Ends a grant the way spew does when a refresh token is reused or a device is revoked.
    pub fn kill_grant(&mut self, grant: u32) {
        if let Some(g) = self.grants.get_mut(&grant) {
            g.alive = false;
        }
        self.access.retain(|_, g| *g != grant);
        self.refresh.retain(|_, (g, _)| *g != grant);
    }

    fn issue(&mut self, grant: u32, scope: Option<&str>) -> Value {
        let n = self.next();
        let access = format!("at-{n}");
        let refresh = format!("rt-{n}");
        self.access.insert(access.clone(), grant);
        self.refresh.insert(refresh.clone(), (grant, false));
        let mut body = json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": self.expires_in,
            "refresh_token": refresh,
        });
        if let Some(scope) = scope {
            body["scope"] = json!(scope);
        }
        body
    }
}

pub struct MockSpew {
    pub base: String,
    pub addr: SocketAddr,
    pub state: Arc<Mutex<State>>,
    task: JoinHandle<()>,
}

impl Drop for MockSpew {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// An endpoint id for a seeded device.
pub fn key_hex(seed: u8) -> String {
    iroh::SecretKey::from_bytes(&[seed; 32])
        .public()
        .to_string()
}

impl MockSpew {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base = format!("http://127.0.0.1:{}", addr.port());
        let state = Arc::new(Mutex::new(State {
            base: base.clone(),
            granted_scope: Some("mesh".into()),
            refresh_scope: Some("mesh".into()),
            expires_in: 3600,
            no_discovery: false,
            bad_nonce: false,
            reject_all_bearer: false,
            clients: Vec::new(),
            authorize_queries: Vec::new(),
            token_requests: Vec::new(),
            codes: HashMap::new(),
            grants: HashMap::new(),
            access: HashMap::new(),
            refresh: HashMap::new(),
            revoked: Vec::new(),
            nonces: HashMap::new(),
            // Another computer of sam's, registered earlier under its own grant.
            devices: vec![Device {
                public_key: key_hex(40),
                name: "sam-linux".into(),
                platform: "linux".into(),
                grant: None,
                relay: None,
            }],
            peers: json!({ "version": "v1", "ttl_secs": 60, "rooms": [] }),
            etag: "\"v1\"".into(),
            if_none_match: Vec::new(),
            log: Vec::new(),
            next: 0,
        }));
        let shared = state.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                let state = shared.clone();
                tokio::spawn(async move { serve(stream, state).await });
            }
        });
        Self {
            base,
            addr,
            state,
            task,
        }
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }

    /// Sets the peers body and its ETag.
    pub fn set_peers(&self, peers: Value, etag: &str) {
        self.with(|s| {
            s.peers = peers;
            s.etag = etag.to_string();
        });
    }

    /// A client for this server whose auth file is in `dir`.
    pub fn api(&self, dir: &std::path::Path, clock: Arc<TestClock>) -> SpewApi {
        api_for(&self.base, dir, clock)
    }
}

/// A client for `base` that ignores proxy variables, with its auth file in `dir`.
pub fn api_for(base: &str, dir: &std::path::Path, clock: Arc<TestClock>) -> SpewApi {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    SpewApi::with_http(
        BaseUrl::parse(base).unwrap(),
        dir.join("auth").join("spew.json"),
        clock,
        http,
    )
}

/// What the test browser does besides delivering the real callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detour {
    /// Before it: the callback with a wrong state.
    WrongState,
    /// Before it: the callback without a state.
    NoState,
    /// Before it: another path on the listener.
    WrongPath,
    /// After it: a second callback with the right state and a forged code.
    SecondCallback,
}

/// A browser that follows the authorize URL to the mock's redirect, then makes the requests its detours ask for
/// around the real callback. Every status it gets is pushed to `seen`, the authorize redirect first; a request
/// that fails records 0. Nothing panics in the browser task, so a failure shows in the test's own asserts.
pub fn browser(
    detours: Vec<Detour>,
    seen: Arc<Mutex<Vec<u16>>>,
) -> impl Fn(&str) + Send + Sync + 'static {
    move |url: &str| {
        let url = url.to_string();
        let detours = detours.clone();
        let seen = seen.clone();
        tokio::spawn(async move {
            let record = |status: u16| seen.lock().unwrap().push(status);
            let Ok(http) = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()
            else {
                return record(0);
            };
            let get = |target: url::Url| {
                let http = http.clone();
                async move {
                    http.get(target)
                        .send()
                        .await
                        .map_or(0, |r| r.status().as_u16())
                }
            };
            let Ok(resp) = http.get(&url).send().await else {
                return record(0);
            };
            record(resp.status().as_u16());
            let Some(callback) = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .and_then(|l| url::Url::parse(l).ok())
            else {
                return;
            };
            let with_query = |state: Option<&str>, code: Option<&str>| {
                let mut target = callback.clone();
                let pairs: Vec<(String, String)> = callback
                    .query_pairs()
                    .into_owned()
                    .filter(|(k, _)| k != "state" && (code.is_none() || k != "code"))
                    .collect();
                let mut q = target.query_pairs_mut();
                q.clear();
                for (k, v) in &pairs {
                    q.append_pair(k, v);
                }
                if let Some(code) = code {
                    q.append_pair("code", code);
                }
                if let Some(state) = state {
                    q.append_pair("state", state);
                }
                drop(q);
                target
            };
            let state = callback
                .query_pairs()
                .find(|(k, _)| k == "state")
                .map(|(_, v)| v.into_owned());
            for detour in &detours {
                let target = match detour {
                    Detour::WrongPath => {
                        let mut t = callback.clone();
                        t.set_path("/favicon.ico");
                        t
                    }
                    Detour::WrongState => with_query(Some("not-the-state"), None),
                    Detour::NoState => with_query(None, None),
                    Detour::SecondCallback => continue,
                };
                record(get(target).await);
            }
            record(get(callback.clone()).await);
            if detours.contains(&Detour::SecondCallback) {
                record(get(with_query(state.as_deref(), Some("forged-code"))).await);
            }
        });
    }
}

/// Signs in through [`browser`] and checks that the authorize request redirected before anything else, so a
/// broken redirect fails here instead of as a sign-in timeout.
pub async fn sign_in_with(
    api: &SpewApi,
    detours: Vec<Detour>,
    timeout: std::time::Duration,
) -> (
    Result<spew_mesh::spew_api::SignedIn, spew_mesh::spew_api::ApiError>,
    Vec<u16>,
) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let result = api.login(&browser(detours, seen.clone()), timeout).await;
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen.first(),
        Some(&302),
        "authorize should redirect: {seen:?}"
    );
    (result, seen)
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

fn resp(status: u16, body: Value) -> Resp {
    Resp {
        status,
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: if body.is_null() {
            String::new()
        } else {
            body.to_string()
        },
    }
}

fn api_error(status: u16, code: &str, message: &str) -> Resp {
    resp(
        status,
        json!({ "error": { "code": code, "message": message } }),
    )
}

fn oauth_error(code: &str) -> Resp {
    resp(400, json!({ "error": code, "error_description": code }))
}

async fn serve(mut stream: TcpStream, state: Arc<Mutex<State>>) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let method = first.next().unwrap_or("").to_string();
    let target = first.next().unwrap_or("").to_string();
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => body.extend_from_slice(&chunk[..n]),
        }
    }
    let url = url::Url::parse(&format!("http://mock{target}")).unwrap();
    let request = Request {
        method,
        path: url.path().to_string(),
        query: url.query_pairs().into_owned().collect(),
        headers,
        body,
    };
    let reply = {
        let mut s = state.lock().unwrap();
        s.log.push(format!("{} {}", request.method, request.path));
        route(&mut s, &request)
    };
    let reason = match reply.status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        302 => "Found",
        304 => "Not Modified",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Error",
    };
    let mut out = format!("HTTP/1.1 {} {reason}\r\n", reply.status);
    for (k, v) in &reply.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        reply.body.len(),
        reply.body
    ));
    let _ = stream.write_all(out.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn form(body: &[u8]) -> HashMap<String, String> {
    url::form_urlencoded::parse(body).into_owned().collect()
}

fn route(s: &mut State, r: &Request) -> Resp {
    let base = s.base.clone();
    match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/.well-known/oauth-authorization-server") => {
            if s.no_discovery {
                return resp(404, json!({}));
            }
            resp(
                200,
                json!({
                    "issuer": base,
                    "authorization_endpoint": format!("{base}/oauth/authorize"),
                    "token_endpoint": format!("{base}/oauth/token"),
                    "registration_endpoint": format!("{base}/oauth/register"),
                    "revocation_endpoint": format!("{base}/oauth/revoke"),
                    "scopes_supported": ["mcp", "mesh"],
                    "code_challenge_methods_supported": ["S256"],
                    "response_types_supported": ["code"],
                }),
            )
        }
        ("POST", "/oauth/register") => {
            let body: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
            let expected = json!({
                "client_name": "spew mesh",
                "redirect_uris": ["http://127.0.0.1/callback"],
                "token_endpoint_auth_method": "none",
                "grant_types": ["authorization_code", "refresh_token"],
            });
            if body != expected {
                return oauth_error("invalid_client_metadata");
            }
            let id = format!("client-{}", s.next());
            s.clients.push(id.clone());
            resp(201, json!({ "client_id": id, "client_name": "spew mesh" }))
        }
        ("GET", "/oauth/authorize") => {
            let q = &r.query;
            s.authorize_queries.push(q.clone());
            let get = |k: &str| q.get(k).map(String::as_str).unwrap_or("");
            let redirect_ok = url::Url::parse(get("redirect_uri")).is_ok_and(|u| {
                u.scheme() == "http"
                    && u.host_str() == Some("127.0.0.1")
                    && u.port().is_some()
                    && u.path() == "/callback"
            });
            if get("response_type") != "code"
                || !s.clients.iter().any(|c| c == get("client_id"))
                || !redirect_ok
                || get("code_challenge_method") != "S256"
                || get("code_challenge").len() != 43
                || get("scope") != "mesh"
                || get("state").is_empty()
                || q.contains_key("resource")
            {
                return oauth_error("invalid_request");
            }
            let code = format!("code-{}", s.next());
            s.codes.insert(
                code.clone(),
                PendingCode {
                    client_id: get("client_id").into(),
                    redirect_uri: get("redirect_uri").into(),
                    challenge: get("code_challenge").into(),
                },
            );
            let mut location = url::Url::parse(get("redirect_uri")).unwrap();
            location
                .query_pairs_mut()
                .append_pair("code", &code)
                .append_pair("state", get("state"));
            Resp {
                status: 302,
                headers: vec![("Location".into(), location.to_string())],
                body: String::new(),
            }
        }
        ("POST", "/oauth/token") => {
            let f = form(&r.body);
            s.token_requests.push(f.clone());
            let get = |k: &str| f.get(k).cloned().unwrap_or_default();
            match get("grant_type").as_str() {
                "authorization_code" => {
                    let Some(pending) = s.codes.remove(&get("code")) else {
                        return oauth_error("invalid_grant");
                    };
                    let challenge =
                        URL_SAFE_NO_PAD.encode(Sha256::digest(get("code_verifier").as_bytes()));
                    if pending.client_id != get("client_id")
                        || pending.redirect_uri != get("redirect_uri")
                        || pending.challenge != challenge
                    {
                        return oauth_error("invalid_grant");
                    }
                    let grant = s.next();
                    s.grants.insert(
                        grant,
                        Grant {
                            alive: true,
                            device: None,
                        },
                    );
                    let scope = s.granted_scope.clone();
                    resp(200, s.issue(grant, scope.as_deref()))
                }
                "refresh_token" => {
                    let token = get("refresh_token");
                    match s.refresh.get(&token).cloned() {
                        None => oauth_error("invalid_grant"),
                        Some((grant, true)) => {
                            // Reuse of a rotated refresh token ends the whole grant.
                            s.kill_grant(grant);
                            oauth_error("invalid_grant")
                        }
                        Some((grant, false)) => {
                            s.refresh.insert(token, (grant, true));
                            let scope = s.refresh_scope.clone();
                            resp(200, s.issue(grant, scope.as_deref()))
                        }
                    }
                }
                _ => oauth_error("unsupported_grant_type"),
            }
        }
        ("POST", "/oauth/revoke") => {
            let token = form(&r.body).get("token").cloned().unwrap_or_default();
            s.access.remove(&token);
            s.refresh.remove(&token);
            s.revoked.push(token);
            resp(200, Value::Null)
        }
        (_, path) if path.starts_with("/api/mesh/v1/") => {
            let token = r
                .headers
                .get("authorization")
                .and_then(|h| h.strip_prefix("Bearer "))
                .unwrap_or("");
            let grant = match s.access.get(token) {
                Some(g) if !s.reject_all_bearer && s.grants.get(g).is_some_and(|g| g.alive) => *g,
                _ => return api_error(401, "unauthorized", "token not accepted"),
            };
            mesh_api(s, r, grant)
        }
        _ => api_error(404, "not-found", "no such route"),
    }
}

fn device_json(d: &Device, grant: u32) -> Value {
    json!({
        "name": d.name,
        "public_key": d.public_key,
        "platform": d.platform,
        "relay": d.relay,
        "created_at": 1_700_000_000_000u64,
        "last_seen_at": null,
        "self": d.grant == Some(grant),
    })
}

fn mesh_api(s: &mut State, r: &Request, grant: u32) -> Resp {
    let body: Value = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
    match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/api/mesh/v1/me") => {
            let device = s
                .devices
                .iter()
                .find(|d| d.grant == Some(grant))
                .map(|d| json!({ "name": d.name, "public_key": d.public_key }));
            resp(
                200,
                json!({
                    "user": { "username": "sam", "name": "Sam" },
                    "device": device,
                    "projects": [ { "key": "spew", "name": "spew" } ],
                }),
            )
        }
        ("POST", "/api/mesh/v1/devices/challenge") => {
            let n = s.next();
            let nonce = if s.bad_nonce {
                "abc\nhttps://elsewhere.example.com".to_string()
            } else {
                URL_SAFE_NO_PAD.encode(Sha256::digest(format!("nonce-{n}").as_bytes()))
            };
            s.nonces.insert(nonce.clone(), false);
            resp(
                200,
                json!({ "nonce": nonce, "expires_at": 1_700_000_300_000u64 }),
            )
        }
        ("POST", "/api/mesh/v1/devices") => {
            let field = |k: &str| {
                body.get(k)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            let (public_key, name, platform, nonce, signature) = (
                field("public_key"),
                field("name"),
                field("platform"),
                field("nonce"),
                field("signature"),
            );
            match s.nonces.get(&nonce) {
                Some(false) => {
                    s.nonces.insert(nonce.clone(), true);
                }
                _ => return api_error(400, "bad-nonce", "unknown or used nonce"),
            }
            let Ok(key) = public_key.parse::<iroh::PublicKey>() else {
                return api_error(400, "bad-key", "not a key");
            };
            let Some(sig) = URL_SAFE_NO_PAD
                .decode(signature.as_bytes())
                .ok()
                .and_then(|b| <[u8; 64]>::try_from(b.as_slice()).ok())
            else {
                return api_error(400, "bad-signature", "not a signature");
            };
            let message = format!("spew-mesh-device-v1\n{}\n{public_key}\n{nonce}", s.base);
            if key
                .verify(message.as_bytes(), &iroh::Signature::from_bytes(&sig))
                .is_err()
            {
                return api_error(400, "bad-signature", "signature doesn't verify");
            }
            let held = s.grants.get(&grant).and_then(|g| g.device.clone());
            if held.as_ref().is_some_and(|k| *k != public_key) {
                return api_error(409, "grant-has-device", "this grant already holds a device");
            }
            if let Some(d) = s.devices.iter_mut().find(|d| d.public_key == public_key) {
                d.grant = Some(grant);
            } else {
                s.devices.push(Device {
                    public_key: public_key.clone(),
                    name: name.clone(),
                    platform,
                    grant: Some(grant),
                    relay: None,
                });
            }
            if let Some(g) = s.grants.get_mut(&grant) {
                g.device = Some(public_key.clone());
            }
            resp(
                201,
                json!({ "name": name, "public_key": public_key, "created_at": 1_700_000_000_000u64 }),
            )
        }
        ("GET", "/api/mesh/v1/devices") => {
            let list: Vec<Value> = s.devices.iter().map(|d| device_json(d, grant)).collect();
            resp(200, json!({ "devices": list }))
        }
        ("PATCH", "/api/mesh/v1/devices/self") => {
            let Some(d) = s.devices.iter_mut().find(|d| d.grant == Some(grant)) else {
                return api_error(404, "not-found", "no device for this grant");
            };
            d.relay = body
                .get("relay")
                .and_then(Value::as_str)
                .map(str::to_string);
            if let Some(name) = body.get("name").and_then(Value::as_str) {
                d.name = name.to_string();
            }
            resp(200, json!({}))
        }
        ("DELETE", "/api/mesh/v1/devices/self") => {
            let before = s.devices.len();
            s.devices.retain(|d| d.grant != Some(grant));
            if let Some(g) = s.grants.get_mut(&grant) {
                g.device = None;
            }
            if s.devices.len() == before {
                api_error(404, "not-found", "no device for this grant")
            } else {
                resp(204, Value::Null)
            }
        }
        ("DELETE", path) if path.starts_with("/api/mesh/v1/devices/") => {
            let key = path.trim_start_matches("/api/mesh/v1/devices/");
            let Some(i) = s.devices.iter().position(|d| d.public_key == key) else {
                return api_error(404, "not-found", "no such device");
            };
            let removed = s.devices.remove(i);
            if let Some(g) = removed.grant {
                s.kill_grant(g);
            }
            resp(204, Value::Null)
        }
        ("GET", "/api/mesh/v1/peers") => {
            let inm = r.headers.get("if-none-match").cloned();
            s.if_none_match.push(inm.clone());
            if inm.as_deref() == Some(s.etag.as_str()) {
                return Resp {
                    status: 304,
                    headers: vec![("ETag".into(), s.etag.clone())],
                    body: String::new(),
                };
            }
            let mut reply = resp(200, s.peers.clone());
            reply.headers.push(("ETag".into(), s.etag.clone()));
            reply
        }
        _ => api_error(404, "not-found", "no such route"),
    }
}
