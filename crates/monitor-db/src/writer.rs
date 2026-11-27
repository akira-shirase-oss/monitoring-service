//! The single-writer task: the only code path that writes to the store.
//!
//! One task owns both the write connection *and* the per-monitor `Health` machines, so a
//! slot observation, its incident transition and the matching outbox row commit in a single
//! transaction — and the in-memory machine only advances once the commit lands. If the
//! commit fails, the observation never happened: nothing is painted green that the store
//! didn't keep.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use thiserror::Error;
use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use monitor_domain::{Effect, Health, MonitorId, SlotObservation, Statuses, WorkspaceId};

use crate::{Db, QueryError, RetainReport, RetentionCutoffs};

/// Rows of each kind one retention command handles (D-012). Small enough that one command
/// stays short, so an observation queued behind retention waits for one batch, not for a
/// whole first pass over months of history. Not a measured optimum.
pub const RETAIN_BATCH: u32 = 5_000;

/// The statuses readers see — written only after commits, never from in-flight results.
pub type SharedStatuses = Arc<RwLock<Statuses>>;

/// Consecutive failed *observation* commits. `0` means evidence is landing; anything above
/// it means the last N observations could not be stored (disk full, I/O error) and readers
/// like `/readyz` must report unavailable rather than lie green. Bumped on every failed
/// observation commit and reset on the first successful one — recovery is proven by a
/// landed write, not by a timer. Retention and delivery bookkeeping do not count.
pub type WriteHealth = Arc<AtomicU64>;

/// One slot's product, sent to the writer by the coordinator.
#[derive(Debug)]
pub struct Observation {
    pub monitor_id: MonitorId,
    /// The workspace the monitor belongs to — its evidence lands scoped by it (D-020).
    pub workspace: WorkspaceId,
    /// The monitor generation the slot was claimed under; stale results are dropped.
    pub generation: u64,
    /// The scheduled slot time — not when the row lands.
    pub slot_at: OffsetDateTime,
    pub slot: SlotObservation,
    /// HTTP status when a response arrived (passes keep it too — 204 is useful context).
    pub http_status: Option<u16>,
    pub latency_ms: Option<u64>,
    /// Classified, sanitized detail — never tokens, headers or bodies.
    pub detail: Option<String>,
}

/// What the writer did with an observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// Committed; the state machine produced this effect.
    Committed(Effect),
    /// The monitor's generation moved on — an edit or pause invalidated the slot.
    DroppedStaleGeneration,
    /// The monitor is gone.
    DroppedUnknownMonitor,
}

#[derive(Debug, Error)]
pub enum WriterError {
    #[error("writer task is gone")]
    Dead,
    #[error(transparent)]
    Query(#[from] QueryError),
}

enum Command {
    Record {
        observation: Box<Observation>,
        reply: oneshot::Sender<Result<WriteOutcome, QueryError>>,
    },
    /// The deliverer's bookkeeping: attempt count, scheduled retry or terminal state,
    /// and the classified reason the attempt failed. Delivery writes go through the
    /// same single writer — the store has one owner.
    MarkDelivery {
        event_id: String,
        attempts: i64,
        next_attempt_at: Option<OffsetDateTime>,
        delivered_at: Option<OffsetDateTime>,
        detail: Option<String>,
        reply: oneshot::Sender<Result<(), QueryError>>,
    },
    /// A retention pass: aggregate + delete expired checks in one transaction. Goes
    /// through the writer like everything else — the store has one owner. Each
    /// workspace expires on its own plan's cutoffs (S3, D-022).
    Retain {
        workspaces: Arc<[(WorkspaceId, RetentionCutoffs)]>,
        batch: u32,
        reply: oneshot::Sender<Result<RetainReport, QueryError>>,
    },
}

/// Cloneable sender for the single writer.
#[derive(Debug, Clone)]
pub struct WriterHandle {
    tx: mpsc::Sender<Command>,
}

impl WriterHandle {
    /// Submit an observation and await its committed effect. Callers that only need fire-and-
    /// forward semantics can still await this — the writer *is* the serialization point, so
    /// awaiting keeps the coordinator honestly behind the store.
    pub async fn record(&self, observation: Observation) -> Result<WriteOutcome, WriterError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::Record {
                observation: Box::new(observation),
                reply,
            })
            .await
            .map_err(|_| WriterError::Dead)?;
        rx.await.map_err(|_| WriterError::Dead)?.map_err(Into::into)
    }

    /// Book a delivery outcome. `delivered_at: Some` marks the event terminal-delivered;
    /// `delivered_at: None, next_attempt_at: Some` schedules a retry; `None`/`None` with
    /// `attempts` at the max is terminal-exhausted. `detail` is the classified failure
    /// (`rejected_local`, `deadline`, `http_503`…); a delivery clears it.
    pub async fn mark_delivery(
        &self,
        event_id: &str,
        attempts: i64,
        next_attempt_at: Option<OffsetDateTime>,
        delivered_at: Option<OffsetDateTime>,
        detail: Option<&str>,
    ) -> Result<(), WriterError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::MarkDelivery {
                event_id: event_id.to_owned(),
                attempts,
                next_attempt_at,
                delivered_at,
                detail: detail.map(str::to_owned),
                reply,
            })
            .await
            .map_err(|_| WriterError::Dead)?;
        rx.await.map_err(|_| WriterError::Dead)?.map_err(Into::into)
    }

    /// Run one retention pass (D-012, `Db::retain`): expired checks roll into daily
    /// aggregates; old aggregates, resolved incidents, ended windows and delivered outbox
    /// rows are pruned — one transaction. `workspaces` maps each workspace to the
    /// cutoffs its plan resolves to (S3, D-022); open incidents are never touched.
    pub async fn retain(
        &self,
        workspaces: &[(WorkspaceId, RetentionCutoffs)],
    ) -> Result<RetainReport, WriterError> {
        self.retain_in_batches(workspaces, RETAIN_BATCH).await
    }

    /// A full retention pass as a series of bounded writer commands — `batch` rows of each
    /// kind per command, repeated until a batch comes back short. Every command is its own
    /// transaction, so observations queued meanwhile commit between batches instead of
    /// waiting out the whole pass. Returns the folded report.
    pub async fn retain_in_batches(
        &self,
        workspaces: &[(WorkspaceId, RetentionCutoffs)],
        batch: u32,
    ) -> Result<RetainReport, WriterError> {
        let workspaces: Arc<[(WorkspaceId, RetentionCutoffs)]> = workspaces.into();
        let mut total = RetainReport::default();
        loop {
            let (reply, rx) = oneshot::channel();
            self.tx
                .send(Command::Retain {
                    workspaces: workspaces.clone(),
                    batch,
                    reply,
                })
                .await
                .map_err(|_| WriterError::Dead)?;
            let report = rx.await.map_err(|_| WriterError::Dead)??;
            total.add(&report);
            if report.exhausted(batch) {
                return Ok(total);
            }
        }
    }
}

