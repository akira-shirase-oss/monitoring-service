//! The notification deliverer: turns committed outbox rows into at-least-once delivery on
//! their channel — webhook POST or email — on its own pool and its own deadlines, so a hung
//! or failing receiver can occupy a delivery slot, never a probe slot.
//!
//! Writes still go through the single writer (`mark_delivery`): the deliverer computes the
//! schedule, the store records it. Every outbox row keeps one stable `event_id`, so a retried
//! delivery may arrive twice at the receiver but is never a second *logical* event.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use hmac::{Hmac, Mac};
use lettre::message::{Mailbox, header};
use lettre::transport::smtp::AsyncSmtpTransport;
use lettre::{AsyncTransport, Message, Tokio1Executor};
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::Serialize;
use sha2::Sha256;
use time::OffsetDateTime;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::{debug, info, warn};
use url::Url;

use monitor_db::{Db, DeliveryRow, WriterHandle};

type HmacSha256 = Hmac<Sha256>;

/// One channel's delivery endpoint — where an outbox row goes. `Sink::channel` is the
/// `notify_outbox.channel` filter, so a deliverer can only ever claim its own rows.
pub enum Sink {
    /// `POST` the JSON payload, optionally HMAC-signed.
    Webhook {
        target: Url,
        secret: Option<Vec<u8>>,
    },
    /// A plaintext message per event through an SMTP relay.
    Email {
        mailer: AsyncSmtpTransport<Tokio1Executor>,
        from: Mailbox,
        to: Vec<Mailbox>,
    },
}

impl Sink {
    fn channel(&self) -> &'static str {
        match self {
            Self::Webhook { .. } => "webhook",
            Self::Email { .. } => "email",
        }
    }

    /// What the logs may say — never credentials or full recipient lists.
    fn describe(&self) -> String {
        match self {
            Self::Webhook { target, .. } => redact(target),
            Self::Email { to, .. } => format!("smtp relay → {} recipient(s)", to.len()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// How often the outbox is polled for due events.
    pub poll_tick: Duration,
    /// Per-attempt deadline: a hanging receiver is a slow retry, not a stalled pool.
    pub attempt_deadline: Duration,
    /// Attempts before an event is terminal-exhausted (it stays in the table, marked).
    pub max_attempts: i64,
    /// Backoff base and ceiling; jitter is deterministic per event+attempt.
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
    /// Concurrent delivery attempts.
    pub concurrency: usize,
    /// Rows per poll.
    pub batch: i64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            poll_tick: Duration::from_secs(5),
            attempt_deadline: Duration::from_secs(5),
            max_attempts: 8,
            backoff_base: Duration::from_secs(1),
            backoff_cap: Duration::from_secs(60),
            concurrency: 8,
            batch: 32,
        }
    }
}

/// The JSON body receivers get. `event_id` is the dedup key — receivers should treat a
/// repeat as a retry, not a new incident.
#[derive(Debug, Serialize)]
struct Payload<'a> {
    event_id: &'a str,
    kind: &'a str,
    occurred_at: &'a OffsetDateTime,
    monitor: MonitorRef<'a>,
    incident: IncidentRef<'a>,
}

#[derive(Debug, Serialize)]
struct MonitorRef<'a> {
    id: &'a str,
    name: &'a str,
    url: &'a str,
}

#[derive(Debug, Serialize)]
struct IncidentRef<'a> {
    id: i64,
    opened_at: &'a OffsetDateTime,
    /// When the streak started — "failing since" is the fact a receiver acts on first.
    first_fail_at: Option<&'a OffsetDateTime>,
    last_fail_at: Option<&'a OffsetDateTime>,
    resolved_at: Option<&'a OffsetDateTime>,
}

impl<'a> From<&'a DeliveryRow> for Payload<'a> {
    fn from(row: &'a DeliveryRow) -> Self {
        Self {
            event_id: &row.event_id,
            kind: &row.kind,
            occurred_at: &row.created_at,
            monitor: MonitorRef {
                id: &row.monitor_id,
                name: &row.monitor_name,
                url: &row.monitor_url,
            },
            incident: IncidentRef {
                id: row.incident_id,
                opened_at: &row.opened_at,
                first_fail_at: row.first_fail_at.as_ref(),
                last_fail_at: row.last_fail_at.as_ref(),
                resolved_at: row.resolved_at.as_ref(),
            },
        }
    }
}

