//! Service Monitor's domain model.
//!
//! The rebuild contract (Project Guides, `02_Service_Monitor_Rebuild`) lives here as types and a
//! pure state machine — no I/O, no async, so every transition can be tested deterministically:
//!
//! - safe authorized `GET`/`HEAD` probes only, one attempt per slot, no overlap;
//! - three consecutive failures open an incident; two fresh passes confirm recovery;
//! - missing checks reset streaks and display `Unknown` — an open incident still persists until
//!   new evidence resolves it;
//! - a timeout, an HTTP failure and a missed observation are different things.
//!
//! Status semantics here are the first cut (decision D-005 in `docs/DECISIONS.md`): they get
//! sharpened when the coordinator and the public status renderer land.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;
use uuid::Uuid;

/// Identifies one monitor. `Uuid` v7, so ids sort by creation time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MonitorId(pub Uuid);

impl MonitorId {
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for MonitorId {
    fn default() -> Self {
        Self::new()
    }
}

/// The smallest interval the contract allows; the lab profile runs at 60 seconds.
pub const MIN_INTERVAL: Duration = Duration::from_secs(15);
/// The default probe interval from the guide's probe contract.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);
/// The default total deadline for one probe; the whole attempt must fit in it.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// A registered target to probe.
#[derive(Debug, Clone)]
pub struct Monitor {
    pub id: MonitorId,
    pub name: String,
    /// The health URL. Only `http`/`https` survive validation — probes are safe `GET`/`HEAD`.
    pub url: Url,
    pub interval: Duration,
    /// Total deadline for one probe, always `<= interval`.
    pub timeout: Duration,
    /// Bumped on every edit or pause; a probe result carrying an older generation is stale and
    /// must not update current status (the "edit/pause during a probe" failure case).
    pub generation: u64,
    /// Paused means *not monitored*: no slots are scheduled and status is not reported.
    pub paused: bool,
}

/// Everything needed to register a monitor, validated once at the boundary ("parse, don't
/// validate"): after `NewMonitor::try_new` the rest of the system never re-checks these rules.
#[derive(Debug, Clone)]
pub struct NewMonitor {
    pub name: String,
    pub url: Url,
    pub interval: Duration,
    pub timeout: Duration,
}

#[derive(Debug, Error)]
pub enum ValidationError {
    #[error("monitor name is empty")]
    EmptyName,
    #[error("monitor URL must be http or https, got `{scheme}`")]
    UrlNotHttp { scheme: String },
    #[error("interval {interval:?} is below the minimum {MIN_INTERVAL:?}")]
    IntervalBelowMinimum { interval: Duration },
    #[error("timeout {timeout:?} exceeds interval {interval:?}")]
    TimeoutExceedsInterval {
        timeout: Duration,
        interval: Duration,
    },
}

impl NewMonitor {
    /// Defaults: 60-second interval, 5-second deadline, per the probe contract.
    pub fn new(name: impl Into<String>, url: Url) -> Result<Self, ValidationError> {
        Self::with_schedule(name, url, DEFAULT_INTERVAL, DEFAULT_TIMEOUT)
    }

    pub fn with_schedule(
        name: impl Into<String>,
        url: Url,
        interval: Duration,
        timeout: Duration,
    ) -> Result<Self, ValidationError> {
        let name = name.into();
        if name.trim().is_empty() {
            return Err(ValidationError::EmptyName);
        }
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(ValidationError::UrlNotHttp {
                scheme: url.scheme().to_owned(),
            });
        }
        if interval < MIN_INTERVAL {
            return Err(ValidationError::IntervalBelowMinimum { interval });
        }
        if timeout > interval {
            return Err(ValidationError::TimeoutExceedsInterval { timeout, interval });
        }
        Ok(Self {
            name,
            url,
            interval,
            timeout,
        })
    }

    #[must_use]
    pub fn into_monitor(self) -> Monitor {
        Monitor {
            id: MonitorId::new(),
            name: self.name,
            url: self.url,
            interval: self.interval,
            timeout: self.timeout,
            generation: 1,
            paused: false,
        }
    }
}

