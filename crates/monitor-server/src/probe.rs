//! The HTTP probe: one `GET` attempt per slot, inside one total deadline, classified
//! honestly. A redirect, a refused connection and a hung socket are different evidence and
//! never collapse into each other.

use std::net::IpAddr;
use std::time::Duration;

use reqwest::redirect;
use tokio::time::Instant;
use url::{Host, Url};

use monitor_domain::CheckOutcome;

/// Targets the monitor refuses to contact. The v1 guard lives at the URL layer: scheme plus
/// IP-literal checks. DNS-level checks (a public name resolving to a private address) and
/// redirect following with per-hop revalidation are M5 work — until then redirects are
/// simply `http_fail` evidence, never followed.
#[derive(Debug, Clone, Copy)]
pub struct ProbePolicy {
    /// `lab` profile permits loopback/private literal targets for local fixtures.
    pub allow_local_targets: bool,
}

/// Why a target was refused before a socket was opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    SchemeNotAllowed,
    LocalAddressForbidden,
}

impl ProbePolicy {
    pub fn check(&self, url: &Url) -> Result<(), Rejected> {
        match url.scheme() {
            "http" | "https" => {}
            _ => return Err(Rejected::SchemeNotAllowed),
        }
        if self.allow_local_targets {
            return Ok(());
        }
        if let Some(host) = url.host() {
            let ip = match host {
                Host::Ipv4(v4) => IpAddr::V4(v4),
                Host::Ipv6(v6) => IpAddr::V6(v6),
                Host::Domain(_) => return Ok(()),
            };
            if is_local(ip) {
                return Err(Rejected::LocalAddressForbidden);
            }
        }
        Ok(())
    }
}

fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unique_local()
                || v6.is_unicast_link_local()
                || v6.is_multicast()
                || v6
                    .to_ipv4_mapped()
                    .is_some_and(|v4| is_local(IpAddr::V4(v4)))
        }
    }
}

/// A completed probe attempt. `latency` covers the whole attempt — DNS through the bounded
/// body read — matching the deadline semantics.
#[derive(Debug)]
pub struct ProbeResult {
    pub outcome: CheckOutcome,
    pub http_status: Option<u16>,
    pub latency: Duration,
    /// Classified detail for the `checks.detail` column — never the error string itself
    /// (reqwest errors can embed URLs and headers).
    pub detail: Option<&'static str>,
}

/// One reqwest client shared by every probe: connection reuse, no redirect following.
#[derive(Debug, Clone)]
pub struct ProbeClient {
    client: reqwest::Client,
    policy: ProbePolicy,
    /// Response bodies are read at most this far — evidence is the status, not the page.
    max_body: usize,
}

impl ProbeClient {
    pub fn new(policy: ProbePolicy, max_body: usize) -> Result<Self, reqwest::Error> {
        let client = reqwest::Client::builder()
            .redirect(redirect::Policy::none())
            .user_agent(concat!("service-monitor/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            client,
            policy,
            max_body,
        })
    }

    /// Run one probe against `url`, bounded by `deadline` for the *entire* attempt.
    pub async fn probe(&self, url: &Url, deadline: Duration) -> ProbeResult {
        let start = Instant::now();
        let attempt = async {
            if let Err(rejected) = self.policy.check(url) {
                return Attempt::Rejected(rejected);
            }
            match self.client.get(url.clone()).send().await {
                Err(error) => Attempt::Failed(error),
                Ok(mut response) => {
                    let status = response.status();
                    // Read the body bounded: a target that streams forever must not hold a
                    // worker past the deadline, and we only need enough to know it answered.
                    let mut read = 0usize;
                    let mut failed = None;
                    while failed.is_none() {
                        match response.chunk().await {
                            Err(error) => failed = Some(error),
                            Ok(None) => break,
                            Ok(Some(chunk)) => {
                                read += chunk.len();
                                if read > self.max_body {
                                    break;
                                }
                            }
                        }
                    }
                    match failed {
                        Some(error) => Attempt::Failed(error),
                        None => Attempt::Answered(status),
                    }
                }
            }
        };

        match tokio::time::timeout(deadline, attempt).await {
            Err(_elapsed) => ProbeResult {
                outcome: CheckOutcome::Timeout,
                http_status: None,
                latency: start.elapsed(),
                detail: Some("deadline"),
            },
            Ok(Attempt::Rejected(Rejected::SchemeNotAllowed)) => ProbeResult {
                outcome: CheckOutcome::Transport,
                http_status: None,
                latency: start.elapsed(),
                detail: Some("rejected_scheme"),
            },
            Ok(Attempt::Rejected(Rejected::LocalAddressForbidden)) => ProbeResult {
                outcome: CheckOutcome::Transport,
                http_status: None,
                latency: start.elapsed(),
                detail: Some("rejected_local"),
            },
            Ok(Attempt::Failed(error)) if error.is_timeout() => ProbeResult {
                outcome: CheckOutcome::Timeout,
                http_status: None,
                latency: start.elapsed(),
                detail: Some("deadline"),
            },
            Ok(Attempt::Failed(error)) => ProbeResult {
                outcome: CheckOutcome::Transport,
                http_status: None,
                latency: start.elapsed(),
                detail: Some(classify(&error)),
            },
            Ok(Attempt::Answered(status)) if status.is_success() => ProbeResult {
                outcome: CheckOutcome::Pass,
                http_status: Some(status.as_u16()),
                latency: start.elapsed(),
                detail: None,
            },
            Ok(Attempt::Answered(status)) => ProbeResult {
                outcome: CheckOutcome::HttpFail {
                    status: status.as_u16(),
                },
                http_status: Some(status.as_u16()),
                latency: start.elapsed(),
                detail: None,
            },
        }
    }
}

enum Attempt {
    Rejected(Rejected),
    Failed(reqwest::Error),
    Answered(reqwest::StatusCode),
}

/// A short stable classification — the stored `detail` must stay free of URLs and headers,
/// so reqwest's Display is never stored.
fn classify(error: &reqwest::Error) -> &'static str {
    if error.is_connect() {
        "connect"
    } else if error.is_body() || error.is_decode() {
        "body"
    } else if error.is_request() {
        "request"
    } else {
        "transport"
    }
}
