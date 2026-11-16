//! Engine vitals: is the monitor actually observing right now?
//!
//! A status is only as current as the machinery that produced it. If the writer task dies,
//! the coordinator dies or wedges, or every commit fails, the committed status map freezes
//! at its last value — and a frozen "up" is the most dangerous lie this project can tell.
//! `Vitals` is the one verdict every reader consults: `/readyz`, the statuses the API
//! serves, the public snapshot and (through the API) the dashboard.
//!
//! Inputs, all cheap to update from the hot path:
//! * the writer's consecutive-failure counter (`WriteHealth`);
//! * a coordinator heartbeat — the loop stamps it on every pass, so a coordinator that is
//!   alive but stuck is caught by staleness, not only one that has exited;
//! * supervised task exits — the [`Supervisor`] owns every engine task's `JoinHandle` and
//!   records any exit (return, panic or abort) that is not part of shutdown;
//! * the drain flag, set when shutdown begins: a draining monitor is not ready.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use monitor_db::WriteHealth;
use serde::Serialize;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// What a supervised task is for — decides what its death means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// On the observation path (writer, coordinator). If it stops, no status is current:
    /// monitoring is *unavailable*.
    Observing,
    /// Beside it (deliverers, snapshot, retention). Observation continues, but something an
    /// operator relies on does not: monitoring is *degraded*.
    Supporting,
}

/// The overall verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Availability {
    /// Every engine task is running and the write path lands commits.
    Ok,
    /// Observing, but a supporting duty (alert delivery, the public heartbeat, retention)
    /// has stopped.
    Degraded,
    /// Not observing: statuses are not current evidence and must read `unknown`.
    Unavailable,
}

/// One reading of the vitals, with the reasons spelled out for operators.
#[derive(Debug, Clone, Serialize)]
pub struct Verdict {
    pub status: Availability,
    /// Human-readable causes, empty when `status` is `ok`.
    pub faults: Vec<String>,
    /// The writer's trailing failed-commit count (0 = the last commit landed).
    pub consecutive_write_failures: u64,
}

impl Verdict {
    /// Whether statuses derived from committed evidence are current.
    pub fn observing(&self) -> bool {
        self.status != Availability::Unavailable
    }
}

/// `coordinator_beat_ms` before a coordinator attaches: no schedule to judge yet.
const NO_COORDINATOR: u64 = u64::MAX;

#[derive(Debug)]
struct Stopped {
    task: &'static str,
    role: Role,
    how: &'static str,
}

#[derive(Debug)]
struct Inner {
    epoch: Instant,
    stall_after: Duration,
    /// Milliseconds since `epoch` of the coordinator's last loop pass.
    coordinator_beat_ms: AtomicU64,
    write_failures: WriteHealth,
    draining: AtomicBool,
    stopped: Mutex<Vec<Stopped>>,
    /// Supporting duties whose last run failed (e.g. a retention pass), with why.
    failing: Mutex<std::collections::BTreeMap<&'static str, String>>,
}

/// Shared, cheap-to-clone engine health. See the module docs.
#[derive(Debug, Clone)]
pub struct Vitals {
    inner: Arc<Inner>,
}

