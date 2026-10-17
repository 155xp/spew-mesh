//! The spew directory client against a mock spew server (14): sign-in, tokens, devices and peers.

mod spew_api_mock;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use spew_api_mock::{Detour, MockSpew, TestClock, api_for, key_hex, sign_in_with};
use spew_mesh::roster::spew::PeersFetch;
use spew_mesh::spew_api::{ApiError, AuthStatus, SignedIn, SpewApi};

const T0: u64 = 1_800_000_000_000;

async fn sign_in(api: &SpewApi) -> SignedIn {
    let (result, seen) = sign_in_with(api, vec![], Duration::from_secs(10)).await;
    assert_eq!(seen, vec![302, 200]);
    result.expect("sign-in")
}

fn auth_json(dir: &std::path::Path) -> serde_json::Value {
    let text = std::fs::read_to_string(dir.join("auth").join("spew.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

#[tokio::test]
async fn sign_in_happy_path() {
    let mock = MockSpew::start().await;
    let dir = tempfile::tempdir().unwrap();
    let api = mock.api(dir.path(), TestClock::at(T0));
    assert_eq!(api.auth_status().unwrap(), AuthStatus::NotSignedIn);

    let signed = sign_in(&api).await;
    assert_eq!(signed.me.username, "sam");
    assert_eq!(signed.me.display, "Sam");
    assert_eq!(signed.projects[0].key, "spew");

    mock.with(|s| {
        assert_eq!(s.clients.len(), 1, "one dynamic registration");
        let q = &s.authorize_queries[0];
        assert_eq!(q["scope"], "mesh");
        assert_eq!(q["code_challenge_method"], "S256");
        assert!(!q.contains_key("resource"));
        let t = &s.token_requests[0];
        assert_eq!(t["grant_type"], "authorization_code");
        assert_eq!(t["redirect_uri"], q["redirect_uri"]);
        assert!(!t.contains_key("resource"));
    });

    let auth = auth_json(dir.path());
    assert_eq!(auth["url"], mock.base.as_str());
    assert!(auth["access_token"].as_str().unwrap().starts_with("at-"));
    assert_eq!(auth["scope"], "mesh");
    assert_eq!(auth["expires_at"], T0 + 3600 * 1000);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.path().join("auth").join("spew.json");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    match api.auth_status().unwrap() {
        AuthStatus::SignedIn(me) => assert_eq!(me.username, "sam"),
        other => panic!("{other:?}"),
    }
    assert_eq!(api.me().await.unwrap().user.username, "sam");

    // Signing in again reuses the cached client id.
    sign_in(&api).await;
    assert_eq!(mock.with(|s| s.clients.len()), 1);
}

#[tokio::test]
async fn bad_state_gets_400_and_the_listener_keeps_waiting() {
    let mock = MockSpew::start().await;
    let dir = tempfile::tempdir().unwrap();
    let api = mock.api(dir.path(), TestClock::at(T0));
    let detours = vec![Detour::WrongState, Detour::NoState, Detour::WrongPath];
    let (result, seen) = sign_in_with(&api, detours, Duration::from_secs(10)).await;
    assert_eq!(seen, vec![302, 400, 400, 404, 200]);
    assert_eq!(result.unwrap().me.username, "sam");
}

#[tokio::test]
async fn a_second_callback_after_the_first_is_ignored() {
    let mock = MockSpew::start().await;
    let dir = tempfile::tempdir().unwrap();
    let api = mock.api(dir.path(), TestClock::at(T0));
    let (result, seen) =
        sign_in_with(&api, vec![Detour::SecondCallback], Duration::from_secs(10)).await;
    assert_eq!(result.unwrap().me.username, "sam");
    assert_eq!(seen[..2], [302, 200]);
    // The browser may still be sending the second callback; give it time to land if it is going to.
    tokio::time::sleep(Duration::from_millis(200)).await;
    mock.with(|s| {
        let exchanged: Vec<&str> = s
            .token_requests
            .iter()
            .filter(|t| t["grant_type"] == "authorization_code")
            .map(|t| t["code"].as_str())
            .collect();
        assert_eq!(exchanged.len(), 1, "one code exchanged");
        assert_ne!(exchanged[0], "forged-code");
    });
}

#[tokio::test]
async fn sign_in_gives_up_after_the_timeout() {
    let mock = MockSpew::start().await;
    let dir = tempfile::tempdir().unwrap();
    let api = mock.api(dir.path(), TestClock::at(T0));
    let opened = Arc::new(Mutex::new(Vec::<String>::new()));
    let record = opened.clone();
    let browser = move |url: &str| record.lock().unwrap().push(url.to_string());
    let err = api
        .login(&browser, Duration::from_millis(300))
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::Login(_)), "{err}");
    assert_eq!(opened.lock().unwrap().len(), 1, "the URL was shown");
    // The client id is kept, but no tokens.
    let auth = auth_json(dir.path());
    assert!(auth["client_id"].as_str().unwrap().starts_with("client-"));
    assert!(auth.get("access_token").is_none() && auth.get("refresh_token").is_none());
    assert_eq!(api.auth_status().unwrap(), AuthStatus::NotSignedIn);

    // Trying again after giving up reuses that client.
    sign_in(&api).await;
    assert_eq!(mock.with(|s| s.clients.len()), 1);
}

#[tokio::test]
async fn a_scope_other_than_mesh_is_revoked_and_nothing_is_stored() {
    for scope in [Some("mesh issues"), Some("issues"), None] {
        let mock = MockSpew::start().await;
        mock.with(|s| s.granted_scope = scope.map(str::to_string));
        let dir = tempfile::tempdir().unwrap();
        let api = mock.api(dir.path(), TestClock::at(T0));
        let (result, _) = sign_in_with(&api, vec![], Duration::from_secs(10)).await;
        let err = result.unwrap_err();
        assert!(
            matches!(err, ApiError::MeshUnsupported(_)),
            "{scope:?}: {err}"
        );
        assert!(err.to_string().contains("doesn't support mesh sign-in yet"));
        mock.with(|s| {
            assert_eq!(s.revoked.len(), 2, "{scope:?}");
            assert!(s.revoked.iter().any(|t| t.starts_with("at-")));
            assert!(s.revoked.iter().any(|t| t.starts_with("rt-")));
            assert!(
                s.access.is_empty() && s.refresh.is_empty(),
                "nothing left usable"
            );
            assert_eq!(s.count("GET /api/mesh/v1/me"), 0);
        });
        let auth = auth_json(dir.path());
        assert!(auth.get("access_token").is_none() && auth.get("refresh_token").is_none());
        assert!(auth.get("username").is_none() && auth.get("scope").is_none());
    }
}

#[tokio::test]
async fn a_spew_without_mesh_sign_in_says_so() {
    let mock = MockSpew::start().await;
    mock.with(|s| s.no_discovery = true);
    let dir = tempfile::tempdir().unwrap();
    let api = mock.api(dir.path(), TestClock::at(T0));
    let err = api.start_login().await.unwrap_err();
    assert!(matches!(err, ApiError::MeshUnsupported(_)), "{err}");
}

#[tokio::test]
async fn refresh_rotates_and_reuse_means_sign_in_again() {
    let mock = MockSpew::start().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = TestClock::at(T0);
    let api = mock.api(dir.path(), clock.clone());
    sign_in(&api).await;
    let first = std::fs::read(dir.path().join("auth").join("spew.json")).unwrap();
    let old_refresh = auth_json(dir.path())["refresh_token"].clone();

    // Past expiry the next call refreshes first and stores the rotated pair.
    clock.advance_secs(3600);
    api.me().await.unwrap();
    let auth = auth_json(dir.path());
    assert_ne!(auth["refresh_token"], old_refresh);
    assert_eq!(auth["expires_at"], T0 + 7200 * 1000);
    mock.with(|s| {
        let refreshes: Vec<_> = s
            .token_requests
            .iter()
            .filter(|t| t["grant_type"] == "refresh_token")
            .collect();
        assert_eq!(refreshes.len(), 1);
        assert_eq!(refreshes[0]["refresh_token"], old_refresh.as_str().unwrap());
    });

    // Presenting the rotated-out refresh token again is a reuse: spew ends the grant.
    std::fs::write(dir.path().join("auth").join("spew.json"), first).unwrap();
    let err = api.me().await.unwrap_err();
    assert!(matches!(err, ApiError::SignedOut(_)), "{err}");
    assert!(err.is_signed_out());
    match api.auth_status().unwrap() {
        AuthStatus::SignedOut(me) => assert_eq!(me.username, "sam"),
        other => panic!("{other:?}"),
    }
    assert!(auth_json(dir.path()).get("refresh_token").is_none());
    mock.with(|s| assert!(s.grants.values().all(|g| !g.alive)));
    // Signed out stays signed out without asking spew again.
    let before = mock.with(|s| s.log.len());
    assert!(api.me().await.unwrap_err().is_signed_out());
    assert_eq!(mock.with(|s| s.log.len()), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_processes_never_present_the_same_refresh_token() {
    let mock = MockSpew::start().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = TestClock::at(T0);
    let daemon = mock.api(dir.path(), clock.clone());
    sign_in(&daemon).await;
    // A second client on the same auth file has its own in-process lock, like the CLI next to the daemon;
    // only the file lock keeps them apart.
    let cli = mock.api(dir.path(), clock.clone());
    clock.advance_secs(3600);
    let (a, b) = tokio::join!(daemon.me(), cli.me());
    a.unwrap();
    b.unwrap();
    mock.with(|s| {
        let refreshes = s
            .token_requests
            .iter()
            .filter(|t| t["grant_type"] == "refresh_token")
            .count();
        assert_eq!(
            refreshes, 1,
            "the second client used the first one's refresh"
        );
        assert!(s.grants.values().all(|g| g.alive), "no reuse, grant alive");
    });
    assert!(dir.path().join("auth").join("spew.lock").exists());
}

#[tokio::test]
async fn a_refresh_may_leave_scope_out_but_not_widen_it() {
    let mock = MockSpew::start().await;
    let dir = tempfile::tempdir().unwrap();
    let clock = TestClock::at(T0);
    let api = mock.api(dir.path(), clock.clone());
    sign_in(&api).await;

    mock.with(|s| s.refresh_scope = None);
    clock.advance_secs(3600);
    api.me().await.unwrap();
    assert_eq!(auth_json(dir.path())["scope"], "mesh");

    mock.with(|s| s.refresh_scope = Some("mesh admin".into()));
    clock.advance_secs(3600);
    let err = api.me().await.unwrap_err();
    assert!(matches!(err, ApiError::MeshUnsupported(_)), "{err}");
    mock.with(|s| assert!(s.revoked.iter().any(|t| t.starts_with("rt-"))));
    assert!(auth_json(dir.path()).get("access_token").is_none());
}

#[tokio::test]
async fn a_401_is_fixed_by_one_refresh() {
    let mock = MockSpew::start().await;
    let dir = tempfile::tempdir().unwrap();
    let api = mock.api(dir.path(), TestClock::at(T0));
    sign_in(&api).await;
    // spew drops the access token early; the refresh token still works.
    mock.with(|s| s.access.clear());
    assert_eq!(api.me().await.unwrap().user.username, "sam");
    mock.with(|s| {
        assert_eq!(s.count("GET /api/mesh/v1/me"), 3, "login, 401, retry");
        assert_eq!(s.count("POST /oauth/token"), 2, "code, then one refresh");
    });
}

#[tokio::test]
async fn a_401_a_refresh_cannot_fix_is_signed_out() {
    let mock = MockSpew::start().await;
    let dir = tempfile::tempdir().unwrap();
    let api = mock.api(dir.path(), TestClock::at(T0));
    sign_in(&api).await;
    mock.with(|s| s.reject_all_bearer = true);
    let err = api.fetch_peers(None).await.unwrap_err();
    assert!(matches!(err, ApiError::SignedOut(_)), "{err}");
    // The tokens stay: the refresh worked, so spew may accept them again later.
    mock.with(|s| s.reject_all_bearer = false);
    assert!(api.fetch_peers(None).await.is_ok());
}

#[tokio::test]
async fn peers_use_the_etag() {
    let mock = MockSpew::start().await;
    let dir = tempfile::tempdir().unwrap();
    let api = mock.api(dir.path(), TestClock::at(T0));
    sign_in(&api).await;

    let PeersFetch::Modified { etag, peers } = api.fetch_peers(None).await.unwrap() else {
        panic!("expected a roster");
    };
    assert_eq!(etag.as_deref(), Some("\"v1\""));
    assert_eq!(peers.version, "v1");
    assert_eq!(peers.ttl_secs, 60);

    assert_eq!(
        api.fetch_peers(etag.as_deref()).await.unwrap(),
        PeersFetch::NotModified
    );
    mock.set_peers(
        json!({ "version": "v2", "ttl_secs": 30, "rooms": [] }),
        "\"v2\"",
    );
    let PeersFetch::Modified { etag, peers } = api.fetch_peers(Some("\"v1\"")).await.unwrap()
    else {
        panic!("expected a new roster");
    };
    assert_eq!(etag.as_deref(), Some("\"v2\""));
    assert_eq!(peers.version, "v2");
    mock.with(|s| {
        assert_eq!(
            s.if_none_match,
            vec![None, Some("\"v1\"".into()), Some("\"v1\"".into())]
        )
    });
}

#[tokio::test]
async fn device_registration_proves_the_key() {
    let mock = MockSpew::start().await;
    let dir = tempfile::tempdir().unwrap();
    let api = mock.api(dir.path(), TestClock::at(T0));
    sign_in(&api).await;
    let key = iroh::SecretKey::from_bytes(&[9; 32]);

    let device = api.register_device(&key, "sam-mbp", "macos").await.unwrap();
    assert_eq!(device.public_key, key.public());
    assert_eq!(device.name, "sam-mbp");
    mock.with(|s| {
        let d = s
            .devices
            .iter()
            .find(|d| d.public_key == key.public().to_string())
            .unwrap();
        assert_eq!((d.name.as_str(), d.platform.as_str()), ("sam-mbp", "macos"));
        assert!(s.nonces.values().all(|used| *used), "nonce used once");
    });
    let me = api.me().await.unwrap();
    assert_eq!(
        me.device.and_then(|d| d.public_key),
        Some(key.public().to_string())
    );

    // The same key again is idempotent.
    api.register_device(&key, "sam-mbp", "macos").await.unwrap();

    // A different key under the same grant is refused with a clear way out.
    let other = iroh::SecretKey::from_bytes(&[10; 32]);
    let err = api
        .register_device(&other, "sam-mbp", "macos")
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::GrantHasDevice), "{err}");
    assert!(err.to_string().contains("spew mesh logout"));

    // The client checks its inputs before signing anything.
    assert!(matches!(
        api.register_device(&key, "sam/mbp", "macos").await,
        Err(ApiError::Device(_))
    ));
}

#[tokio::test]
async fn the_mock_refuses_a_proof_over_another_url() {
    let mock = MockSpew::start().await;
    let dir = tempfile::tempdir().unwrap();
    // Same server, but the client knows it as localhost, so it signs a different <url> than spew expects.
    let base = format!("http://localhost:{}", mock.addr.port());
    let api = api_for(&base, dir.path(), TestClock::at(T0));
    sign_in(&api).await;
    let key = iroh::SecretKey::from_bytes(&[9; 32]);
    let err = api
        .register_device(&key, "sam-mbp", "linux")
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ApiError::Server { status: 400, code, .. } if code == "bad-signature"),
        "{err}"
    );
}