/// Spawn the writer. `health` is the boot-rebuilt map (`Health::rebuild` per monitor); the
/// writer owns it from here. `write_health` is bumped on every failed commit and reset on
/// every landed one. `queue_email` mirrors the alert config: when SMTP is configured,
/// incident transitions enqueue an `email` outbox row alongside the webhook one. The task
/// exits when every handle is dropped.
pub fn spawn(
    db: Db,
    statuses: SharedStatuses,
    health: impl IntoIterator<Item = (MonitorId, Health)>,
    write_health: WriteHealth,
    channel_bound: usize,
    queue_email: bool,
) -> (WriterHandle, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(channel_bound);
    let handle = WriterHandle { tx };
    let task = tokio::spawn(run(
        db,
        statuses,
        health.into_iter().collect(),
        write_health,
        queue_email,
        rx,
    ));
    (handle, task)
}

async fn run(
    db: Db,
    statuses: SharedStatuses,
    mut health: HashMap<MonitorId, Health>,
    write_health: WriteHealth,
    queue_email: bool,
    mut rx: mpsc::Receiver<Command>,
) {
    while let Some(command) = rx.recv().await {
        match command {
            Command::Record { observation, reply } => {
                let outcome = record(&db, &statuses, &mut health, queue_email, *observation).await;
                track_write(&write_health, &outcome);
                if reply.send(outcome).is_err() {
                    debug!("observation reply channel closed; result still committed");
                }
            }
            Command::MarkDelivery {
                event_id,
                attempts,
                next_attempt_at,
                delivered_at,
                detail,
                reply,
            } => {
                // Delivery bookkeeping is a supporting duty: its failure is the
                // deliverer's to log and retry, not evidence that observing stopped.
                let outcome = db
                    .update_delivery(
                        &event_id,
                        attempts,
                        next_attempt_at,
                        delivered_at,
                        detail.as_deref(),
                    )
                    .await;
                if reply.send(outcome).is_err() {
                    debug!("delivery reply channel closed; mark still committed");
                }
            }
            Command::Retain {
                workspaces,
                batch,
                reply,
            } => {
                // Retention is a supporting duty too: a failed pass degrades the monitor
                // (the retention task reports it), it does not make it unavailable.
                let outcome = db.retain(&workspaces, batch).await;
                if reply.send(outcome).is_err() {
                    debug!("retain reply channel closed; pass still committed");
                }
            }
        }
    }
}

