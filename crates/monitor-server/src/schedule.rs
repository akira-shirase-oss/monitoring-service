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
use monitor_domain::{Monitor, MonitorId, SlotObservation, WorkspaceId};

use crate::probe::ProbeClient;
use crate::vitals::Vitals;

#[derive(Debug, Clone)]
pub struct Config {
    /// How often the coordinator re-evaluates due slots.
    pub tick: Duration,
    /// How late a slot may start before it is counted `missed` (capped at the interval).
    pub dispatch_grace: Duration,
    /// Global in-flight probe bound's floor; the coordinator raises it to what the
    /// schedule needs, up to [`MAX_GLOBAL_SLOTS`] (D-018).
    pub global_slots: usize,
    /// In-flight probes per (origin, workspace) share — an origin is an
    /// `addr:port|host` pair (origin.rs), and each workspace holds its own bound
    /// on it (S2 re-review): one tenant's load on a shared host never eats
    /// another tenant's allowance.
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

/// Wall time read off the monotonic clock from one epoch, so the two clocks the schedule
/// keeps (`Instant` decides, the wall-clock copy is recorded) can't drift apart — and move
/// together under paused time in tests.
#[derive(Debug, Clone, Copy)]
struct SlotClock {
    instant: Instant,
    wall: OffsetDateTime,
}

impl SlotClock {
    fn start() -> Self {
        Self {
            instant: Instant::now(),
            wall: OffsetDateTime::now_utc(),
        }
    }

    fn wall_at(self, at: Instant) -> OffsetDateTime {
        self.wall + at.saturating_duration_since(self.instant)
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
    /// The first slot at least `min_delay` from now on the monitor's own phase grid (D-018):
    /// slot times are `phase (mod interval)` in Unix milliseconds, with the phase fixed by
    /// the monitor's id. Monitors created together therefore spread across their interval
    /// instead of falling due on the same tick, and a restart or an edit keeps the grid.
    fn new(monitor: Monitor, min_delay: Duration, clock: &SlotClock) -> Self {
        let now = Instant::now();
        let now_wall = clock.wall_at(now);
        let delay = delay_to_phase(
            now_wall,
            min_delay,
            phase(monitor.id, monitor.interval),
            monitor.interval,
        );
        Self {
            next_slot: now + delay,
            next_slot_at: now_wall + delay,
            monitor,
            in_flight: false,
        }
    }

    fn advance(&mut self) {
        self.next_slot += self.monitor.interval;
        self.next_slot_at += self.monitor.interval;
    }
}

/// The most probes one coordinator keeps in flight (D-018): outbound sockets, well inside
/// the unit's 4096 file descriptors beside `SMON_MAX_CONNECTIONS` inbound ones. A
/// schedule that would need more is refused when it is written (`check_capacity`).
pub const MAX_GLOBAL_SLOTS: usize = 512;

/// S3: the share of the probe budget one workspace's own schedule may need —
/// a quarter of the global ceiling. Writes past it are refused (`check_capacity`), so
/// a tenant's schedule can't silently crowd the engine's slots (D-020 tenancy).
pub const MAX_WORKSPACE_SLOTS: usize = MAX_GLOBAL_SLOTS / 4;

/// Probe slots a schedule needs (D-018). Each tick starts `Σ tick/interval` probes, and
/// each holds its slot until it ends — at worst its whole timeout — so the bound is
/// `Σ (tick + timeout) / interval` over the unpaused monitors.
pub fn required_slots<'a>(monitors: impl IntoIterator<Item = &'a Monitor>, tick: Duration) -> f64 {
    monitors
        .into_iter()
        .filter(|monitor| !monitor.paused)
        .map(|monitor| {
            let interval = monitor.interval.as_secs_f64().max(f64::EPSILON);
            (tick.as_secs_f64() + monitor.timeout.min(monitor.interval).as_secs_f64()) / interval
        })
        .sum()
}

/// A slot count as `f64` (slot counts are small; anything past `u32::MAX` saturates).
pub(crate) fn slots_f64(slots: usize) -> f64 {
    f64::from(u32::try_from(slots).unwrap_or(u32::MAX))
}

/// Global in-flight probe slots for a schedule: what it needs, never below the configured
/// floor (`Config::global_slots`) nor above [`MAX_GLOBAL_SLOTS`].
fn global_budget(monitors: &[Monitor], config: &Config) -> usize {
    let needed = required_slots(monitors, config.tick).ceil();
    let needed = if needed.is_finite() && needed > 0.0 {
        // Bounded above by the ceiling, so the cast can't truncate meaningfully.
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "finite, positive, and clamped to MAX_GLOBAL_SLOTS next"
        )]
        let needed = needed.min(slots_f64(MAX_GLOBAL_SLOTS)) as usize;
        needed
    } else {
        0
    };
    needed.clamp(config.global_slots.min(MAX_GLOBAL_SLOTS), MAX_GLOBAL_SLOTS)
}

/// Starts per tick on each origin across the given monitors (paused excluded). The
/// per-origin bound is a per-workspace share (S2 re-review), so the write check calls
/// this on one workspace's monitors; the bound's keys are `addr:port|host` pairs, so
/// sites sharing one hosting address are already distinct entries.
pub fn per_origin_loads<'a>(
    monitors: impl Iterator<Item = &'a Monitor>,
    tick: Duration,
) -> HashMap<String, f64> {
    let mut per_origin: HashMap<String, f64> = HashMap::new();
    for monitor in monitors.filter(|monitor| !monitor.paused) {
        *per_origin.entry(origin_key(&monitor.url)).or_default() +=
            tick.as_secs_f64() / monitor.interval.as_secs_f64().max(f64::EPSILON);
    }
    per_origin
}