impl Vitals {
    /// `write_failures` is the writer's counter; `stall_after` is how long the coordinator
    /// may go without a loop pass before it counts as stalled.
    pub fn new(write_failures: WriteHealth, stall_after: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                epoch: Instant::now(),
                stall_after,
                coordinator_beat_ms: AtomicU64::new(NO_COORDINATOR),
                write_failures,
                draining: AtomicBool::new(false),
                stopped: Mutex::new(Vec::new()),
                failing: Mutex::new(std::collections::BTreeMap::new()),
            }),
        }
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.inner.epoch.elapsed().as_millis()).unwrap_or(u64::MAX - 1)
    }

    /// The coordinator's loop pass. Also arms the stall check: until a coordinator beats,
    /// there is no schedule whose staleness could be judged.
    pub fn coordinator_beat(&self) {
        self.inner
            .coordinator_beat_ms
            .store(self.now_ms(), Ordering::Relaxed);
    }

    /// How long the coordinator has gone without a loop pass; `None` before it first beats.
    pub fn coordinator_silence(&self) -> Option<Duration> {
        let beat = self.inner.coordinator_beat_ms.load(Ordering::Relaxed);
        (beat != NO_COORDINATOR).then(|| Duration::from_millis(self.now_ms().saturating_sub(beat)))
    }

    /// Shutdown has begun: readiness goes false and stays false.
    pub fn begin_drain(&self) {
        self.inner.draining.store(true, Ordering::Relaxed);
    }

    /// A supporting duty's latest run: `Err(why)` marks the monitor degraded until the
    /// duty next succeeds (`Ok`). Observing is unaffected.
    pub fn duty_result(&self, duty: &'static str, result: Result<(), String>) {
        let mut failing = self
            .inner
            .failing
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        match result {
            Ok(()) => {
                failing.remove(duty);
            }
            Err(why) => {
                failing.insert(duty, why);
            }
        }
    }

    /// Record an engine task that stopped outside shutdown. Logged by the supervisor.
    pub fn task_stopped(&self, task: &'static str, role: Role, how: &'static str) {
        self.inner
            .stopped
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Stopped { task, role, how });
    }

    pub fn verdict(&self) -> Verdict {
        let mut faults = Vec::new();
        let mut observing = true;

        if self.inner.draining.load(Ordering::Relaxed) {
            faults.push("shutting down".to_owned());
            observing = false;
        }
        let write_failures = self.inner.write_failures.load(Ordering::Relaxed);
        if write_failures > 0 {
            faults.push(format!(
                "store writes failing ({write_failures} consecutive)"
            ));
            observing = false;
        }
        {
            let stopped = self
                .inner
                .stopped
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for task in stopped.iter() {
                faults.push(format!("{} {}", task.task, task.how));
                if task.role == Role::Observing {
                    observing = false;
                }
            }
        }
        {
            let failing = self
                .inner
                .failing
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for (duty, why) in failing.iter() {
                faults.push(format!("{duty} failing: {why}"));
            }
        }
        // A stall is only news while everything else on the observation path is fine — a
        // dead writer or failing disk already explains a coordinator stuck behind it.
        let beat = self.inner.coordinator_beat_ms.load(Ordering::Relaxed);
        if observing && beat != NO_COORDINATOR {
            let silent = Duration::from_millis(self.now_ms().saturating_sub(beat));
            if silent > self.inner.stall_after {
                faults.push(format!(
                    "coordinator stalled (no schedule pass for {}s)",
                    silent.as_secs()
                ));
                observing = false;
            }
        }

        let status = if !observing {
            Availability::Unavailable
        } else if faults.is_empty() {
            Availability::Ok
        } else {
            Availability::Degraded
        };
        Verdict {
            status,
            faults,
            consecutive_write_failures: write_failures,
        }
    }
}

/// Owns every engine task's `JoinHandle`: an exit that is not part of shutdown — a return, a
/// panic, an abort — is logged once and recorded in [`Vitals`], so the monitor reports
/// itself unavailable instead of serving the last statuses it happened to hold. Dropping a
/// `JoinHandle` detaches the task and loses exactly this information.
#[derive(Debug)]
pub struct Supervisor {
    vitals: Vitals,
    shutdown: CancellationToken,
    tracker: TaskTracker,
    failure: EngineFailure,
}

/// Raised when an `Observing` task stops outside shutdown. Honest "unavailable" is only half
/// the answer: a process that stays alive without its writer or coordinator never heals,
/// and `Restart=on-failure` never fires. Whoever owns the process waits on this, then
/// exits non-zero so the service manager restarts it (D-011).
#[derive(Debug, Clone, Default)]
pub struct EngineFailure {
    raised: CancellationToken,
    cause: Arc<Mutex<Option<String>>>,
}

impl EngineFailure {
    fn raise(&self, cause: String) {
        self.cause
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(cause);
        self.raised.cancel();
    }

    /// Resolves with the first failure's cause (`"writer panicked"`, …).
    pub async fn wait(&self) -> String {
        self.raised.cancelled().await;
        self.cause
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .unwrap_or_else(|| "an engine task stopped".to_owned())
    }
}

impl Supervisor {
    pub fn new(vitals: Vitals, shutdown: CancellationToken) -> Self {
        Self {
            vitals,
            shutdown,
            tracker: TaskTracker::new(),
            failure: EngineFailure::default(),
        }
    }

    /// The handle the process owner waits on to exit when observation stops.
    pub fn engine_failure(&self) -> EngineFailure {
        self.failure.clone()
    }

    /// Watch one engine task until it ends.
    pub fn watch(&self, task: &'static str, role: Role, handle: JoinHandle<()>) {
        let vitals = self.vitals.clone();
        let shutdown = self.shutdown.clone();
        let failure = self.failure.clone();
        self.tracker.spawn(async move {
            let how = match handle.await {
                Ok(()) if shutdown.is_cancelled() => return,
                Ok(()) => "exited",
                Err(error) if error.is_panic() => {
                    tracing::error!(task, %error, "engine task panicked");
                    "panicked"
                }
                Err(_) if shutdown.is_cancelled() => return,
                Err(_) => "was cancelled",
            };
            tracing::error!(task, how, ?role, "engine task stopped outside shutdown");
            vitals.task_stopped(task, role, how);
            if role == Role::Observing {
                failure.raise(format!("{task} {how}"));
            }
        });
    }