pub struct Deliverer;

impl Deliverer {
    /// Spawn the delivery loop for one channel. Callers spawn it only when the channel is
    /// configured; rows for an unconfigured channel simply never exist to claim.
    pub fn spawn(
        db: Db,
        writer: WriterHandle,
        config: Config,
        sink: Sink,
        shutdown: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(run(db, writer, config, sink, shutdown))
    }
}

/// Retry delay for attempt `n` (0-indexed after the failed one): exponential backoff to the
/// cap, jittered deterministically by `event_id` so a storm of events spreads instead of
/// synchronizing. FNV-1a keeps this dependency-free and stable across restarts.
fn retry_delay(event_id: &str, attempt: i64, base: Duration, cap: Duration) -> Duration {
    let shift = u32::try_from(attempt.max(0)).unwrap_or(u32::MAX).min(20);
    let exponential = base.saturating_mul(1u32 << shift).min(cap);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in event_id.as_bytes().iter().chain(&attempt.to_le_bytes()) {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    // Full jitter in [0, exponential): integer math, no float precision loss.
    let jittered_ms = exponential.as_millis() * u128::from(hash % 1000) / 1000;
    Duration::from_millis(u64::try_from(jittered_ms).unwrap_or(u64::MAX))
}

/// A sink with its one-time construction done — the webhook client is built once and reused
/// so delivery attempts share its pool.
enum Prepared {
    Webhook {
        client: reqwest::Client,
        target: Url,
        secret: Option<Vec<u8>>,
    },
    Email {
        mailer: AsyncSmtpTransport<Tokio1Executor>,
        from: Mailbox,
        to: Vec<Mailbox>,
    },
}

async fn run(
    db: Db,
    writer: WriterHandle,
    config: Config,
    sink: Sink,
    shutdown: CancellationToken,
) {
    let channel = sink.channel();
    let endpoint = sink.describe();
    let prepared = match sink {
        Sink::Webhook { target, secret } => {
            let client = match reqwest::Client::builder()
                .user_agent(concat!("service-monitor/", env!("CARGO_PKG_VERSION")))
                .build()
            {
                Ok(client) => client,
                Err(error) => {
                    warn!(%error, "webhook deliverer could not build an HTTP client; not delivering");
                    return;
                }
            };
            Prepared::Webhook {
                client,
                target,
                secret,
            }
        }
        Sink::Email { mailer, from, to } => Prepared::Email { mailer, from, to },
    };
    let prepared = Arc::new(prepared);

    let permits = Arc::new(Semaphore::new(config.concurrency));
    let tracker = TaskTracker::new();
    let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<String>(
        usize::try_from(config.batch).unwrap_or(32).max(1) * 2,
    );
    let mut in_flight: HashSet<String> = HashSet::new();
    let mut ticker = tokio::time::interval(config.poll_tick);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    info!(channel, endpoint, "deliverer running");

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                match db.pending_outbox(channel, config.max_attempts, config.batch).await {
                    Ok(rows) => {
                        for row in rows {
                            if in_flight.contains(&row.event_id) {
                                continue;
                            }
                            let Ok(permit) = permits.clone().try_acquire_owned() else {
                                debug!("delivery slots exhausted; remaining events wait a tick");
                                break;
                            };
                            in_flight.insert(row.event_id.clone());
                            tracker.spawn(attempt(
                                prepared.clone(),
                                writer.clone(),
                                config.clone(),
                                row,
                                permit,
                                done_tx.clone(),
                            ));
                        }
                    }
                    Err(error) => warn!(%error, channel, "outbox poll failed; retrying next tick"),
                }
            }
            done = done_rx.recv() => {
                match done {
                    Some(event_id) => {
                        in_flight.remove(&event_id);
                    }
                    None => break,
                }
            }
            () = shutdown.cancelled() => break,
        }
    }

    tracker.close();
    if tokio::time::timeout(config.attempt_deadline * 2, tracker.wait())
        .await
        .is_err()
    {
        warn!("deliverer shutdown deadline hit with attempts in flight");
    }
    info!("deliverer stopped");
}