/// Starts per tick on each resolved ADDRESS across the given monitors — the
/// write-time ceiling the cross-host view needs (N1 re-review). The key is the
/// `addr:port` half of the origin key alone, so `localhost` and `127.0.0.1`
/// charge one IP's budget: a tenant can't buy more starts at an address by
/// naming it more ways. A name not yet resolved keys by its spelling — distinct
/// spellings stay distinct until DNS proves them one address.
pub fn per_address_loads<'a>(
    monitors: impl Iterator<Item = &'a Monitor>,
    tick: Duration,
) -> HashMap<String, f64> {
    let mut per_address: HashMap<String, f64> = HashMap::new();
    for monitor in monitors.filter(|monitor| !monitor.paused) {
        let origin = origin_key(&monitor.url);
        let address = origin
            .split_once('|')
            .map_or(origin.as_str(), |(address, _)| address);
        *per_address.entry(address.to_owned()).or_default() +=
            tick.as_secs_f64() / monitor.interval.as_secs_f64().max(f64::EPSILON);
    }
    per_address
}

/// The global semaphore, resized to the schedule's budget whenever the registry changes.
/// Growing adds permits; shrinking forgets the free ones (permits held by probes in
/// flight are forgotten on a later resize).
struct GlobalSlots {
    semaphore: Arc<Semaphore>,
    total: usize,
}

impl GlobalSlots {
    fn new(total: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(total)),
            total,
        }
    }

    fn resize(&mut self, target: usize) {
        if target > self.total {
            self.semaphore.add_permits(target - self.total);
            self.total = target;
        } else if target < self.total {
            self.total -= self.semaphore.forget_permits(self.total - target);
        }
    }
}

/// A monitor's phase: a fixed offset within its interval, derived from its id. Both halves
/// of the id are mixed (`splitmix64`): a `UUIDv7` starts with a timestamp, so ids minted
/// together share their high bits.
fn phase(id: MonitorId, interval: Duration) -> Duration {
    let interval_ms = u64::try_from(interval.as_millis())
        .unwrap_or(u64::MAX)
        .max(1);
    let (high, low) = id.0.as_u64_pair();
    let mut x = high ^ low.rotate_left(29);
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
    Duration::from_millis(x % interval_ms)
}

/// How long from `now` until the first grid point (`phase` mod `interval`, in Unix ms)
/// that is at least `min_delay` away.
fn delay_to_phase(
    now: OffsetDateTime,
    min_delay: Duration,
    phase: Duration,
    interval: Duration,
) -> Duration {
    let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
    let interval_ms = ms(interval).max(1);
    let now_ms = u64::try_from(now.unix_timestamp_nanos() / 1_000_000).unwrap_or(0);
    let earliest = now_ms.saturating_add(ms(min_delay));
    let into = earliest % interval_ms;
    let phase_ms = ms(phase) % interval_ms;
    let next = earliest - into + phase_ms + if phase_ms < into { interval_ms } else { 0 };
    Duration::from_millis(next - now_ms)
}

/// What the coordinator hands to a spawned probe worker.
struct Dispatch {
    monitor_id: MonitorId,
    workspace: monitor_domain::WorkspaceId,
    generation: u64,
    slot_at: OffsetDateTime,
    url: Url,
    /// The monitor's probe method + response assertions (D-025) — snapshotted at
    /// claim so an edit mid-flight stays inside its own generation.
    method: monitor_domain::ProbeMethod,
    expect: monitor_domain::Expect,
    deadline: Duration,
    _global: tokio::sync::OwnedSemaphorePermit,
    _origin: tokio::sync::OwnedSemaphorePermit,
}

pub struct Coordinator;

impl Coordinator {
    /// Spawn the coordinator task. `registry` is the live monitor set — the watch channel is
    /// re-sent by management writes; `writer` is the only path to the store. The loop
    /// stamps `vitals` on every pass, so a coordinator that is alive but stuck (behind a
    /// wedged writer, say) reads as stalled instead of quietly freezing every status.
    pub fn spawn(
        registry: watch::Receiver<Arc<Vec<Monitor>>>,
        writer: WriterHandle,
        probes: ProbeClient,
        config: Config,
        vitals: Vitals,
        shutdown: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(run(registry, writer, probes, config, vitals, shutdown))
    }
}