/// What one probe produced. These variants are kept distinct end to end because the contract
/// treats them differently: an HTTP failure is evidence about the target, a timeout is evidence
/// about *this observation*, and a missed slot is no evidence at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckOutcome {
    /// The target answered with an acceptable status inside the deadline.
    Pass,
    /// The target answered, with a status the contract counts as failing.
    HttpFail { status: u16 },
    /// No usable response: DNS, connect, TLS or body-read failure.
    Transport,
    /// The probe exceeded its total deadline.
    Timeout,
}

impl CheckOutcome {
    /// Only `Pass` is a pass; every completed non-pass probe is one failure for streak purposes.
    #[must_use]
    pub fn is_pass(self) -> bool {
        matches!(self, Self::Pass)
    }
}

/// What one scheduled slot produced. `Missed` is deliberately not a `CheckOutcome`: the slot
/// never ran, so there is no observation — it must reset streaks, never count as a pass, and
/// never be recorded as evidence about the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotObservation {
    Probe(CheckOutcome),
    Missed,
}

/// The displayed status of a monitor (D-005, provisional).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// No fresh evidence: nothing recorded yet, or the last slot was missed.
    Unknown,
    /// No open incident and a recent pass.
    Up,
    /// An incident is open.
    Down,
}

/// What `Health::record` decided, so the caller can persist the transition and enqueue a
/// notification in the same commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Unchanged,
    IncidentOpened,
    IncidentRecovered,
}

/// Failure and recovery thresholds from the contract.
const FAILURES_TO_OPEN: u8 = 3;
const PASSES_TO_RECOVER: u8 = 2;

/// The per-monitor incident state machine: streaks plus whether an incident is open.
///
/// Recovery passes count only while an incident is open — passes before it mean nothing — and a
/// missed slot clears *both* streaks: a gap in coverage is not evidence in either direction.
/// What a missed slot does **not** clear is the open incident itself.
#[derive(Debug, Clone, Default)]
pub struct Health {
    consecutive_failures: u8,
    recovery_passes: u8,
    incident_open: bool,
    /// Whether the last slot produced an observation. Drives `Unknown`.
    fresh: bool,
}

impl Health {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild from persisted state after a restart. An open incident survives the restart;
    /// streaks and freshness do not — the first new observation must earn them back.
    #[must_use]
    pub fn with_open_incident() -> Self {
        Self {
            incident_open: true,
            ..Self::default()
        }
    }

    pub fn record(&mut self, observation: SlotObservation) -> Effect {
        match observation {
            SlotObservation::Missed => {
                // No evidence: streaks reset and the display goes back to `Unknown` unless an
                // incident is open. The incident itself is untouched — only new evidence closes it.
                self.consecutive_failures = 0;
                self.recovery_passes = 0;
                self.fresh = false;
                Effect::Unchanged
            }
            SlotObservation::Probe(outcome) => {
                self.fresh = true;
                if self.incident_open {
                    if outcome.is_pass() {
                        self.recovery_passes += 1;
                        if self.recovery_passes >= PASSES_TO_RECOVER {
                            self.incident_open = false;
                            self.consecutive_failures = 0;
                            self.recovery_passes = 0;
                            return Effect::IncidentRecovered;
                        }
                    } else {
                        // A failure while open cannot count toward recovery.
                        self.recovery_passes = 0;
                    }
                    Effect::Unchanged
                } else if outcome.is_pass() {
                    self.consecutive_failures = 0;
                    Effect::Unchanged
                } else {
                    self.consecutive_failures += 1;
                    if self.consecutive_failures >= FAILURES_TO_OPEN {
                        self.incident_open = true;
                        self.consecutive_failures = 0;
                        return Effect::IncidentOpened;
                    }
                    Effect::Unchanged
                }
            }
        }
    }