async fn attempt(
    sink: Arc<Prepared>,
    writer: WriterHandle,
    config: Config,
    row: DeliveryRow,
    _permit: tokio::sync::OwnedSemaphorePermit,
    done: tokio::sync::mpsc::Sender<String>,
) {
    let event_id = row.event_id.clone();
    deliver(&sink, &writer, &config, &row).await;
    if done.send(event_id).await.is_err() {
        debug!("deliverer loop gone; completion not reported");
    }
}

async fn deliver(sink: &Prepared, writer: &WriterHandle, config: &Config, row: &DeliveryRow) {
    let delivered = match sink {
        Prepared::Webhook {
            client,
            target,
            secret,
        } => deliver_webhook(client, target, secret.as_deref(), config, row).await,
        Prepared::Email { mailer, from, to } => deliver_email(mailer, from, to, config, row).await,
    };

    let attempts = row.attempts + 1;
    if delivered {
        if let Err(error) = writer
            .mark_delivery(
                &row.event_id,
                attempts,
                None,
                Some(OffsetDateTime::now_utc()),
            )
            .await
        {
            warn!(%error, event = %row.event_id, "delivery not marked; it will retry");
        }
        return;
    }

    // Rejected or failed: either another attempt is scheduled, or the event is terminal-
    // exhausted and stays in the table as such.
    let exhausted = attempts >= config.max_attempts;
    let next = (!exhausted).then(|| {
        OffsetDateTime::now_utc()
            + retry_delay(
                &row.event_id,
                attempts,
                config.backoff_base,
                config.backoff_cap,
            )
    });
    debug!(event = %row.event_id, attempts, exhausted, "delivery attempt failed");
    if let Err(error) = writer
        .mark_delivery(&row.event_id, attempts, next, None)
        .await
    {
        warn!(%error, event = %row.event_id, "attempt mark failed");
    }
}

async fn deliver_webhook(
    client: &reqwest::Client,
    target: &Url,
    secret: Option<&[u8]>,
    config: &Config,
    row: &DeliveryRow,
) -> bool {
    let body = match serde_json::to_vec(&Payload::from(row)) {
        Ok(body) => body,
        Err(error) => {
            warn!(%error, event = %row.event_id, "payload serialize failed; event stays pending");
            return false;
        }
    };

    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Ok(id) = HeaderValue::from_str(&row.event_id) {
        headers.insert("x-smon-event", id);
    }
    if let Some(secret) = secret
        && let Ok(mut mac) = HmacSha256::new_from_slice(secret)
    {
        mac.update(&body);
        let signature = hex_lower(&mac.finalize().into_bytes());
        if let Ok(value) = HeaderValue::from_str(&format!("sha256={signature}")) {
            headers.insert("x-smon-signature", value);
        }
    }

    let send = client
        .post(target.clone())
        .headers(headers)
        .body(body)
        .send();
    matches!(
        tokio::time::timeout(config.attempt_deadline, send).await,
        Ok(Ok(response)) if response.status().is_success()
    )
}

/// One plaintext message per event. The subject is sanitized of CR/LF so a monitor name can
/// never inject a header line; `Message-ID` carries the `event_id` so a retry threads onto
/// the same logical event at the receiver.
async fn deliver_email(
    mailer: &AsyncSmtpTransport<Tokio1Executor>,
    from: &Mailbox,
    to: &[Mailbox],
    config: &Config,
    row: &DeliveryRow,
) -> bool {
    let mut message = Message::builder().from(from.clone());
    for recipient in to {
        message = message.to(recipient.clone());
    }
    let kind = row.kind.replace('_', " ");
    let subject = format!(
        "[service-monitor] {}: {kind}",
        row.monitor_name.replace(['\r', '\n'], " ")
    );
    let body = email_body(row, &kind);
    let message = match message
        .subject(subject)
        .header(header::MessageId::from(format!(
            "<{}@service-monitor>",
            row.event_id
        )))
        .body(body)
    {
        Ok(message) => message,
        Err(error) => {
            warn!(%error, event = %row.event_id, "email build failed; event stays pending");
            return false;
        }
    };
    matches!(
        tokio::time::timeout(config.attempt_deadline, mailer.send(message)).await,
        Ok(Ok(response)) if response.is_positive()
    )
}

