//! Picking and keeping the roster in force (4.2): the local roster, a roster file re-read when it changes, or
//! the spew roster refreshed on a timer and cached in the store.

use std::path::PathBuf;

use iroh::EndpointId;

use crate::courier::RosterInfo;
use crate::roster::file::{self as roster_file, FileWatch};
use crate::roster::local::{local_roster, local_username, os_user_name};
use crate::roster::spew::{self as spew_roster, RefreshOutcome, SpewRosterCache, SpewRosterState};
use crate::roster::{Me, Roster};
use crate::settings::{RosterKind, Settings};
use crate::spew_api::{AuthStatus, SpewApi};
use crate::store::{CachedRoster, RosterProvider};

/// The provider names `daemon.status` and `doctor` show.
pub fn kind_name(kind: RosterKind) -> &'static str {
    match kind {
        RosterKind::Local => "local",
        RosterKind::File => "file",
        RosterKind::Spew => "spew",
    }
}

/// The roster in force, with who I am and how it was chosen.
#[derive(Debug, Clone)]
pub struct Chosen {
    /// Who I am.
    pub me: Me,
    /// The roster.
    pub roster: Roster,
    /// The provider's state.
    pub info: RosterInfo,
}

/// Chooses the provider and keeps its roster current.
pub struct RosterManager {
    kind: RosterKind,
    username: Option<String>,
    file: FileWatch,
    max_stale_seconds: u64,
    refresh_ms: u64,
    device: EndpointId,
    device_name: String,
    api: Option<SpewApi>,
    api_problem: Option<String>,
    cache: Option<SpewRosterCache>,
    stored_cache: Option<String>,
    last_refresh: Option<u64>,
}

impl RosterManager {
    /// A manager for these settings. `api` is `None` when the spew URL can't be used, with `api_problem` saying
    /// why; `stored_cache` is the spew roster cache from the store.
    pub fn new(
        settings: &Settings,
        device: EndpointId,
        device_name: &str,
        api: Result<SpewApi, String>,
        stored_cache: Option<CachedRoster>,
    ) -> RosterManager {
        let (api, api_problem) = match api {
            Ok(api) => (Some(api), None),
            Err(e) => (None, Some(e)),
        };
        RosterManager {
            kind: settings.roster.kind,
            username: settings.username.clone(),
            file: FileWatch::new(settings.roster.file.clone()),
            max_stale_seconds: settings.spew.max_stale_seconds,
            refresh_ms: settings.spew.refresh_seconds.max(1).saturating_mul(1000),
            device,
            device_name: device_name.to_string(),
            api,
            api_problem,
            cache: None,
            stored_cache: stored_cache
                .filter(|c| c.provider == RosterProvider::Spew)
                .map(|c| c.roster),
            last_refresh: None,
        }
    }

    /// The spew API client, when the spew URL is usable.
    pub fn api(&self) -> Option<&SpewApi> {
        self.api.as_ref()
    }

    /// The roster file this manager watches.
    pub fn file_path(&self) -> PathBuf {
        self.file.path().to_path_buf()
    }

    fn local(&self, now: u64, problem: Option<String>) -> Chosen {
        let username = local_username(self.username.as_deref(), os_user_name().as_deref())
            .unwrap_or_else(|_| "local".to_string());
        let me = Me {
            display: username.clone(),
            username,
        };
        let roster = match local_roster(&me, &self.device, &self.device_name, now) {
            Ok(r) => r,
            Err(e) => {
                // The local roster only fails on a broken device name, which identity rules out.
                tracing::warn!(error = %e.0, "the local roster is invalid");
                Roster {
                    version: crate::roster::local::LOCAL_VERSION.to_string(),
                    fetched_at: now,
                    rooms: Vec::new(),
                }
            }
        };
        Chosen {
            me,
            roster,
            info: RosterInfo {
                provider: "local".into(),
                configured: kind_name(self.kind).into(),
                state: "only this device".into(),
                problem,
            },
        }
    }

