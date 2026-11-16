//! The HTTP probe: one `GET` attempt per slot, inside one total deadline, classified
//! honestly. A redirect, a refused connection and a hung socket are different evidence and
//! never collapse into each other.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
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

/// IPv4 blocks the monitor never dials: every entry of the IANA IPv4 special-purpose
/// registry that is not globally reachable, plus multicast and the reserved 240/4. A few
/// registry sub-blocks that *are* global anycast services (inside 192.0.0.0/24) are
/// refused too — none is a meaningful health-check target.
const LOCAL_V4: &[(Ipv4Addr, u8)] = &[
    (Ipv4Addr::UNSPECIFIED, 8),           // "this network"
    (Ipv4Addr::new(10, 0, 0, 0), 8),      // private
    (Ipv4Addr::new(100, 64, 0, 0), 10),   // shared address space (CGNAT)
    (Ipv4Addr::new(127, 0, 0, 0), 8),     // loopback
    (Ipv4Addr::new(169, 254, 0, 0), 16),  // link-local (cloud metadata lives here)
    (Ipv4Addr::new(172, 16, 0, 0), 12),   // private
    (Ipv4Addr::new(192, 0, 0, 0), 24),    // IETF protocol assignments
    (Ipv4Addr::new(192, 0, 2, 0), 24),    // TEST-NET-1
    (Ipv4Addr::new(192, 88, 99, 0), 24),  // deprecated 6to4 relay anycast
    (Ipv4Addr::new(192, 168, 0, 0), 16),  // private
    (Ipv4Addr::new(198, 18, 0, 0), 15),   // benchmarking
    (Ipv4Addr::new(198, 51, 100, 0), 24), // TEST-NET-2
    (Ipv4Addr::new(203, 0, 113, 0), 24),  // TEST-NET-3
    (Ipv4Addr::new(224, 0, 0, 0), 4),     // multicast
    (Ipv4Addr::new(240, 0, 0, 0), 4),     // reserved, incl. 255.255.255.255
];

/// IPv6 blocks the monitor never dials (IANA IPv6 special-purpose registry, plus
/// multicast and deprecated site-local). Forms that embed an IPv4 address — mapped,
/// NAT64 `64:ff9b::/96`, 6to4 `2002::/16` — are judged by that address first, in
/// `embedded_v4`. `2001::/23` is refused whole (Teredo, benchmarking, ORCHID, a few anycast
/// services): conservative, and nothing in it is a health-check target.
const LOCAL_V6: &[(Ipv6Addr, u8)] = &[
    (Ipv6Addr::UNSPECIFIED, 96), // ::/96 — unspecified, loopback, v4-compatible
    (Ipv6Addr::new(0x64, 0xff9b, 1, 0, 0, 0, 0, 0), 48), // local-use NAT64
    (Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, 0), 64), // discard-only
    (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23), // IETF protocol assignments
    (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32), // documentation
    (Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20), // documentation (RFC 9637)
    (Ipv6Addr::new(0x5f00, 0, 0, 0, 0, 0, 0, 0), 16), // SRv6 SIDs
    (Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7), // unique-local
    (Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10), // link-local
    (Ipv6Addr::new(0xfec0, 0, 0, 0, 0, 0, 0, 0), 10), // deprecated site-local
    (Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8), // multicast
];

fn in_v4(ip: Ipv4Addr, (net, len): (Ipv4Addr, u8)) -> bool {
    let mask = u32::MAX.checked_shl(32 - u32::from(len)).unwrap_or(0);
    u32::from(ip) & mask == u32::from(net) & mask
}

fn in_v6(ip: Ipv6Addr, (net, len): (Ipv6Addr, u8)) -> bool {
    let mask = u128::MAX.checked_shl(128 - u32::from(len)).unwrap_or(0);
    u128::from(ip) & mask == u128::from(net) & mask
}

/// The IPv4 address an IPv6 address stands for, when it is a translation form a dialer
/// (or a NAT64/6to4 gateway) would turn into an IPv4 connection.
fn embedded_v4(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(v4) = v6.to_ipv4_mapped() {
        return Some(v4);
    }
    let segments = v6.segments();
    let join = |high: u16, low: u16| Ipv4Addr::from((u32::from(high) << 16) | u32::from(low));
    if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return Some(join(segments[6], segments[7])); // NAT64 well-known prefix
    }
    if segments[0] == 0x2002 {
        return Some(join(segments[1], segments[2])); // 6to4
    }
    None
}

fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => LOCAL_V4.iter().any(|&block| in_v4(v4, block)),
        IpAddr::V6(v6) => {
            embedded_v4(v6).is_some_and(|v4| is_local(IpAddr::V4(v4)))
                || LOCAL_V6.iter().any(|&block| in_v6(v6, block))
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
    use std::str::FromStr;

    #[test]
    fn is_local_catches_the_common_local_ranges() {
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

    fn assert_matrix(refused: &[&str], allowed: &[&str]) {
        for ip in refused {
            let parsed: IpAddr = ip.parse().expect(ip);
            assert!(is_local(parsed), "{ip} must be refused");
        }
        for ip in allowed {
            let parsed: IpAddr = ip.parse().expect(ip);
            assert!(!is_local(parsed), "{ip} must be allowed");
        }
    }

    /// The IANA IPv4 special-purpose registry, block by block: the first and last address
    /// of every non-global block is refused, the public neighbours just outside it pass.
    #[test]
    fn is_local_matches_the_ipv4_special_purpose_registry() {
        let refused = [
            "0.0.0.0",
            "0.255.255.255", // "this network" 0/8
            "10.0.0.0",
            "10.255.255.255", // private 10/8
            "100.64.0.0",
            "100.127.255.255", // shared / CGNAT 100.64/10
            "127.0.0.0",
            "127.255.255.255", // loopback 127/8
            "169.254.0.0",
            "169.254.255.255", // link-local 169.254/16
            "172.16.0.0",
            "172.31.255.255", // private 172.16/12
            "192.0.0.0",
            "192.0.0.255", // IETF protocol assignments 192.0.0/24
            "192.0.2.0",
            "192.0.2.255", // TEST-NET-1
            "192.88.99.0",
            "192.88.99.255", // deprecated 6to4 relay anycast
            "192.168.0.0",
            "192.168.255.255", // private 192.168/16
            "198.18.0.0",
            "198.19.255.255", // benchmarking 198.18/15
            "198.51.100.0",
            "198.51.100.255", // TEST-NET-2
            "203.0.113.0",
            "203.0.113.255", // TEST-NET-3
            "224.0.0.0",
            "239.255.255.255", // multicast 224/4
            "240.0.0.0",
            "255.255.255.254", // reserved 240/4
            "255.255.255.255", // limited broadcast
        ];
        let allowed = [
            "1.0.0.0",
            "9.255.255.255",
            "11.0.0.0",
            "100.63.255.255",
            "100.128.0.0",
            "126.255.255.255",
            "128.0.0.0",
            "172.15.255.255",
            "172.32.0.0",
            "192.0.1.0",
            "192.0.3.0",
            "192.88.98.255",
            "192.88.100.0",
            "192.167.255.255",
            "192.169.0.0",
            "198.17.255.255",
            "198.20.0.0",
            "203.0.112.255",
            "203.0.114.0",
            "223.255.255.255",
            "8.8.8.8",
            "1.1.1.1",
        ];
        assert_matrix(&refused, &allowed);
    }

    /// The IANA IPv6 special-purpose registry, same shape — plus the forms that embed an
    /// IPv4 address (mapped, compatible, NAT64, 6to4), which inherit its verdict.
    #[test]
    fn is_local_matches_the_ipv6_special_purpose_registry() {
        let refused = [
            "::",
            "::1", // unspecified, loopback
            "::8.8.8.8",
            "::0.0.0.2", // deprecated IPv4-compatible ::/96
            "::ffff:10.0.0.1",
            "::ffff:100.64.0.1",
            "::ffff:198.18.0.1", // v4-mapped local
            "64:ff9b::a00:1",
            "64:ff9b::6440:1", // NAT64 of 10.0.0.1, 100.64.0.1
            "64:ff9b:1::",
            "64:ff9b:1:ffff:ffff:ffff:ffff:ffff", // local-use NAT64 /48
            "100::",
            "100::ffff:ffff:ffff:ffff", // discard-only 100::/64
            "2001::",
            "2001:1ff:ffff:ffff:ffff:ffff:ffff:ffff", // IETF 2001::/23 (Teredo…)
            "2001:db8::",
            "2001:db8:ffff:ffff:ffff:ffff:ffff:ffff", // documentation /32
            "2002:a00:1::",
            "2002:7f00:1::1", // 6to4 of 10.0.0.1, 127.0.0.1
            "3fff::",
            "3fff:fff:ffff:ffff:ffff:ffff:ffff:ffff", // documentation 3fff::/20
            "5f00::",
            "5f00:ffff::1", // SRv6 SIDs 5f00::/16
            "fc00::",
            "fdff:ffff:ffff:ffff:ffff:ffff:ffff:ffff", // unique-local fc00::/7
            "fe80::",
            "febf:ffff:ffff:ffff:ffff:ffff:ffff:ffff", // link-local fe80::/10
            "fec0::",
            "feff:ffff:ffff:ffff:ffff:ffff:ffff:ffff", // deprecated site-local
            "ff00::",
            "ff02::1", // multicast ff00::/8
        ];
        let allowed = [
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "2002:808:808::1",
            "2001:200::1",
            "2001:4860:4860::8888",
            "2001:db9::1",
            "2606:4700:4700::1111",
            "3fff:1000::1",
            "5f01::1",
            "fbff::1",
        ];
        assert_matrix(&refused, &allowed);
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
