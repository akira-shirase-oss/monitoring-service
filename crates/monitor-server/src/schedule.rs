//! The coordinator: one task that owns the schedule.
//!
//! Every slot gets exactly one attempt. A slot that cannot start in its grace window —
//! because a probe is still in flight, or a concurrency bound is held — is recorded as
//! `missed`, never queued: a coverage gap is honest state, a catch-up storm is not. Bounds
//! are `try_acquire` on semaphores, so contention converts to `missed` instead of backlog.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{debug, info, warn};
use url::Url;

use monitor_db::{Observation, WriterHandle};
use monitor_domain::{Monitor, MonitorId, SlotObservation};

use crate::probe::ProbeClient;

#[derive(Debug, Clone)]
pub struct Config {
    /// How often the coordinator re-evaluates due slots.
    pub tick: Duration,
    /// How late a slot may start before it is counted `missed` (capped at the interval).
    pub dispatch_grace: Duration,
    /// Global in-flight probe bound.
    pub global_slots: usize,
    /// In-flight probes per `host:port`.
    pub per_origin_slots: usize,
    /// Capacity of the probe-results channel; full means workers park, slots go `missed`.
    pub results_bound: usize,
    /// How long shutdown waits for in-flight probes past their own deadlines.
    pub shutdown_deadline: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            tick: Duration::from_secs(1),
            dispatch_grace: Duration::from_secs(5),
            global_slots: 20,
            per_origin_slots: 2,
            results_bound: 256,
            shutdown_deadline: Duration::from_secs(6),
        }
    }
}

/// A monitor's schedule state — the two clocks stay in lockstep: `Instant` decides, the
/// wall-clock copy is what gets recorded.
struct Runtime {
    monitor: Monitor,
    next_slot: Instant,
    next_slot_at: OffsetDateTime,
    /// A probe claimed a slot and has not reported back.
    in_flight: bool,
}

impl Runtime {
    fn new(monitor: Monitor, delay: Duration) -> Self {
        Self {
            next_slot: Instant::now() + delay,
            next_slot_at: OffsetDateTime::now_utc() + delay,
            monitor,
            in_flight: false,
        }
    }

    fn advance(&mut self) {
        self.next_slot += self.monitor.interval;
        self.next_slot_at += self.monitor.interval;
    }
}

/// What the coordinator hands to a spawned probe worker.
struct Dispatch {
    monitor_id: MonitorId,
    generation: u64,
    slot_at: OffsetDateTime,
    url: Url,
    deadline: Duration,
    _global: tokio::sync::OwnedSemaphorePermit,
    _origin: tokio::sync::OwnedSemaphorePermit,
}

pub struct Coordinator;

impl Coordinator {
    /// Spawn the coordinator task. `registry` is the live monitor set — the watch channel is
    /// re-sent by management writes; `writer` is the only path to the store.
    pub fn spawn(
        registry: watch::Receiver<Arc<Vec<Monitor>>>,
        writer: WriterHandle,
        probes: ProbeClient,
        config: Config,
        shutdown: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(run(registry, writer, probes, config, shutdown))
    }
}

async fn run(
    mut registry: watch::Receiver<Arc<Vec<Monitor>>>,
    writer: WriterHandle,
    probes: ProbeClient,
    config: Config,
    shutdown: CancellationToken,
) {
    let (results_tx, mut results_rx) = mpsc::channel::<Observation>(config.results_bound);
    let global = Arc::new(Semaphore::new(config.global_slots));
    let mut origins: HashMap<String, Arc<Semaphore>> = HashMap::new();
    let mut runtimes: HashMap<MonitorId, Runtime> = HashMap::new();
    let tracker = TaskTracker::new();
    let mut ticker = tokio::time::interval(config.tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    reconcile(
        &mut runtimes,
        &mut origins,
        &registry.borrow_and_update(),
        &config,
    );

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                plan_due(&mut runtimes, &mut origins, &global, &config)
                    .dispatch(&probes, &writer, &results_tx, &tracker)
                    .await;
            }
            received = results_rx.recv() => {
                match received {
                    Some(observation) => {
                        // Only a result from the *current* generation may release the
                        // in-flight flag; a stale probe reporting late must not unblock
                        // a new-generation runtime.
                        if let Some(entry) = runtimes.get_mut(&observation.monitor_id)
                            && observation.generation == entry.monitor.generation
                        {
                            entry.in_flight = false;
                        }
                        if let Err(error) = writer.record(observation).await {
                            warn!(%error, "observation not recorded");
                        }
                    }
                    None => break,
                }
            }
            changed = registry.changed() => {
                match changed {
                    Ok(()) => reconcile(
                        &mut runtimes,
                        &mut origins,
                        &registry.borrow_and_update(),
                        &config,
                    ),
                    Err(_) => break,
                }
            }
            () = shutdown.cancelled() => break,
        }
    }

    tracker.close();
    if tokio::time::timeout(config.shutdown_deadline, tracker.wait())
        .await
        .is_err()
    {
        warn!("shutdown deadline hit with probes still in flight");
    }
    info!("coordinator stopped");
}

