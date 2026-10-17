//! Rosters (4.2, 4.3): spew roster staleness with an injected clock, refreshes against a mock spew, and
//! membership changes as the roster in force changes.

mod spew_api_mock;

use std::time::Duration;

use serde_json::{Value, json};
use spew_api_mock::{MockSpew, TestClock, sign_in_with};
use spew_mesh::roster::spew::{
    Peers, PeersFetch, RefreshOutcome, SpewRosterCache, SpewRosterState, UnknownPeerRefresh,
    refresh,
};
use spew_mesh::roster::{ActiveRoster, Admission, EndpointId, Me, PRIVATE_ROOM_ID, file, local};

const T0: u64 = 1_800_000_000_000;
const DAY: u64 = 86_400;

fn key(seed: u8) -> EndpointId {
    iroh::SecretKey::from_bytes(&[seed; 32]).public()
}

fn sam() -> Me {
    Me {
        username: "sam".into(),
        display: "Sam".into(),
    }
}

/// This device is sam-mbp (key 1); sam also has sam-linux (key 2); maya has maya-mbp (key 3).
fn peers_json(version: &str, ttl: u64) -> Value {
    json!({
        "version": version,
        "ttl_secs": ttl,
        "me": { "username": "sam", "display": "Sam", "devices": [
            { "endpoint_id": key(1).to_string(), "name": "sam-mbp", "relay": null },
            { "endpoint_id": key(2).to_string(), "name": "sam-linux",
              "relay": "https://relay.example.com" }
        ] },
        "rooms": [{
            "id": "spew", "name": "spew", "kind": "project",
            "members": [
                { "username": "sam", "display": "Sam", "devices": [
                    { "endpoint_id": key(1).to_string(), "name": "sam-mbp", "relay": null },
                    { "endpoint_id": key(2).to_string(), "name": "sam-linux",
                      "relay": "https://relay.example.com" }
                ] },
                { "username": "maya", "display": "Maya", "devices": [
                    { "endpoint_id": key(3).to_string(), "name": "maya-mbp", "relay": null }
                ] }
            ]
        }]
    })
}

fn peers(version: &str, ttl: u64) -> Peers {
    serde_json::from_value(peers_json(version, ttl)).unwrap()
}

fn modified(p: Peers) -> PeersFetch {
    PeersFetch::Modified {
        etag: Some(format!("\"{}\"", p.version)),
        peers: p,
    }
}

fn admission_at(cache: &SpewRosterCache, now: u64, max_stale: u64) -> (SpewRosterState, Admission) {
    let view = cache.view(&key(1), "sam-mbp", now, max_stale);
    let admission = Admission::compute(cache.me(), &key(1), &view.roster);
    (view.state, admission)
}

#[test]
fn staleness_moves_from_fresh_to_own_devices_to_this_device() {
    let mut cache = SpewRosterCache::new(sam());
    let (state, a) = admission_at(&cache, T0, DAY);
    assert_eq!(state, SpewRosterState::NoRoster);
    assert!(!a.admits_others());

    cache
        .apply(modified(peers("v1", 60)), &key(1), "sam-mbp", T0)
        .unwrap();
    let (state, a) = admission_at(&cache, T0, DAY);
    assert_eq!(state, SpewRosterState::Fresh);
    assert!(a.is_admitted(&key(2)) && a.is_admitted(&key(3)));
    assert!(a.get(&key(2)).unwrap().own);
    assert!(a.shares_room(&key(3), "spew"));
    assert!(!a.shares_room(&key(3), PRIVATE_ROOM_ID));
    assert!(!a.is_admitted(&key(1)), "never this device");

    // ttl 60: teammates stay while younger than max(15 x 60, 900) = 900 s.
    let (state, a) = admission_at(&cache, T0 + 899_999, DAY);
    assert_eq!(state, SpewRosterState::Fresh);
    assert!(a.is_admitted(&key(3)));
    let (state, a) = admission_at(&cache, T0 + 900_000, DAY);
    assert_eq!(state, SpewRosterState::Stale);
    assert!(!a.is_admitted(&key(3)), "teammate dropped");
    assert!(a.is_admitted(&key(2)), "own device kept");

    // Own devices stay for max_stale_seconds, then only this device.
    let (state, a) = admission_at(&cache, T0 + DAY * 1000 - 1, DAY);
    assert_eq!(state, SpewRosterState::Stale);
    assert!(a.is_admitted(&key(2)));
    let (state, a) = admission_at(&cache, T0 + DAY * 1000, DAY);
    assert_eq!(state, SpewRosterState::Expired);
    assert!(!a.admits_others());
    let view = cache.view(&key(1), "sam-mbp", T0 + DAY * 1000, DAY);
    assert_eq!(view.roster.rooms.len(), 1);
    assert!(view.describe().contains("only this device"));

    // A 304 confirms the roster and starts the clock again.
    cache
        .apply(PeersFetch::NotModified, &key(1), "sam-mbp", T0 + DAY * 1000)
        .unwrap();
    let (state, a) = admission_at(&cache, T0 + DAY * 1000 + 1, DAY);
    assert_eq!(state, SpewRosterState::Fresh);
    assert!(a.is_admitted(&key(3)));
}

