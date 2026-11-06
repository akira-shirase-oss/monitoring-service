//! `monitor` — the command line. It talks to the server's HTTP API, the same path the dashboard
//! uses: there is no private back door into the store.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use uuid::Uuid;

#[derive(Parser)]
#[command(name = "monitor", version, about = "Service Monitor's command line")]
struct Cli {
    /// The monitor-server base URL.
    #[arg(long, env = "SMON_SERVER", default_value = "http://127.0.0.1:58400")]
    server: String,
    /// Management credential — sent as `Authorization: Bearer` when the server requires it.
    #[arg(long, env = "SMON_ADMIN_TOKEN")]
    token: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check whether the server answers livez and readyz.
    Ping,
    /// Manage monitors.
    Monitors {
        #[command(subcommand)]
        action: Monitors,
    },
}

#[derive(Subcommand)]
enum Monitors {
    /// List registered monitors.
    List,
    /// Register a monitor (defaults: 60-second interval, 5-second deadline, 3-fail / 2-pass).
    Add {
        name: String,
        url: String,
        #[arg(long)]
        interval_s: Option<u64>,
        #[arg(long)]
        timeout_s: Option<u64>,
        /// Consecutive failures that open an incident (1-10, default 3).
        #[arg(long)]
        failures_to_open: Option<u8>,
        /// Fresh passes while open that confirm recovery (1-10, default 2).
        #[arg(long)]
        passes_to_recover: Option<u8>,
    },
    /// Show one monitor with its recent checks and incidents.
    Show { id: String },
    /// Change a monitor's fields. Any change bumps its generation, which is what makes a
    /// probe that was already in flight stale — the edit and the invalidation are the same
    /// committed write.
    Edit {
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        interval_s: Option<u64>,
        #[arg(long)]
        timeout_s: Option<u64>,
        /// Consecutive failures that open an incident (1-10).
        #[arg(long)]
        failures_to_open: Option<u8>,
        /// Fresh passes while open that confirm recovery (1-10).
        #[arg(long)]
        passes_to_recover: Option<u8>,
    },
    /// Stop scheduling checks. The monitor's record stays; nothing is checked until `unpause`.
    Pause { id: String },
    /// Resume scheduling checks.
    Unpause { id: String },
    /// Delete a monitor that has never recorded a check or incident. A monitor with history
    /// can't be deleted — pause it instead.
    Delete { id: String },
}

#[derive(Deserialize)]
struct MonitorView {
    id: Uuid,
    name: String,
    url: String,
    interval_s: u64,
    failures_to_open: u8,
    passes_to_recover: u8,
    generation: u64,
    paused: bool,
    status: String,
}

#[derive(Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Deserialize)]
struct ErrorBody {
    code: String,
    message: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(token) = &cli.token {
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {token}")
                .parse()
                .context("SMON_ADMIN_TOKEN must be a valid header value")?,
        );
    }
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(3))
        .timeout(std::time::Duration::from_secs(10))
        .default_headers(headers)
        .build()?;
    let base = cli.server.trim_end_matches('/');

    match cli.command {
        Command::Ping => {
            get(&client, &format!("{base}/livez")).await?;
            get(&client, &format!("{base}/readyz")).await?;
            println!("live and ready");
        }
        Command::Monitors { action } => monitors(&client, base, action).await?,
    }
    Ok(())
}

async fn monitors(client: &reqwest::Client, base: &str, action: Monitors) -> Result<()> {
    match action {
        Monitors::List => {
            let body = get(client, &format!("{base}/api/monitors")).await?;
            let parsed: serde_json::Value = serde_json::from_str(&body)?;
            let Some(monitors) = parsed["monitors"].as_array() else {
                anyhow::bail!("unexpected response shape");
            };
            if monitors.is_empty() {
                println!("no monitors yet — add one with `monitor monitors add <name> <url>`");
            }
            for value in monitors {
                let monitor: MonitorView = serde_json::from_value(value.clone())?;
                println!(
                    "{}\t{}\t{}\tevery {}s\t{}{}",
                    monitor.id,
                    monitor.status,
                    monitor.name,
                    monitor.interval_s,
                    if monitor.paused { "paused\t" } else { "" },
                    monitor.url,
                );
            }
        }
        Monitors::Add {
            name,
            url,
            interval_s,
            timeout_s,
            failures_to_open,
            passes_to_recover,
        } => {
            let mut payload = serde_json::json!({ "name": name, "url": url });
            fill_fields(
                &mut payload,
                interval_s,
                timeout_s,
                failures_to_open,
                passes_to_recover,
            );
            add(client, base, &payload).await?;
        }
        Monitors::Show { id } => show(client, base, &id).await?,
        Monitors::Edit {
            id,
            name,
            url,
            interval_s,
            timeout_s,
            failures_to_open,
            passes_to_recover,
        } => {
            let mut payload = serde_json::json!({});
            if let Some(name) = name {
                payload["name"] = name.into();
            }
            if let Some(url) = url {
                payload["url"] = url.into();
            }
            fill_fields(
                &mut payload,
                interval_s,
                timeout_s,
                failures_to_open,
                passes_to_recover,
            );
            edit(client, base, &id, payload).await?;
        }
        Monitors::Pause { id } => set_paused(client, base, &id, true).await?,
        Monitors::Unpause { id } => set_paused(client, base, &id, false).await?,
        Monitors::Delete { id } => {
            let response = client
                .delete(format!("{base}/api/monitors/{id}"))
                .send()
                .await
                .context("server unreachable")?;
            let status = response.status();
            let body = response.text().await?;
            if !status.is_success() {
                explain_error(&body)?;
            }
            println!("deleted {id}");
        }
    }
    Ok(())
}

