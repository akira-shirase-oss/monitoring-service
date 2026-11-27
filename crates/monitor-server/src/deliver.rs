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

use hmac::{Hmac, KeyInit, Mac};
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
use uuid::Uuid;

use monitor_db::{Db, DeliveryRow, WriterHandle};

use crate::probe;

type HmacSha256 = Hmac<Sha256>;

/// One channel's delivery endpoint — where an outbox row goes. `Sink::channel` is the
/// `notify_outbox.channel` filter, so a deliverer can only ever claim its own rows.
pub enum Sink {
    /// `POST` the JSON payload, optionally HMAC-signed.
    Webhook {
        target: Url,
        secret: Option<Vec<u8>>,
    },
    /// The tenants' own webhooks (D-017): target and secret come from each row's
    /// workspace at send time, and the DNS answer is re-validated inside the connect
    /// — a URL that passed at save cannot ride a rebound lookup to a private address.
    /// The sink carries the validating resolver itself: the client is built *from* it,
    /// so there is no tenant send that could resolve unvalidated.
    TenantWebhook { resolver: probe::PolicyResolver },
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
            Self::TenantWebhook { .. } => "tenant_webhook",
            Self::Email { .. } => "email",
        }
    }

    /// What the logs may say — scheme and host only (chat webhooks carry their secret in
    /// the path or query), never credentials or full recipient lists.
    fn describe(&self) -> String {
        match self {
            Self::Webhook { target, .. } => crate::redact::origin(target),
            // Tenant targets live on the rows, not the sink — there is no single
            // endpoint to name, and logging per-row targets is the URL leak D-017 bars.
            Self::TenantWebhook { .. } => "per-workspace webhooks".to_owned(),
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
    /// Scheme, host, port and path — userinfo and query are stripped (`redact::for_alert`):
    /// an alert lands in a channel or inbox whose readers are not the monitor's admins.
    url: String,
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
                url: crate::redact::for_alert_str(&row.monitor_url),
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
    /// The strict client every tenant send shares — the validating resolver is inside
    /// it, so there is no client that could ever skip the check. The policy rides
    /// alongside for the literal check: reqwest never calls the resolver for an IP
    /// already spelled out in the URL, so each send re-runs `check` too — the send
    /// gate does not trust that a row was ever screened at save.
    TenantWebhook {
        client: reqwest::Client,
        policy: probe::ProbePolicy,
    },
    Email {
        mailer: AsyncSmtpTransport<Tokio1Executor>,
        from: Mailbox,
        to: Vec<Mailbox>,
    },
}

/// The tenant-webhook client (D-017): no proxy, no redirects — and `PolicyResolver`
/// inside, so every send re-resolves and reqwest only ever dials addresses `is_local`
/// cleared. The check is *inside* the connect: DNS cannot race a validated answer into
/// a different dial between the check and the socket.
fn tenant_client(resolver: probe::PolicyResolver) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .dns_resolver(Arc::new(resolver))
        .user_agent(concat!("service-monitor/", env!("CARGO_PKG_VERSION")))
        .build()
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
            // Direct to the configured receiver: proxy environment variables must not
            // silently route alert payloads (monitor names, incident times) through a
            // third host. Egress proxies are not supported (module 04).
            // Redirects are not followed: a 307/308 would re-send the payload and its
            // signature to wherever the receiver points — another host, or one inside our
            // network. A 3xx is a failed attempt, retried like any other.
            let client = match reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
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
        Sink::TenantWebhook { resolver } => {
            let allow_local_targets = resolver.allows_local();
            let client = match tenant_client(resolver) {
                Ok(client) => client,
                Err(error) => {
                    warn!(%error, "tenant webhook deliverer could not build an HTTP client; not delivering");
                    return;
                }
            };
            Prepared::TenantWebhook {
                client,
                policy: probe::ProbePolicy {
                    allow_local_targets,
                    extra_roots: Vec::new(),
                },
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
    let result: Result<(), String> = match sink {
        Prepared::Webhook {
            client,
            target,
            secret,
        } => deliver_webhook(client, target, secret.as_deref(), config, row).await,
        Prepared::TenantWebhook { client, policy } => match &row.webhook_url {
            // D-017's send gate, independent of the save gate: re-parse, re-run the
            // scheme/literal policy (`check` — the resolver never sees an IP literal),
            // and let the strict client re-resolve + revalidate names at dial.
            Some(raw) => match Url::parse(raw) {
                Ok(target) => match policy.check(&target) {
                    Ok(()) => {
                        deliver_webhook(
                            client,
                            &target,
                            row.webhook_secret.as_deref().map(str::as_bytes),
                            config,
                            row,
                        )
                        .await
                    }
                    Err(rejected) => Err(probe::rejected_detail(rejected).to_owned()),
                },
                Err(_) => Err("bad_url".to_owned()),
            },
            // The channel was cleared between enqueue and attempt — a visible terminal
            // state, not a silent drop.
            None => Err("no_target".to_owned()),
        },
        Prepared::Email { mailer, from, to } => deliver_email(mailer, from, to, config, row).await,
    };

    let attempts = row.attempts + 1;
    if result.is_ok() {
        if let Err(error) = writer
            .mark_delivery(
                &row.event_id,
                attempts,
                None,
                Some(OffsetDateTime::now_utc()),
                None,
            )
            .await
        {
            warn!(%error, event = %row.event_id, "delivery not marked; it will retry");
        }
        return;
    }

    // Rejected or failed: either another attempt is scheduled, or the event is terminal-
    // exhausted and stays in the table as such. The classified reason lands on the row —
    // a send-time SSRF refusal is evidence (`rejected_local`), never silence.
    let detail = result.err();
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
    debug!(event = %row.event_id, attempts, exhausted, detail = detail.as_deref().unwrap_or(""), "delivery attempt failed");
    if let Err(error) = writer
        .mark_delivery(&row.event_id, attempts, next, None, detail.as_deref())
        .await
    {
        warn!(%error, event = %row.event_id, "attempt mark failed");
    }
}

/// `Err(detail)` is the classified failure — the string committed to
/// `notify_outbox.last_error`, never a reqwest message (they can embed URLs).
async fn deliver_webhook(
    client: &reqwest::Client,
    target: &Url,
    secret: Option<&[u8]>,
    config: &Config,
    row: &DeliveryRow,
) -> Result<(), String> {
    let body = match serde_json::to_vec(&Payload::from(row)) {
        Ok(body) => body,
        Err(error) => {
            warn!(%error, event = %row.event_id, "payload serialize failed; event stays pending");
            return Err("serialize".to_owned());
        }
    };
    post_signed(
        client,
        target,
        secret,
        &row.event_id,
        body,
        config.attempt_deadline,
    )
    .await
}

/// One signed JSON POST under a deadline — the shared send behind the outbox's
/// `deliver_webhook` and the member-facing "send test alert" ([`test_send`]). The
/// response resolves on *headers* — the body is never read, so the response-size
/// cap is literal: zero bytes, and a receiver streaming forever still can't hold
/// the attempt past its deadline. `Err(detail)` is the classified word, never a
/// reqwest message.
async fn post_signed(
    client: &reqwest::Client,
    target: &Url,
    secret: Option<&[u8]>,
    event_id: &str,
    body: Vec<u8>,
    deadline: Duration,
) -> Result<(), String> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Ok(id) = HeaderValue::from_str(event_id) {
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
    match tokio::time::timeout(deadline, send).await {
        Ok(Ok(response)) if response.status().is_success() => Ok(()),
        Ok(Ok(response)) => Err(format!("http_{}", response.status().as_u16())),
        Ok(Err(error)) => Err(probe::classify(&error).to_owned()),
        Err(_elapsed) => Err("deadline".to_owned()),
    }
}

/// "Send test alert" (S2): one send through the exact D-017 gate — the same
/// scheme/literal check, a client built from the validating `PolicyResolver` (so
/// DNS is re-resolved and revalidated inside the connect), no proxy, no redirects,
/// the workspace's own secret on the signature. `kind: "test"` tells receivers this
/// is a probe of the plumbing, not an incident; the row it stands on is synthetic
/// and never lands in the outbox. `Err(detail)` is the classified reason, shown
/// to the member verbatim.
pub async fn test_send(
    allow_local_targets: bool,
    target: &Url,
    secret: Option<&[u8]>,
    deadline: Duration,
) -> Result<(), String> {
    let policy = probe::ProbePolicy {
        allow_local_targets,
        extra_roots: Vec::new(),
    };
    policy
        .check(target)
        .map_err(|rejected| probe::rejected_detail(rejected).to_owned())?;
    let client =
        tenant_client(probe::PolicyResolver::new(allow_local_targets)).map_err(|error| {
            warn!(%error, "test-alert client could not be built");
            "connect".to_owned()
        })?;
    let now = OffsetDateTime::now_utc();
    let row = DeliveryRow {
        event_id: format!("test_{}", Uuid::now_v7().simple()),
        kind: "test".to_owned(),
        channel: "tenant_webhook".to_owned(),
        attempts: 0,
        created_at: now,
        monitor_id: "test".to_owned(),
        monitor_name: "Test alert".to_owned(),
        monitor_url: String::new(),
        incident_id: 0,
        opened_at: now,
        first_fail_at: None,
        last_fail_at: None,
        resolved_at: None,
        webhook_url: None,
        webhook_secret: None,
    };
    let body = serde_json::to_vec(&Payload::from(&row)).map_err(|error| {
        warn!(%error, "test-alert payload serialize failed");
        "serialize".to_owned()
    })?;
    post_signed(&client, target, secret, &row.event_id, body, deadline).await
}

/// One plaintext message per event. The monitor name is made one line ([`one_line`]) in
/// the subject and the body, so it can never inject a header or a line; `Message-ID` carries the `event_id` so a retry threads onto
/// the same logical event at the receiver.
async fn deliver_email(
    mailer: &AsyncSmtpTransport<Tokio1Executor>,
    from: &Mailbox,
    to: &[Mailbox],
    config: &Config,
    row: &DeliveryRow,
) -> Result<(), String> {
    let mut message = Message::builder().from(from.clone());
    for recipient in to {
        message = message.to(recipient.clone());
    }
    let kind = row.kind.replace('_', " ");
    let subject = format!("[service-monitor] {}: {kind}", one_line(&row.monitor_name));
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
            return Err("build".to_owned());
        }
    };
    match tokio::time::timeout(config.attempt_deadline, mailer.send(message)).await {
        Ok(Ok(response)) if response.is_positive() => Ok(()),
        Ok(Ok(_)) => Err("smtp_rejected".to_owned()),
        Ok(Err(_)) => Err("smtp".to_owned()),
        Err(_elapsed) => Err("deadline".to_owned()),
    }
}