    fn file_roster(&self, now: u64) -> Chosen {
        let path = self.file.path();
        let loaded = roster_file::load(path)
            .and_then(|f| f.roster_for(self.username.as_deref(), &self.device, now));
        match loaded {
            Ok((me, roster)) => {
                let state = format!(
                    "roster file {}, version {}",
                    path.display(),
                    short(&roster.version)
                );
                Chosen {
                    me,
                    roster,
                    info: RosterInfo {
                        provider: "file".into(),
                        configured: "file".into(),
                        state,
                        problem: None,
                    },
                }
            }
            Err(e) => self.local(now, Some(e.to_string())),
        }
    }

    fn cache_for(&mut self, me: &Me) -> &mut SpewRosterCache {
        let fits = self
            .cache
            .as_ref()
            .is_some_and(|c| c.me().username == me.username);
        if !fits {
            let cache = match &self.stored_cache {
                Some(json) => SpewRosterCache::restore(json, me.clone()),
                None => SpewRosterCache::new(me.clone()),
            };
            self.cache = Some(cache);
            self.last_refresh = None;
        }
        self.cache
            .get_or_insert_with(|| SpewRosterCache::new(me.clone()))
    }

    // The signed-in person, when there is one: (me, signed out).
    fn spew_person(&self) -> Result<Option<(Me, bool)>, String> {
        let Some(api) = &self.api else {
            return Err(self
                .api_problem
                .clone()
                .unwrap_or_else(|| "the spew URL can't be used".into()));
        };
        match api.auth_status() {
            Ok(AuthStatus::SignedIn(me)) => Ok(Some((me, false))),
            Ok(AuthStatus::SignedOut(me)) => Ok(Some((me, true))),
            Ok(AuthStatus::NotSignedIn) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn spew_view(&mut self, now: u64, me: Me, signed_out: bool) -> Chosen {
        let device = self.device;
        let name = self.device_name.clone();
        let max_stale = self.max_stale_seconds;
        let cache = self.cache_for(&me);
        if signed_out {
            cache.mark_signed_out();
        }
        let view = cache.view(&device, &name, now, max_stale);
        let problem = (view.state == SpewRosterState::SignedOut)
            .then(|| "signed out of spew; run `spew mesh login`".to_string());
        Chosen {
            me: cache.me().clone(),
            roster: view.roster.clone(),
            info: RosterInfo {
                provider: "spew".into(),
                configured: kind_name(self.kind).into(),
                state: view.describe(),
                problem,
            },
        }
    }

    /// The roster in force now, from what is on disk and cached (no network). A `local` setting moves to the
    /// spew roster once this device is signed in (4.2).
    pub fn current(&mut self, now: u64) -> Chosen {
        if self.kind == RosterKind::File {
            return self.file_roster(now);
        }
        match self.spew_person() {
            Ok(Some((me, signed_out))) => self.spew_view(now, me, signed_out),
            Ok(None) if self.kind == RosterKind::Spew => self.local(
                now,
                Some("not signed in to spew; run `spew mesh login`".into()),
            ),
            Ok(None) => self.local(now, None),
            Err(e) if self.kind == RosterKind::Spew => self.local(now, Some(e)),
            Err(e) => {
                tracing::debug!(error = %e, "can't read the spew sign-in");
                self.local(now, None)
            }
        }
    }

    /// Whether the spew roster may be in force, whose view changes with time and with the sign-in.
    pub fn uses_spew(&self) -> bool {
        self.kind != RosterKind::File
    }

    /// Whether the roster file changed since the last look.
    pub fn file_changed(&mut self) -> bool {
        self.kind == RosterKind::File && self.file.changed()
    }

    /// Whether a spew refresh is due at `now`.
    pub fn refresh_due(&self, now: u64) -> bool {
        self.last_refresh
            .is_none_or(|t| now.saturating_sub(t) >= self.refresh_ms)
    }

    /// Asks spew for the roster when it is the provider in force (4.2, 14.4). Returns the cache to store when
    /// it changed.
    pub async fn refresh_spew(&mut self, now: u64) -> Option<CachedRoster> {
        if self.kind == RosterKind::File {
            return None;
        }
        let (me, _) = self.spew_person().ok().flatten()?;
        let api = self.api.clone()?;
        let device = self.device;
        let name = self.device_name.clone();
        self.last_refresh = Some(now);
        let cache = self.cache_for(&me);
        let outcome = spew_roster::refresh(cache, &api, &device, &name).await;
        match &outcome {
            RefreshOutcome::Failed(e) => tracing::info!(error = %e, "spew roster refresh failed"),
            RefreshOutcome::SignedOut => {
                tracing::warn!("spew refused the sign-in; run `spew mesh login`")
            }
            RefreshOutcome::Updated | RefreshOutcome::Unchanged => {}
        }
        let json = cache.to_json();
        let fetched_at = cache.fetched_at().unwrap_or(0);
        let version = cache.peers().map(|p| p.version.clone());
        self.stored_cache = Some(json.clone());
        Some(CachedRoster {
            provider: RosterProvider::Spew,
            version,
            etag: None,
            roster: json,
            fetched_at,
        })
    }

    /// Whether the spew roster lists this device among my own; `None` before spew sent a roster.
    pub fn spew_lists_this_device(&self) -> Option<bool> {
        let peers = self.cache.as_ref()?.peers()?;
        let hex = self.device.to_string();
        Some(peers.me.as_ref().is_some_and(|me| {
            me.devices
                .iter()
                .any(|d| d.endpoint_id.eq_ignore_ascii_case(&hex))
        }))
    }

    /// Forgets the spew cache, after a sign-out or a sign-in as someone else.
    pub fn reset_spew(&mut self) {
        self.cache = None;
        self.stored_cache = None;
        self.last_refresh = None;
    }
}

fn short(version: &str) -> &str {
    version.get(..12).unwrap_or(version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Home;

    fn key(n: u8) -> EndpointId {
        iroh::SecretKey::from_bytes(&[n; 32]).public()
    }

    #[test]
    fn a_file_roster_falls_back_to_local_with_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path()).unwrap();
        let mut settings = Settings::defaults(&home);
        settings.roster.kind = RosterKind::File;
        settings.username = Some("sam".into());
        let me = key(1);
        let mut mgr = RosterManager::new(&settings, me, "sam-mac", Err("no spew".into()), None);
        let chosen = mgr.current(1000);
        assert_eq!(chosen.info.provider, "local");
        assert_eq!(chosen.info.configured, "file");
        assert!(chosen.info.problem.is_some());
        let text = format!(
            "[[person]]\nname = \"sam\"\ndevices = [ {{ id = \"{}\", name = \"sam-mac\" }} ]\n\n\
             [[person]]\nname = \"maya\"\ndevices = [ {{ id = \"{}\", name = \"maya-mbp\" }} ]\n\n\
             [[room]]\nname = \"team\"\npeople = [\"sam\", \"maya\"]\n",
            me,
            key(2)
        );
        std::fs::write(&settings.roster.file, text).unwrap();
        assert!(mgr.file_changed());
        let chosen = mgr.current(2000);
        assert_eq!(chosen.info.provider, "file");
        assert_eq!(chosen.me.username, "sam");
        assert!(chosen.roster.rooms.iter().any(|r| r.id == "team"));
        assert!(!mgr.file_changed());
    }

    #[test]
    fn local_without_a_sign_in_stays_local() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path()).unwrap();
        let mut settings = Settings::defaults(&home);
        settings.username = Some("sam".into());
        let mut mgr = RosterManager::new(&settings, key(1), "sam-mac", Err("no spew".into()), None);
        let chosen = mgr.current(1000);
        assert_eq!(chosen.info.provider, "local");
        assert_eq!(chosen.info.problem, None);
        assert_eq!(chosen.me.username, "sam");
    }
}
