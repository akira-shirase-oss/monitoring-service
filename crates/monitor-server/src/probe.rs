//! The HTTP probe: one `GET` attempt per slot, inside one total deadline, classified
//! honestly. A redirect, a refused connection and a hung socket are different evidence and
//! never collapse into each other.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::redirect;
use tokio::time::Instant;
use url::{Host, Url};

use monitor_domain::CheckOutcome;

/// Redirects followed per hop, each revalidated — a hop limit, never a free pass.
const MAX_REDIRECT_HOPS: usize = 5;

/// Targets the monitor refuses to contact. Two layers, both M5-complete:
///  * URL layer — scheme and IP literals, checked at registration and per redirect hop;
///  * DNS layer — every address a name resolves to must pass `is_local` too, enforced by
///    `PolicyResolver` so reqwest only ever dials validated addresses.
#[derive(Debug, Clone)]
pub struct ProbePolicy {
    /// `lab` profile permits loopback/private literal targets for local fixtures.
    pub allow_local_targets: bool,
    /// Additional trust anchors beyond the public webpki bundle — an internal CA
    /// for operators monitoring inside their own PKI. Empty in the default profile.
    pub extra_roots: Vec<rustls_pki_types::CertificateDer<'static>>,
}

/// Why a target was refused before a socket was opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejected {
    SchemeNotAllowed,
    LocalAddressForbidden,
}

/// A DNS answer containing a local address — the whole resolution is refused, because
/// answering "we found 10.0.0.1" to a dialer that might pick it is a rebinding hole.
#[derive(Debug)]
struct ResolvedLocal;

impl std::fmt::Display for ResolvedLocal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("target resolves to a local address")
    }
}

impl std::error::Error for ResolvedLocal {}

/// reqwest's resolver with the policy inside: names resolve through the OS, then every
/// returned address must be public (unless `lab`). reqwest receives only the validated
/// set — the check is the resolution itself, so there is no validate-then-re-resolve gap.
#[derive(Debug, Clone, Copy)]
struct PolicyResolver {
    allow_local_targets: bool,
}

impl Resolve for PolicyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow = self.allow_local_targets;
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            validate_addrs(addrs.iter().map(SocketAddr::ip), allow)?;
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

/// Every resolved address must be public (unless `lab`): a single local address fails the
/// whole resolution — round-robin DNS means "usually public" is not a defense.
fn validate_addrs(
    addrs: impl IntoIterator<Item = IpAddr>,
    allow_local_targets: bool,
) -> Result<(), ResolvedLocal> {
    if allow_local_targets {
        return Ok(());
    }
    if addrs.into_iter().any(is_local) {
        return Err(ResolvedLocal);
    }
    Ok(())
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

/// One reqwest client shared by every probe: connection reuse, policy-checked DNS, and
/// redirects followed up to a bound with every hop revalidated — a redirect is a target
/// the target chose, so it must clear the same policy as the original.
#[derive(Debug, Clone)]
pub struct ProbeClient {
    client: reqwest::Client,
    policy: ProbePolicy,
    /// Response bodies are read at most this far — evidence is the status, not the page.
    max_body: usize,
}

impl ProbeClient {
    pub fn new(policy: ProbePolicy, max_body: usize) -> Result<Self, reqwest::Error> {
        // TLS truth (D-010): a preconfigured rustls client on the webpki-roots bundle.
        // The platform verifier (Security.framework / schannel) flattens certificate
        // failures into opaque OS errors — the detail taxonomy below is only possible
        // because webpki reports rich variants. A monitor should also see what a public
        // client sees: local enterprise roots must not mask a real trust failure.
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        for cert in &policy.extra_roots {
            let _ = roots.add(cert.clone());
        }
        let tls = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let redirect_policy = policy.clone();
        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .redirect(redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= MAX_REDIRECT_HOPS {
                    attempt.stop()
                } else if redirect_policy.check(attempt.url()).is_err() {
                    // A hop the policy refuses is evidence, not a path: keep the 3xx.
                    attempt.stop()
                } else {
                    attempt.follow()
                }
            }))
            .dns_resolver(Arc::new(PolicyResolver {
                allow_local_targets: policy.allow_local_targets,
            }))
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

/// A TLS failure gets its own committed detail — "couldn't connect" is the least useful
/// thing a monitor can say about a broken certificate. Names mirror the operator's
/// remediation: renew, wait, fix the CA chain, fix the name.
fn classify_tls(error: &rustls::Error) -> &'static str {
    use rustls::CertificateError as C;
    match error {
        rustls::Error::InvalidCertificate(inner) => match inner {
            C::Expired | C::ExpiredContext { .. } => "cert_expired",
            C::NotValidYet | C::NotValidYetContext { .. } => "cert_not_yet_valid",
            C::NotValidForName | C::NotValidForNameContext { .. } => "cert_name_mismatch",
            C::Revoked => "cert_revoked",
            _ => "cert_untrusted",
        },
        _ => "tls",
    }
}

