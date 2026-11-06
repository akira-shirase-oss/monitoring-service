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

use monitor_domain::{Effect, Health, MonitorId, SlotObservation, Statuses};

use crate::{Db, QueryError, RetainReport};

/// The statuses readers see — written only after commits, never from in-flight results.
pub type SharedStatuses = Arc<RwLock<Statuses>>;

/// Consecutive store-write failures observed by the writer. `0` means the write path is
/// healthy; anything above it means the last N commits failed (disk full, I/O error) and
/// readers like `/readyz` must report unavailable rather than lie green. The counter is
/// bumped by the writer on every failed commit and reset on the first successful one —
/// recovery is proven by a landed write, not by a timer.
pub type WriteHealth = Arc<AtomicU64>;

/// One slot's product, sent to the writer by the coordinator.
#[derive(Debug)]
pub struct Observation {
    pub monitor_id: MonitorId,
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
    /// The deliverer's bookkeeping: attempt count, scheduled retry or terminal state.
    /// Delivery writes go through the same single writer — the store has one owner.
    MarkDelivery {
        event_id: String,
        attempts: i64,
        next_attempt_at: Option<OffsetDateTime>,
        delivered_at: Option<OffsetDateTime>,
        reply: oneshot::Sender<Result<(), QueryError>>,
    },
    /// A retention pass: aggregate + delete expired checks in one transaction. Goes
    /// through the writer like everything else — the store has one owner.
    Retain {
        cutoff: OffsetDateTime,
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
    /// `attempts` at the max is terminal-exhausted.
    pub async fn mark_delivery(
        &self,
        event_id: &str,
        attempts: i64,
        next_attempt_at: Option<OffsetDateTime>,
        delivered_at: Option<OffsetDateTime>,
    ) -> Result<(), WriterError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::MarkDelivery {
                event_id: event_id.to_owned(),
                attempts,
                next_attempt_at,
                delivered_at,
                reply,
            })
            .await
            .map_err(|_| WriterError::Dead)?;
        rx.await.map_err(|_| WriterError::Dead)?.map_err(Into::into)
    }

    /// Run one retention pass: checks older than `cutoff` are rolled into daily
    /// aggregates and deleted in the same transaction. Open incidents and the outbox
    /// are never touched.
    pub async fn retain(&self, cutoff: OffsetDateTime) -> Result<RetainReport, WriterError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::Retain { cutoff, reply })
            .await
            .map_err(|_| WriterError::Dead)?;
        rx.await.map_err(|_| WriterError::Dead)?.map_err(Into::into)
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
                reply,
            } => {
                let outcome = db
                    .update_delivery(&event_id, attempts, next_attempt_at, delivered_at)
                    .await;
                track_write(&write_health, &outcome);
                if reply.send(outcome).is_err() {
                    debug!("delivery reply channel closed; mark still committed");
                }
            }
            Command::Retain { cutoff, reply } => {
                let outcome = db.retain(cutoff).await;
                track_write(&write_health, &outcome);
                if reply.send(outcome).is_err() {
                    debug!("retain reply channel closed; pass still committed");
                }
            }
        }
    }
}

/// Commit landed → the write path is proven healthy; commit failed → consecutive failures
/// grow. Dropped observations are *not* write failures — no commit was attempted.
fn track_write<T>(write_health: &WriteHealth, outcome: &Result<T, QueryError>) {
    match outcome {
        Ok(_) => write_health.store(0, Ordering::Relaxed),
        Err(_) => {
            write_health.fetch_add(1, Ordering::Relaxed);
        }
    }
}

async fn record(
    db: &Db,
    statuses: &SharedStatuses,
    health: &mut HashMap<MonitorId, Health>,
    queue_email: bool,
    observation: Observation,
) -> Result<WriteOutcome, QueryError> {
    let Some(gate) = db.monitor_gate(observation.monitor_id).await? else {
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
    db.commit_observation(&observation, effect, next.incident_open(), queue_email)
        .await?;
    let status = next.status();
    health.insert(observation.monitor_id, next);
    if let Ok(mut map) = statuses.write() {
        map.insert(observation.monitor_id, status);
    }
    Ok(WriteOutcome::Committed(effect))
}