/// POST a create payload and print the created id.
async fn add(client: &reqwest::Client, base: &str, payload: &serde_json::Value) -> Result<()> {
    let response = client
        .post(format!("{base}/api/monitors"))
        .json(payload)
        .send()
        .await
        .context("server unreachable")?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        explain_error(&body)?;
    }
    let created: serde_json::Value = serde_json::from_str(&body)?;
    println!("created {}", created["id"].as_str().unwrap_or("(no id)"));
    Ok(())
}

/// PATCH an edit payload — rejecting an empty one, which would be a no-op call.
async fn edit(
    client: &reqwest::Client,
    base: &str,
    id: &str,
    payload: serde_json::Value,
) -> Result<()> {
    if payload.as_object().is_none_or(serde_json::Map::is_empty) {
        anyhow::bail!(
            "nothing to change — pass at least one of --name/--url/--interval-s/--timeout-s/--failures-to-open/--passes-to-recover"
        );
    }
    let body = patch(client, &format!("{base}/api/monitors/{id}"), &payload).await?;
    print_monitor(&serde_json::from_str(&body)?);
    Ok(())
}

/// Copy the optional schedule/threshold fields into a create or edit payload — `None`
/// means "don't send", which is what keeps a patch partial.
fn fill_fields(
    payload: &mut serde_json::Value,
    interval_s: Option<u64>,
    timeout_s: Option<u64>,
    failures_to_open: Option<u8>,
    passes_to_recover: Option<u8>,
) {
    for (key, value) in [
        ("interval_s", interval_s.map(serde_json::Value::from)),
        ("timeout_s", timeout_s.map(serde_json::Value::from)),
        (
            "failures_to_open",
            failures_to_open.map(serde_json::Value::from),
        ),
        (
            "passes_to_recover",
            passes_to_recover.map(serde_json::Value::from),
        ),
    ] {
        if let Some(value) = value {
            payload[key] = value;
        }
    }
}

/// One monitor plus its recent evidence: checks first, then incidents.
async fn show(client: &reqwest::Client, base: &str, id: &str) -> Result<()> {
    let body = get(client, &format!("{base}/api/monitors/{id}")).await?;
    print_monitor(&serde_json::from_str(&body)?);
    let checks = get(client, &format!("{base}/api/monitors/{id}/checks?limit=20")).await?;
    let checks: serde_json::Value = serde_json::from_str(&checks)?;
    println!("recent checks:");
    for check in checks["checks"].as_array().map_or(&[][..], Vec::as_slice) {
        println!(
            "  {}  {}{}{}",
            check["slot_at"].as_str().unwrap_or("?"),
            check["outcome"].as_str().unwrap_or("?"),
            check["http_status"]
                .as_i64()
                .map_or_else(String::new, |code| format!(" (HTTP {code})")),
            check["latency_ms"]
                .as_i64()
                .map_or_else(String::new, |ms| format!("  {ms}ms")),
        );
    }
    let incidents = get(
        client,
        &format!("{base}/api/incidents?monitor_id={id}&limit=20"),
    )
    .await?;
    let incidents: serde_json::Value = serde_json::from_str(&incidents)?;
    println!("incidents:");
    for incident in incidents["incidents"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
    {
        println!(
            "  {}  {}  opened {}",
            incident["state"].as_str().unwrap_or("?"),
            incident["id"].as_i64().unwrap_or(0),
            incident["opened_at"].as_str().unwrap_or("?"),
        );
    }
    Ok(())
}

async fn set_paused(client: &reqwest::Client, base: &str, id: &str, paused: bool) -> Result<()> {
    let body = patch(
        client,
        &format!("{base}/api/monitors/{id}"),
        &serde_json::json!({ "paused": paused }),
    )
    .await?;
    print_monitor(&serde_json::from_str(&body)?);
    Ok(())
}

fn print_monitor(monitor: &MonitorView) {
    println!(
        "{}\t{}\t{}\tevery {}s\t{}f/{}r\tgeneration {}\t{}",
        monitor.id,
        monitor.status,
        monitor.name,
        monitor.interval_s,
        monitor.failures_to_open,
        monitor.passes_to_recover,
        monitor.generation,
        if monitor.paused {
            format!("paused\t{}", monitor.url)
        } else {
            monitor.url.clone()
        },
    );
}

async fn get(client: &reqwest::Client, url: &str) -> Result<String> {
    let response = client.get(url).send().await.context("server unreachable")?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        explain_error(&body)?;
    }
    Ok(body)
}

async fn patch(client: &reqwest::Client, url: &str, payload: &serde_json::Value) -> Result<String> {
    let response = client
        .patch(url)
        .json(payload)
        .send()
        .await
        .context("server unreachable")?;
    let status = response.status();
    let body = response.text().await?;
    if !status.is_success() {
        explain_error(&body)?;
    }
    Ok(body)
}

fn explain_error(body: &str) -> Result<()> {
    match serde_json::from_str::<ErrorEnvelope>(body) {
        Ok(envelope) => anyhow::bail!("{}: {}", envelope.error.code, envelope.error.message),
        Err(_) => anyhow::bail!("request failed: {body}"),
    }
}