/// Find the rustls error inside a cause-chain node — reqwest/hyper wrap it inside
/// `io::Error` inner refs, sometimes nested (observed: io → io → rustls).
fn find_tls<'a>(mut node: &'a (dyn std::error::Error + 'static)) -> Option<&'a rustls::Error> {
    loop {
        if let Some(tls) = node.downcast_ref::<rustls::Error>() {
            return Some(tls);
        }
        node = node
            .downcast_ref::<std::io::Error>()
            .and_then(|io| io.get_ref())?;
    }
}

/// A short stable classification — the stored `detail` must stay free of URLs and
/// headers, so reqwest's Display is never stored.
fn classify(error: &reqwest::Error) -> &'static str {
    // A DNS refusal from PolicyResolver rides inside reqwest's connect error — walk the
    // cause chain so it reports `rejected_local`, not an ambiguous `connect`. TLS
    // failures ride the same chain — catch them on the way down.
    let mut cause: Option<&dyn std::error::Error> = std::error::Error::source(error);
    while let Some(e) = cause {
        if e.is::<ResolvedLocal>() {
            return "rejected_local";
        }
        // TLS errors ride the connect chain, wrapped one or more levels inside
        // io::Error inner refs — recurse until the error or the wrap chain ends.
        if let Some(tls) = find_tls(e) {
            return classify_tls(tls);
        }
        cause = e.source();
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::str::FromStr;

    #[test]
    fn is_local_catches_every_non_public_range() {
        for ip in [
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(172, 16, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
            IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)), // link-local cloud metadata
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V4(Ipv4Addr::BROADCAST),
            IpAddr::V4(Ipv4Addr::new(224, 0, 0, 1)),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            IpAddr::V6("fd00::1".parse().expect("fc00::/7")),
            IpAddr::V6("fe80::1".parse().expect("link-local")),
            IpAddr::V6("::ffff:127.0.0.1".parse().expect("v4-mapped loopback")),
            IpAddr::V6("::ffff:10.0.0.1".parse().expect("v4-mapped private")),
        ] {
            assert!(is_local(ip), "{ip} must be refused");
        }
        for ip in [
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
            IpAddr::V6("2606:4700:4700::1111".parse().expect("public v6")),
            IpAddr::V6("::ffff:8.8.8.8".parse().expect("v4-mapped public")),
        ] {
            assert!(!is_local(ip), "{ip} must be allowed");
        }
    }

    #[test]
    fn validate_addrs_refuses_when_any_answer_is_local() {
        let mixed = [
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254)),
        ];
        assert!(validate_addrs(mixed, false).is_err());
        assert!(validate_addrs(mixed, true).is_ok());
        let public = [IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))];
        assert!(validate_addrs(public, false).is_ok());
        assert!(validate_addrs([], false).is_ok());
    }

    #[tokio::test]
    async fn the_resolver_itself_refuses_a_name_that_resolves_local() {
        let resolver = PolicyResolver {
            allow_local_targets: false,
        };
        let name = Name::from_str("localhost").expect("name");
        assert!(
            resolver.resolve(name).await.is_err(),
            "localhost must not resolve under the strict profile"
        );
    }
}
