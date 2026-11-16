//! Log redaction, checked against the deliverer's real log output. This test lives in its
//! own binary on purpose: `tracing` caches per-callsite interest process-wide, and other
//! tests that spawn deliverers on parallel threads without a subscriber can race that cache
//! so the captured log comes back empty. Alone in its process, the capture is exact.

use std::sync::Arc;
use std::time::Duration;

use monitor_db::{Db, writer};
use monitor_server::deliver::{Config, Deliverer, Sink};
use tokio_util::sync::CancellationToken;

async fn rig() -> anyhow::Result<(Db, writer::WriterHandle)> {
    let db = Db::connect("sqlite::memory:", 1).await?;
    db.migrate().await?;
    let (handle, _task) = writer::spawn(
        db.clone(),
        Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
        std::collections::HashMap::new(),
        Arc::default(),
        8,
        false,
    );
    Ok((db, handle))
}

fn fast_config() -> Config {
    Config {
        poll_tick: Duration::from_millis(30),
        attempt_deadline: Duration::from_millis(400),
        max_attempts: 3,
        backoff_base: Duration::from_millis(20),
        backoff_cap: Duration::from_millis(200),
        concurrency: 4,
        batch: 8,
    }
}

fn webhook_sink(target: &str, secret: Option<&[u8]>) -> anyhow::Result<Sink> {
    Ok(Sink::Webhook {
        target: url::Url::parse(target)?,
        secret: secret.map(<[u8]>::to_vec),
    })
}

/// A `MakeWriter` that appends every formatted log line to a shared buffer, so a test can
/// read exactly what an operator's log pipeline would receive.
#[derive(Clone, Default)]
struct Captured(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(
            &self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
        .into_owned()
    }
}

/// Slack, Discord and most chat webhooks carry their credential in the path (or a query
/// token): whoever reads the URL can post to the channel. The deliverer logs its endpoint at
/// every boot, so the log line may say scheme + host and nothing else.
#[tokio::test]
async fn webhook_secrets_in_the_path_or_query_never_reach_the_logs() -> anyhow::Result<()> {
    let captured = Captured::default();
    let writer_for_logs = captured.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .json()
        .with_writer(move || writer_for_logs.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let secrets = [
        (
            "https://hooks.slack.com/services/T0SECRET1/B0SECRET2/XoXoSlackToken123",
            "https://hooks.slack.com",
            &["T0SECRET1", "B0SECRET2", "XoXoSlackToken123", "/services"][..],
        ),
        (
            "https://discord.com/api/webhooks/1234567890/DiscordTokenAbc",
            "https://discord.com",
            &["1234567890", "DiscordTokenAbc", "/api/webhooks"][..],
        ),
        (
            "https://ops:hunter2@hooks.example.com:8443/in?token=QueryToken42#frag",
            "https://hooks.example.com:8443",
            &["hunter2", "ops", "QueryToken42", "token=", "/in", "frag"][..],
        ),
    ];
    for (n, (target, shown, hidden)) in secrets.into_iter().enumerate() {
        let (db, writer) = rig().await?;
        let shutdown = CancellationToken::new();
        let task = Deliverer::spawn(
            db,
            writer,
            fast_config(),
            webhook_sink(target, Some(b"the-secret"))?,
            shutdown.clone(),
        );
        // The endpoint line is logged once the client is built; wait for it, not a timer.
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while captured.text().matches("deliverer running").count() <= n
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        shutdown.cancel();
        task.await?;

        let logs = captured.text();
        assert!(
            logs.contains(shown),
            "the log should still say where alerts go ({shown}): {logs}"
        );
        for secret in hidden {
            assert!(
                !logs.contains(secret),
                "{secret:?} from {target} leaked into the logs: {logs}"
            );
        }
    }
    Ok(())
}