async fn run(
    mut registry: watch::Receiver<Arc<Vec<Monitor>>>,
    writer: WriterHandle,
    probes: ProbeClient,
    config: Config,
    vitals: Vitals,
    shutdown: CancellationToken,
) {
    let (results_tx, mut results_rx) = mpsc::channel::<Observation>(config.results_bound);
    let mut global = GlobalSlots::new(config.global_slots);
    let clock = SlotClock::start();
    // Background name lookups for the per-origin keys; aborted when the loop ends.
    let mut lookups = tokio::task::JoinSet::new();
    let mut ticks_since_refresh = 0_u32;
    let mut origins: HashMap<(String, WorkspaceId), Arc<Semaphore>> = HashMap::new();
    let mut runtimes: HashMap<MonitorId, Runtime> = HashMap::new();
    let tracker = TaskTracker::new();
    let mut ticker = tokio::time::interval(config.tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    {
        let monitors = Arc::clone(&registry.borrow_and_update());
        reconcile(&mut runtimes, &mut origins, &monitors, &config, &clock);
        global.resize(global_budget(&monitors, &config));
        refresh_origins(&mut lookups, &runtimes);
    }

    loop {
        vitals.coordinator_beat();
        tokio::select! {
            _ = ticker.tick() => {
                if lookups.try_join_next().is_some() {
                    prune_origins(&mut origins, &runtimes); // names may have been re-keyed
                }
                ticks_since_refresh += 1;
                if ticks_since_refresh >= ORIGIN_REFRESH_TICKS {
                    ticks_since_refresh = 0;
                    refresh_origins(&mut lookups, &runtimes);
                }
                plan_due(&mut runtimes, &mut origins, &global.semaphore, &config)
                    .dispatch(&probes, &writer, &results_tx, &tracker, &vitals)
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
                if changed.is_err() {
                    break;
                }
                let monitors = Arc::clone(&registry.borrow_and_update());
                reconcile(&mut runtimes, &mut origins, &monitors, &config, &clock);
                global.resize(global_budget(&monitors, &config));
                refresh_origins(&mut lookups, &runtimes);
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
    origins: &mut HashMap<(String, WorkspaceId), Arc<Semaphore>>,
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
        // The bound is a per-workspace share (S2 re-review): the semaphore's key is
        // the (addr|host) origin *and* the workspace, so a shared host's load is
        // each tenant's own business.
        let key = (origin_key(&entry.monitor.url), entry.monitor.workspace);
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
            workspace: entry.monitor.workspace,
            generation: entry.monitor.generation,
            slot_at: entry.next_slot_at,
            url: entry.monitor.url.clone(),
            method: entry.monitor.method,
            expect: entry.monitor.expect.clone(),
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
        workspace: entry.monitor.workspace,
        generation: entry.monitor.generation,
        slot_at: entry.next_slot_at,
        slot: SlotObservation::Missed,
        http_status: None,
        latency_ms: None,
        detail: None,
    }
}

/// The sync pass's product: commit every miss (beating the heartbeat after each — F2),
/// then spawn every dispatch.
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
        vitals: &Vitals,
    ) {
        for miss in self.misses {
            if let Err(error) = writer.record(miss).await {
                warn!(%error, "missed slot not recorded");
            }
            // Staleness is judged by progress, not by loop passes: every miss the writer
            // has taken is progress, so a long catch-up after a freeze (962 commits for
            // 100 monitors after 130 s) never reads as a wedged loop. A writer that stops
            // taking commits stops these beats, and the stall is caught as before.
            vitals.coordinator_beat();
        }
        for dispatch in self.dispatches {
            let probes = probes.clone();
            let results = results.clone();
            tracker.spawn(async move {
                // The permits ride inside `dispatch` and release when the task ends.
                // The workspace attribution rides a task-local so the address
                // charge — wherever it happens — spends the right tenant's
                // share of the address, never a neighbour's (B re-review).
                let result = crate::probe::PROBE_WORKSPACE
                    .scope(
                        dispatch.workspace,
                        probes.probe_expect(
                            &dispatch.url,
                            dispatch.method,
                            &dispatch.expect,
                            dispatch.deadline,
                        ),
                    )
                    .await;
                // A probe the SSRF guard refused at probe time (a name resolving to a local
                // address, a redirect to one) is a *target failure*: every real visitor of
                // that target is broken too. It commits as `transport` + `rejected_local`
                // and pages like any failure (D-013). Only registration refuses literals.
                let observation = Observation {
                    monitor_id: dispatch.monitor_id,
                    workspace: dispatch.workspace,
                    generation: dispatch.generation,
                    slot_at: dispatch.slot_at,
                    // A start OUR bound refused is a coverage gap, never target
                    // evidence — and it is *neutral* on the monitor's streaks:
                    // a starved slot must not launder a real outage's failure
                    // count away. The commit is `missed` + `address_budget`.
                    slot: if result.limited {
                        SlotObservation::Budgeted
                    } else {
                        SlotObservation::Probe(result.outcome)
                    },
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

/// Rebuild runtimes from a new registry snapshot: new and changed monitors re-anchor on
/// their phase grid, at least one tick out (D-018); removed and paused monitors stop
/// scheduling. A changed monitor's stale in-flight probe still completes — its generation
/// mismatch makes the writer drop it.
fn reconcile(
    runtimes: &mut HashMap<MonitorId, Runtime>,
    origins: &mut HashMap<(String, WorkspaceId), Arc<Semaphore>>,
    monitors: &[Monitor],
    config: &Config,
    clock: &SlotClock,
) {
    runtimes.retain(|_, entry| {
        monitors
            .iter()
            .any(|monitor| monitor.id == entry.monitor.id && !monitor.paused)
    });
    prune_origins(origins, runtimes);

    for monitor in monitors.iter().filter(|monitor| !monitor.paused) {
        match runtimes.get_mut(&monitor.id) {
            Some(entry)
                if entry.monitor.generation != monitor.generation
                    || entry.monitor.interval != monitor.interval
                    || entry.monitor.timeout != monitor.timeout
                    || entry.monitor.url != monitor.url =>
            {
                // No minimum delay: on the same grid the pending slot stays exactly where
                // it was (an edit just before it no longer skips it a whole interval); a
                // slot already passed is not run twice, the grid's next one is.
                *entry = Runtime::new(monitor.clone(), Duration::ZERO, clock);
            }
            Some(_) => {}
            None => {
                runtimes.insert(
                    monitor.id,
                    Runtime::new(monitor.clone(), config.tick, clock),
                );
            }
        }
    }
}

/// The per-origin bound's key: the resolved address and the requested host
/// (`addr:port|host`, origin.rs) — sites sharing one hosting address stay distinct.
fn origin_key(url: &Url) -> String {
    crate::origin::key(url)
}

/// Drop the per-origin semaphores no runtime keys to any more (after a registry change, or
/// after a refresh re-keyed a name).
fn prune_origins(
    origins: &mut HashMap<(String, WorkspaceId), Arc<Semaphore>>,
    runtimes: &HashMap<MonitorId, Runtime>,
) {
    let in_use: std::collections::HashSet<(String, WorkspaceId)> = runtimes
        .values()
        .map(|entry| (origin_key(&entry.monitor.url), entry.monitor.workspace))
        .collect();
    origins.retain(|key, _| in_use.contains(key));
}

/// How often the coordinator re-resolves its monitors' names (ticks).
const ORIGIN_REFRESH_TICKS: u32 = 60;

/// Re-resolve every monitor's name in the background (at most one refresh at a time).
fn refresh_origins(lookups: &mut tokio::task::JoinSet<()>, runtimes: &HashMap<MonitorId, Runtime>) {
    if !lookups.is_empty() {
        return;
    }
    let urls: Vec<Url> = runtimes
        .values()
        .map(|entry| entry.monitor.url.clone())
        .collect();
    lookups.spawn(crate::origin::refresh(urls, Duration::from_secs(30)));
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::RwLock;

    use axum::Router;
    use axum::response::Redirect;
    use axum::routing::get;
    use monitor_db::{Db, writer};
    use monitor_domain::{NewMonitor, WorkspaceId};

    use super::*;
    use crate::probe::ProbePolicy;
    use crate::vitals::Vitals;

    fn fast() -> Config {
        Config {
            tick: Duration::from_millis(20),
            dispatch_grace: Duration::from_millis(60),
            global_slots: 4,
            per_origin_slots: 2,
            results_bound: 64,
            shutdown_deadline: Duration::from_millis(500),
        }
    }

    /// Run the real coordinator + writer against `url` under the *strict* profile for a
    /// few slots; return the committed (outcome, detail) rows and whether an incident is open.
    async fn observe_strict(
        url: &str,
        pins: &[(&str, std::net::SocketAddr)],
    ) -> (Vec<(String, Option<String>)>, bool) {
        let db = Db::connect("sqlite::memory:", 1).await.expect("db");
        db.migrate().await.expect("migrate");
        let stored = NewMonitor::new("guarded", Url::parse(url).expect("url"))
            .expect("monitor")
            .into_monitor();
        db.workspace(WorkspaceId::DEFAULT)
            .insert_monitor(&stored)
            .await
            .expect("insert");
        let mut monitor = stored.clone();
        monitor.interval = Duration::from_millis(60);
        // The loopback fixture answers on accept — the deadline only bounds a
        // pathological hang, it is never the measured quantity, so it is sized
        // to be unreachable rather than "usually fast enough" (N-6 re-review:
        // a 400 ms real-time budget can flake a loaded CI box).
        monitor.timeout = Duration::from_secs(5);

        let statuses = Arc::new(RwLock::new(HashMap::new()));
        let (handle, _writer) = writer::spawn(
            db.clone(),
            statuses,
            HashMap::new(),
            Arc::default(),
            32,
            false,
        );
        let probes = ProbeClient::pinned(
            ProbePolicy {
                allow_local_targets: false,
                extra_roots: Vec::new(),
            },
            64 * 1024,
            pins,
        )
        .expect("probe client");
        let (_registry_tx, registry_rx) = watch::channel(Arc::new(vec![monitor.clone()]));
        let shutdown = CancellationToken::new();
        let task = Coordinator::spawn(
            registry_rx,
            handle,
            probes,
            fast(),
            Vitals::new(Arc::default(), Duration::from_secs(30)),
            shutdown.clone(),
        );
        tokio::time::sleep(Duration::from_millis(900)).await;
        shutdown.cancel();
        let _ = task.await;
        let rows = db
            .workspace(WorkspaceId::DEFAULT)
            .list_checks(monitor.id, 100)
            .await
            .expect("checks")
            .into_iter()
            .map(|row| (row.outcome, row.detail))
            .collect();
        let open = db
            .workspace(WorkspaceId::DEFAULT)
            .open_incident(monitor.id)
            .await
            .expect("incident")
            .is_some();
        (rows, open)
    }

    fn assert_refusals_page(rows: &[(String, Option<String>)], open: bool, case: &str) {
        let refused: Vec<_> = rows
            .iter()
            .filter(|(_, detail)| detail.as_deref() == Some(crate::probe::REJECTED_LOCAL))
            .collect();
        assert!(
            refused.len() >= 3,
            "{case}: expected refused probes, got {rows:?}"
        );
        for (outcome, _) in &refused {
            assert_eq!(
                outcome, "transport",
                "{case}: a refusal is a target failure: {rows:?}"
            );
        }
        assert!(
            open,
            "{case}: three refused probes must open an incident (page)"
        );
    }

    /// A public target that starts redirecting to a local *name* (`localhost`) is broken
    /// for every real visitor. The guard rightly refuses the hop at the resolver — and that
    /// refusal must page, not vanish into a `missed` slot.
    #[tokio::test]
    async fn a_redirect_to_a_local_name_is_a_failure_that_pages() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let port = addr.port();
        let app =
            Router::new().route(
                "/moved",
                get(move || async move {
                    Redirect::temporary(&format!("http://localhost:{port}/health"))
                }),
            );
        tokio::spawn(axum::serve(listener, app).into_future());

        // `public.test` stands for a public name: pinned to the fixture, past the policy.
        let (rows, open) = observe_strict(
            &format!("http://public.test:{port}/moved"),
            &[("public.test", addr)],
        )
        .await;
        assert_refusals_page(&rows, open, "redirect to localhost");
    }

    /// A public name whose DNS answer turned private (parked at 127.0.0.1, or hijacked)
    /// is an outage for its users: refused by the resolver, recorded as a failure, paged.
    #[tokio::test]
    async fn a_name_that_resolves_to_a_local_address_is_a_failure_that_pages() {
        let (rows, open) = observe_strict("http://localhost:9/health", &[]).await;
        assert_refusals_page(&rows, open, "name resolving to loopback");
    }

    /// F2: after a long freeze (SIGSTOP, a VM pause) the first pass commits every lost
    /// slot as `missed` — 962 commits for 100 monitors after 130 s. The watchdog judges
    /// the coordinator by heartbeat silence, and the coordinator used to beat only once per
    /// loop pass, so a backlog that took longer than the watchdog's next check read as a
    /// wedged loop and the process exited. Each committed miss is progress: the heartbeat
    /// must stay fresh through the backlog, measured right after it.
    #[tokio::test]
    async fn committing_a_backlog_of_misses_keeps_the_heartbeat_fresh() {
        let db = Db::connect("sqlite::memory:", 1).await.expect("db");
        db.migrate().await.expect("migrate");
        let monitor = NewMonitor::new("backlog", Url::parse("https://example.com/").expect("url"))
            .expect("monitor")
            .into_monitor();
        db.workspace(WorkspaceId::DEFAULT)
            .insert_monitor(&monitor)
            .await
            .expect("insert");
        let (writer, _task) = writer::spawn(
            db.clone(),
            Arc::new(RwLock::new(HashMap::new())),
            HashMap::new(),
            Arc::default(),
            32,
            false,
        );
        let vitals = Vitals::new(Arc::default(), Duration::from_secs(30));
        vitals.coordinator_beat();
        tokio::time::sleep(Duration::from_millis(300)).await;

        let start = time::OffsetDateTime::now_utc() - time::Duration::hours(1);
        let mut plan = Plan::default();
        for n in 0..200 {
            plan.misses.push(Observation {
                monitor_id: monitor.id,
                workspace: monitor.workspace,
                generation: monitor.generation,
                slot_at: start + time::Duration::seconds(15 * n),
                slot: SlotObservation::Missed,
                http_status: None,
                latency_ms: None,
                detail: None,
            });
        }
        let probes = ProbeClient::new(
            ProbePolicy {
                allow_local_targets: false,
                extra_roots: Vec::new(),
            },
            64 * 1024,
        )
        .expect("probe client");
        let (results, _results_rx) = mpsc::channel(8);
        plan.dispatch(&probes, &writer, &results, &TaskTracker::new(), &vitals)
            .await;

        let silence = vitals.coordinator_silence().expect("armed");
        assert!(
            silence < Duration::from_millis(100),
            "the heartbeat went {silence:?} without a beat while the backlog committed"
        );
        assert_eq!(
            db.workspace(WorkspaceId::DEFAULT)
                .list_checks(monitor.id, 500)
                .await
                .expect("checks")
                .len(),
            200
        );
    }

    /// `n` monitors of `interval`/`timeout`, created together, spread over `origins`
    /// origins (so the per-origin bound is not what is under test).
    fn fleet(n: usize, origins: usize, interval: Duration, timeout: Duration) -> Vec<Monitor> {
        (0..n)
            .map(|i| {
                let url =
                    Url::parse(&format!("http://o{}.example/health", i % origins)).expect("url");
                let mut monitor = NewMonitor::new(format!("m{i}"), url)
                    .expect("monitor")
                    .into_monitor();
                monitor.interval = interval;
                monitor.timeout = timeout;
                monitor
            })
            .collect()
    }

    /// Monitors created together used to share a phase: every one fell due on the same
    /// tick, a wave the global bound could only partly start. Each monitor's slots now sit
    /// at a deterministic offset (from its id) within its interval, so they spread evenly.
    #[tokio::test(start_paused = true)]
    async fn monitors_created_together_fall_due_spread_across_their_interval() {
        let config = Config::default();
        let interval = Duration::from_secs(15);
        let monitors = fleet(1500, 300, interval, Duration::from_secs(3));
        let mut runtimes = HashMap::new();
        let mut origins = HashMap::new();
        let clock = SlotClock::start();
        reconcile(&mut runtimes, &mut origins, &monitors, &config, &clock);
        let mut per_second = vec![0_usize; 16];
        let start = Instant::now();
        for entry in runtimes.values() {
            let offset = entry.next_slot.duration_since(start).as_secs();
            per_second[usize::try_from(offset).unwrap_or(15).min(15)] += 1;
        }
        // 1500 over 15 s is 100 a second; allow sampling noise, never a wave.
        let busiest = per_second.iter().copied().max().unwrap_or(0);
        assert!(busiest <= 160, "a wave formed: {per_second:?}");
        // Deterministic: the same id gets the same phase again (a restart, an edit).
        let again = Runtime::new(monitors[0].clone(), config.tick, &clock);
        let first = &runtimes[&monitors[0].id];
        let drift = first.next_slot_at - again.next_slot_at;
        assert!(drift.abs() < time::Duration::milliseconds(50), "{drift}");
    }

    /// The load probe's finding, as a test: 1,600 monitors on one 15 s interval (spread
    /// over 200 origins, instant probes) must miss nothing for two whole intervals. With
    /// a fixed 20 slots a tick, about 107 fall due each second and most were missed.
    #[tokio::test(start_paused = true)]
    async fn sixteen_hundred_monitors_on_one_interval_miss_nothing() {
        let config = Config::default();
        let monitors = fleet(1600, 200, Duration::from_secs(15), Duration::from_secs(3));
        let mut runtimes = HashMap::new();
        let mut origins = HashMap::new();
        let clock = SlotClock::start();
        reconcile(&mut runtimes, &mut origins, &monitors, &config, &clock);
        let global = Arc::new(Semaphore::new(global_budget(&monitors, &config)));
        let (mut missed, mut dispatched) = (0, 0);
        for _ in 0..32 {
            tokio::time::advance(config.tick).await;
            let plan = plan_due(&mut runtimes, &mut origins, &global, &config);
            missed += plan.misses.len();
            dispatched += plan.dispatches.len();
            for dispatch in plan.dispatches {
                // An instant probe: its result releases the slot before the next tick.
                if let Some(entry) = runtimes.get_mut(&dispatch.monitor_id) {
                    entry.in_flight = false;
                }
            }
        }
        assert_eq!(missed, 0, "{missed} slots missed ({dispatched} started)");
        assert!(dispatched >= 3200, "every monitor ran twice: {dispatched}");
    }

    /// An edit re-anchors the runtime. With a one-tick minimum delay, an edit made less
    /// than a tick before a due slot pushed it a whole interval later, and no `missed` row
    /// said so. The pending slot is kept: same grid, so it still runs on time.
    #[tokio::test(start_paused = true)]
    async fn an_edit_just_before_a_due_slot_keeps_that_slot() {
        let config = Config::default();
        let mut monitor = fleet(1, 1, Duration::from_secs(15), Duration::from_secs(3)).remove(0);
        let mut runtimes = HashMap::new();
        let mut origins = HashMap::new();
        let clock = SlotClock::start();
        reconcile(
            &mut runtimes,
            &mut origins,
            std::slice::from_ref(&monitor),
            &config,
            &clock,
        );
        let due = runtimes[&monitor.id].next_slot;
        let due_at = runtimes[&monitor.id].next_slot_at;
        // Half a tick before the slot, the operator edits the name.
        let until_due = due.saturating_duration_since(Instant::now());
        tokio::time::advance(until_due.saturating_sub(config.tick / 2)).await;
        monitor.name = "renamed".into();
        monitor.generation += 1;
        reconcile(
            &mut runtimes,
            &mut origins,
            std::slice::from_ref(&monitor),
            &config,
            &clock,
        );
        let edited = &runtimes[&monitor.id];
        assert_eq!(
            edited.next_slot.saturating_duration_since(due),
            Duration::ZERO,
            "the slot was pushed later"
        );
        let drift = (edited.next_slot_at - due_at).abs();
        assert!(
            drift < time::Duration::milliseconds(50),
            "the slot due at {due_at} moved to {} without a missed row",
            edited.next_slot_at
        );
    }

    /// S2 re-review: origins are (address, host) pairs. A name and the address it
    /// resolves to are different origins — on shared hosting the host is the site,
    /// and one address is many tenants' worth of vhosts.
    #[tokio::test(start_paused = true)]
    async fn a_name_and_the_address_it_resolves_to_are_different_origins() {
        let config = Config {
            per_origin_slots: 1,
            ..Config::default()
        };
        let mut monitors = fleet(2, 1, Duration::from_secs(15), Duration::from_secs(3));
        monitors[0].url = Url::parse("http://localhost:9/a").expect("url");
        monitors[1].url = Url::parse("http://127.0.0.1:9/b").expect("url");
        crate::origin::resolve(&monitors[0].url).await;
        let mut runtimes = HashMap::new();
        let mut origins = HashMap::new();
        let clock = SlotClock::start();
        reconcile(&mut runtimes, &mut origins, &monitors, &config, &clock);
        for entry in runtimes.values_mut() {
            entry.next_slot = Instant::now(); // both due on this tick
        }
        let global = Arc::new(Semaphore::new(16));
        let plan = plan_due(&mut runtimes, &mut origins, &global, &config);
        assert_eq!(
            plan.dispatches.len(),
            2,
            "(127.0.0.1:9, localhost) and (127.0.0.1:9, 127.0.0.1) are different origins"
        );
    }

    /// The bound itself is a per-workspace share: two workspaces on the same origin
    /// each get their own slot — a shared host's load is never a commons one
    /// tenant's schedule can exhaust for the others (S2 re-review).
    #[tokio::test(start_paused = true)]
    async fn each_workspace_gets_its_own_share_of_an_origin() {
        let config = Config {
            per_origin_slots: 1,
            ..Config::default()
        };
        let mut monitors = fleet(2, 1, Duration::from_secs(15), Duration::from_secs(3));
        let other = WorkspaceId(uuid::Uuid::now_v7());
        monitors[1].workspace = other;
        let mut runtimes = HashMap::new();
        let mut origins = HashMap::new();
        let clock = SlotClock::start();
        reconcile(&mut runtimes, &mut origins, &monitors, &config, &clock);
        for entry in runtimes.values_mut() {
            entry.next_slot = Instant::now(); // both due on this tick
        }
        let global = Arc::new(Semaphore::new(16));
        let plan = plan_due(&mut runtimes, &mut origins, &global, &config);
        assert_eq!(
            plan.dispatches.len(),
            2,
            "two workspaces on one origin share no slot budget"
        );
    }

    /// A listener that answers a bare `200` — the dial evidence, on the address
    /// the budget binds. Returns its socket address for the override map.
    async fn http_fixture() -> std::net::SocketAddr {
        use tokio::io::AsyncWriteExt as _;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fixture listener");
        let addr = listener.local_addr().expect("fixture addr");
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                    .await;
            }
        });
        addr
    }

    /// N1 re-review: the per-ADDRESS runtime bound is charged on the ONE
    /// candidate the validating resolver hands to the dialer — shared across
    /// every workspace (`try_acquire`, misses counted), and max-min fair once
    /// the ceiling is spent: a second workspace's first starts still pay (a
    /// tenant behind the same IP never goes dark), and only a workspace
    /// already holding its fair split is refused (N-1). The refused slot
    /// commits `missed` with its reason — our own budget's refusal is a
    /// coverage gap, never evidence about the target (H: on a loopback
    /// fixture — the budget binds the address, and the 3600 s window makes
    /// the clock irrelevant).
    #[tokio::test]
    async fn two_workspaces_on_one_address_share_the_runtime_cap() {
        let db = Db::connect("sqlite::memory:", 1).await.expect("db");
        db.migrate().await.expect("migrate");
        let (writer, _writer_task) = writer::spawn(
            db.clone(),
            Arc::new(RwLock::new(HashMap::new())),
            HashMap::new(),
            Arc::default(),
            32,
            false,
        );
        let vitals = Vitals::new(Arc::default(), Duration::from_secs(30));
        // `victim.test` resolves to loopback; the budget admits one start an hour.
        let overrides = crate::probe::test_overrides();
        let fixture = http_fixture().await;
        overrides
            .lock()
            .expect("map")
            .insert("victim.test".to_owned(), vec![fixture]);
        let probes = ProbeClient::overridden(
            ProbePolicy {
                allow_local_targets: true,
                extra_roots: Vec::new(),
            },
            64 * 1024,
            overrides,
            // Ceiling 2, share 1: one workspace's take leaves exactly a second
            // workspace's fair split.
            Arc::new(crate::probe::AddressBudget::with_share(
                2,
                Duration::from_secs(3600),
                1,
                Duration::from_secs(3600),
            )),
        )
        .expect("probe client");
        let (results, mut results_rx) = mpsc::channel::<Observation>(8);
        let tracker = TaskTracker::new();
        let other = WorkspaceId::new();
        let dispatch = |name: &str, workspace: WorkspaceId| Dispatch {
            monitor_id: MonitorId(uuid::Uuid::now_v7()),
            workspace,
            generation: 1,
            slot_at: OffsetDateTime::now_utc(),
            url: Url::parse(&format!("http://victim.test/{name}")).expect("url"),
            method: monitor_domain::ProbeMethod::Get,
            expect: monitor_domain::Expect::standard(),
            // The fixture answers on accept; the deadline only bounds a hang,
            // so it is sized unreachable (N-6 re-review).
            deadline: Duration::from_secs(5),
            _global: Arc::new(Semaphore::new(4)).try_acquire_owned().expect("slot"),
            _origin: Arc::new(Semaphore::new(4)).try_acquire_owned().expect("slot"),
        };
        // The first workspace's probe pays the address's only start.
        let mut plan = Plan::default();
        plan.dispatches.push(dispatch("a", WorkspaceId::DEFAULT));
        plan.dispatch(&probes, &writer, &results, &tracker, &vitals)
            .await;
        let first = results_rx.recv().await.expect("an observation");
        assert!(
            matches!(first.slot, SlotObservation::Probe(_)),
            "the paying probe produces evidence, not a miss: {first:?}"
        );
        // The same workspace's next probe meets its spent share — but the OTHER
        // workspace's FIRST probe is still admitted (N-1): once the ceiling is
        // spent the split is max-min fair, so a second tenant behind the same
        // address never goes dark. Its SECOND probe — the fair split now held —
        // is the counted miss.
        let mut plan = Plan::default();
        plan.dispatches.push(dispatch("b", WorkspaceId::DEFAULT));
        plan.dispatches.push(dispatch("c1", other));
        plan.dispatches.push(dispatch("c2", other));
        plan.dispatch(&probes, &writer, &results, &tracker, &vitals)
            .await;
        let mut seen = Vec::new();
        for _ in 0..3 {
            seen.push(results_rx.recv().await.expect("an observation"));
        }
        let of = |workspace: WorkspaceId| {
            seen.iter()
                .filter(|obs| obs.workspace == workspace)
                .collect::<Vec<_>>()
        };
        let b = of(WorkspaceId::DEFAULT);
        assert_eq!(b.len(), 1);
        assert_eq!(
            b[0].slot,
            SlotObservation::Budgeted,
            "the same workspace's share is spent: {b:?}"
        );
        assert_eq!(b[0].detail.as_deref(), Some("address_budget"));
        let c = of(other);
        assert_eq!(
            c.iter()
                .filter(|obs| matches!(obs.slot, SlotObservation::Probe(_)))
                .count(),
            1,
            "the second workspace keeps its fair split — one start pays: {c:?}"
        );
        let c_missed: Vec<_> = c
            .iter()
            .filter(|obs| obs.slot == SlotObservation::Budgeted)
            .collect();
        assert_eq!(
            c_missed.len(),
            1,
            "past its split the second workspace misses too: {c:?}"
        );
        assert_eq!(c_missed[0].detail.as_deref(), Some("address_budget"));
    }

    /// B re-review: under the global per-address ceiling each workspace pays
    /// only its own share — the heaviest spender stops at its share of the
    /// address while a neighbour's schedule on the same IP keeps its own. The
    /// refused starts commit `missed` + `address_budget`, neutral on streaks.
    #[tokio::test]
    async fn the_heaviest_spender_stops_at_its_share_and_neighbours_keep_theirs() {
        let db = Db::connect("sqlite::memory:", 1).await.expect("db");
        db.migrate().await.expect("migrate");
        let (writer, _writer_task) = writer::spawn(
            db.clone(),
            Arc::new(RwLock::new(HashMap::new())),
            HashMap::new(),
            Arc::default(),
            32,
            false,
        );
        let vitals = Vitals::new(Arc::default(), Duration::from_secs(30));
        let overrides = crate::probe::test_overrides();
        let fixture = http_fixture().await;
        overrides
            .lock()
            .expect("map")
            .insert("victim.test".to_owned(), vec![fixture]);
        // The global ceiling admits 4; each workspace's share of the address
        // is 2 — the heaviest spender stops at its share, the ceiling for all.
        let probes = ProbeClient::overridden(
            ProbePolicy {
                allow_local_targets: true,
                extra_roots: Vec::new(),
            },
            64 * 1024,
            overrides,
            Arc::new(crate::probe::AddressBudget::with_share(
                4,
                Duration::from_secs(3600),
                2,
                Duration::from_secs(3600),
            )),
        )
        .expect("probe client");
        let (results, mut results_rx) = mpsc::channel::<Observation>(8);
        let tracker = TaskTracker::new();
        let spender = WorkspaceId::new();
        let neighbour = WorkspaceId::new();
        let dispatch = |name: &str, workspace: WorkspaceId| Dispatch {
            monitor_id: MonitorId(uuid::Uuid::now_v7()),
            workspace,
            generation: 1,
            slot_at: OffsetDateTime::now_utc(),
            url: Url::parse(&format!("http://victim.test/{name}")).expect("url"),
            method: monitor_domain::ProbeMethod::Get,
            expect: monitor_domain::Expect::standard(),
            // The fixture answers on accept; the deadline only bounds a hang,
            // so it is sized unreachable (N-6 re-review).
            deadline: Duration::from_secs(5),
            _global: Arc::new(Semaphore::new(4))
                .try_acquire_owned()
                .expect("slot"),
            _origin: Arc::new(Semaphore::new(4))
                .try_acquire_owned()
                .expect("slot"),
        };
        // The spender burns its own share of the address in two starts; the
        // third meets its share spent — while the global ceiling still has room.
        let mut plan = Plan::default();
        plan.dispatches.push(dispatch("a1", spender));
        plan.dispatches.push(dispatch("a2", spender));
        plan.dispatches.push(dispatch("a3", spender));
        plan.dispatches.push(dispatch("b1", neighbour));
        plan.dispatch(&probes, &writer, &results, &tracker, &vitals)
            .await;
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(results_rx.recv().await.expect("an observation"));
        }
        let of = |workspace: WorkspaceId| {
            seen.iter()
                .filter(|obs| obs.workspace == workspace)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            of(spender)
                .iter()
                .filter(|obs| matches!(obs.slot, SlotObservation::Probe(_)))
                .count(),
            2,
            "the spender's share paid its two starts: {seen:?}"
        );
        assert_eq!(
            of(spender)
                .iter()
                .filter(|obs| matches!(obs.slot, SlotObservation::Budgeted))
                .count(),
            1,
            "the spender's third start met its own spent share: {seen:?}"
        );
        assert!(
            of(neighbour)
                .iter()
                .all(|obs| matches!(obs.slot, SlotObservation::Probe(_))),
            "the neighbour's monitor kept its own share — never starved: {seen:?}"
        );
    }

    /// Within a workspace the share still binds: two same-origin monitors of one
    /// tenant compete for that workspace's per-origin slots.
    #[tokio::test(start_paused = true)]
    async fn the_share_binds_within_a_workspace() {
        let config = Config {
            per_origin_slots: 1,
            ..Config::default()
        };
        let monitors = fleet(2, 1, Duration::from_secs(15), Duration::from_secs(3));
        let mut runtimes = HashMap::new();
        let mut origins = HashMap::new();
        let clock = SlotClock::start();
        reconcile(&mut runtimes, &mut origins, &monitors, &config, &clock);
        for entry in runtimes.values_mut() {
            entry.next_slot = Instant::now(); // both due on this tick
        }
        let global = Arc::new(Semaphore::new(16));
        let plan = plan_due(&mut runtimes, &mut origins, &global, &config);
        assert_eq!(
            plan.dispatches.len(),
            1,
            "one workspace's two monitors on an origin got two per-origin slots"
        );
    }
}
