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

fn validate_fields(
    name: &str,
    url: &Url,
    interval: Duration,
    timeout: Duration,
) -> Result<(), ValidationError> {
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
    Ok(())
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
        validate_fields(&name, &url, interval, timeout)?;
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

/// A partial management write. `None` fields keep their stored values; the merged result is
/// revalidated against the same rules a new monitor faces — a patch can't smuggle in what a
/// create couldn't. `generation` is not set here: the store bumps it atomically so a stale
/// in-flight result is invalidated by the same write that changes the monitor.
#[derive(Debug, Clone, Default)]
pub struct MonitorPatch {
    pub name: Option<String>,
    pub url: Option<Url>,
    pub interval: Option<Duration>,
    pub timeout: Option<Duration>,
    pub paused: Option<bool>,
}

impl MonitorPatch {
    /// Merge onto the stored monitor and revalidate. Returns the monitor to persist.
    pub fn apply(&self, monitor: &Monitor) -> Result<Monitor, ValidationError> {
        let merged = Monitor {
            id: monitor.id,
            name: self.name.clone().unwrap_or_else(|| monitor.name.clone()),
            url: self.url.clone().unwrap_or_else(|| monitor.url.clone()),
            interval: self.interval.unwrap_or(monitor.interval),
            timeout: self.timeout.unwrap_or(monitor.timeout),
            generation: monitor.generation,
            paused: self.paused.unwrap_or(monitor.paused),
        };
        validate_fields(&merged.name, &merged.url, merged.interval, merged.timeout)?;
        Ok(merged)
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

/// Per-monitor statuses as seen by readers, keyed by monitor id.
pub type Statuses = std::collections::HashMap<MonitorId, Status>;

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

    /// Rebuild after restart from persisted truth: the authoritative open-incident flag plus
    /// the monitor's most recent stored slot observations, oldest→newest. A few rows are
    /// enough — streaks never exceed the thresholds.
    ///
    /// Replaying the tail through `record` recovers streaks *and* partial recovery progress:
    /// a stored pass while the incident is open counts toward recovery, because it was fresh
    /// evidence. At the end the store's flag wins: if replay disagrees with the incident row
    /// (e.g. the store is inconsistent), `incident_open` is forced back to the stored value.
    /// Forcing `open` in that case is the conservative direction — recovery waits for fresh
    /// evidence rather than announcing it early.
    #[must_use]
    pub fn rebuild(incident_open: bool, tail: &[SlotObservation]) -> Self {
        let mut health = if incident_open {
            Self::with_open_incident()
        } else {
            Self::new()
        };
        for observation in tail {
            health.record(*observation);
        }
        health.incident_open = incident_open;
        health
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
    fn rebuild_recovers_failure_streak() {
        let tail = [
            SlotObservation::Probe(CheckOutcome::Pass),
            SlotObservation::Probe(CheckOutcome::Timeout),
            SlotObservation::Probe(CheckOutcome::Transport),
        ];
        let mut health = Health::rebuild(false, &tail);
        // One more failure opens the incident — the stored streak was rebuilt.
        assert_eq!(
            health.record(SlotObservation::Probe(CheckOutcome::HttpFail {
                status: 500
            })),
            Effect::IncidentOpened
        );
    }

    #[test]
    fn rebuild_counts_stored_passes_toward_recovery() {
        let tail = [
            SlotObservation::Probe(CheckOutcome::Timeout),
            SlotObservation::Probe(CheckOutcome::Pass),
        ];
        let mut health = Health::rebuild(true, &tail);
        // The stored pass counts; one fresh pass completes recovery.
        assert_eq!(
            health.record(SlotObservation::Probe(CheckOutcome::Pass)),
            Effect::IncidentRecovered
        );
    }

    #[test]
    fn rebuild_store_flag_wins_over_replay() {
        // Store says the incident is open even though the tail ends in two passes —
        // inconsistent store, so the incident stays open and recovery waits for fresh evidence.
        let tail = [
            SlotObservation::Probe(CheckOutcome::Pass),
            SlotObservation::Probe(CheckOutcome::Pass),
        ];
        let health = Health::rebuild(true, &tail);
        assert!(health.incident_open());
        assert_eq!(health.status(), Status::Down);
    }

    #[test]
    fn rebuild_missed_tail_resets_streaks() {
        let tail = [
            SlotObservation::Probe(CheckOutcome::Timeout),
            SlotObservation::Probe(CheckOutcome::Timeout),
            SlotObservation::Missed,
        ];
        let mut health = Health::rebuild(false, &tail);
        assert_eq!(health.status(), Status::Unknown);
        // The miss reset the streak: two more failures do not open an incident.
        for _ in 0..2 {
            health.record(SlotObservation::Probe(CheckOutcome::Timeout));
        }
        assert!(!health.incident_open());
    }

    fn monitor() -> Monitor {
        NewMonitor::with_schedule("m", url(), Duration::from_secs(60), Duration::from_secs(30))
            .expect("valid")
            .into_monitor()
    }

    #[test]
    fn patch_merges_unset_fields_and_keeps_identity() {
        let monitor = monitor();
        let merged = MonitorPatch {
            name: Some("renamed".to_owned()),
            ..MonitorPatch::default()
        }
        .apply(&monitor)
        .expect("valid patch");
        assert_eq!(merged.id, monitor.id);
        assert_eq!(merged.generation, monitor.generation);
        assert_eq!(merged.name, "renamed");
        assert_eq!(merged.url, monitor.url);
        assert_eq!(merged.interval, monitor.interval);
        assert_eq!(merged.timeout, monitor.timeout);
        assert!(!merged.paused);
    }

    #[test]
    fn an_empty_patch_is_valid_and_changes_nothing() {
        let monitor = monitor();
        let merged = MonitorPatch::default()
            .apply(&monitor)
            .expect("no-op patch");
        assert_eq!(merged.name, monitor.name);
        assert_eq!(merged.generation, monitor.generation);
    }

    #[test]
    fn patch_cannot_smuggle_what_a_create_could_not() {
        let monitor = monitor();
        for (patch, matches) in [
            (
                MonitorPatch {
                    name: Some("   ".to_owned()),
                    ..MonitorPatch::default()
                },
                ValidationError::EmptyName,
            ),
            (
                MonitorPatch {
                    url: Some(Url::parse("ftp://example.com").unwrap()),
                    ..MonitorPatch::default()
                },
                ValidationError::UrlNotHttp {
                    scheme: String::new(),
                },
            ),
            (
                MonitorPatch {
                    interval: Some(Duration::from_secs(10)),
                    ..MonitorPatch::default()
                },
                ValidationError::IntervalBelowMinimum {
                    interval: Duration::ZERO,
                },
            ),
        ] {
            let err = patch.apply(&monitor).unwrap_err();
            assert!(
                std::mem::discriminant(&err) == std::mem::discriminant(&matches),
                "expected {matches:?}, got {err:?}"
            );
        }
    }

    #[test]
    fn patch_interval_below_stored_timeout_is_rejected() {
        // The merged monitor is what gets validated: lowering the interval to 20s while the
        // stored timeout is 30s produces an invalid monitor, even though each field alone
        // would pass its own bound.
        let monitor = monitor();
        let err = MonitorPatch {
            interval: Some(Duration::from_secs(20)),
            ..MonitorPatch::default()
        }
        .apply(&monitor)
        .unwrap_err();
        assert!(matches!(
            err,
            ValidationError::TimeoutExceedsInterval { .. }
        ));
    }

    #[test]
    fn patch_flips_pause_and_preserves_the_rest() {
        let monitor = monitor();
        let merged = MonitorPatch {
            paused: Some(true),
            ..MonitorPatch::default()
        }
        .apply(&monitor)
        .expect("pause");
        assert!(merged.paused);
        assert_eq!(merged.name, monitor.name);
        // Pausing again is legal and idempotent at the domain level.
        let again = MonitorPatch {
            paused: Some(true),
            ..MonitorPatch::default()
        }
        .apply(&merged)
        .expect("re-pause");
        assert!(again.paused);
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
