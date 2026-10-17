//! Who may ask a responder, how often, and how many may wait (9). Plain components with an injected clock;
//! the daemon applies them when it accepts a question (5.5 step 6).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::{Asker, ServeFrom, ServeSettings};

/// Most questions that may wait behind the running one.
pub const MAX_WAITING: usize = 8;

/// The largest run limit a responder may announce, the same bound settings.toml has (11).
pub const MAX_RUN_LIMIT: u32 = 10_000;

/// The part of `[serve]` the daemon applies when a question for a responder arrives (9, 5.5 step 6).
///
/// A responder sends the values it read at its start in `session.open`, so what `spew mesh serve` reports is
/// what the daemon enforces even when settings.toml changed after the daemon started.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateSettings {
    /// Who may ask: `room` or `own`.
    #[serde(serialize_with = "serialize_from")]
    pub from: ServeFrom,
    /// Runs per asking person per rolling hour; asks from your own devices don't count here.
    pub max_runs_per_person_per_hour: u32,
    /// Runs per rolling hour in all.
    pub max_runs_per_hour: u32,
}

// `ServeFrom` reads `room` and `own` from settings.toml; the control protocol writes the same words.
fn serialize_from<S: serde::Serializer>(from: &ServeFrom, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(match from {
        ServeFrom::Room => "room",
        ServeFrom::Own => "own",
    })
}

impl GateSettings {
    /// Refuses limits above [`MAX_RUN_LIMIT`]. Zero is allowed and refuses every question, as in settings.toml.
    pub fn validate(&self) -> Result<(), String> {
        if self.max_runs_per_person_per_hour > MAX_RUN_LIMIT
            || self.max_runs_per_hour > MAX_RUN_LIMIT
        {
            return Err(format!(
                "serve run limits are at most {MAX_RUN_LIMIT} per hour"
            ));
        }
        Ok(())
    }
}

impl From<&ServeSettings> for GateSettings {
    fn from(s: &ServeSettings) -> Self {
        GateSettings {
            from: s.from,
            max_runs_per_person_per_hour: s.max_runs_per_person_per_hour,
            max_runs_per_hour: s.max_runs_per_hour,
        }
    }
}

/// The run limits' rolling window.
pub const WINDOW: Duration = Duration::from_secs(3600);

/// A source of wall-clock time in milliseconds since the Unix epoch.
pub trait Clock: Send + Sync {
    /// Now, in milliseconds since the epoch.
    fn now_ms(&self) -> u64;
}

/// The system clock.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }
}

/// A clock tests move by hand.
#[derive(Debug, Default)]
pub struct ManualClock(AtomicU64);

impl ManualClock {
    /// A clock reading `ms`.
    pub fn new(ms: u64) -> Self {
        ManualClock(AtomicU64::new(ms))
    }

    /// Moves the clock forward.
    pub fn advance(&self, by: Duration) {
        let ms = u64::try_from(by.as_millis()).unwrap_or(u64::MAX);
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

impl<C: Clock + ?Sized> Clock for std::sync::Arc<C> {
    fn now_ms(&self) -> u64 {
        (**self).now_ms()
    }
}

/// Why a question is refused, with the refusal code from 5.5.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// Over a run limit or the queue is full: `busy`.
    Busy,
    /// The responder doesn't take questions from this asker: `no-such-session`, so existence doesn't leak.
    NoSuchSession,
}

impl Refusal {
    /// The refusal code sent back to the asker's device.
    pub fn code(self) -> &'static str {
        match self {
            Refusal::Busy => "busy",
            Refusal::NoSuchSession => "no-such-session",
        }
    }
}

/// `[serve] from`: own devices always; with `room`, also anyone admitted who shares the responder's room.
pub fn may_ask(from: ServeFrom, asker: &Asker, shares_room: bool) -> Result<(), Refusal> {
    match from {
        _ if asker.own => Ok(()),
        ServeFrom::Room if shares_room => Ok(()),
        _ => Err(Refusal::NoSuchSession),
    }
}

/// `max_runs_per_person_per_hour` and `max_runs_per_hour` over a rolling hour. Asks from the person's own
/// devices count only toward the total.
#[derive(Clone, Debug)]
pub struct RunLimits {
    per_person: u32,
    total: u32,
    runs: VecDeque<(u64, Option<String>)>,
}

impl RunLimits {
    /// Limits from the settings.
    pub fn new(settings: &GateSettings) -> Self {
        RunLimits {
            per_person: settings.max_runs_per_person_per_hour,
            total: settings.max_runs_per_hour,
            runs: VecDeque::new(),
        }
    }

    /// New caps; the runs already counted stay counted.
    pub fn set_caps(&mut self, settings: &GateSettings) {
        self.per_person = settings.max_runs_per_person_per_hour;
        self.total = settings.max_runs_per_hour;
    }

