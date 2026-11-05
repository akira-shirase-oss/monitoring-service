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
}

#[derive(Deserialize)]
struct MonitorView {
    id: Uuid,
    name: String,
    url: String,
    interval_s: u64,
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
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(3))
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let base = cli.server.trim_end_matches('/');

    match cli.command {
        Command::Ping => {
            get(&client, &format!("{base}/livez")).await?;
            get(&client, &format!("{base}/readyz")).await?;
            println!("live and ready");
        }
        Command::Monitors {
            action: Monitors::List,
        } => {
            let body = get(&client, &format!("{base}/api/monitors")).await?;
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
        Command::Monitors {
            action:
                Monitors::Add {
                    name,
                    url,
                    interval_s,
                    timeout_s,
                },
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
    }
    Ok(())
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

fn explain_error(body: &str) -> Result<()> {
    match serde_json::from_str::<ErrorEnvelope>(body) {
        Ok(envelope) => anyhow::bail!("{}: {}", envelope.error.code, envelope.error.message),
        Err(_) => anyhow::bail!("request failed: {body}"),
    }
}
