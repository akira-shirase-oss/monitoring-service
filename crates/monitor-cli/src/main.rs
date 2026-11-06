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
    /// Register a monitor (defaults: 60-second interval, 5-second deadline).
    Add {
        name: String,
        url: String,
        #[arg(long)]
        interval_s: Option<u64>,
        #[arg(long)]
        timeout_s: Option<u64>,
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
        } => {
            let mut payload = serde_json::json!({ "name": name, "url": url });
            if let Some(interval) = interval_s {
                payload["interval_s"] = interval.into();
            }
            if let Some(timeout) = timeout_s {
                payload["timeout_s"] = timeout.into();
            }
            let response = client
                .post(format!("{base}/api/monitors"))
                .json(&payload)
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
        }
        Monitors::Show { id } => show(client, base, &id).await?,
        Monitors::Edit {
            id,
            name,
            url,
            interval_s,
            timeout_s,
        } => {
            let mut payload = serde_json::json!({});
            if let Some(name) = name {
                payload["name"] = name.into();
            }
            if let Some(url) = url {
                payload["url"] = url.into();
            }
            if let Some(interval) = interval_s {
                payload["interval_s"] = interval.into();
            }
            if let Some(timeout) = timeout_s {
                payload["timeout_s"] = timeout.into();
            }
            if payload.as_object().is_none_or(serde_json::Map::is_empty) {
                anyhow::bail!(
                    "nothing to change — pass at least one of --name/--url/--interval-s/--timeout-s"
                );
            }
            let body = patch(client, &format!("{base}/api/monitors/{id}"), &payload).await?;
            print_monitor(&serde_json::from_str(&body)?);
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
        "{}\t{}\t{}\tevery {}s\tgeneration {}\t{}",
        monitor.id,
        monitor.status,
        monitor.name,
        monitor.interval_s,
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