/// Operator text (a monitor name) as one line: every control character — CR and LF
/// above all — becomes a space, so it can neither start a header in the Subject nor fake a
/// line in the body.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn email_body(row: &DeliveryRow, kind: &str) -> String {
    use std::fmt::Write;
    let mut body = format!(
        "Service Monitor: {kind}\n\nEvent:    {}\nMonitor:  {} <{}>\nIncident: #{}\nOpened:   {}",
        row.event_id,
        one_line(&row.monitor_name),
        crate::redact::for_alert_str(&row.monitor_url),
        row.incident_id,
        row.opened_at
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

pub(crate) fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
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
            webhook_url: None,
            webhook_secret: None,
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

    /// Known answer, RFC 4231 test case 2: what every receiver verifies against. Pinned so
    /// a crate upgrade (hmac, sha2) can never change a signature silently.
    #[test]
    fn hmac_sha256_matches_rfc_4231() {
        let mut mac = HmacSha256::new_from_slice(b"Jefe").expect("key");
        mac.update(b"what do ya want for nothing?");
        assert_eq!(
            hex_lower(&mac.finalize().into_bytes()),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
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
    fn the_logged_endpoint_is_the_origin_only() {
        let sink = Sink::Webhook {
            target: Url::parse("https://user:hunter2@hooks.slack.com/services/T0/B0/XYZ?x=1")
                .expect("url"),
            secret: None,
        };
        assert_eq!(sink.describe(), "https://hooks.slack.com");
    }

    #[test]
    fn email_body_strips_credentials_from_the_monitor_url() {
        let mut row = row();
        row.monitor_url = "https://u:pw@example.com/health?token=T0K".to_owned();
        let body = email_body(&row, "incident opened");
        assert!(body.contains("<https://example.com/health>"), "{body}");
        assert!(!body.contains("pw") && !body.contains("T0K"), "{body}");
    }

    // ---- D-017 tenant-webhook send gate --------------------------------------
    // These live in-crate because `PolicyResolver::for_test` is `cfg(test)`: the
    // rebinding proof needs a DNS answer that flips beneath a stored target.

    use std::collections::HashMap;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, RwLock};

    use axum::body::Body;
    use axum::extract::State;
    use axum::http::{Request, StatusCode};
    use axum::response::Response;
    use axum::routing::post;
    use http_body_util::BodyExt;
    use monitor_db::{Observation, OutboxRow, writer};
    use monitor_domain::{CheckOutcome, Monitor, NewMonitor, SlotObservation, WorkspaceId};
    use tokio::net::TcpListener;
    use tokio::sync::mpsc;

    #[derive(Clone)]
    struct Stub {
        hits: mpsc::Sender<(HeaderMap, Vec<u8>)>,
        fail_first: Arc<AtomicUsize>,
        hang: bool,
        redirect_to: Option<String>,
    }

    async fn receiver(State(stub): State<Stub>, request: Request<Body>) -> Response {
        let (parts, body) = request.into_parts();
        let bytes = body
            .collect()
            .await
            .map(|c| c.to_bytes().to_vec())
            .unwrap_or_default();
        let _ = stub.hits.send((parts.headers, bytes)).await;
        if stub.hang {
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        if let Some(location) = &stub.redirect_to {
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::FOUND;
            response.headers_mut().insert(
                "location",
                HeaderValue::from_str(location).expect("ascii url"),
            );
            return response;
        }
        if stub
            .fail_first
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            let mut response = Response::new(Body::from("fail"));
            *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            return response;
        }
        Response::new(Body::empty())
    }

    async fn stub(
        fail_first: usize,
        hang: bool,
        redirect_to: Option<String>,
    ) -> (String, mpsc::Receiver<(HeaderMap, Vec<u8>)>) {
        let (hits_tx, hits_rx) = mpsc::channel(64);
        let app = axum::Router::new()
            .route("/", post(receiver))
            .with_state(Stub {
                hits: hits_tx,
                fail_first: Arc::new(AtomicUsize::new(fail_first)),
                hang,
                redirect_to,
            });
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://127.0.0.1:{port}/"), hits_rx)
    }

    async fn rig() -> (Db, writer::WriterHandle) {
        let db = Db::connect("sqlite::memory:", 1).await.expect("db");
        db.migrate().await.expect("migrate");
        let statuses = Arc::new(RwLock::new(HashMap::new()));
        let (handle, _task) = writer::spawn(
            db.clone(),
            statuses,
            HashMap::new(),
            Arc::default(),
            8,
            false,
        );
        (db, handle)
    }

    /// Set the workspace webhook *before* the incident so its `tenant_webhook` row is
    /// planned; then drive three failures. `url` writes through the store directly —
    /// the send gate is what is under test here, so a URL the save gate would have
    /// refused is exactly the input wanted.
    async fn tenant_event(
        db: &Db,
        writer: &writer::WriterHandle,
        url: &str,
    ) -> (Monitor, monitor_db::WebhookRow) {
        let webhook = db
            .member(WorkspaceId::DEFAULT)
            .set_webhook(url)
            .await
            .expect("webhook stored");
        let monitor = NewMonitor::new("fixture", Url::parse("https://example.com").expect("url"))
            .expect("monitor")
            .into_monitor();
        db.workspace(WorkspaceId::DEFAULT)
            .insert_monitor(&monitor)
            .await
            .expect("insert");
        for _ in 0..3 {
            writer
                .record(Observation {
                    monitor_id: monitor.id,
                    workspace: monitor.workspace,
                    generation: monitor.generation,
                    slot_at: OffsetDateTime::now_utc(),
                    slot: SlotObservation::Probe(CheckOutcome::Transport),
                    http_status: None,
                    latency_ms: None,
                    detail: None,
                })
                .await
                .expect("recorded");
        }
        (monitor, webhook)
    }

    async fn tenant_row(
        db: &Db,
        monitor: &Monitor,
        pred: impl Fn(&OutboxRow) -> bool,
    ) -> OutboxRow {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(row) = db
                .workspace(WorkspaceId::DEFAULT)
                .list_outbox(monitor.id)
                .await
                .expect("outbox")
                .into_iter()
                .find(|row| row.channel == "tenant_webhook")
                && (pred(&row) || std::time::Instant::now() > deadline)
            {
                return row;
            }
            assert!(
                std::time::Instant::now() <= deadline,
                "no tenant_webhook row appeared for {}",
                monitor.id.0
            );
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }

    fn fast() -> Config {
        Config {
            poll_tick: Duration::from_millis(30),
            attempt_deadline: Duration::from_millis(400),
            max_attempts: 1,
            backoff_base: Duration::from_millis(20),
            backoff_cap: Duration::from_millis(200),
            concurrency: 4,
            batch: 8,
        }
    }

    fn strict_sink() -> Sink {
        Sink::TenantWebhook {
            resolver: probe::PolicyResolver::new(false),
        }
    }

    /// The store's `last_error` after one refused send for `url` — a fresh rig per URL
    /// so every range is judged on its own.
    async fn refused_at_send(url: &str) -> Option<String> {
        let (db, writer) = rig().await;
        let (monitor, _wh) = tenant_event(&db, &writer, url).await;
        let shutdown = CancellationToken::new();
        let task = Deliverer::spawn(db.clone(), writer, fast(), strict_sink(), shutdown.clone());
        let row = tenant_row(&db, &monitor, |r| r.attempts >= 1).await;
        shutdown.cancel();
        task.await.expect("deliverer joins");
        row.last_error
    }

    /// The literal side of the send gate: the URL was never screened at save (the test
    /// wrote it straight into the store) and reqwest never consults DNS for an IP
    /// literal — `policy.check` at send is the only thing between it and a socket, and
    /// it refuses every non-public form with the same word as the probes.
    #[tokio::test]
    async fn tenant_send_refuses_every_blocked_literal() {
        let (stub_url, mut hits) = stub(0, false, None).await;
        let port: u16 = stub_url
            .trim_end_matches('/')
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .expect("stub port");
        for (url, detail) in [
            ("http://127.0.0.1/hook", "rejected_local"),
            ("http://0.0.0.0/hook", "rejected_local"),
            ("http://255.255.255.255/hook", "rejected_local"),
            ("http://10.1.2.3/hook", "rejected_local"),
            ("http://172.16.5.4/hook", "rejected_local"),
            ("http://192.168.1.1/hook", "rejected_local"),
            ("http://169.254.0.1/hook", "rejected_local"),
            ("http://169.254.169.254/latest/meta-data", "rejected_local"),
            ("http://100.64.1.1/hook", "rejected_local"),
            ("http://192.0.2.1/hook", "rejected_local"),
            ("http://198.51.100.7/hook", "rejected_local"),
            ("http://203.0.113.9/hook", "rejected_local"),
            ("http://198.18.0.1/hook", "rejected_local"),
            ("http://192.0.0.9/hook", "rejected_local"),
            ("http://192.88.99.1/hook", "rejected_local"),
            ("http://224.0.0.1/hook", "rejected_local"),
            ("http://240.0.0.1/hook", "rejected_local"),
            ("http://[::1]/hook", "rejected_local"),
            ("http://[::]/hook", "rejected_local"),
            ("http://[fe80::1]/hook", "rejected_local"),
            ("http://[fd12:3456::1]/hook", "rejected_local"),
            ("http://[fd00:ec2::254]/hook", "rejected_local"),
            ("http://[ff02::1]/hook", "rejected_local"),
            ("http://[::ffff:127.0.0.1]/hook", "rejected_local"),
            ("http://[::ffff:10.0.0.1]/hook", "rejected_local"),
            ("http://[::ffff:169.254.169.254]/hook", "rejected_local"),
            ("ftp://hooks.example.com/x", "rejected_scheme"),
            ("file:///etc/passwd", "rejected_scheme"),
        ] {
            assert_eq!(refused_at_send(url).await.as_deref(), Some(detail), "{url}");
        }
        // The strongest "never sent": this target is a live listener — the refusal
        // happened before any dial, so it saw nothing.
        assert_eq!(
            refused_at_send(&format!("http://127.0.0.1:{port}/hook"))
                .await
                .as_deref(),
            Some("rejected_local")
        );
        assert!(
            hits.try_recv().is_err(),
            "the stub saw a dial it must not have"
        );
    }

    /// D-017's core scenario, end to end: a name that was public when the webhook was
    /// configured re-answers with a private address at send. The override map is the
    /// lookup — validation is the resolution itself, so the flipped answer is refused
    /// inside the connect, recorded as `rejected_local`, and the receiver it names is
    /// a live stub that never sees a packet.
    #[tokio::test]
    async fn a_rebound_name_is_refused_inside_the_connect_and_recorded() {
        let (db, writer) = rig().await;
        let (stub_url, mut hits) = stub(0, false, None).await;
        let stub_port: u16 = stub_url
            .trim_end_matches('/')
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .expect("stub port");
        let url = format!("http://hooks.tenant.example:{stub_port}/hook");
        let (monitor, _wh) = tenant_event(&db, &writer, &url).await;

        let overrides: Arc<Mutex<HashMap<String, Vec<SocketAddr>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        // "Public at save": the name first answers a public (if unreachable) address.
        overrides.lock().expect("map").insert(
            "hooks.tenant.example".to_owned(),
            vec![SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34)),
                stub_port,
            )],
        );
        let shutdown = CancellationToken::new();
        let task = Deliverer::spawn(
            db.clone(),
            writer,
            Config {
                max_attempts: 3,
                ..fast()
            },
            Sink::TenantWebhook {
                resolver: probe::PolicyResolver::for_test(false, overrides.clone()),
            },
            shutdown.clone(),
        );
        // Attempt one dials the public answer and fails there — evidence the override
        // was validated as public and used.
        let row = tenant_row(&db, &monitor, |r| r.attempts >= 1 && r.last_error.is_some()).await;
        assert_ne!(row.last_error.as_deref(), Some("rejected_local"));

        // "Private at send": the same name now answers the stub's own loopback address.
        overrides.lock().expect("map").insert(
            "hooks.tenant.example".to_owned(),
            vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), stub_port)],
        );
        let row = tenant_row(&db, &monitor, |r| r.attempts >= 3 && r.last_error.is_some()).await;
        shutdown.cancel();
        task.await.expect("deliverer joins");
        assert_eq!(row.last_error.as_deref(), Some("rejected_local"));
        assert!(row.delivered_at.is_none());
        assert!(row.next_attempt_at.is_none(), "exhausted stays terminal");
        // The flip pointed at a live receiver — nothing ever reached it.
        assert!(hits.try_recv().is_err(), "payload must never be sent");
    }

    /// Redirects are off for tenant sends: a receiver answering 302 at a private
    /// destination is not followed — the attempt fails as `http_302` and the redirect
    /// target never sees the payload. (`lab` lets both stubs live on loopback; had the
    /// hop been followed it would have reached the second.)
    #[tokio::test]
    async fn a_redirect_to_a_private_address_is_never_followed() {
        let (db, writer) = rig().await;
        let (b_url, mut b_hits) = stub(0, false, None).await;
        let (a_url, mut a_hits) = stub(0, false, Some(b_url.clone())).await;
        let (monitor, _wh) = tenant_event(&db, &writer, &a_url).await;
        let shutdown = CancellationToken::new();
        let task = Deliverer::spawn(
            db.clone(),
            writer,
            fast(),
            Sink::TenantWebhook {
                resolver: probe::PolicyResolver::new(true),
            },
            shutdown.clone(),
        );
        let row = tenant_row(&db, &monitor, |r| r.attempts >= 1).await;
        shutdown.cancel();
        task.await.expect("deliverer joins");
        assert_eq!(row.last_error.as_deref(), Some("http_302"));
        assert!(
            a_hits.try_recv().is_ok(),
            "the post did reach the first hop"
        );
        assert!(
            b_hits.try_recv().is_err(),
            "the redirect target saw nothing"
        );
    }

    /// A real tenant send: the per-workspace secret signs the body, the row is marked
    /// delivered — and a failure's note on an earlier attempt is cleared, not kept as
    /// stale evidence.
    #[tokio::test]
    async fn a_tenant_send_signs_with_its_workspace_secret_and_clears_notes() {
        let (db, writer) = rig().await;
        let (stub_url, mut hits) = stub(1, false, None).await; // first attempt 500s
        let (monitor, webhook) = tenant_event(&db, &writer, &stub_url).await;
        let shutdown = CancellationToken::new();
        let task = Deliverer::spawn(
            db.clone(),
            writer,
            Config {
                max_attempts: 3,
                ..fast()
            },
            Sink::TenantWebhook {
                resolver: probe::PolicyResolver::new(true),
            },
            shutdown.clone(),
        );
        // The failed first attempt records its classified note.
        let row = tenant_row(&db, &monitor, |r| r.attempts >= 1).await;
        assert_eq!(row.last_error.as_deref(), Some("http_500"));

        let (headers, body) = tokio::time::timeout(Duration::from_secs(5), hits.recv())
            .await
            .expect("hit")
            .expect("the retry arrives");
        let signature = headers
            .get("x-smon-signature")
            .and_then(|v| v.to_str().ok())
            .expect("signed");
        let mut mac = HmacSha256::new_from_slice(webhook.secret.as_bytes()).expect("key");
        mac.update(&body);
        assert_eq!(
            signature,
            format!("sha256={}", hex_lower(&mac.finalize().into_bytes()))
        );
        let row = tenant_row(&db, &monitor, |r| r.delivered_at.is_some()).await;
        assert!(row.last_error.is_none(), "success clears the stale note");
        shutdown.cancel();
        task.await.expect("deliverer joins");
    }

    /// A workspace that cleared its webhook after the event was queued is a visible
    /// terminal state on the row — never a silent drop.
    #[tokio::test]
    async fn a_cleared_channel_reports_no_target() {
        let (db, writer) = rig().await;
        let (monitor, _wh) = tenant_event(&db, &writer, "http://93.184.216.34/hook").await;
        db.member(WorkspaceId::DEFAULT)
            .clear_webhook()
            .await
            .expect("cleared");
        let shutdown = CancellationToken::new();
        let task = Deliverer::spawn(db.clone(), writer, fast(), strict_sink(), shutdown.clone());
        let row = tenant_row(&db, &monitor, |r| r.attempts >= 1).await;
        shutdown.cancel();
        task.await.expect("deliverer joins");
        assert_eq!(row.last_error.as_deref(), Some("no_target"));
    }

    /// The per-attempt deadline applies to tenant sends too — a hanging receiver is a
    /// `deadline` note, not a stalled delivery slot.
    #[tokio::test]
    async fn a_hanging_tenant_receiver_meets_the_attempt_deadline() {
        let (db, writer) = rig().await;
        let (stub_url, _hits) = stub(0, true, None).await;
        let (monitor, _wh) = tenant_event(&db, &writer, &stub_url).await;
        let shutdown = CancellationToken::new();
        let task = Deliverer::spawn(
            db.clone(),
            writer,
            fast(),
            Sink::TenantWebhook {
                resolver: probe::PolicyResolver::new(true),
            },
            shutdown.clone(),
        );
        let row = tenant_row(&db, &monitor, |r| r.attempts >= 1).await;
        shutdown.cancel();
        task.await.expect("deliverer joins");
        assert_eq!(row.last_error.as_deref(), Some("deadline"));
    }
}