#[test]
fn the_teammate_window_follows_ttl() {
    let mut cache = SpewRosterCache::new(sam());
    cache
        .apply(modified(peers("v1", 120)), &key(1), "sam-mbp", T0)
        .unwrap();
    assert_eq!(
        admission_at(&cache, T0 + 1_799_999, DAY).0,
        SpewRosterState::Fresh
    );
    assert_eq!(
        admission_at(&cache, T0 + 1_800_000, DAY).0,
        SpewRosterState::Stale
    );
    // A small ttl never goes under 900 s.
    cache
        .apply(modified(peers("v2", 1)), &key(1), "sam-mbp", T0)
        .unwrap();
    assert_eq!(
        admission_at(&cache, T0 + 899_999, DAY).0,
        SpewRosterState::Fresh
    );
    // A max_stale_seconds below the teammate window caps it: teammates never outstay own devices.
    assert_eq!(
        admission_at(&cache, T0 + 600_000, 600).0,
        SpewRosterState::Expired
    );
    assert_eq!(
        admission_at(&cache, T0 + 599_999, 600).0,
        SpewRosterState::Fresh
    );
}

#[test]
fn signed_out_drops_teammates_at_once() {
    let mut cache = SpewRosterCache::new(sam());
    cache
        .apply(modified(peers("v1", 60)), &key(1), "sam-mbp", T0)
        .unwrap();
    cache.mark_signed_out();
    let view = cache.view(&key(1), "sam-mbp", T0 + 1, DAY);
    assert_eq!(view.state, SpewRosterState::SignedOut);
    assert!(!view.teammates && view.own_devices);
    assert!(view.describe().contains("spew mesh login"));
    let a = Admission::compute(cache.me(), &key(1), &view.roster);
    assert!(!a.is_admitted(&key(3)));
    assert!(a.is_admitted(&key(2)));
    // Own devices from the last roster still expire after max_stale_seconds.
    let (state, a) = admission_at(&cache, T0 + DAY * 1000, DAY);
    assert_eq!(state, SpewRosterState::SignedOut);
    assert!(!a.admits_others());
    // A good answer from spew ends it.
    cache
        .apply(PeersFetch::NotModified, &key(1), "sam-mbp", T0 + 5)
        .unwrap();
    assert!(!cache.is_signed_out());
    assert_eq!(admission_at(&cache, T0 + 6, DAY).0, SpewRosterState::Fresh);
}

