//! Email channel integration tests: a real outbox row on the `email` channel, a real SMTP
//! stub on loopback, the real writer and the real deliverer — the same wiring main.rs
//! builds, driven at lab speed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use lettre::message::Mailbox;
use lettre::transport::smtp::AsyncSmtpTransport;
use lettre::transport::smtp::client::Tls;
use lettre::{AsyncTransport, Tokio1Executor};
use monitor_db::{Db, Observation, writer};
use monitor_domain::{CheckOutcome, Monitor, NewMonitor, SlotObservation};
use monitor_server::deliver::{Config, Deliverer, Sink};
use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// A minimal SMTP peer: greets, answers EHLO, accepts MAIL/RCPT/DATA, and captures each
/// line of the wire (commands and DATA payload) for assertions. `reject_mail` replies
/// 550 to the first N `MAIL FROM` commands — a relay-side refusal the deliverer must retry.
async fn smtp_stub(reject_mail: usize) -> anyhow::Result<(u16, mpsc::Receiver<String>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let (tx, rx) = mpsc::channel(64);
    let reject = Arc::new(AtomicUsize::new(reject_mail));
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let tx = tx.clone();
            let reject = Arc::clone(&reject);
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut lines = BufReader::new(read).lines();
                let mut in_data = false;
                let _ = write.write_all(b"220 stub.local ESMTP\r\n").await;
                while let Ok(Some(line)) = lines.next_line().await {
                    let _ = tx.send(line.clone()).await;
                    if in_data {
                        if line == "." {
                            in_data = false;
                            let _ = write.write_all(b"250 2.0.0 queued\r\n").await;
                        }
                        continue;
                    }
                    let upper = line.to_ascii_uppercase();
                    let reply: &[u8] = if upper.starts_with("EHLO") || upper.starts_with("HELO") {
                        b"250-stub greets you\r\n250 8BITMIME\r\n"
                    } else if upper.starts_with("MAIL FROM") {
                        if reject
                            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                            .is_ok()
                        {
                            b"550 5.7.1 rejected for test\r\n"
                        } else {
                            b"250 2.1.0 OK\r\n"
                        }
                    } else if upper.starts_with("RCPT TO") {
                        b"250 2.1.5 OK\r\n"
                    } else if upper.starts_with("DATA") {
                        in_data = true;
                        b"354 End data with <CR><LF>.<CR><LF>\r\n"
                    } else if upper.starts_with("QUIT") {
                        let _ = write.write_all(b"221 2.0.0 bye\r\n").await;
                        break;
                    } else {
                        b"250 OK\r\n"
                    };
                    if write.write_all(reply).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    Ok((port, rx))
}

/// The rig with the email channel enabled — exactly what `queue_email` mirrors when
/// `SMON_SMTP_DSN` is set: incident transitions enqueue a row per channel.
async fn rig() -> anyhow::Result<(Db, writer::WriterHandle)> {
    let db = Db::connect("sqlite::memory:", 1).await?;
    db.migrate().await?;
    let statuses = Arc::new(RwLock::new(HashMap::new()));
    let (handle, _task) = writer::spawn(
        db.clone(),
        statuses,
        HashMap::new(),
        Arc::default(),
        8,
        true,
    );
    Ok((db, handle))
}

async fn monitor_with_events(db: &Db, writer: &writer::WriterHandle) -> anyhow::Result<Monitor> {
    let monitor =
        NewMonitor::new("fixture", url::Url::parse("https://example.com")?)?.into_monitor();
    db.insert_monitor(&monitor).await?;
    for _ in 0..3 {
        writer
            .record(Observation {
                monitor_id: monitor.id,
                generation: monitor.generation,
                slot_at: OffsetDateTime::now_utc(),
                slot: SlotObservation::Probe(CheckOutcome::Transport),
                http_status: None,
                latency_ms: None,
                detail: None,
            })
            .await?;
    }
    Ok(monitor)
}

fn email_sink(port: u16) -> anyhow::Result<Sink> {
    let mailer = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous("127.0.0.1")
        .port(port)
        .tls(Tls::None)
        .build();
    Ok(Sink::Email {
        mailer,
        from: "alerts@monitor.local".parse::<Mailbox>()?,
        to: vec!["oncall@example.com".parse::<Mailbox>()?],
    })
}

fn fast_config() -> Config {
    Config {
        poll_tick: Duration::from_millis(30),
        attempt_deadline: Duration::from_millis(500),
        max_attempts: 4,
        backoff_base: Duration::from_millis(20),
        backoff_cap: Duration::from_millis(200),
        concurrency: 4,
        batch: 8,
    }
}

/// The incident commits one outbox row per channel; the email deliverer claims only its own.
#[tokio::test]
async fn an_email_event_is_delivered_and_marked() -> anyhow::Result<()> {
    let (db, writer) = rig().await?;
    let monitor = monitor_with_events(&db, &writer).await?;
    let rows = db.list_outbox(monitor.id).await?;
    assert_eq!(rows.len(), 2, "one outbox row per configured channel");

    let (port, mut wire) = smtp_stub(0).await?;
    let shutdown = CancellationToken::new();
    let task = Deliverer::spawn(
        db.clone(),
        writer,
        fast_config(),
        email_sink(port)?,
        shutdown.clone(),
    );

    // Wait for the DATA payload to arrive, then for the row to be marked delivered.
    let mut transcript = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !transcript.contains("Incident: #") && std::time::Instant::now() < deadline {
        let Some(line) = tokio::time::timeout(Duration::from_secs(2), wire.recv()).await? else {
            break;
        };
        transcript.push_str(&line);
        transcript.push('\n');
    }
    let (email_event_id, email_attempts) = loop {
        let rows = db.list_outbox(monitor.id).await?;
        let email = rows.iter().find(|row| row.channel == "email");
        if let Some(row) = email
            && row.delivered_at.is_some()
        {
            break (row.event_id.clone(), row.attempts);
        }
        if std::time::Instant::now() > deadline {
            return Err(anyhow::anyhow!("email row never marked delivered"));
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    };

    assert!(
        transcript.contains("MAIL FROM:<alerts@monitor.local>"),
        "sender on the wire"
    );
    assert!(
        transcript.contains("RCPT TO:<oncall@example.com>"),
        "recipient on the wire"
    );
    assert!(
        transcript.contains("[service-monitor] fixture: incident opened"),
        "subject carries monitor + kind"
    );
    assert!(
        transcript.contains(&format!("Message-ID: <{email_event_id}@service-monitor>")),
        "the event id threads the message"
    );
    assert_eq!(email_attempts, 1);
    // The webhook row is a different channel's business — untouched by this deliverer.
    let webhook = rows
        .iter()
        .find(|row| row.channel == "webhook")
        .ok_or_else(|| anyhow::anyhow!("webhook row exists"))?;
    assert!(webhook.delivered_at.is_none());
    assert_eq!(webhook.attempts, 0);

    shutdown.cancel();
    task.await?;
    Ok(())
}

/// A relay-side refusal is a scheduled retry, not a lost event: attempt 1 gets 550, the row
/// books `next_attempt_at`, attempt 2 delivers.
#[tokio::test]
async fn a_rejected_email_is_a_scheduled_retry() -> anyhow::Result<()> {
    let (db, writer) = rig().await?;
    let monitor = monitor_with_events(&db, &writer).await?;
    let (port, mut wire) = smtp_stub(1).await?; // first MAIL FROM gets 550
    let shutdown = CancellationToken::new();
    let task = Deliverer::spawn(
        db.clone(),
        writer,
        fast_config(),
        email_sink(port)?,
        shutdown.clone(),
    );

    // First attempt fails; the retry lands and delivers.
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    let email_attempts = loop {
        let rows = db.list_outbox(monitor.id).await?;
        let email = rows.iter().find(|row| row.channel == "email");
        if let Some(row) = email
            && row.delivered_at.is_some()
        {
            break row.attempts;
        }
        if std::time::Instant::now() > deadline {
            return Err(anyhow::anyhow!("retry never delivered"));
        }
        tokio::time::sleep(Duration::from_millis(15)).await;
    };
    assert_eq!(email_attempts, 2, "rejected once, delivered on the retry");

    // Drain the wire so the assertion isn't order-fragile; both connections must appear.
    let mut mail_froms = 0;
    let drain_deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < drain_deadline {
        match tokio::time::timeout(Duration::from_millis(50), wire.recv()).await {
            Ok(Some(line)) if line.starts_with("MAIL FROM") => mail_froms += 1,
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    assert_eq!(mail_froms, 2, "one refused attempt + one delivered retry");

    shutdown.cancel();
    task.await?;
    Ok(())
}

/// A monitor name is operator text that lands in the email's Subject header and body. One
/// carrying CR/LF must stay one Subject line and one body line: no injected header (an
/// extra `Bcc:` would silently copy every alert to a stranger), no extra recipient.
#[tokio::test]
async fn a_name_with_line_breaks_cannot_inject_a_header_or_a_line() -> anyhow::Result<()> {
    let (db, writer) = rig().await?;
    let name = "web\r\nBcc: attacker@example.net\nX-Injected: 1";
    let monitor = NewMonitor::new(name, url::Url::parse("https://example.com")?)?.into_monitor();
    db.insert_monitor(&monitor).await?;
    for _ in 0..3 {
        writer
            .record(Observation {
                monitor_id: monitor.id,
                generation: monitor.generation,
                slot_at: OffsetDateTime::now_utc(),
                slot: SlotObservation::Probe(CheckOutcome::Transport),
                http_status: None,
                latency_ms: None,
                detail: None,
            })
            .await?;
    }
    let (port, mut wire) = smtp_stub(0).await?;
    let shutdown = CancellationToken::new();
    let task = Deliverer::spawn(
        db.clone(),
        writer,
        fast_config(),
        email_sink(port)?,
        shutdown.clone(),
    );
    let mut lines = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !lines.iter().any(|line: &String| line == ".") && std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(2), wire.recv()).await {
            Ok(Some(line)) => lines.push(line),
            _ => break,
        }
    }
    shutdown.cancel();
    task.await?;

    let data = lines
        .iter()
        .position(|line| line.eq_ignore_ascii_case("DATA"))
        .ok_or_else(|| anyhow::anyhow!("no DATA on the wire: {lines:?}"))?;
    let message = &lines[data + 1..];
    let blank = message
        .iter()
        .position(String::is_empty)
        .ok_or_else(|| anyhow::anyhow!("no header/body separator: {message:?}"))?;
    let (headers, body) = message.split_at(blank);
    let header_names: Vec<&str> = headers
        .iter()
        .filter(|line| !line.starts_with([' ', '\t']))
        .filter_map(|line| line.split_once(':').map(|(name, _)| name))
        .collect();
    for injected in ["Bcc", "X-Injected"] {
        assert!(
            !header_names
                .iter()
                .any(|h| h.eq_ignore_ascii_case(injected)),
            "{injected} header injected: {headers:?}"
        );
    }
    assert_eq!(
        header_names
            .iter()
            .filter(|h| h.eq_ignore_ascii_case("Subject"))
            .count(),
        1
    );
    assert!(
        !lines
            .iter()
            .any(|line| line.contains("attacker@example.net")
                && line.to_ascii_uppercase().starts_with("RCPT")),
        "no extra recipient: {lines:?}"
    );
    // The body names the monitor on one line too.
    let monitor_line = body
        .iter()
        .find(|line| line.starts_with("Monitor:"))
        .ok_or_else(|| anyhow::anyhow!("no Monitor line: {body:?}"))?;
    assert!(
        monitor_line.contains("Bcc: attacker@example.net") && monitor_line.contains("X-Injected"),
        "the whole name stays on the Monitor line: {body:?}"
    );
    assert!(
        !body
            .iter()
            .any(|line| line.starts_with("Bcc:") || line.starts_with("X-Injected")),
        "no line of the body starts with the injected text: {body:?}"
    );
    Ok(())
}

/// RUSTSEC-2026-0561: a `.` line inside the payload ends DATA early on a server that takes
/// a bare LF for an ending, and everything after it is read as fresh SMTP commands — a
/// second, attacker-written message over our authenticated session. Operator text can no
/// longer place a lone `.` on the wire: `one_line` keeps it inside the Monitor line, and
/// lettre 0.11.24 normalizes and dot-stuffs whatever the body does contain. The wire must
/// show ONE message, one terminator, no extra commands.
#[tokio::test]
async fn a_dot_terminated_name_cannot_splice_a_second_message() -> anyhow::Result<()> {
    let (db, writer) = rig().await?;
    let name = "web\n.\r\nMAIL FROM:<x@evil>\r\nRCPT TO:<x@evil>\r\nDATA\r\n.\r\npwned";
    let monitor = NewMonitor::new(name, url::Url::parse("https://example.com")?)?.into_monitor();
    db.insert_monitor(&monitor).await?;
    for _ in 0..3 {
        writer
            .record(Observation {
                monitor_id: monitor.id,
                generation: monitor.generation,
                slot_at: OffsetDateTime::now_utc(),
                slot: SlotObservation::Probe(CheckOutcome::Transport),
                http_status: None,
                latency_ms: None,
                detail: None,
            })
            .await?;
    }
    let (port, mut wire) = smtp_stub(0).await?;
    let shutdown = CancellationToken::new();
    let task = Deliverer::spawn(
        db.clone(),
        writer,
        fast_config(),
        email_sink(port)?,
        shutdown.clone(),
    );
    let mut lines = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(2), wire.recv()).await {
            Ok(Some(line)) if line.eq_ignore_ascii_case("QUIT") => {
                lines.push(line);
                break;
            }
            Ok(Some(line)) => lines.push(line),
            _ => break,
        }
    }
    shutdown.cancel();
    task.await?;

    // Every captured line is client→server: exactly one MAIL (the real sender), one DATA,
    // one `.` terminator, then QUIT — no second message was spliced in.
    let commands = |prefix: &str| {
        lines
            .iter()
            .filter(|line| line.to_ascii_uppercase().starts_with(prefix))
            .count()
    };
    assert_eq!(commands("MAIL FROM"), 1, "{lines:?}");
    assert_eq!(commands("DATA"), 1, "{lines:?}");
    assert_eq!(commands("RCPT TO"), 1, "{lines:?}");
    assert_eq!(
        lines.iter().filter(|line| line.as_str() == ".").count(),
        1,
        "one DATA terminator: {lines:?}"
    );
    // The attacker's addresses may only ever appear inside the DATA window as body
    // text — never on a command line.
    assert!(
        lines.iter().all(|line| {
            !line.contains("x@evil")
                || (!line.to_ascii_uppercase().starts_with("MAIL FROM")
                    && !line.to_ascii_uppercase().starts_with("RCPT TO"))
        }),
        "x@evil must never appear on a command line: {lines:?}"
    );
    let terminator = lines
        .iter()
        .position(|line| line == ".")
        .ok_or_else(|| anyhow::anyhow!("no DATA terminator: {lines:?}"))?;
    assert!(
        lines[terminator + 1..]
            .iter()
            .all(|line| line.eq_ignore_ascii_case("QUIT")),
        "nothing follows the terminator but QUIT: {lines:?}"
    );
    // The injected text did land — flattened onto the Monitor line, harmless.
    let data = lines
        .iter()
        .position(|line| line.eq_ignore_ascii_case("DATA"))
        .unwrap();
    assert!(
        lines[data..terminator]
            .iter()
            .any(|line| line.contains("x@evil")),
        "the payload is present as inert body text: {lines:?}"
    );
    Ok(())
}