fn email_body(row: &DeliveryRow, kind: &str) -> String {
    use std::fmt::Write;
    let mut body = format!(
        "Service Monitor: {kind}\n\nEvent:    {}\nMonitor:  {} <{}>\nIncident: #{}\nOpened:   {}",
        row.event_id, row.monitor_name, row.monitor_url, row.incident_id, row.opened_at
    );
    if let Some(first_fail) = row.first_fail_at {
        let _ = write!(body, "\nFailing since: {first_fail}");
    }
    if let Some(last_fail) = row.last_fail_at {
        let _ = write!(body, "\nLast failure:  {last_fail}");
    }
    if let Some(resolved) = row.resolved_at {
        let _ = write!(body, "\nResolved:      {resolved}");
    }
    let _ = write!(body, "\nOccurred: {}\n", row.created_at);
    body
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
}

fn redact(url: &Url) -> String {
    let mut redacted = url.clone();
    let _ = redacted.set_password(None);
    redacted.set_username("").ok();
    redacted.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> DeliveryRow {
        let now = OffsetDateTime::now_utc();
        DeliveryRow {
            event_id: "evt_01".to_owned(),
            kind: "incident_opened".to_owned(),
            channel: "webhook".to_owned(),
            attempts: 0,
            created_at: now,
            monitor_id: "mon_1".to_owned(),
            monitor_name: "fixture".to_owned(),
            monitor_url: "https://example.com/health".to_owned(),
            incident_id: 7,
            opened_at: now,
            first_fail_at: Some(now),
            last_fail_at: Some(now),
            resolved_at: None,
        }
    }

    #[test]
    fn retry_delay_is_deterministic_per_event_and_attempt() {
        let base = Duration::from_secs(1);
        let cap = Duration::from_secs(60);
        assert_eq!(
            retry_delay("evt_a", 2, base, cap),
            retry_delay("evt_a", 2, base, cap)
        );
        // Different events scatter instead of retrying in lockstep.
        assert_ne!(
            retry_delay("evt_a", 2, base, cap),
            retry_delay("evt_b", 2, base, cap)
        );
    }

    #[test]
    fn retry_delay_never_exceeds_the_exponential_bound() {
        let base = Duration::from_millis(500);
        let cap = Duration::from_secs(8);
        for attempt in 0..10_i64 {
            for event in ["a", "b", "c", "evt_01", "xyz"] {
                let delay = retry_delay(event, attempt, base, cap);
                let shift = u32::try_from(attempt).unwrap_or(u32::MAX).min(20);
                let bound = base.saturating_mul(1u32 << shift).min(cap);
                assert!(
                    delay <= bound,
                    "event {event} attempt {attempt}: {delay:?} > {bound:?}"
                );
            }
        }
    }

    #[test]
    fn retry_delay_saturates_at_the_cap() {
        let delay = retry_delay(
            "evt",
            i64::MAX,
            Duration::from_secs(1),
            Duration::from_secs(60),
        );
        assert!(delay <= Duration::from_secs(60));
    }

    #[test]
    fn payload_carries_event_identity_and_incident() {
        let row = row();
        let body = serde_json::to_vec(&Payload::from(&row)).expect("serializes");
        let parsed: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(parsed["event_id"], "evt_01");
        assert_eq!(parsed["kind"], "incident_opened");
        assert_eq!(parsed["monitor"]["id"], "mon_1");
        assert_eq!(parsed["monitor"]["name"], "fixture");
        assert_eq!(parsed["incident"]["id"], 7);
        assert!(parsed["incident"]["resolved_at"].is_null());
    }

    #[test]
    fn signature_is_hmac_sha256_over_the_body() {
        let row = row();
        let body = serde_json::to_vec(&Payload::from(&row)).expect("serializes");
        let secret = b"test-secret";
        let mut mac = HmacSha256::new_from_slice(secret).expect("key");
        mac.update(&body);
        let signature = hex_lower(&mac.finalize().into_bytes());
        assert_eq!(signature.len(), 64);
        // Tampered body produces a different signature — the receiver can detect it.
        let mut tampered = HmacSha256::new_from_slice(secret).expect("key");
        tampered.update(b"{}");
        assert_ne!(signature, hex_lower(&tampered.finalize().into_bytes()));
    }

    #[test]
    fn redact_strips_credentials_from_logged_urls() {
        let url = Url::parse("https://user:hunter2@example.com/hook").expect("url");
        let shown = redact(&url);
        assert!(!shown.contains("hunter2"), "password must not be logged");
        assert!(!shown.contains("user"), "username must not be logged");
    }
}