#[test]
fn a_bad_roster_is_refused_and_the_last_good_one_stays() {
    let mut cache = SpewRosterCache::new(sam());
    cache
        .apply(modified(peers("v1", 60)), &key(1), "sam-mbp", T0)
        .unwrap();
    let before = cache.clone();

    let mut bad = Vec::new();
    // Maya's device claims this device's key.
    let mut stolen = peers_json("v2", 60);
    stolen["rooms"][0]["members"][1]["devices"][0]["endpoint_id"] = json!(key(1).to_string());
    bad.push(stolen);
    // A roster for another person.
    let mut not_me = peers_json("v2", 60);
    not_me["me"]["username"] = json!("maya");
    bad.push(not_me);
    bad.push(peers_json("v2", 86_401));
    bad.push(peers_json("v2", 0));
    for body in bad {
        let p: Peers = serde_json::from_value(body).unwrap();
        assert!(
            cache
                .apply(modified(p), &key(1), "sam-mbp", T0 + 10)
                .is_err()
        );
        assert_eq!(cache, before);
    }
    // A 304 with nothing cached is refused too.
    let mut empty = SpewRosterCache::new(sam());
    assert!(
        empty
            .apply(PeersFetch::NotModified, &key(1), "sam-mbp", T0)
            .is_err()
    );
}

#[test]
fn entries_outside_the_name_rules_are_dropped_and_the_rest_applies() {
    let mut body = peers_json("v2", 60);
    let members = body["rooms"][0]["members"].as_array_mut().unwrap();
    members.push(json!({ "username": "Lee", "devices": [
        { "endpoint_id": key(4).to_string(), "name": "lee-mac" } ] }));
    members.push(json!({ "username": "kim", "display": "Kim", "devices": [
        { "endpoint_id": key(5).to_string(), "name": "Kim-PC" },
        { "endpoint_id": "not-a-key", "name": "kim-old" },
        { "endpoint_id": key(6).to_string(), "name": "kim-linux", "relay": "ftp://relay.example.com" } ] }));
    members.push(json!({ "username": "me", "devices": [] }));
    body["rooms"].as_array_mut().unwrap().extend([
        json!({ "id": "team", "name": "team", "kind": "team", "members": [] }),
        json!({ "id": "Big Room", "kind": "project", "members": [] }),
    ]);
    let mut cache = SpewRosterCache::new(sam());
    cache
        .apply(
            modified(serde_json::from_value(body).unwrap()),
            &key(1),
            "sam-mbp",
            T0,
        )
        .unwrap();
    let view = cache.view(&key(1), "sam-mbp", T0, DAY);
    assert_eq!(view.state, SpewRosterState::Fresh);
    assert_eq!(view.dropped.len(), 7, "{:#?}", view.dropped);
    assert!(
        view.describe()
            .contains("7 roster entries from spew dropped")
    );
    let a = Admission::compute(cache.me(), &key(1), &view.roster);
    assert!(
        a.is_admitted(&key(2)) && a.is_admitted(&key(3)),
        "the rest applies"
    );
    assert!(!a.is_admitted(&key(4)), "person with a bad name dropped");
    assert!(!a.is_admitted(&key(5)), "device with a bad name dropped");
    let kim = a.get(&key(6)).unwrap();
    assert_eq!(kim.person, "kim");
    assert_eq!(kim.relay, None, "bad relay hint ignored, device kept");
    let ids: Vec<&str> = view.roster.rooms.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec![PRIVATE_ROOM_ID, "spew"]);
}

#[test]
fn own_devices_come_from_me_with_no_shared_project() {
    let body = json!({
        "version": "v1",
        "ttl_secs": 60,
        "me": { "username": "sam", "display": "Sam", "devices": [
            { "endpoint_id": key(1).to_string(), "name": "sam-mbp" },
            { "endpoint_id": key(2).to_string(), "name": "sam-linux" } ] },
        "rooms": []
    });
    let mut cache = SpewRosterCache::new(sam());
    cache
        .apply(
            modified(serde_json::from_value(body).unwrap()),
            &key(1),
            "sam-mbp",
            T0,
        )
        .unwrap();
    let (state, a) = admission_at(&cache, T0, DAY);
    assert_eq!(state, SpewRosterState::Fresh);
    assert!(a.get(&key(2)).is_some_and(|d| d.own));
    // Teammates drop with staleness; own devices from `me` stay.
    let (state, a) = admission_at(&cache, T0 + 900_000, DAY);
    assert_eq!(state, SpewRosterState::Stale);
    assert!(a.is_admitted(&key(2)));

    // A spew that sends no `me` still gives my devices from the project rooms.
    let mut old = peers_json("v1", 60);
    old.as_object_mut().unwrap().remove("me");
    let mut cache = SpewRosterCache::new(sam());
    cache
        .apply(
            modified(serde_json::from_value(old).unwrap()),
            &key(1),
            "sam-mbp",
            T0,
        )
        .unwrap();
    let (_, a) = admission_at(&cache, T0 + 900_000, DAY);
    assert!(a.is_admitted(&key(2)) && !a.is_admitted(&key(3)));
}