/// The second layer under the one above: lettre 0.11.24 itself (RUSTSEC-2026-0561)
/// normalizes a String body's bare CR/LF to CRLF and dot-stuffs at send time, so the
/// defense does not rest on `one_line` alone. A body that DOES carry a bare `.` line —
/// sent straight through the transport, not through `deliver_email` — must still arrive
/// as one message: the `.` doubled, the command words inside the data window.
#[tokio::test]
async fn lettre_dot_stuffs_a_bare_dot_line_at_the_transport() -> anyhow::Result<()> {
    let (port, mut wire) = smtp_stub(0).await?;
    let mailer = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous("127.0.0.1")
        .port(port)
        .tls(Tls::None)
        .build();
    let message = lettre::Message::builder()
        .from("alerts@monitor.local".parse::<Mailbox>()?)
        .to("oncall@example.com".parse::<Mailbox>()?)
        .subject("x")
        .body(
            "before\n.\r\nMAIL FROM:<x@evil>\r\nRCPT TO:<x@evil>\r\nDATA\r\nafter\n".to_owned(),
        )?;
    mailer.send(message).await?;

    let mut lines = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(2), wire.recv()).await {
            Ok(Some(line)) if line.eq_ignore_ascii_case("QUIT") => {
                lines.push(line);
                break;
            }
            Ok(Some(line)) => lines.push(line),
            _ => break,
        }
    }
    // The stub records every wire line, commands and DATA payload alike — so verb counts
    // only apply OUTSIDE the data window (before DATA and after its `.` terminator).
    let data_at = lines
        .iter()
        .position(|line| line.eq_ignore_ascii_case("DATA"))
        .ok_or_else(|| anyhow::anyhow!("no DATA command: {lines:?}"))?;
    let terminator = lines
        .iter()
        .position(|line| line == ".")
        .ok_or_else(|| anyhow::anyhow!("no DATA terminator: {lines:?}"))?;
    let outside: Vec<&String> = lines[..data_at]
        .iter()
        .chain(lines[terminator + 1..].iter())
        .collect();
    let commands = |prefix: &str| {
        outside
            .iter()
            .filter(|line| line.to_ascii_uppercase().starts_with(prefix))
            .count()
    };
    assert_eq!(commands("MAIL FROM"), 1, "{lines:?}");
    assert_eq!(commands("RCPT TO"), 1, "{lines:?}");
    // The payload's `.` went over the wire doubled, so the stub never left DATA mode
    // for it; the single `.` is the real terminator, followed only by QUIT.
    assert!(
        lines[data_at..terminator].iter().any(|line| line == ".."),
        "the bare dot line arrived stuffed: {lines:?}"
    );
    assert!(
        outside
            .iter()
            .all(|line| { line.eq_ignore_ascii_case("QUIT") || !line.contains("x@evil") }),
        "nothing command-like follows or precedes the one message: {lines:?}"
    );
    Ok(())
}