    /// Watch the coordinator's heartbeat: a coordinator that is alive but makes no loop
    /// pass for longer than `limit` never "stops", so without this it would stay
    /// unavailable forever. Past the limit it is an engine failure like a dead task — the
    /// process exits and the service manager restarts it (D-011). Checked every `every`;
    /// ends on shutdown.
    pub fn watch_stall(&self, limit: Duration, every: Duration) {
        let vitals = self.vitals.clone();
        let shutdown = self.shutdown.clone();
        let failure = self.failure.clone();
        self.tracker.spawn(async move {
            let mut ticks = tokio::time::interval(every);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = ticks.tick() => {}
                    () = shutdown.cancelled() => return,
                }
                if let Some(silent) = vitals.coordinator_silence()
                    && silent > limit
                {
                    let cause = format!(
                        "coordinator stalled (no schedule pass for {}s)",
                        silent.as_secs()
                    );
                    tracing::error!(%cause, "engine stalled past its limit");
                    failure.raise(cause);
                    return;
                }
            }
        });
    }

    /// Wait for every watched task to end (shutdown), bounded by `deadline`. Returns
    /// whether they all finished in time.
    pub async fn drain(&self, deadline: Duration) -> bool {
        self.tracker.close();
        tokio::time::timeout(deadline, self.tracker.wait())
            .await
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vitals(stall_after: Duration) -> (Vitals, WriteHealth) {
        let write_failures: WriteHealth = Arc::default();
        (
            Vitals::new(Arc::clone(&write_failures), stall_after),
            write_failures,
        )
    }

    #[test]
    fn a_fresh_engine_is_ok() {
        let (vitals, _) = vitals(Duration::from_secs(30));
        vitals.coordinator_beat();
        let verdict = vitals.verdict();
        assert_eq!(verdict.status, Availability::Ok);
        assert_eq!(verdict.faults, Vec::<String>::new());
    }

    #[test]
    fn failing_writes_mean_unavailable() {
        let (vitals, failures) = vitals(Duration::from_secs(30));
        failures.store(3, Ordering::Relaxed);
        let verdict = vitals.verdict();
        assert_eq!(verdict.status, Availability::Unavailable);
        assert_eq!(verdict.consecutive_write_failures, 3);
    }

    #[test]
    fn a_stopped_supporting_task_degrades_and_an_observing_one_takes_it_down() {
        let (vitals, _) = vitals(Duration::from_secs(30));
        vitals.task_stopped("webhook deliverer", Role::Supporting, "exited");
        let verdict = vitals.verdict();
        assert_eq!(verdict.status, Availability::Degraded);
        assert_eq!(verdict.faults, ["webhook deliverer exited"]);
        vitals.task_stopped("writer", Role::Observing, "panicked");
        assert_eq!(vitals.verdict().status, Availability::Unavailable);
    }

    #[test]
    fn a_silent_coordinator_is_stalled_but_only_once_one_has_beaten() {
        let (vitals, _) = vitals(Duration::from_millis(20));
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(
            vitals.verdict().status,
            Availability::Ok,
            "no coordinator attached yet — nothing to judge"
        );
        vitals.coordinator_beat();
        assert_eq!(vitals.verdict().status, Availability::Ok);
        std::thread::sleep(Duration::from_millis(40));
        let verdict = vitals.verdict();
        assert_eq!(verdict.status, Availability::Unavailable);
        assert!(verdict.faults[0].starts_with("coordinator stalled"));
        vitals.coordinator_beat();
        assert_eq!(vitals.verdict().status, Availability::Ok, "a beat recovers");
    }

    #[test]
    fn a_failing_supporting_duty_degrades_until_it_next_succeeds() {
        let (vitals, _) = vitals(Duration::from_secs(30));
        vitals.duty_result("retention", Err("database is locked".into()));
        let verdict = vitals.verdict();
        assert_eq!(
            verdict.status,
            Availability::Degraded,
            "observing continues"
        );
        assert_eq!(verdict.faults, ["retention failing: database is locked"]);
        vitals.duty_result("retention", Ok(()));
        assert_eq!(vitals.verdict().status, Availability::Ok);
    }

    #[test]
    fn draining_is_never_ready() {
        let (vitals, _) = vitals(Duration::from_secs(30));
        vitals.begin_drain();
        assert_eq!(vitals.verdict().status, Availability::Unavailable);
    }

    #[tokio::test]
    async fn the_supervisor_records_panics_and_exits_but_not_shutdown() {
        let (vitals, _) = vitals(Duration::from_secs(30));
        let shutdown = CancellationToken::new();
        let supervisor = Supervisor::new(vitals.clone(), shutdown.clone());
        supervisor.watch(
            "writer",
            Role::Observing,
            tokio::spawn(async { panic!("boom") }),
        );
        supervisor.watch("snapshot", Role::Supporting, tokio::spawn(async {}));
        let waiter = shutdown.clone();
        supervisor.watch(
            "coordinator",
            Role::Observing,
            tokio::spawn(async move { waiter.cancelled().await }),
        );
        // Let the first two end, then shut down: the third must not count as a fault.
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown.cancel();
        assert!(supervisor.drain(Duration::from_secs(5)).await);
        let verdict = vitals.verdict();
        assert_eq!(verdict.status, Availability::Unavailable);
        let mut faults = verdict.faults;
        faults.sort();
        assert_eq!(faults, ["snapshot exited", "writer panicked"]);
    }
}
