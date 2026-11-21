//! The HTTP probe: one `GET` attempt per slot, inside one total deadline, classified
//! honestly. A redirect, a refused connection and a hung socket are different evidence and
//! never collapse into each other.

use std::collections::HashMap;
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

/// The committed detail for a probe the SSRF guard refused (a local address, by literal or
/// by DNS answer, on the first hop or a redirect). It rides a `transport` failure (D-013).
pub const REJECTED_LOCAL: &str = "rejected_local";

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
    /// The name could not be resolved within the bound — at save time that is a
    /// refusal (a target that cannot be proven public is not proven safe), at send
    /// time it is just another failed attempt.
    Unresolvable,
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

/// A probe start refused because the address's shared budget is spent (N1
/// re-review): the scheduler commits it as a counted `missed` — our own bound's
/// refusal, a coverage gap, never evidence about the target.
#[derive(Debug)]
pub(crate) struct AddressBusy;

impl std::fmt::Display for AddressBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the address's probe-start budget is spent")
    }
}

impl std::error::Error for AddressBusy {}

/// Token bucket per resolved address — the cross-tenant bound on probe starts
/// at one IP (N1 re-review). The per-workspace (origin, host) share keeps
/// fairness between sites, but it cannot bound what many names or many tenants
/// aim at one address: this cap rides the address the validating resolver
/// actually hands to the dialer — every tenant, every redirect hop. Ports are
/// invisible to DNS, so the bucket's key is the address alone.
#[derive(Debug)]
pub struct AddressBudget {
    /// Burst size — the most starts one address may pay at once.
    burst: f64,
    /// Sustained refill — tokens per second.
    rate: f64,
    buckets: std::sync::Mutex<HashMap<IpAddr, Bucket>>,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    at: tokio::time::Instant,
}

/// Addresses remembered at once — a wildcard-DNS spray must not grow it unbounded.
const MAX_BUCKETS: usize = 8192;

