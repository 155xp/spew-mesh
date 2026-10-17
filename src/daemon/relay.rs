//! Reporting the home relay to spew (5.1, 14.3): in `n0` and `own` modes, when it changes, at most once a
//! minute, and only while signed in.

use iroh::RelayUrl;

use crate::spew_api::{AuthStatus, RELAY_REPORT_INTERVAL_MS, RelayReporter, SpewApi};

/// What one look at the home relay did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportOutcome {
    /// spew took the new relay.
    Sent,
    /// Nothing to send now: unchanged, too soon after the last attempt, or no relay yet.
    NotDue,
    /// This device isn't signed in to spew.
    NotSignedIn,
    /// spew refused it or couldn't be reached; it is tried again a minute later.
    Failed(String),
}

/// The reporting state of one daemon.
#[derive(Debug, Default)]
pub struct RelayReport {
    reporter: RelayReporter,
    last_attempt: Option<u64>,
    reported_any: bool,
    signed_in_as: Option<String>,
}

impl RelayReport {
    /// Sends `relay` when it is due. Failures count against the once-a-minute limit too, so spew is never asked
    /// more often than that. spew learns of no relay until the first one comes: a fresh registration has none.
    pub async fn step(
        &mut self,
        api: &SpewApi,
        relay: Option<&RelayUrl>,
        now: u64,
    ) -> ReportOutcome {
        let person = match api.auth_status() {
            Ok(AuthStatus::SignedIn(me)) => me.username,
            _ => return ReportOutcome::NotSignedIn,
        };
        // A sign-in as someone else is a new device record at spew.
        if self.signed_in_as.as_deref() != Some(person.as_str()) {
            *self = RelayReport {
                signed_in_as: Some(person),
                ..RelayReport::default()
            };
        }
        if relay.is_none() && !self.reported_any {
            return ReportOutcome::NotDue;
        }
        let recent = self
            .last_attempt
            .is_some_and(|t| now.saturating_sub(t) < RELAY_REPORT_INTERVAL_MS);
        if recent || !self.reporter.due(relay, now) {
            return ReportOutcome::NotDue;
        }
        self.last_attempt = Some(now);
        match api.update_self(relay, None).await {
            Ok(()) => {
                self.reporter.sent(relay, now);
                self.reported_any = true;
                ReportOutcome::Sent
            }
            Err(e) => ReportOutcome::Failed(e.to_string()),
        }
    }
}