#[tokio::test]
async fn a_nonce_that_is_not_32_bytes_is_never_signed() {
    let mock = MockSpew::start().await;
    mock.with(|s| s.bad_nonce = true);
    let dir = tempfile::tempdir().unwrap();
    let api = mock.api(dir.path(), TestClock::at(T0));
    sign_in(&api).await;
    let key = iroh::SecretKey::from_bytes(&[9; 32]);
    let err = api
        .register_device(&key, "sam-mbp", "linux")
        .await
        .unwrap_err();
    assert!(matches!(err, ApiError::InvalidResponse(_)), "{err}");
    assert_eq!(mock.with(|s| s.count("POST /api/mesh/v1/devices")), 0);
}

#[tokio::test]
async fn devices_list_update_and_revoke() {
    let mock = MockSpew::start().await;
    let dir = tempfile::tempdir().unwrap();
    let api = mock.api(dir.path(), TestClock::at(T0));
    sign_in(&api).await;
    let key = iroh::SecretKey::from_bytes(&[9; 32]);
    api.register_device(&key, "sam-mbp", "macos").await.unwrap();

    let devices = api.list_devices().await.unwrap();
    assert_eq!(devices.len(), 2);
    let me = devices.iter().find(|d| d.is_self).unwrap();
    assert_eq!(me.name, "sam-mbp");
    assert_eq!(me.created_at, Some(1_700_000_000_000));
    assert!(devices.iter().any(|d| d.name == "sam-linux" && !d.is_self));

    let relay: spew_mesh::roster::RelayUrl = "https://relay.example.com".parse().unwrap();
    api.update_self(Some(&relay), None).await.unwrap();
    mock.with(|s| {
        let d = s.devices.iter().find(|d| d.name == "sam-mbp").unwrap();
        assert_eq!(d.relay.as_deref(), Some(relay.to_string().as_str()));
    });
    let listed = api.list_devices().await.unwrap();
    let me = listed.iter().find(|d| d.is_self).unwrap();
    assert_eq!(me.relay.as_deref(), Some(relay.to_string().as_str()));

    assert!(matches!(
        api.revoke_device_named("sam-mbp").await,
        Err(ApiError::Device(m)) if m.contains("logout")
    ));
    assert!(matches!(
        api.revoke_device_named("nope").await,
        Err(ApiError::Device(_))
    ));
    let revoked = api.revoke_device_named("sam-linux").await.unwrap();
    assert_eq!(revoked.public_key.to_string(), key_hex(40));
    mock.with(|s| {
        assert_eq!(
            s.count(&format!("DELETE /api/mesh/v1/devices/{}", key_hex(40))),
            1
        )
    });
    let devices = api.list_devices().await.unwrap();
    assert_eq!(devices.len(), 1);
    assert!(devices[0].is_self);
}