impl AddressBudget {
    /// `burst` paid starts at once, refilling to `burst` over `per`.
    pub fn new(burst: u32, per: Duration) -> Self {
        Self {
            burst: f64::from(burst),
            rate: f64::from(burst) / per.as_secs_f64().max(f64::EPSILON),
            buckets: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Charge one probe start at `ip`: false while its window is spent. `Instant`
    /// is tokio's clock — under paused time the refill is a test-clock fact.
    pub fn take(&self, ip: IpAddr) -> bool {
        let now = tokio::time::Instant::now();
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !buckets.contains_key(&ip) && buckets.len() >= MAX_BUCKETS {
            buckets.retain(|_, b| now.duration_since(b.at) < Duration::from_secs(3600));
            if buckets.len() >= MAX_BUCKETS {
                buckets.clear();
            }
        }
        let bucket = buckets.entry(ip).or_insert(Bucket {
            tokens: self.burst,
            at: now,
        });
        bucket.tokens = (bucket.tokens + now.duration_since(bucket.at).as_secs_f64() * self.rate)
            .min(self.burst);
        bucket.at = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// The budget every probe shares (N1 re-review): the write-time cap already
/// gives one workspace 2 starts a second on an address; 8 starts a second
/// across ALL tenants leaves headroom for a few workspaces sharing an IP
/// legitimately while keeping a wildcard-DNS spray an order under the engine's
/// 512-slot ceiling.
static PER_ADDRESS: std::sync::LazyLock<Arc<AddressBudget>> =
    std::sync::LazyLock::new(|| Arc::new(AddressBudget::new(8, Duration::from_secs(1))));

/// Test-only DNS answers, keyed by name — see `PolicyResolver::for_test`.
#[cfg(test)]
pub(crate) type Overrides =
    Arc<std::sync::Mutex<std::collections::HashMap<String, Vec<SocketAddr>>>>;

/// A fresh overrides map for tests that drive a `PolicyResolver` by hand.
#[cfg(test)]
pub(crate) fn test_overrides() -> Overrides {
    Overrides::default()
}

/// reqwest's resolver with the policy inside: names resolve through the OS, then every
/// returned address must be public (unless `lab`). reqwest receives only the validated
/// set — the check is the resolution itself, so there is no validate-then-re-resolve gap.
#[derive(Debug, Clone)]
pub struct PolicyResolver {
    allow_local_targets: bool,
    /// The cross-tenant per-address starts budget — `Some` on the probe path,
    /// `None` on the tenant-webhook deliverer (its sends are not probe slots).
    budget: Option<Arc<AddressBudget>>,
    /// Test-only DNS answers: present names return these addresses *through the same
    /// `validate_addrs` gate* — validation is never bypassed, only the lookup source is
    /// swapped. `None` in every build but tests.
    #[cfg(test)]
    overrides: Option<Overrides>,
}

impl PolicyResolver {
    /// The production constructor: strict unless `SMON_PROFILE=lab` (fixtures and drills
    /// on loopback). The tenant-webhook sink is built with this — its sends are not
    /// probe slots, so it shares no starts budget.
    pub fn new(allow_local_targets: bool) -> Self {
        Self {
            allow_local_targets,
            budget: None,
            #[cfg(test)]
            overrides: None,
        }
    }

    /// The resolver the probe client dials through: the strict gate plus the
    /// shared per-address starts budget (N1 re-review).
    fn for_probes(allow_local_targets: bool) -> Self {
        Self {
            allow_local_targets,
            budget: Some(PER_ADDRESS.clone()),
            #[cfg(test)]
            overrides: None,
        }
    }

    /// A resolver whose answers for the mapped names come from `overrides` — flip the
    /// map between calls to simulate DNS changing beneath a stored target (rebinding).
    /// Real lookups still run for names not in the map; every answer is validated.
    #[cfg(test)]
    pub(crate) fn for_test(allow_local_targets: bool, overrides: Overrides) -> Self {
        Self {
            allow_local_targets,
            budget: None,
            overrides: Some(overrides),
        }
    }

    /// `for_test` plus an injected starts budget — the runtime cap's tests drive
    /// it directly so a crowded address is a fact, not a race.
    #[cfg(test)]
    pub(crate) fn for_test_budgeted(
        allow_local_targets: bool,
        overrides: Overrides,
        budget: Arc<AddressBudget>,
    ) -> Self {
        Self {
            allow_local_targets,
            budget: Some(budget),
            overrides: Some(overrides),
        }
    }

    /// The lab flag the resolver was built with — the tenant deliverer rebuilds its
    /// literal-side `ProbePolicy` from it so both halves of the send gate agree.
    pub(crate) fn allows_local(&self) -> bool {
        self.allow_local_targets
    }
}

impl Resolve for PolicyResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let allow = self.allow_local_targets;
        let budget = self.budget.clone();
        #[cfg(test)]
        let overrides = self.overrides.clone();
        Box::pin(async move {
            let host = name.as_str().to_owned();
            #[cfg(test)]
            let mapped = overrides
                .as_ref()
                .and_then(|m| m.lock().ok().and_then(|m| m.get(&host).cloned()));
            #[cfg(not(test))]
            let mapped: Option<Vec<SocketAddr>> = None;
            let addrs: Vec<SocketAddr> = match mapped {
                Some(addrs) => addrs,
                None => tokio::net::lookup_host((host.as_str(), 0)).await?.collect(),
            };
            validate_addrs(addrs.iter().map(SocketAddr::ip), allow)?;
            // The per-address bound pays where the truth is (N1 re-review): every
            // candidate in the answer pays for the start it may take — happy
            // eyeballs may dial several, and a redirect hop re-enters here on the
            // address it lands on. A name whose whole answer is spent gets no dial.
            let paid: Vec<SocketAddr> = addrs
                .into_iter()
                .filter(|addr| budget.as_ref().is_none_or(|b| b.take(addr.ip())))
                .collect();
            if paid.is_empty() {
                return Err(Box::new(AddressBusy) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(paid.into_iter()) as Addrs)
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

    /// The validating resolver run at *save* time (D-017): `check`, then a bounded
    /// DNS resolution of domain hosts — every returned address must be public, the
    /// same gate `PolicyResolver` enforces again at every send. A name that cannot be
    /// resolved is refused (`Unresolvable`): an unproven host is not a proven-public
    /// host. Literal hosts need no lookup — `check` already judged them.
    pub async fn check_resolved(&self, url: &Url) -> Result<(), Rejected> {
        self.check(url)?;
        if self.allow_local_targets {
            return Ok(());
        }
        let Some(Host::Domain(name)) = url.host() else {
            return Ok(());
        };
        let looked = tokio::time::timeout(SAVE_RESOLVE, tokio::net::lookup_host((name, 0))).await;
        let addrs: Vec<SocketAddr> = match looked {
            Ok(Ok(iter)) => iter.collect(),
            _ => return Err(Rejected::Unresolvable),
        };
        validate_addrs(addrs.iter().map(SocketAddr::ip), false)
            .map_err(|_| Rejected::LocalAddressForbidden)
    }
}

/// Save-time DNS gets its own short bound — the write shouldn't wait on a slow resolver.
const SAVE_RESOLVE: Duration = Duration::from_secs(3);

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
    (Ipv6Addr::new(0x100, 0, 0, 1, 0, 0, 0, 0), 64), // dummy prefix (RFC 9780)
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
    /// `true` when OUR bound refused the start (the resolver's per-address
    /// budget): the slot is the monitor's coverage gap — a `missed`, never an
    /// outcome worth reporting about the target.
    pub limited: bool,
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
        let resolver = PolicyResolver::for_probes(policy.allow_local_targets);
        Self::build(policy, max_body, &[], resolver)
    }

    /// A client whose DNS answers for `pins` bypass the policy — unit tests only: it lets an
    /// offline test stand a "public" first hop on a loopback fixture, so a *later* hop can
    /// be refused by the real resolver. Never reachable from production code.
    #[cfg(test)]
    pub(crate) fn pinned(
        policy: ProbePolicy,
        max_body: usize,
        pins: &[(&str, SocketAddr)],
    ) -> Result<Self, reqwest::Error> {
        let resolver = PolicyResolver::for_test(policy.allow_local_targets, Overrides::default());
        Self::build(policy, max_body, pins, resolver)
    }

    /// A client whose DNS answers come from `overrides` through the same validating
    /// resolver the dialer uses, with an injected starts budget — the runtime
    /// cap's tests (N1 re-review). Never reachable from production code.
    #[cfg(test)]
    pub(crate) fn overridden(
        policy: ProbePolicy,
        max_body: usize,
        overrides: Overrides,
        budget: Arc<AddressBudget>,
    ) -> Result<Self, reqwest::Error> {
        let resolver =
            PolicyResolver::for_test_budgeted(policy.allow_local_targets, overrides, budget);
        Self::build(policy, max_body, &[], resolver)
    }

    fn build(
        policy: ProbePolicy,
        max_body: usize,
        pins: &[(&str, SocketAddr)],
        resolver: PolicyResolver,
    ) -> Result<Self, reqwest::Error> {
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
        // No proxy, ever: a proxied request is resolved by the proxy, so the DNS-layer
        // guard below would never see the name (HTTP_PROXY would turn
        // `http://metadata.internal…` into whatever the proxy answers). Probes go direct.
        let mut builder = reqwest::Client::builder().no_proxy();
        for (name, addr) in pins {
            builder = builder.resolve(name, *addr);
        }
        let client = builder
            // No connection reuse between probes (N1 re-review): a pooled dial
            // never reaches the resolver, so it would never pay the address's
            // starts budget. Every probe resolves — and pays — its target.
            .pool_max_idle_per_host(0)
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
            .dns_resolver(Arc::new(resolver))
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
                limited: false,
            },
            Ok(Attempt::Rejected(rejected)) => ProbeResult {
                outcome: CheckOutcome::Transport,
                http_status: None,
                latency: start.elapsed(),
                detail: Some(rejected_detail(rejected)),
                limited: false,
            },
            // Our own bound's refusal is not evidence about the target: the
            // resolver's per-address budget spent means the slot is a `missed`,
            // so the monitor shows a coverage gap, never a failure it didn't have.
            Ok(Attempt::Failed(error)) if is_budget_refusal(&error) => ProbeResult {
                outcome: CheckOutcome::Transport,
                http_status: None,
                latency: start.elapsed(),
                detail: Some("address_budget"),
                limited: true,
            },
            Ok(Attempt::Failed(error)) if error.is_timeout() => ProbeResult {
                outcome: CheckOutcome::Timeout,
                http_status: None,
                latency: start.elapsed(),
                detail: Some("deadline"),
                limited: false,
            },
            Ok(Attempt::Failed(error)) => ProbeResult {
                outcome: CheckOutcome::Transport,
                http_status: None,
                latency: start.elapsed(),
                detail: Some(classify(&error)),
                limited: false,
            },
            Ok(Attempt::Answered(status)) if status.is_success() => ProbeResult {
                outcome: CheckOutcome::Pass,
                http_status: Some(status.as_u16()),
                latency: start.elapsed(),
                detail: None,
                limited: false,
            },
            Ok(Attempt::Answered(status)) => ProbeResult {
                outcome: CheckOutcome::HttpFail {
                    status: status.as_u16(),
                },
                http_status: Some(status.as_u16()),
                latency: start.elapsed(),
                detail: None,
                limited: false,
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

/// Whether `error` carries the resolver's `AddressBusy` — walks the same cause
/// chain `classify` does (reqwest wraps the resolver's boxed error inside its
/// connect error).
fn is_budget_refusal(error: &reqwest::Error) -> bool {
    let mut cause: Option<&dyn std::error::Error> = std::error::Error::source(error);
    while let Some(e) = cause {
        if e.is::<AddressBusy>() {
            return true;
        }
        cause = e.source();
    }
    false
}

/// The committed detail a `Rejected` maps to. `check()` never yields `Unresolvable` —
/// `check_resolved` alone can — but the enum is shared, so the word must exist: a name
/// that cannot resolve is a connect failure, the word the taxonomy already knows.
/// `pub(crate)` so the tenant deliverer words its send-side `check` refusals identically.
pub(crate) fn rejected_detail(rejected: Rejected) -> &'static str {
    match rejected {
        Rejected::SchemeNotAllowed => "rejected_scheme",
        Rejected::LocalAddressForbidden => REJECTED_LOCAL,
        Rejected::Unresolvable => "connect",
    }
}

/// A short stable classification — the stored `detail` must stay free of URLs and
/// headers, so reqwest's Display is never stored. `pub(crate)` so the deliverer classifies
/// tenant-webhook send errors the same way.
pub(crate) fn classify(error: &reqwest::Error) -> &'static str {
    // A DNS refusal from PolicyResolver rides inside reqwest's connect error — walk the
    // cause chain so it reports `rejected_local`, not an ambiguous `connect`. TLS
    // failures ride the same chain — catch them on the way down.
    let mut cause: Option<&dyn std::error::Error> = std::error::Error::source(error);
    while let Some(e) = cause {
        if e.is::<ResolvedLocal>() {
            return REJECTED_LOCAL;
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
            "100:0:0:1::",
            "100:0:0:1:ffff:ffff:ffff:ffff", // dummy prefix 100:0:0:1::/64 (RFC 9780)
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
            "100:0:0:2::1",
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
        let resolver = PolicyResolver::new(false);
        let name = Name::from_str("localhost").expect("name");
        assert!(
            resolver.resolve(name).await.is_err(),
            "localhost must not resolve under the strict profile"
        );
    }

    fn overrides() -> Overrides {
        Overrides::default()
    }

    /// N1 re-review: the per-ADDRESS ceiling lives where the truth is — inside the
    /// resolver, charged on the addresses it actually hands to the dialer. A burst
    /// pays; a name answered once its address's starts are spent is refused
    /// (`AddressBusy`, a counted miss — never a dial).
    #[tokio::test]
    async fn the_resolver_spends_and_refuses_the_address_budget() {
        let map = overrides();
        map.lock().expect("map").insert(
            "victim.test".to_owned(),
            vec!["8.8.8.8:0".parse().expect("addr")],
        );
        map.lock().expect("map").insert(
            "other.test".to_owned(),
            vec!["1.1.1.1:0".parse().expect("addr")],
        );
        let budget = Arc::new(AddressBudget::new(2, Duration::from_secs(3600)));
        let resolver = PolicyResolver::for_test_budgeted(false, map, budget);
        let name = || Name::from_str("victim.test").expect("name");
        assert!(resolver.resolve(name()).await.is_ok(), "first dial pays");
        assert!(resolver.resolve(name()).await.is_ok(), "second dial pays");
        let Err(err) = resolver.resolve(name()).await else {
            panic!("the address's starts are spent — the resolve must refuse")
        };
        assert!(
            err.is::<AddressBusy>()
                || err
                    .source()
                    .is_some_and(<dyn std::error::Error + 'static>::is::<AddressBusy>),
            "the refusal is the budget, not a DNS failure: {err:?}"
        );
        // Another address has its own bucket — a crowded address never starves it.
        assert!(
            resolver
                .resolve(Name::from_str("other.test").expect("name"))
                .await
                .is_ok(),
            "a different address pays from its own budget"
        );
    }

    /// Every address in the answer pays its own way: a candidate whose bucket is
    /// empty is skipped, the ones that paid are handed on — hyper dials paid
    /// addresses only (happy eyeballs may dial several).
    #[tokio::test]
    async fn the_resolver_yields_only_the_addresses_that_paid() {
        let map = overrides();
        map.lock().expect("map").insert(
            "two.test".to_owned(),
            vec![
                "8.8.8.8:0".parse().expect("addr"),
                "1.1.1.1:0".parse().expect("addr"),
            ],
        );
        let budget = Arc::new(AddressBudget::new(1, Duration::from_secs(3600)));
        // 8.8.8.8 is already spent by an earlier resolution.
        assert!(budget.take("8.8.8.8".parse().expect("ip")));
        let resolver = PolicyResolver::for_test_budgeted(false, map, budget);
        let addrs = resolver
            .resolve(Name::from_str("two.test").expect("name"))
            .await
            .expect("1.1.1.1 still pays")
            .collect::<Vec<_>>();
        assert_eq!(
            addrs,
            vec!["1.1.1.1:0".parse::<SocketAddr>().expect("addr")],
            "only the paying address is offered to the dialer"
        );
    }

    /// The budget itself: a burst pays, the window refills — under paused time the
    /// refill is a test-clock fact, never a real-time flake.
    #[tokio::test(start_paused = true)]
    async fn the_address_budget_refills_at_its_rate() {
        let budget = AddressBudget::new(2, Duration::from_secs(60));
        let ip = "8.8.8.8".parse().expect("ip");
        assert!(budget.take(ip));
        assert!(budget.take(ip));
        assert!(!budget.take(ip), "the burst is spent");
        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(budget.take(ip), "a window later the bucket refilled");
    }
}