/// Sync pass over due slots. Returns the work to do — misses to commit, probes to spawn —
/// so no mutable borrow crosses an `.await`.
fn plan_due(
    runtimes: &mut HashMap<MonitorId, Runtime>,
    origins: &mut HashMap<String, Arc<Semaphore>>,
    global: &Arc<Semaphore>,
    config: &Config,
) -> Plan {
    let now = Instant::now();
    let mut plan = Plan::default();

    for entry in runtimes.values_mut() {
        let grace = entry.monitor.interval.min(config.dispatch_grace);

        // Slots too old to start are missed — collapse a backlog without a storm.
        while entry.next_slot + grace <= now {
            plan.misses.push(missed_slot(entry));
            entry.advance();
        }
        if entry.next_slot > now {
            continue;
        }

        if entry.in_flight {
            // One attempt per slot, never overlapping: this slot cannot run, so it is
            // missed — and the miss is counted, because coverage gaps are real state.
            plan.misses.push(missed_slot(entry));
            entry.advance();
            continue;
        }

        // Bounds are try_acquire: contention means "not this tick" — the grace window above
        // turns a persistent shortage into a counted miss rather than silent lag.
        let Ok(global_permit) = global.clone().try_acquire_owned() else {
            debug!(monitor = %entry.monitor.id.0, "global probe slots exhausted");
            continue;
        };
        let key = origin_key(&entry.monitor.url);
        let origin = origins
            .entry(key)
            .or_insert_with(|| Arc::new(Semaphore::new(config.per_origin_slots)))
            .clone();
        let Ok(origin_permit) = origin.try_acquire_owned() else {
            debug!(monitor = %entry.monitor.id.0, "origin probe slots exhausted");
            continue;
        };

        entry.in_flight = true;
        plan.dispatches.push(Dispatch {
            monitor_id: entry.monitor.id,
            generation: entry.monitor.generation,
            slot_at: entry.next_slot_at,
            url: entry.monitor.url.clone(),
            deadline: entry.monitor.timeout,
            _global: global_permit,
            _origin: origin_permit,
        });
        entry.advance();
    }
    plan
}

fn missed_slot(entry: &Runtime) -> Observation {
    Observation {
        monitor_id: entry.monitor.id,
        generation: entry.monitor.generation,
        slot_at: entry.next_slot_at,
        slot: SlotObservation::Missed,
        http_status: None,
        latency_ms: None,
        detail: None,
    }
}

/// The sync pass's product: commit every miss, then spawn every dispatch.
#[derive(Default)]
struct Plan {
    misses: Vec<Observation>,
    dispatches: Vec<Dispatch>,
}

impl Plan {
    async fn dispatch(
        self,
        probes: &ProbeClient,
        writer: &WriterHandle,
        results: &mpsc::Sender<Observation>,
        tracker: &TaskTracker,
    ) {
        for miss in self.misses {
            if let Err(error) = writer.record(miss).await {
                warn!(%error, "missed slot not recorded");
            }
        }
        for dispatch in self.dispatches {
            let probes = probes.clone();
            let results = results.clone();
            tracker.spawn(async move {
                // The permits ride inside `dispatch` and release when the task ends.
                let result = probes.probe(&dispatch.url, dispatch.deadline).await;
                let observation = Observation {
                    monitor_id: dispatch.monitor_id,
                    generation: dispatch.generation,
                    slot_at: dispatch.slot_at,
                    slot: SlotObservation::Probe(result.outcome),
                    http_status: result.http_status,
                    latency_ms: Some(u64::try_from(result.latency.as_millis()).unwrap_or(u64::MAX)),
                    detail: result.detail.map(str::to_owned),
                };
                if results.send(observation).await.is_err() {
                    debug!("results channel closed; observation dropped");
                }
            });
        }
    }
}

/// Rebuild runtimes from a new registry snapshot: new and changed monitors re-anchor one
/// tick out (a fast first check, then the steady grid); removed and paused monitors stop
/// scheduling. A changed monitor's stale in-flight probe still completes — its generation
/// mismatch makes the writer drop it.
fn reconcile(
    runtimes: &mut HashMap<MonitorId, Runtime>,
    origins: &mut HashMap<String, Arc<Semaphore>>,
    monitors: &[Monitor],
    config: &Config,
) {
    runtimes.retain(|_, entry| {
        monitors
            .iter()
            .any(|monitor| monitor.id == entry.monitor.id && !monitor.paused)
    });
    origins.retain(|origin, _| {
        runtimes
            .values()
            .any(|entry| origin_key(&entry.monitor.url) == *origin)
    });

    for monitor in monitors.iter().filter(|monitor| !monitor.paused) {
        match runtimes.get_mut(&monitor.id) {
            Some(entry)
                if entry.monitor.generation != monitor.generation
                    || entry.monitor.interval != monitor.interval
                    || entry.monitor.timeout != monitor.timeout
                    || entry.monitor.url != monitor.url =>
            {
                *entry = Runtime::new(monitor.clone(), config.tick);
            }
            Some(_) => {}
            None => {
                runtimes.insert(monitor.id, Runtime::new(monitor.clone(), config.tick));
            }
        }
    }
}

fn origin_key(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    let port = url.port_or_known_default().unwrap_or_default();
    format!("{host}:{port}")
}