/// Observation commit landed → the write path is proven healthy; failed → consecutive
/// failures grow. Only observation commits count (`WriteHealth` means "evidence is not
/// landing"); dropped observations are not failures — no commit was attempted.
fn track_write<T>(write_health: &WriteHealth, outcome: &Result<T, QueryError>) {
    match outcome {
        Ok(_) => write_health.store(0, Ordering::Relaxed),
        Err(_) => {
            write_health.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Commit one observation; `false` when the monitor no longer exists.
///
/// The gate is read before the commit's own transaction, so a delete (D-026) can land
/// in between. The store then refuses the check row because its monitor is gone. That
/// is not the store failing — returning the error would count against `WriteHealth`
/// and flip every workspace's verdict to "unavailable" over one deleted monitor — so a
/// refused commit is asked one more question: is the monitor still there? Only when it
/// is does the error stand.
async fn commit_unless_gone(
    db: &Db,
    observation: &Observation,
    effect: Effect,
    open_after: bool,
    queue_email: bool,
) -> Result<bool, QueryError> {
    let Err(error) = db
        .commit_observation(observation, effect, open_after, queue_email)
        .await
    else {
        return Ok(true);
    };
    let gone = db
        .workspace(observation.workspace)
        .monitor_gate(observation.monitor_id)
        .await;
    match gone {
        Ok(None) => Ok(false),
        // Still there, or the store can't even be asked: the commit's own error stands.
        Ok(Some(_)) | Err(_) => Err(error),
    }
}

async fn record(
    db: &Db,
    statuses: &SharedStatuses,
    health: &mut HashMap<MonitorId, Health>,
    queue_email: bool,
    observation: Observation,
) -> Result<WriteOutcome, QueryError> {
    let Some(gate) = db
        .workspace(observation.workspace)
        .monitor_gate(observation.monitor_id)
        .await?
    else {
        // A deleted monitor leaves nothing behind in memory either.
        health.remove(&observation.monitor_id);
        if let Ok(mut map) = statuses.write() {
            map.remove(&observation.monitor_id);
        }
        warn!(monitor = %observation.monitor_id.0, "dropping observation for unknown monitor");
        return Ok(WriteOutcome::DroppedUnknownMonitor);
    };
    if gate.generation != observation.generation {
        debug!(
            monitor = %observation.monitor_id.0,
            stored = gate.generation, claimed = observation.generation,
            "dropping stale-generation observation"
        );
        return Ok(WriteOutcome::DroppedStaleGeneration);
    }

    // The state machine only advances on a clone: if the commit fails, `health` keeps the
    // last committed state, so memory can never be ahead of the store. Thresholds come from
    // the same committed read as the generation — an edit takes effect here atomically.
    let mut next = health
        .get(&observation.monitor_id)
        .cloned()
        .unwrap_or_default();
    let effect = next.record(observation.slot, gate.thresholds, gate.maintenance);
    if !commit_unless_gone(db, &observation, effect, next.incident_open(), queue_email).await? {
        // Deleted between the gate read and the commit: forget it here too.
        health.remove(&observation.monitor_id);
        if let Ok(mut map) = statuses.write() {
            map.remove(&observation.monitor_id);
        }
        warn!(monitor = %observation.monitor_id.0, "dropping observation: the monitor was deleted");
        return Ok(WriteOutcome::DroppedUnknownMonitor);
    }
    let status = next.status();
    health.insert(observation.monitor_id, next);
    if let Ok(mut map) = statuses.write() {
        map.insert(observation.monitor_id, status);
    }
    Ok(WriteOutcome::Committed(effect))
}

#[cfg(test)]
mod tests {
    use super::*;
    use monitor_domain::{CheckOutcome, NewMonitor, SlotObservation, WorkspaceId};

    /// The second half of the delete race, made deterministic: the gate was read, the
    /// monitor is deleted with its evidence, and only then does the commit arrive. It
    /// reports "gone" instead of a store error — the error is what used to count as a
    /// failed write and turn the whole engine's verdict to unavailable.
    #[tokio::test]
    async fn a_commit_for_a_monitor_deleted_meanwhile_reports_gone() -> anyhow::Result<()> {
        let db = Db::connect("sqlite::memory:", 1).await?;
        db.migrate().await?;
        let scoped = db.workspace(WorkspaceId::DEFAULT);
        let monitor = NewMonitor::new("fixture", url::Url::parse("https://example.com/health")?)?
            .into_monitor();
        scoped.insert_monitor(&monitor).await?;
        let observation = Observation {
            monitor_id: monitor.id,
            workspace: monitor.workspace,
            generation: monitor.generation,
            slot_at: time::OffsetDateTime::now_utc(),
            slot: SlotObservation::Probe(CheckOutcome::Pass),
            http_status: Some(200),
            latency_ms: Some(5),
            detail: None,
        };
        let effect = Health::default().record(observation.slot, monitor.thresholds, false);

        // While the monitor exists the commit lands.
        assert!(commit_unless_gone(&db, &observation, effect, false, false).await?);

        assert!(scoped.delete_monitor_with_evidence(monitor.id).await?);
        // The raw commit is refused by the store…
        assert!(
            db.commit_observation(&observation, effect, false, false)
                .await
                .is_err(),
            "a check row for a deleted monitor has nothing to belong to"
        );
        // …and the writer's wrapper turns that refusal into "gone", not an error.
        assert!(!commit_unless_gone(&db, &observation, effect, false, false).await?);
        Ok(())
    }
}