    #[must_use]
    pub fn status(&self) -> Status {
        if self.incident_open {
            Status::Down
        } else if self.fresh {
            Status::Up
        } else {
            Status::Unknown
        }
    }

    #[must_use]
    pub fn incident_open(&self) -> bool {
        self.incident_open
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url() -> Url {
        Url::parse("https://example.com/health").expect("test URL parses")
    }

    #[test]
    fn rejects_non_http_urls() {
        let err = NewMonitor::new("x", Url::parse("ftp://example.com").unwrap()).unwrap_err();
        assert!(matches!(err, ValidationError::UrlNotHttp { .. }));
    }

    #[test]
    fn rejects_timeout_above_interval() {
        let err =
            NewMonitor::with_schedule("x", url(), Duration::from_secs(30), Duration::from_secs(31))
                .unwrap_err();
        assert!(matches!(
            err,
            ValidationError::TimeoutExceedsInterval { .. }
        ));
    }

    #[test]
    fn three_consecutive_failures_open_an_incident() {
        let mut health = Health::new();
        for _ in 0..2 {
            assert_eq!(
                health.record(SlotObservation::Probe(CheckOutcome::Timeout)),
                Effect::Unchanged
            );
        }
        assert_eq!(
            health.record(SlotObservation::Probe(CheckOutcome::Transport)),
            Effect::IncidentOpened
        );
        assert_eq!(health.status(), Status::Down);
    }

    #[test]
    fn a_missed_slot_resets_the_failure_streak() {
        let mut health = Health::new();
        for _ in 0..2 {
            health.record(SlotObservation::Probe(CheckOutcome::HttpFail {
                status: 500,
            }));
        }
        health.record(SlotObservation::Missed);
        // The next failure is the first of a new streak, not the third.
        assert_eq!(
            health.record(SlotObservation::Probe(CheckOutcome::Timeout)),
            Effect::Unchanged
        );
        assert!(!health.incident_open());
    }

    #[test]
    fn a_missed_slot_does_not_close_an_open_incident() {
        let mut health = Health::new();
        for _ in 0..3 {
            health.record(SlotObservation::Probe(CheckOutcome::Transport));
        }
        assert!(health.incident_open());
        health.record(SlotObservation::Missed);
        assert!(health.incident_open());
        assert_eq!(health.status(), Status::Down);
    }

    #[test]
    fn two_fresh_passes_confirm_recovery() {
        let mut health = Health::new();
        for _ in 0..3 {
            health.record(SlotObservation::Probe(CheckOutcome::Timeout));
        }
        assert_eq!(
            health.record(SlotObservation::Probe(CheckOutcome::Pass)),
            Effect::Unchanged
        );
        assert!(health.incident_open());
        // A failure between passes resets the recovery streak.
        health.record(SlotObservation::Probe(CheckOutcome::Transport));
        health.record(SlotObservation::Probe(CheckOutcome::Pass));
        assert_eq!(
            health.record(SlotObservation::Probe(CheckOutcome::Pass)),
            Effect::IncidentRecovered
        );
        assert_eq!(health.status(), Status::Up);
    }

    #[test]
    fn missing_checks_display_unknown() {
        let mut health = Health::new();
        assert_eq!(health.status(), Status::Unknown);
        health.record(SlotObservation::Probe(CheckOutcome::Pass));
        assert_eq!(health.status(), Status::Up);
        health.record(SlotObservation::Missed);
        assert_eq!(health.status(), Status::Unknown);
    }

    #[test]
    fn open_incident_survives_a_restart() {
        // Rebuilt from the persisted incident: still down until two fresh passes land.
        let mut health = Health::with_open_incident();
        assert_eq!(health.status(), Status::Down);
        health.record(SlotObservation::Probe(CheckOutcome::Pass));
        assert_eq!(health.status(), Status::Down);
        assert_eq!(
            health.record(SlotObservation::Probe(CheckOutcome::Pass)),
            Effect::IncidentRecovered
        );
    }
}
