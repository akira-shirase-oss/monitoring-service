//! The single-writer task: the only code path that writes to the store.
//!
//! One task owns both the write connection *and* the per-monitor `Health` machines, so a
//! slot observation, its incident transition and the matching outbox row commit in a single
//! transaction — and the in-memory machine only advances once the commit lands. If the
//! commit fails, the observation never happened: nothing is painted green that the store
//! didn't keep.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use thiserror::Error;
use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use monitor_domain::{Effect, Health, MonitorId, SlotObservation, Statuses};

use crate::{Db, QueryError};

/// The statuses readers see — written only after commits, never from in-flight results.
pub type SharedStatuses = Arc<RwLock<Statuses>>;

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
}

/// Spawn the writer. `health` is the boot-rebuilt map (`Health::rebuild` per monitor); the
/// writer owns it from here. The task exits when every handle is dropped.
pub fn spawn(
    db: Db,
    statuses: SharedStatuses,
    health: impl IntoIterator<Item = (MonitorId, Health)>,
    channel_bound: usize,
) -> (WriterHandle, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(channel_bound);
    let handle = WriterHandle { tx };
    let task = tokio::spawn(run(db, statuses, health.into_iter().collect(), rx));
    (handle, task)
}

async fn run(
    db: Db,
    statuses: SharedStatuses,
    mut health: HashMap<MonitorId, Health>,
    mut rx: mpsc::Receiver<Command>,
) {
    while let Some(command) = rx.recv().await {
        match command {
            Command::Record { observation, reply } => {
                let outcome = record(&db, &statuses, &mut health, *observation).await;
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
                if reply.send(outcome).is_err() {
                    debug!("delivery reply channel closed; mark still committed");
                }
            }
        }
    }
}

async fn record(
    db: &Db,
    statuses: &SharedStatuses,
    health: &mut HashMap<MonitorId, Health>,
    observation: Observation,
) -> Result<WriteOutcome, QueryError> {
    let Some(stored) = db.monitor_generation(observation.monitor_id).await? else {
        warn!(monitor = %observation.monitor_id.0, "dropping observation for unknown monitor");
        return Ok(WriteOutcome::DroppedUnknownMonitor);
    };
    if stored != observation.generation {
        debug!(
            monitor = %observation.monitor_id.0,
            stored, claimed = observation.generation,
            "dropping stale-generation observation"
        );
        return Ok(WriteOutcome::DroppedStaleGeneration);
    }

    // The state machine only advances on a clone: if the commit fails, `health` keeps the
    // last committed state, so memory can never be ahead of the store.
    let mut next = health
        .get(&observation.monitor_id)
        .cloned()
        .unwrap_or_default();
    let effect = next.record(observation.slot);
    db.commit_observation(&observation, effect).await?;
    let status = next.status();
    health.insert(observation.monitor_id, next);
    if let Ok(mut map) = statuses.write() {
        map.insert(observation.monitor_id, status);
    }
    Ok(WriteOutcome::Committed(effect))
}