    /// Whether one more run for this asker fits, without counting it.
    pub fn check(&mut self, now_ms: u64, asker: &Asker) -> Result<(), Refusal> {
        self.expire(now_ms);
        if self.runs.len() >= self.total as usize {
            return Err(Refusal::Busy);
        }
        if !asker.own {
            let theirs = self
                .runs
                .iter()
                .filter(|(_, p)| p.as_deref() == Some(asker.person.as_str()))
                .count();
            if theirs >= self.per_person as usize {
                return Err(Refusal::Busy);
            }
        }
        Ok(())
    }

    /// Counts one run for this asker when it fits; otherwise refuses with `busy` and counts nothing.
    pub fn admit(&mut self, now_ms: u64, asker: &Asker) -> Result<(), Refusal> {
        self.check(now_ms, asker)?;
        self.record(now_ms, asker);
        Ok(())
    }

    /// Counts one run for this asker without checking.
    pub fn record(&mut self, now_ms: u64, asker: &Asker) {
        let person = (!asker.own).then(|| asker.person.clone());
        self.runs.push_back((now_ms, person));
    }

    /// Runs counted in the last hour.
    pub fn total_in_window(&mut self, now_ms: u64) -> usize {
        self.expire(now_ms);
        self.runs.len()
    }

    fn expire(&mut self, now_ms: u64) {
        let window = u64::try_from(WINDOW.as_millis()).unwrap_or(u64::MAX);
        // A run stamped in the future (the clock went back) stays counted: the conservative choice.
        self.runs
            .retain(|(at, _)| *at > now_ms || now_ms - *at < window);
    }
}

/// One run at a time, up to [`MAX_WAITING`] waiting behind it.
#[derive(Clone, Debug)]
pub struct ServeQueue<T> {
    waiting: VecDeque<T>,
    running: bool,
    max_waiting: usize,
}

impl<T> Default for ServeQueue<T> {
    fn default() -> Self {
        ServeQueue::new()
    }
}

impl<T> ServeQueue<T> {
    /// An empty queue holding at most [`MAX_WAITING`].
    pub fn new() -> Self {
        ServeQueue {
            waiting: VecDeque::new(),
            running: false,
            max_waiting: MAX_WAITING,
        }
    }

    /// Whether one more question would fit.
    pub fn has_room(&self) -> bool {
        self.waiting.len() < self.max_waiting
    }

    /// Adds a question, or refuses with `busy` when [`MAX_WAITING`] already wait.
    pub fn push(&mut self, item: T) -> Result<(), (Refusal, T)> {
        if !self.has_room() {
            return Err((Refusal::Busy, item));
        }
        self.waiting.push_back(item);
        Ok(())
    }

    /// The next question to run, when nothing is running.
    pub fn start_next(&mut self) -> Option<T> {
        if self.running {
            return None;
        }
        let next = self.waiting.pop_front()?;
        self.running = true;
        Some(next)
    }

    /// Marks the running question done.
    pub fn finish(&mut self) {
        self.running = false;
    }

    /// Questions waiting (presence's `queue`).
    pub fn waiting(&self) -> usize {
        self.waiting.len()
    }

    /// Whether a run is in progress (presence's busy or idle).
    pub fn is_running(&self) -> bool {
        self.running
    }
}

/// The checks a responder applies to a new question, in order: who may ask, queue room, run limits.
#[derive(Clone, Debug)]
pub struct Gate {
    from: ServeFrom,
    limits: RunLimits,
}

impl Gate {
    /// A gate from the settings.
    pub fn new(settings: &GateSettings) -> Self {
        Gate {
            from: settings.from,
            limits: RunLimits::new(settings),
        }
    }

    /// Takes new settings (a responder re-opened its session); runs already counted stay counted.
    pub fn update(&mut self, settings: &GateSettings) {
        self.from = settings.from;
        self.limits.set_caps(settings);
    }

    /// Says whether a question may be stored, without counting it. `queue_has_room` is false when
    /// [`MAX_WAITING`] questions already wait. [`Gate::record`] counts it once it is stored, so a question a
    /// later check refuses uses up nothing.
    pub fn check(
        &mut self,
        now_ms: u64,
        asker: &Asker,
        shares_room: bool,
        queue_has_room: bool,
    ) -> Result<(), Refusal> {
        may_ask(self.from, asker, shares_room)?;
        if !queue_has_room {
            return Err(Refusal::Busy);
        }
        self.limits.check(now_ms, asker)
    }

    /// Counts the run of a question that was stored.
    pub fn record(&mut self, now_ms: u64, asker: &Asker) {
        self.limits.record(now_ms, asker);
    }