#[test]
fn this_device_keeps_the_name_spew_registered() {
    let mut cache = SpewRosterCache::new(sam());
    // Locally renamed to sam-air, still registered as sam-mbp: not a conflict.
    cache
        .apply(modified(peers("v1", 60)), &key(1), "sam-air", T0)
        .unwrap();
    let view = cache.view(&key(1), "sam-air", T0, DAY);
    assert_eq!(view.state, SpewRosterState::Fresh);
    assert_eq!(view.roster.rooms[0].members[0].devices[0].name, "sam-mbp");
}

#[test]
fn the_cache_survives_a_restart() {
    let mut cache = SpewRosterCache::new(sam());
    cache
        .apply(modified(peers("v1", 60)), &key(1), "sam-mbp", T0)
        .unwrap();
    let json = cache.to_json();
    let restored = SpewRosterCache::restore(&json, sam());
    assert_eq!(restored, cache);
    assert_eq!(restored.if_none_match(), Some("\"v1\""));
    let maya = Me {
        username: "maya".into(),
        display: "maya".into(),
    };
    assert!(SpewRosterCache::restore(&json, maya).peers().is_none());
    assert!(SpewRosterCache::restore("{", sam()).peers().is_none());
}

#[test]
fn unknown_peers_refresh_at_most_every_ten_seconds() {
    let mut limit = UnknownPeerRefresh::default();
    assert!(limit.allow(T0));
    assert!(!limit.allow(T0 + 9_999));
    assert!(limit.allow(T0 + 10_000));
    assert!(!limit.allow(T0 + 10_001));
}

#[test]
fn membership_changes_follow_the_roster_in_force() {
    let mut cache = SpewRosterCache::new(sam());
    let first = cache.view(&key(1), "sam-mbp", T0, DAY);
    let mut active = ActiveRoster::new(sam(), key(1), first.roster);
    assert!(!active.admission().admits_others());

    cache
        .apply(modified(peers("v1", 60)), &key(1), "sam-mbp", T0)
        .unwrap();
    let change = active.replace(sam(), cache.view(&key(1), "sam-mbp", T0, DAY).roster);
    let mut both = vec![key(2), key(3)];
    both.sort();
    assert_eq!(change.added, both);
    assert_eq!(change.network_change(), Some(true));

    // Maya leaves the project.
    let mut without_maya = peers_json("v2", 60);
    without_maya["rooms"][0]["members"]
        .as_array_mut()
        .unwrap()
        .remove(1);
    cache
        .apply(
            modified(serde_json::from_value(without_maya).unwrap()),
            &key(1),
            "sam-mbp",
            T0 + 60_000,
        )
        .unwrap();
    let change = active.replace(
        sam(),
        cache.view(&key(1), "sam-mbp", T0 + 60_000, DAY).roster,
    );
    assert_eq!(change.removed, vec![key(3)]);
    assert!(change.added.is_empty());
    assert_eq!(change.network_change(), None);

    // Then spew goes quiet long enough that only this device remains.
    let change = active.replace(
        sam(),
        cache
            .view(&key(1), "sam-mbp", T0 + 60_000 + DAY * 1000, DAY)
            .roster,
    );
    assert_eq!(change.removed, vec![key(2)]);
    assert_eq!(change.network_change(), Some(false));
}