#[tokio::test]
async fn logout_unregisters_revokes_and_keeps_the_client_id() {
    let mock = MockSpew::start().await;
    let dir = tempfile::tempdir().unwrap();
    let api = mock.api(dir.path(), TestClock::at(T0));
    sign_in(&api).await;
    let key = iroh::SecretKey::from_bytes(&[9; 32]);
    api.register_device(&key, "sam-mbp", "macos").await.unwrap();
    let refresh = auth_json(dir.path())["refresh_token"]
        .as_str()
        .unwrap()
        .to_string();

    let report = api.logout().await.unwrap();
    assert!(report.was_signed_in && report.device_unregistered && report.token_revoked);
    mock.with(|s| {
        assert!(!s.devices.iter().any(|d| d.name == "sam-mbp"));
        assert!(s.revoked.contains(&refresh));
        assert_eq!(s.count("DELETE /api/mesh/v1/devices/self"), 1);
    });
    assert_eq!(api.auth_status().unwrap(), AuthStatus::NotSignedIn);
    let auth = auth_json(dir.path());
    assert!(auth.get("access_token").is_none() && auth.get("username").is_none());
    assert!(matches!(api.me().await, Err(ApiError::NotSignedIn)));

    // A new sign-in is a new grant, so registering the same key works again, without a new client.
    sign_in(&api).await;
    api.register_device(&key, "sam-mbp", "macos").await.unwrap();
    assert_eq!(mock.with(|s| s.clients.len()), 1);

    // Logging out twice is harmless.
    api.logout().await.unwrap();
    let report = api.logout().await.unwrap();
    assert!(!report.was_signed_in);
}

#[tokio::test]
async fn tokens_for_another_spew_are_not_used() {
    let mock = MockSpew::start().await;
    let dir = tempfile::tempdir().unwrap();
    let api = mock.api(dir.path(), TestClock::at(T0));
    sign_in(&api).await;
    let other = api_for("https://example.com", dir.path(), TestClock::at(T0));
    assert_eq!(other.auth_status().unwrap(), AuthStatus::NotSignedIn);
    assert!(matches!(other.me().await, Err(ApiError::NotSignedIn)));
}