    /// [`Gate::check`], then [`Gate::record`] when it passes.
    pub fn admit(
        &mut self,
        now_ms: u64,
        asker: &Asker,
        shares_room: bool,
        queue_has_room: bool,
    ) -> Result<(), Refusal> {
        self.check(now_ms, asker, shares_room, queue_has_room)?;
        self.record(now_ms, asker);
        Ok(())
    }

    /// The run limits, for presence and `doctor`.
    pub fn limits(&mut self) -> &mut RunLimits {
        &mut self.limits
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asker(person: &str, own: bool) -> Asker {
        Asker {
            person: person.into(),
            device: format!("{person}-mbp"),
            device_id: format!("{person}-id"),
            session: "01J9ZK0000000000000000000A".into(),
            name: "claude-api".into(),
            client: "claude".into(),
            own,
            room: Some("spew".into()),
        }
    }

    fn gate_settings(s: &ServeSettings) -> GateSettings {
        GateSettings::from(s)
    }

    #[test]
    fn per_person_limit_and_window() {
        let clock = ManualClock::new(1_000_000);
        let mut limits = RunLimits::new(&gate_settings(&ServeSettings::default()));
        let maya = asker("maya", false);
        for _ in 0..10 {
            limits.admit(clock.now_ms(), &maya).unwrap();
            clock.advance(Duration::from_secs(60));
        }
        assert_eq!(limits.admit(clock.now_ms(), &maya), Err(Refusal::Busy));
        // Someone else still fits.
        assert!(limits.admit(clock.now_ms(), &asker("lee", false)).is_ok());
        // The first run leaves the window an hour after it started.
        clock.advance(Duration::from_secs(3600 - 600));
        assert!(limits.admit(clock.now_ms(), &maya).is_ok());
        assert_eq!(limits.admit(clock.now_ms(), &maya), Err(Refusal::Busy));
    }

    #[test]
    fn total_limit_counts_everyone() {
        let clock = ManualClock::new(5);
        let settings = ServeSettings {
            max_runs_per_hour: 3,
            ..Default::default()
        };
        let mut limits = RunLimits::new(&gate_settings(&settings));
        limits.admit(clock.now_ms(), &asker("maya", false)).unwrap();
        limits.admit(clock.now_ms(), &asker("sam", true)).unwrap();
        limits.admit(clock.now_ms(), &asker("lee", false)).unwrap();
        assert_eq!(
            limits.admit(clock.now_ms(), &asker("sam", true)),
            Err(Refusal::Busy)
        );
        assert_eq!(limits.total_in_window(clock.now_ms()), 3);
        clock.advance(WINDOW);
        assert_eq!(limits.total_in_window(clock.now_ms()), 0);
    }

    #[test]
    fn own_devices_count_only_toward_the_total() {
        let clock = ManualClock::new(0);
        let settings = ServeSettings {
            max_runs_per_person_per_hour: 1,
            ..Default::default()
        };
        let mut limits = RunLimits::new(&gate_settings(&settings));
        let sam = asker("sam", true);
        for _ in 0..5 {
            limits.admit(clock.now_ms(), &sam).unwrap();
        }
        assert_eq!(limits.total_in_window(clock.now_ms()), 5);
        // Own runs don't use up the per-person allowance of a person by that name.
        assert!(limits.admit(clock.now_ms(), &asker("sam", false)).is_ok());
        assert_eq!(
            limits.admit(clock.now_ms(), &asker("sam", false)),
            Err(Refusal::Busy)
        );
    }

    #[test]
    fn a_refused_run_is_not_counted() {
        let settings = ServeSettings {
            max_runs_per_person_per_hour: 1,
            max_runs_per_hour: 2,
            ..Default::default()
        };
        let mut limits = RunLimits::new(&gate_settings(&settings));
        let maya = asker("maya", false);
        limits.admit(0, &maya).unwrap();
        for _ in 0..5 {
            assert_eq!(limits.admit(0, &maya), Err(Refusal::Busy));
        }
        assert_eq!(limits.total_in_window(0), 1);
    }

    #[test]
    fn who_may_ask() {
        let maya = asker("maya", false);
        let sam = asker("sam", true);
        assert_eq!(may_ask(ServeFrom::Room, &maya, true), Ok(()));
        assert_eq!(
            may_ask(ServeFrom::Room, &maya, false),
            Err(Refusal::NoSuchSession)
        );
        assert_eq!(
            may_ask(ServeFrom::Own, &maya, true),
            Err(Refusal::NoSuchSession)
        );
        assert_eq!(may_ask(ServeFrom::Own, &sam, false), Ok(()));
        assert_eq!(Refusal::NoSuchSession.code(), "no-such-session");
        assert_eq!(Refusal::Busy.code(), "busy");
    }

    #[test]
    fn queue_holds_one_running_and_eight_waiting() {
        let mut q = ServeQueue::new();
        for i in 0..MAX_WAITING {
            q.push(i).unwrap();
        }
        assert_eq!(q.push(99), Err((Refusal::Busy, 99)));
        assert_eq!(q.start_next(), Some(0));
        assert!(q.is_running());
        assert_eq!(q.start_next(), None, "one run at a time");
        assert_eq!(q.waiting(), 7);
        q.push(8).unwrap();
        q.finish();
        assert_eq!(q.start_next(), Some(1));
    }

    #[test]
    fn gate_checks_in_order() {
        let settings = ServeSettings {
            from: ServeFrom::Own,
            max_runs_per_hour: 1,
            ..Default::default()
        };
        let mut gate = Gate::new(&gate_settings(&settings));
        assert_eq!(
            gate.admit(0, &asker("maya", false), true, true),
            Err(Refusal::NoSuchSession)
        );
        assert_eq!(
            gate.admit(0, &asker("sam", true), true, false),
            Err(Refusal::Busy)
        );
        assert_eq!(gate.limits().total_in_window(0), 0);
        assert_eq!(gate.admit(0, &asker("sam", true), false, true), Ok(()));
        assert_eq!(
            gate.admit(0, &asker("sam", true), false, true),
            Err(Refusal::Busy)
        );
    }

    #[test]
    fn check_counts_nothing_until_recorded() {
        let settings = ServeSettings {
            max_runs_per_person_per_hour: 1,
            ..Default::default()
        };
        let mut gate = Gate::new(&gate_settings(&settings));
        let maya = asker("maya", false);
        for _ in 0..3 {
            assert_eq!(gate.check(0, &maya, true, true), Ok(()));
        }
        gate.record(0, &maya);
        assert_eq!(gate.check(0, &maya, true, true), Err(Refusal::Busy));
    }

    #[test]
    fn an_update_keeps_the_count_and_takes_the_new_rules() {
        let mut gate = Gate::new(&gate_settings(&ServeSettings::default()));
        let maya = asker("maya", false);
        gate.admit(0, &maya, true, true).unwrap();
        gate.admit(0, &maya, true, true).unwrap();
        let tighter = GateSettings {
            from: ServeFrom::Room,
            max_runs_per_person_per_hour: 2,
            max_runs_per_hour: 30,
        };
        gate.update(&tighter);
        assert_eq!(gate.check(0, &maya, true, true), Err(Refusal::Busy));
        gate.update(&GateSettings {
            from: ServeFrom::Own,
            ..tighter
        });
        assert_eq!(
            gate.check(0, &asker("lee", false), true, true),
            Err(Refusal::NoSuchSession)
        );
        // Zero refuses everyone, as `max_runs_per_hour = 0` in settings.toml does.
        gate.update(&GateSettings {
            from: ServeFrom::Room,
            max_runs_per_person_per_hour: 10,
            max_runs_per_hour: 0,
        });
        assert_eq!(
            gate.check(0, &asker("sam", true), true, true),
            Err(Refusal::Busy)
        );
    }

    #[test]
    fn gate_settings_travel_as_json() {
        let s = GateSettings {
            from: ServeFrom::Own,
            max_runs_per_person_per_hour: 2,
            max_runs_per_hour: 5,
        };
        let json = serde_json::to_value(s).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"from": "own", "max_runs_per_person_per_hour": 2, "max_runs_per_hour": 5})
        );
        assert_eq!(serde_json::from_value::<GateSettings>(json).unwrap(), s);
        let room: GateSettings = serde_json::from_value(
            serde_json::json!({"from": "room", "max_runs_per_person_per_hour": 1, "max_runs_per_hour": 1}),
        )
        .unwrap();
        assert_eq!(room.from, ServeFrom::Room);
        for bad in [
            serde_json::json!({"from": "anyone", "max_runs_per_person_per_hour": 1, "max_runs_per_hour": 1}),
            serde_json::json!({"from": "own", "max_runs_per_person_per_hour": 1}),
            serde_json::json!({"from": "own", "max_runs_per_person_per_hour": 1, "max_runs_per_hour": 1, "x": 1}),
            serde_json::json!({"from": "own", "max_runs_per_person_per_hour": -1, "max_runs_per_hour": 1}),
        ] {
            assert!(
                serde_json::from_value::<GateSettings>(bad.clone()).is_err(),
                "{bad}"
            );
        }
        assert!(
            GateSettings {
                max_runs_per_hour: MAX_RUN_LIMIT + 1,
                ..s
            }
            .validate()
            .is_err()
        );
        assert!(s.validate().is_ok());
    }
}