#[test]
fn file_and_local_rosters_admit_what_they_list() {
    let me = Me {
        username: "sam".into(),
        display: "sam".into(),
    };
    let local = local::local_roster(&me, &key(1), "sam-mbp", T0).unwrap();
    let mut active = ActiveRoster::new(me.clone(), key(1), local);
    assert!(!active.admission().admits_others());

    let text = format!(
        "[[person]]\nname = \"sam\"\ndevices = [{{ id = \"{}\", name = \"sam-mbp\" }}]\n\
         [[person]]\nname = \"maya\"\ndevices = [{{ id = \"{}\", name = \"maya-mbp\", addrs = [\"127.0.0.1:7461\"] }}]\n\
         [[room]]\nname = \"team\"\npeople = [\"sam\", \"maya\"]\n",
        key(1),
        key(3)
    );
    let parsed = file::parse(&text).unwrap();
    let (file_me, roster) = parsed.roster_for(Some("sam"), &key(1), T0).unwrap();
    let change = active.replace(file_me, roster);
    assert_eq!(change.added, vec![key(3)]);
    let maya = active.admission().get(&key(3)).unwrap();
    assert_eq!(maya.addrs, vec!["127.0.0.1:7461".parse().unwrap()]);
    assert!(maya.rooms.contains("team"));
}

#[tokio::test]
async fn refresh_against_spew_handles_304_and_401() {
    let mock = MockSpew::start().await;
    mock.set_peers(peers_json("v1", 60), "\"v1\"");
    let dir = tempfile::tempdir().unwrap();
    let clock = TestClock::at(T0);
    let api = mock.api(dir.path(), clock.clone());
    let (result, _) = sign_in_with(&api, vec![], Duration::from_secs(10)).await;
    let signed = result.unwrap();
    let mut cache = SpewRosterCache::new(signed.me);

    assert_eq!(
        refresh(&mut cache, &api, &key(1), "sam-mbp").await,
        RefreshOutcome::Updated
    );
    clock.advance_secs(60);
    assert_eq!(
        refresh(&mut cache, &api, &key(1), "sam-mbp").await,
        RefreshOutcome::Unchanged
    );
    assert_eq!(cache.fetched_at(), Some(T0 + 60_000));
    mock.with(|s| assert_eq!(s.if_none_match, vec![None, Some("\"v1\"".into())]));

    // spew refuses the token and a refresh doesn't help: teammates drop at once.
    mock.with(|s| s.reject_all_bearer = true);
    assert_eq!(
        refresh(&mut cache, &api, &key(1), "sam-mbp").await,
        RefreshOutcome::SignedOut
    );
    let view = cache.view(&key(1), "sam-mbp", clock_now(&clock), DAY);
    assert_eq!(view.state, SpewRosterState::SignedOut);
    let a = Admission::compute(cache.me(), &key(1), &view.roster);
    assert!(!a.is_admitted(&key(3)) && a.is_admitted(&key(2)));

    // An unusable roster (ttl_secs 0): the last good one stays.
    mock.with(|s| s.reject_all_bearer = false);
    mock.set_peers(peers_json("v2", 0), "\"v2\"");
    assert!(matches!(
        refresh(&mut cache, &api, &key(1), "sam-mbp").await,
        RefreshOutcome::Failed(_)
    ));
    assert!(
        cache.is_signed_out(),
        "an unusable roster doesn't end signed-out"
    );
    mock.set_peers(peers_json("v3", 60), "\"v3\"");
    assert_eq!(
        refresh(&mut cache, &api, &key(1), "sam-mbp").await,
        RefreshOutcome::Updated
    );
    assert_eq!(
        cache.view(&key(1), "sam-mbp", clock_now(&clock), DAY).state,
        SpewRosterState::Fresh
    );
}

fn clock_now(clock: &TestClock) -> u64 {
    use spew_mesh::spew_api::Clock;
    clock.now_ms()
}
