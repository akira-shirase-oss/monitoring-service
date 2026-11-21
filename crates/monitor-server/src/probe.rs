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

use monitor_domain::{CheckOutcome, Expect, ProbeMethod, WorkspaceId};

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

// The workspace whose schedule is paying for the in-flight probe, carried as a
// task-local so the charge can reach the places that know the address but not
// the tenant — the resolver (called deep inside reqwest's connect) and the
// redirect closure. Unattributed calls still pay the global per-address bound.
tokio::task_local! {
    pub(crate) static PROBE_WORKSPACE: WorkspaceId;
}

/// Whose schedule is paying, when the call site can't pass it: `None` outside a
/// probe dispatch (webhook sends, bare resolves) — those pay only the global
/// per-address bound.
fn workspace() -> Option<WorkspaceId> {
    PROBE_WORKSPACE.try_with(|w| *w).ok()
}

/// The address a URL literal means — the bucket key, normalised like
/// `origin.rs`'s `address_key` so `::ffff:V` spends `V`'s share, not a twin.
fn literal_ip(url: &Url) -> Option<IpAddr> {
    match url.host()? {
        Host::Ipv4(v4) => Some(IpAddr::V4(v4)),
        Host::Ipv6(v6) => Some(v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4)),
        Host::Domain(_) => None,
    }
}

/// Token bucket per resolved address — the cross-tenant bound on probe starts
/// at one IP (N1 re-review). The per-workspace (origin, host) share keeps
/// fairness between sites, but it cannot bound what many names or many tenants
/// aim at one address: this cap rides the address the validating resolver
/// actually hands to the dialer — every tenant, every redirect hop. Ports are
/// invisible to DNS, so the bucket's key is the address alone.
///
/// Under the global ceiling each workspace has its own share at the address
/// (B re-review): the heaviest spender stops at its own 2 starts a second
/// while a neighbour's share — and the global room left — is untouched.
#[derive(Debug)]
pub struct AddressBudget {
    /// Burst size — the most starts one address may pay at once.
    burst: f64,
    /// Sustained refill — tokens per second.
    rate: f64,

    /// The per-workspace sub-bucket's burst and refill at one address — the
    /// same share the write-time per-address cap sells a workspace.
    share_burst: f64,
    share_rate: f64,
    buckets: std::sync::Mutex<HashMap<IpAddr, IpBucket>>,
}

/// One address's buckets: the global ceiling, each workspace's share of it,
/// and the FIFO of workspaces whose pending start is waiting on the ceiling.
#[derive(Debug)]
struct IpBucket {
    global: Bucket,
    shares: HashMap<WorkspaceId, Bucket>,
    /// Waiting workspaces in arrival order — the next global token goes to
    /// the front (G1 re-review: round-robin, no split arithmetic).
    waiters: std::collections::VecDeque<WorkspaceId>,
    /// Each queued workspace's last ask — membership lookup and staleness.
    queued: HashMap<WorkspaceId, tokio::time::Instant>,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    at: tokio::time::Instant,
    /// The last time a take succeeded (share buckets only — `active` counts).
    last_admit: Option<tokio::time::Instant>,
}

/// Addresses remembered at once — a wildcard-DNS spray must not grow it unbounded.
const MAX_BUCKETS: usize = 8192;

/// Workspaces queued on one address at once — bounded like everything else;
/// a fuller queue refuses, it never grows memory with the spray.
const MAX_WAITERS: usize = 128;

/// A pending claim outlives its workspace's last ask by this long — slots
/// re-ask every monitor interval, so a workspace silent this long left the
/// queue; only the head is stale-checked, which bounds a dead claim's stall.
const WAITER_IDLE: Duration = Duration::from_secs(300);

fn refill(bucket: &mut Bucket, burst: f64, rate: f64, now: tokio::time::Instant) {
    bucket.tokens = (bucket.tokens + now.duration_since(bucket.at).as_secs_f64() * rate).min(burst);
    bucket.at = now;
}

impl AddressBudget {
    /// `burst` paid starts at once, refilling to `burst` over `per`; the
    /// per-workspace share equals the global bound (unattributed behaviour).
    pub fn new(burst: u32, per: Duration) -> Self {
        Self::with_share(burst, per, burst, per)
    }

    /// A global `burst`/`per` ceiling per address, and under it each
    /// workspace's own `share_burst`/`share_per` share at that address.
    pub fn with_share(burst: u32, per: Duration, share_burst: u32, share_per: Duration) -> Self {
        Self {
            burst: f64::from(burst),
            rate: f64::from(burst) / per.as_secs_f64().max(f64::EPSILON),
            share_burst: f64::from(share_burst),
            share_rate: f64::from(share_burst) / share_per.as_secs_f64().max(f64::EPSILON),
            buckets: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Charge one probe start at `ip` on behalf of `workspace`: false while
    /// either bound — the address's global ceiling, or the workspace's own
    /// share at it — is spent. `Instant` is tokio's clock: under paused time
    /// the refill is a test-clock fact.
    pub fn take(&self, ip: IpAddr, workspace: Option<WorkspaceId>) -> bool {
        // E re-review: the mapped spelling is the same address (`address_key`).
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
            v4 @ IpAddr::V4(_) => v4,
        };
        let now = tokio::time::Instant::now();
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !buckets.contains_key(&ip) && buckets.len() >= MAX_BUCKETS {
            // Fail closed (C re-review): evict only buckets that are whole
            // again — the global token recovered AND every workspace share in
            // it recovered — so a still-spent bucket (a victim's, or a
            // spender's share) is never laundered back to full by churn.
            buckets.retain(|_, b| {
                !(now.duration_since(b.global.at).as_secs_f64() * self.rate >= self.burst
                    && b.shares.values().all(|s| {
                        now.duration_since(s.at).as_secs_f64() * self.share_rate >= self.share_burst
                    }))
            });
            if buckets.len() >= MAX_BUCKETS {
                return false;
            }
        }
        let bucket = buckets.entry(ip).or_insert_with(|| IpBucket {
            global: Bucket {
                tokens: self.burst,
                at: now,
                last_admit: None,
            },
            shares: HashMap::new(),
            waiters: std::collections::VecDeque::new(),
            queued: HashMap::new(),
        });
        refill(&mut bucket.global, self.burst, self.rate, now);
        if let Some(workspace) = workspace {
            let share_can_pay = {
                let share = bucket.shares.entry(workspace).or_insert(Bucket {
                    tokens: self.share_burst,
                    at: now,
                    last_admit: None,
                });
                refill(share, self.share_burst, self.share_rate, now);
                share.tokens >= 1.0
            };
            // G1 re-review: not a fair split — a FIFO. A workspace whose start
            // waits on the ceiling holds its place in arrival order and every
            // global token goes to the head; the per-workspace share still
            // caps. Dead claims leave the queue: only the head is checked —
            // stale beyond WAITER_IDLE it is dropped, and a head that cannot
            // pay a share token yields to whoever can.
            while let Some(&front) = bucket.waiters.front() {
                let stale = bucket
                    .queued
                    .get(&front)
                    .is_some_and(|asked| now.duration_since(*asked) > WAITER_IDLE);
                let broke = bucket.shares.get_mut(&front).is_some_and(|front_share| {
                    refill(front_share, self.share_burst, self.share_rate, now);
                    front_share.tokens < 1.0
                });
                if !stale && !broke {
                    break;
                }
                bucket.waiters.pop_front();
                bucket.queued.remove(&front);
            }
            if let Some(asked) = bucket.queued.get_mut(&workspace) {
                *asked = now;
                if bucket.waiters.front() == Some(&workspace)
                    && share_can_pay
                    && bucket.global.tokens >= 1.0
                {
                    bucket.global.tokens -= 1.0;
                    if let Some(share) = bucket.shares.get_mut(&workspace) {
                        share.tokens -= 1.0;
                        share.last_admit = Some(now);
                    }
                    bucket.waiters.pop_front();
                    bucket.queued.remove(&workspace);
                    return true; // its turn arrived — the claim leaves the queue
                }
                return false; // head waiting on the ceiling, or still in line
            }
            if !share_can_pay {
                return false; // the workspace's own share of the address is spent
            }
            if bucket.waiters.is_empty() && bucket.global.tokens >= 1.0 {
                bucket.global.tokens -= 1.0;
                if let Some(share) = bucket.shares.get_mut(&workspace) {
                    share.tokens -= 1.0;
                    share.last_admit = Some(now);
                }
                return true; // uncontended — never queued, holds no queue state
            }
            if bucket.waiters.len() < MAX_WAITERS {
                bucket.waiters.push_back(workspace);
                bucket.queued.insert(workspace, now);
            }
            return false; // pending — its turn comes in arrival order
        }
        if bucket.global.tokens >= 1.0 {
            bucket.global.tokens -= 1.0;
            return true;
        }
        false
    }
}

/// The budget every probe this client makes shares (N1/B re-review): the
/// global per-address ceiling is 8 starts a second across ALL tenants and each
/// workspace's share of one address is the same 2 starts a second the
/// write-time per-address cap sells it — a spray can never take more than its
/// share, and the ceiling keeps a wildcard-DNS spray an order under the
/// engine's 512-slot bound. A fresh budget per client: the bound is a
/// property of the dialer, not of process state (and tests stay isolated).
fn probe_budget() -> Arc<AddressBudget> {
    Arc::new(AddressBudget::with_share(
        8,
        Duration::from_secs(1),
        2,
        Duration::from_secs(1),
    ))
}

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
    fn for_probes(allow_local_targets: bool, budget: Arc<AddressBudget>) -> Self {
        Self {
            allow_local_targets,
            budget: Some(budget),
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
            let Some(budget) = &budget else {
                return Ok(Box::new(addrs.into_iter()) as Addrs);
            };
            // The bound pays only what the dialer can take (B re-review), and
            // the dialer takes only what resolve() offers (N-2 re-review):
            // offer exactly ONE candidate — the first paying address in answer
            // order — so a fallback family or a sprayed answer's tail is never
            // charged for a connect it never produces. A redirect hop
            // re-enters here on the address it lands on, and a name whose
            // whole answer is spent gets no dial.
            let mut paid = None;
            let mut any_refused = false;
            for addr in addrs {
                if paid.is_some() {
                    break;
                }
                if budget.take(addr.ip(), workspace()) {
                    paid = Some(addr);
                } else {
                    any_refused = true;
                }
            }
            match paid {
                Some(addr) => Ok(Box::new(std::iter::once(addr)) as Addrs),
                None if any_refused => {
                    Err(Box::new(AddressBusy) as Box<dyn std::error::Error + Send + Sync>)
                }
                None => Ok(Box::new(std::iter::empty()) as Addrs),
            }
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
    /// The shared per-address starts budget this client's probes pay — `None`
    /// only where a resolver was built without one (the webhook deliverer, the
    /// pin fixture).
    budget: Option<Arc<AddressBudget>>,
    /// Response bodies are read at most this far — evidence is the status, not the page.
    max_body: usize,
}

impl ProbeClient {
    pub fn new(policy: ProbePolicy, max_body: usize) -> Result<Self, reqwest::Error> {
        let resolver = PolicyResolver::for_probes(policy.allow_local_targets, probe_budget());
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
        let budget = resolver.budget.clone();
        let hop_budget = budget.clone();
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
            // starts budget. Every probe resolves — and pays — its target, and
            // literal targets pay where the literal is still visible (below).
            .pool_max_idle_per_host(0)
            .tls_backend_preconfigured(tls)
            .redirect(redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= MAX_REDIRECT_HOPS {
                    attempt.stop()
                } else if redirect_policy.check(attempt.url()).is_err() {
                    // A hop the policy refuses is evidence, not a path: keep the 3xx.
                    attempt.stop()
                } else if let (Some(budget), Some(ip)) =
                    (hop_budget.as_deref(), literal_ip(attempt.url()))
                {
                    // A literal hop never reaches the resolver — hyper parses
                    // the socket itself — so it pays its start here, where the
                    // address is still visible (A re-review). The refusal rides
                    // the same AddressBusy the resolver's does.
                    if budget.take(ip, workspace()) {
                        attempt.follow()
                    } else {
                        attempt.error(AddressBusy)
                    }
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
            budget,
            max_body,
        })
    }

    /// A `GET` with the standard `200-299` expectation and no marker — the pre-M8
    /// behavior, kept as the convenience entry for callers without assertions.
    pub async fn probe(&self, url: &Url, deadline: Duration) -> ProbeResult {
        self.probe_expect(url, ProbeMethod::Get, &Expect::standard(), deadline)
            .await
    }

    /// One probe with the monitor's method and assertions (D-025). `expect` is the
    /// domain's already-validated set: an acceptable status plus an optional literal
    /// marker searched in the bounded body prefix — never regex, never a target.
    /// `HEAD` is opt-in: it rides the same redirect chain (preserved as `HEAD`,
    /// never rewritten to `GET`), answers `405`/`501` honestly, and never reads a
    /// body — a body marker under `HEAD` can't exist past the write boundary.
    pub async fn probe_expect(
        &self,
        url: &Url,
        method: ProbeMethod,
        expect: &Expect,
        deadline: Duration,
    ) -> ProbeResult {
        let start = Instant::now();
        let attempt = async {
            if let Err(rejected) = self.policy.check(url) {
                return Attempt::Rejected(rejected);
            }
            // An IP literal never reaches the resolver — hyper parses the
            // socket itself (A re-review) — so its start pays where the address
            // is still known: here, once per probe. A named target pays inside
            // `resolve` instead, on the addresses actually dialed.
            if let (Some(budget), Some(ip)) = (self.budget.as_deref(), literal_ip(url))
                && !budget.take(ip, workspace())
            {
                return Attempt::Budgeted;
            }
            match self
                .client
                .request(wire_method(method), url.clone())
                .send()
                .await
            {
                Err(error) => Attempt::Failed(error),
                Ok(mut response) => {
                    let status = response.status();
                    // Read the body bounded: a target that streams forever must not hold a
                    // worker past the deadline, and we only need enough to know it answered.
                    let mut read = 0usize;
                    let mut body = expect.body_contains.as_ref().map(|_| Vec::new());
                    let mut failed = None;
                    while failed.is_none() {
                        match response.chunk().await {
                            Err(error) => failed = Some(error),
                            Ok(None) => break,
                            Ok(Some(chunk)) => {
                                read += chunk.len();
                                if let Some(buf) = body.as_mut() {
                                    let room = self.max_body.saturating_sub(buf.len());
                                    buf.extend_from_slice(&chunk[..chunk.len().min(room)]);
                                }
                                if read > self.max_body {
                                    break;
                                }
                            }
                        }
                    }
                    match failed {
                        Some(error) => Attempt::Failed(error),
                        None => Attempt::Answered { status, body },
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
            // A literal's own bound refused the start — the same counted
            // coverage gap the resolver's refusal commits.
            Ok(Attempt::Budgeted) => ProbeResult {
                outcome: CheckOutcome::Transport,
                http_status: None,
                latency: start.elapsed(),
                detail: Some("address_budget"),
                limited: true,
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
            Ok(Attempt::Answered { status, body }) => {
                Self::answered_result(status, body.as_deref(), expect, start.elapsed())
            }
        }
    }

    /// The response made it — classify it against the monitor's contract.
    /// An acceptable status is still failure evidence when the marker is absent
    /// from the observed <= `max_body` prefix (D-025): a miss is
    /// `http_fail`/`assert_body`, never a pass.
    fn answered_result(
        status: reqwest::StatusCode,
        body: Option<&[u8]>,
        expect: &Expect,
        latency: Duration,
    ) -> ProbeResult {
        let u16_status = status.as_u16();
        let missed = expect.accepts(u16_status)
            && match (&expect.body_contains, body) {
                (Some(marker), Some(buf)) => {
                    !buf.windows(marker.len()).any(|w| w == marker.as_slice())
                }
                // Marker expected but no body was ever buffered — treat as a
                // miss, never a silent pass.
                (Some(_), None) => true,
                (None, _) => false,
            };
        let (outcome, detail) = if expect.accepts(u16_status) && missed {
            (CheckOutcome::HttpFail { status: u16_status }, Some("assert_body"))
        } else if expect.accepts(u16_status) {
            (CheckOutcome::Pass, None)
        } else {
            (CheckOutcome::HttpFail { status: u16_status }, None)
        };
        ProbeResult {
            outcome,
            http_status: Some(u16_status),
            latency,
            detail,
            limited: false,
        }
    }
}

/// The wire method — `monitor-domain` holds no I/O, so `ProbeMethod` maps to
/// reqwest's here. Only safe methods exist on the enum (D-025).
fn wire_method(method: ProbeMethod) -> reqwest::Method {
    match method {
        ProbeMethod::Get => reqwest::Method::GET,
        ProbeMethod::Head => reqwest::Method::HEAD,
    }
}

enum Attempt {
    Rejected(Rejected),
    /// The literal's own per-address bound refused the start (A re-review) —
    /// a reqwest error cannot be built by hand, so this carries the refusal.
    Budgeted,
    Failed(reqwest::Error),
    /// The final answer plus the body prefix when a marker was asserted — kept
    /// only while the marker needs it, bounded by `max_body` like any read.
    Answered {
        status: reqwest::StatusCode,
        body: Option<Vec<u8>>,
    },
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
        assert!(budget.take("8.8.8.8".parse().expect("ip"), None));
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
        assert!(budget.take(ip, None));
        assert!(budget.take(ip, None));
        assert!(!budget.take(ip, None), "the burst is spent");
        tokio::time::advance(Duration::from_secs(61)).await;
        assert!(budget.take(ip, None), "a window later the bucket refilled");
    }

    /// A loopback fixture that counts the requests it answers — the honest
    /// evidence a dial actually happened — and returns its `host:port`
    /// authority for URL building.
    async fn counting_fixture<F, R>(handler: F) -> SocketAddr
    where
        F: Fn() -> R + Clone + Send + Sync + 'static,
        R: axum::response::IntoResponse + Send + 'static,
    {
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(move || {
                let handler = handler.clone();
                async move { handler() }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fixture listener");
        let addr = listener.local_addr().expect("fixture addr");
        tokio::spawn(async move { axum::serve(listener, app).await });
        addr
    }

    /// A re-review: an IP literal never reaches the resolver — hyper parses the
    /// socket address itself — so the start must be paid where the address is
    /// still known, at the probe. Under a one-start budget the first literal
    /// probe dials (the fixture counts it); the second is a counted budget
    /// refusal, and the fixture never sees it.
    #[tokio::test]
    async fn literal_targets_pay_the_budget() {
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = hits.clone();
        let fixture = counting_fixture(move || {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            "ok"
        })
        .await;
        let url: Url = format!("http://{fixture}/").parse().expect("url");
        let client = ProbeClient::overridden(
            ProbePolicy {
                allow_local_targets: true,
                extra_roots: Vec::new(),
            },
            64 * 1024,
            overrides(),
            Arc::new(AddressBudget::new(1, Duration::from_secs(3600))),
        )
        .expect("client");
        let first = client.probe(&url, Duration::from_secs(3)).await;
        assert!(
            matches!(first.outcome, CheckOutcome::Pass),
            "the paid start dials: {first:?}"
        );
        let second = client.probe(&url, Duration::from_secs(3)).await;
        assert!(
            second.limited,
            "the literal's spent budget refuses the start: {second:?}"
        );
        assert_eq!(
            second.detail,
            Some("address_budget"),
            "the miss names its cause"
        );
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly the paid probe reached the fixture"
        );
    }

    /// A re-review: a redirect to a literal target pays the address it lands on
    /// too — the hop never reaches the resolver either, so the redirect closure
    /// is where its start is charged. The destination fixture counts hits; the
    /// spent hop must refuse *before* the dial.
    #[tokio::test]
    async fn literal_redirect_hops_pay_the_budget() {
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = hits.clone();
        let landing = counting_fixture(move || {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            "ok"
        })
        .await;
        let redirector = counting_fixture(move || {
            axum::response::Redirect::temporary(&format!("http://{landing}/"))
        })
        .await;
        let url: Url = format!("http://{redirector}/").parse().expect("url");
        let client = ProbeClient::overridden(
            ProbePolicy {
                allow_local_targets: true,
                extra_roots: Vec::new(),
            },
            64 * 1024,
            overrides(),
            // One start on the shared address: the redirector's literal spends
            // it, the hop's literal finds it spent.
            Arc::new(AddressBudget::new(1, Duration::from_secs(3600))),
        )
        .expect("client");
        let result = client.probe(&url, Duration::from_secs(3)).await;
        assert!(
            result.limited,
            "the literal hop's spent budget refuses the follow: {result:?}"
        );
        assert_eq!(
            result.detail,
            Some("address_budget"),
            "the hop's refusal names the budget"
        );
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "the refused hop never dialed the landing fixture"
        );
    }

    /// C re-review: the bounded map fails closed — a spent bucket must stay
    /// spent no matter how many other addresses churn through it, and a new
    /// address waits rather than evicting a still-spent one.
    #[tokio::test]
    async fn a_spent_bucket_stays_spent_under_map_churn() {
        let budget = AddressBudget::new(1, Duration::from_secs(3600));
        let victim: IpAddr = "8.8.8.8".parse().expect("ip");
        assert!(budget.take(victim, None));
        assert!(!budget.take(victim, None), "spent stays spent");
        for i in 0..MAX_BUCKETS {
            let [_, b1, b2, b3] = u32::try_from(i).expect("index").to_be_bytes();
            let ip = IpAddr::V4(Ipv4Addr::new(9, b1, b2, b3));
            budget.take(ip, None);
        }
        assert!(
            !budget.take(victim, None),
            "churn must not refill a spent bucket"
        );
        let fresh: IpAddr = "7.7.7.7".parse().expect("ip");
        assert!(
            !budget.take(fresh, None),
            "a map full of spent buckets refuses the new address — never a clear"
        );
    }

    /// E re-review: `::ffff:V` is the same address as `V` — the mapped spelling
    /// spends the same bucket the plain one does (the `origin.rs` `address_key`
    /// normalisation, applied at the charge).
    #[tokio::test]
    async fn an_ipv4_mapped_spelling_shares_the_v4_bucket() {
        let budget = AddressBudget::new(1, Duration::from_secs(3600));
        let v4: IpAddr = "8.8.8.8".parse().expect("ip");
        let mapped: IpAddr = "::ffff:8.8.8.8".parse().expect("ip");
        assert!(budget.take(mapped, None));
        assert!(
            !budget.take(v4, None),
            "the mapped spelling spends the same bucket"
        );
    }

    /// B re-review: the global ceiling is shared, but a workspace only ever
    /// spends its own share of it — the heaviest spender stops at its share
    /// while a neighbour's share at the same address is untouched.
    #[tokio::test]
    async fn the_heaviest_spender_stops_at_its_workspace_share() {
        let budget =
            AddressBudget::with_share(4, Duration::from_secs(3600), 2, Duration::from_secs(3600));
        let victim: IpAddr = "8.8.8.8".parse().expect("ip");
        let a = WorkspaceId::new();
        let b = WorkspaceId::new();
        assert!(budget.take(victim, Some(a)));
        assert!(budget.take(victim, Some(a)));
        assert!(
            !budget.take(victim, Some(a)),
            "a's own share at the address is spent — the global room stays"
        );
        assert!(
            budget.take(victim, Some(b)),
            "b's share is untouched: one tenant cannot spend another's"
        );
        // And the global ceiling still binds everyone together: four shares of
        // two are all the room there is.
        assert!(budget.take(victim, Some(b)));
        assert!(
            !budget.take(victim, Some(b)),
            "the global ceiling binds after the shares do"
        );
    }

    /// G1 re-review: not a fair split — a FIFO. Every global token goes to the
    /// next waiting workspace in arrival order, so nobody goes dark: once the
    /// ceiling is spent, a pending start keeps its place and the next refill
    /// serves whoever queued first — first-come no longer starves newcomers.
    #[tokio::test(start_paused = true)]
    async fn a_spent_ceiling_serves_the_waiters_in_order() {
        let budget =
            AddressBudget::with_share(4, Duration::from_secs(4), 2, Duration::from_secs(3600));
        let ip: IpAddr = "9.9.9.9".parse().expect("ip");
        let (spender_a, spender_b, waiter_c, waiter_d) = (
            WorkspaceId::new(),
            WorkspaceId::new(),
            WorkspaceId::new(),
            WorkspaceId::new(),
        );
        assert!(budget.take(ip, Some(spender_a)));
        assert!(budget.take(ip, Some(spender_a)));
        assert!(budget.take(ip, Some(spender_b)));
        assert!(budget.take(ip, Some(spender_b)));
        // The ceiling is spent 2+2 — first-come refused c on every slot,
        // unknown forever, no alert. Round-robin: each refusal registers a
        // pending start; c and d hold their places in arrival order.
        assert!(
            !budget.take(ip, Some(waiter_c)),
            "no global token left — c's start is pending"
        );
        assert!(
            !budget.take(ip, Some(waiter_d)),
            "d queues behind c in arrival order"
        );
        // One refill token per second: it goes to the head — c — not to d,
        // and not to the spenders who emptied the bucket.
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(
            !budget.take(ip, Some(waiter_d)),
            "still c's turn — the head is served first"
        );
        assert!(
            budget.take(ip, Some(waiter_c)),
            "the first refill serves the head of the queue"
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(
            budget.take(ip, Some(waiter_d)),
            "the next token serves the next in order"
        );
        // And the spenders do not jump the queue: even a fresh workspace waits
        // behind whatever is pending once the ceiling is gone.
        let late_e = WorkspaceId::new();
        let later_f = WorkspaceId::new();
        assert!(
            !budget.take(ip, Some(late_e)),
            "the queue is empty but the ceiling is spent — e's start is pending"
        );
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(!budget.take(ip, Some(later_f)), "f waits behind e");
        assert!(budget.take(ip, Some(late_e)), "e's claim was first");
    }

    /// A re-review: every admit takes a global token — the ceiling binds PACED
    /// tenants, not only burst ones. Seven workspaces each doing two starts
    /// against an 8-start ceiling get exactly the ceiling: four take it, three
    /// register pending starts that stay pending without a refill.
    #[tokio::test]
    async fn paced_workspaces_cannot_exceed_the_global_ceiling() {
        let budget =
            AddressBudget::with_share(8, Duration::from_secs(3600), 2, Duration::from_secs(3600));
        let ip: IpAddr = "9.9.9.9".parse().expect("ip");
        let workspaces: Vec<WorkspaceId> = (0..7).map(|_| WorkspaceId::new()).collect();
        let mut admitted = 0;
        for _round in 0..2 {
            for workspace in &workspaces {
                admitted += usize::from(budget.take(ip, Some(*workspace)));
            }
        }
        assert_eq!(
            admitted, 8,
            "the ceiling admits its tokens and queues the rest — never more"
        );
    }

    /// G1 re-review proof, under paused time: admits never exceed the token
    /// stream — `burst + rate·t` over any window — and every waiting workspace
    /// reaches the head within ceil(waiters/burst) seconds while it keeps
    /// asking. Twenty workspaces at 8/s: nobody waits past three seconds.
    #[tokio::test(start_paused = true)]
    async fn the_ceiling_bound_holds_and_every_waiter_is_served_in_turn() {
        let budget =
            AddressBudget::with_share(8, Duration::from_secs(1), 8, Duration::from_secs(1));
        let ip: IpAddr = "9.9.9.9".parse().expect("ip");
        let workspaces: Vec<WorkspaceId> = (0..20).map(|_| WorkspaceId::new()).collect();
        let mut first_admit: Vec<Option<u64>> = vec![None; 20];
        let mut admitted = 0usize;
        for second in 0u64..4 {
            for (i, workspace) in workspaces.iter().enumerate() {
                if budget.take(ip, Some(*workspace)) {
                    admitted += 1;
                    if first_admit[i].is_none() {
                        first_admit[i] = Some(second);
                    }
                }
            }
            assert!(
                admitted <= 8 + usize::try_from(second).expect("small") * 8,
                "the ceiling is a token stream: admits ({admitted}) must not exceed 8+8t"
            );
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        for (i, first) in first_admit.iter().enumerate() {
            assert_eq!(
                *first,
                Some(u64::try_from(i / 8).expect("small")),
                "workspace {i} waits only its position in line: {}",
                first.map_or_else(|| "never".to_owned(), |s| s.to_string())
            );
        }
    }

    /// G1 re-review: eight greedy neighbours cannot starve a ninth tenant —
    /// round-robin serves the victim one start in every cycle of nine.
    #[tokio::test(start_paused = true)]
    async fn a_victim_keeps_its_turn_behind_greedy_neighbours() {
        let budget =
            AddressBudget::with_share(8, Duration::from_secs(1), 8, Duration::from_secs(1));
        let ip: IpAddr = "9.9.9.9".parse().expect("ip");
        let attackers: Vec<WorkspaceId> = (0..8).map(|_| WorkspaceId::new()).collect();
        let victim = WorkspaceId::new();
        let mut victim_admits = 0usize;
        for _ in 0..60 {
            for attacker in &attackers {
                budget.take(ip, Some(*attacker));
            }
            victim_admits += usize::from(budget.take(ip, Some(victim)));
            tokio::time::advance(Duration::from_secs(1)).await;
        }
        assert!(
            victim_admits >= 25,
            "queued first each cycle, the victim turns roughly every other second — got {victim_admits}"
        );
    }

    /// N-6 re-review: the global ceiling itself must bind — earlier tests
    /// only ever refused on a workspace's own share, so deleting the global
    /// check survived them. Give the share more than the ceiling: the only
    /// thing that can refuse the fifth take is the ceiling.
    #[tokio::test]
    async fn a_workspace_cannot_exceed_the_global_ceiling_even_inside_its_share() {
        let budget =
            AddressBudget::with_share(4, Duration::from_secs(3600), 8, Duration::from_secs(3600));
        let ip: IpAddr = "9.9.9.9".parse().expect("ip");
        let a = WorkspaceId::new();
        for _ in 0..4 {
            assert!(budget.take(ip, Some(a)));
        }
        // Share 8, spent 4 — plenty of share left; only the ceiling can refuse
        // this take (a spent ceiling queues the start, never admits it).
        assert!(
            !budget.take(ip, Some(a)),
            "the ceiling alone refuses even with share to spare"
        );
    }

    /// N-2 re-review: hyper dials what `resolve()` hands it — so the resolver
    /// offers AND charges only the candidate actually offered: the first paying
    /// address in answer order. A second candidate — including the other
    /// family's — is never dialed, so it must leave its bucket untouched: a
    /// sprayed [own-v6, victim-v4] answer taxes the victim nothing per hop.
    #[tokio::test]
    async fn the_resolver_offers_and_charges_only_the_dialed_candidate() {
        let map = overrides();
        let many: Vec<SocketAddr> = (0..200)
            .map(|i| {
                format!("8.8.{}.{}:0", i / 256, i % 256)
                    .parse()
                    .expect("addr")
            })
            .collect();
        map.lock()
            .expect("map")
            .insert("spray.test".to_owned(), many.clone());
        let budget = Arc::new(AddressBudget::new(1, Duration::from_secs(3600)));
        let resolver = PolicyResolver::for_test_budgeted(false, map, budget.clone());
        let yielded = resolver
            .resolve(Name::from_str("spray.test").expect("name"))
            .await
            .expect("the first paying candidate pays")
            .collect::<Vec<_>>();
        assert_eq!(
            yielded,
            vec![many[0]],
            "exactly one candidate is offered — the one the dialer will use"
        );
        assert!(
            !budget.take(many[0].ip(), None),
            "the dialed candidate was charged"
        );
        assert!(
            budget.take(many[50].ip(), None),
            "an undialed candidate was never charged"
        );
    }

    /// N-2 re-review: the mixed-family case the ruling names — an answer of
    /// [attacker-v6, victim-v4] leaves the victim's bucket untouched: the v6 is
    /// the candidate offered, the v4 fallback is never dialed, never charged.
    #[tokio::test]
    async fn the_fallback_family_leaves_its_bucket_untouched() {
        let map = overrides();
        let v6: SocketAddr = "[2001:4860:4860::8888]:0".parse().expect("addr");
        let v4: SocketAddr = "8.8.8.8:0".parse().expect("addr");
        map.lock()
            .expect("map")
            .insert("mixed.test".to_owned(), vec![v6, v4]);
        let budget = Arc::new(AddressBudget::new(1, Duration::from_secs(3600)));
        let resolver = PolicyResolver::for_test_budgeted(false, map, budget.clone());
        let yielded = resolver
            .resolve(Name::from_str("mixed.test").expect("name"))
            .await
            .expect("the paying v6 pays")
            .collect::<Vec<_>>();
        assert_eq!(yielded, vec![v6], "only the dialed candidate is offered");
        assert!(
            budget.take(v4.ip(), None),
            "the fallback v4 was never charged — it was never dialed"
        );
        assert!(!budget.take(v6.ip(), None), "the offered v6 paid");
    }

    /// B re-review: the workspace attribution reaches the resolver through the
    /// probe task-local — the same address resolves against a different
    /// workspace's share when the schedule changes hands.
    #[tokio::test]
    async fn the_resolver_charges_the_attributed_workspace() {
        let map = overrides();
        map.lock().expect("map").insert(
            "victim.test".to_owned(),
            vec!["8.8.8.8:0".parse().expect("addr")],
        );
        let budget = Arc::new(AddressBudget::with_share(
            4,
            Duration::from_secs(3600),
            2,
            Duration::from_secs(3600),
        ));
        let resolver = PolicyResolver::for_test_budgeted(false, map, budget);
        let a = WorkspaceId::new();
        let b = WorkspaceId::new();
        let name = || Name::from_str("victim.test").expect("name");
        for _ in 0..2 {
            assert!(
                PROBE_WORKSPACE
                    .scope(a, resolver.resolve(name()))
                    .await
                    .is_ok(),
                "a's own share pays"
            );
        }
        assert!(
            PROBE_WORKSPACE
                .scope(a, resolver.resolve(name()))
                .await
                .is_err(),
            "a's third start meets its own spent share"
        );
        assert!(
            PROBE_WORKSPACE
                .scope(b, resolver.resolve(name()))
                .await
                .is_ok(),
            "b's share at the same address is untouched"
        );
    }

    /// A loopback axum fixture for the M8 tests: arbitrary router, literal host —
    /// the `for_test` policy allows local targets and its resolver has no budget,
    /// so literal probes dial free of the starts accounting.
    async fn serve(router: axum::Router) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fixture listener");
        let addr = listener.local_addr().expect("fixture addr");
        tokio::spawn(async move { axum::serve(listener, router).await });
        addr
    }

    fn lab_client(max_body: usize) -> ProbeClient {
        ProbeClient::pinned(
            ProbePolicy {
                allow_local_targets: true,
                extra_roots: Vec::new(),
            },
            max_body,
            &[],
        )
        .expect("client")
    }

    /// D-025: a `HEAD` probe stays `HEAD` through a 303 — the one redirect status
    /// that rewrites every other method to `GET`. The landing handler answers 501
    /// to anything but `HEAD`, so a rewrite could only land as `http_fail`.
    #[tokio::test]
    async fn head_stays_head_across_a_redirect() {
        use axum::extract::Request;
        use axum::response::IntoResponse;
        let land = |req: Request| async move {
            if req.method() == reqwest::Method::HEAD {
                axum::http::StatusCode::OK.into_response()
            } else {
                axum::http::StatusCode::NOT_IMPLEMENTED.into_response()
            }
        };
        let router = axum::Router::new()
            .route(
                "/go",
                axum::routing::any(|| async {
                    (
                        axum::http::StatusCode::SEE_OTHER,
                        [(axum::http::header::LOCATION, "/land")],
                    )
                }),
            )
            .route("/land", axum::routing::any(land));
        let url: Url = format!("http://{}/go", serve(router).await)
            .parse()
            .expect("url");
        let result = lab_client(64 * 1024)
            .probe_expect(
                &url,
                ProbeMethod::Head,
                &Expect::standard(),
                Duration::from_secs(5),
            )
            .await;
        assert_eq!(
            result.outcome,
            CheckOutcome::Pass,
            "HEAD must land as HEAD through the 303: {result:?}"
        );
    }

    /// D-025: a `HEAD` answered `405`/`501` is recorded as that answer — never
    /// silently retried as `GET` (a GET retry would see this fixture's 200).
    #[tokio::test]
    async fn a_head_probe_that_answers_405_is_recorded_honestly() {
        use axum::extract::Request;
        use axum::response::IntoResponse;
        let router = axum::Router::new().route(
            "/",
            axum::routing::any(|req: Request| async move {
                if req.method() == reqwest::Method::GET {
                    "ok".into_response()
                } else {
                    axum::http::StatusCode::METHOD_NOT_ALLOWED.into_response()
                }
            }),
        );
        let url: Url = format!("http://{}/", serve(router).await)
            .parse()
            .expect("url");
        let result = lab_client(64 * 1024)
            .probe_expect(
                &url,
                ProbeMethod::Head,
                &Expect::standard(),
                Duration::from_secs(5),
            )
            .await;
        assert_eq!(
            result.outcome,
            CheckOutcome::HttpFail { status: 405 },
            "the 405 is the committed answer, not a GET retry: {result:?}"
        );
    }

    /// D-025: the marker is searched literally in the observed prefix — a present
    /// marker passes, an absent one is `http_fail`/`assert_body`, and `.*` is just
    /// bytes (no engine exists to exploit).
    #[tokio::test]
    async fn the_marker_is_literal_and_the_miss_is_failure_evidence() {
        let router = axum::Router::new().route("/", axum::routing::get(|| async { "xZZy" }));
        let url: Url = format!("http://{}/", serve(router).await)
            .parse()
            .expect("url");
        let client = lab_client(64 * 1024);
        let expect = Expect {
            body_contains: Some(b"x.*y".to_vec()),
            ..Expect::standard()
        };
        let miss = client
            .probe_expect(&url, ProbeMethod::Get, &expect, Duration::from_secs(5))
            .await;
        assert_eq!(miss.outcome, CheckOutcome::HttpFail { status: 200 });
        assert_eq!(
            miss.detail,
            Some("assert_body"),
            "a body that only matches the pattern loosely still misses: {miss:?}"
        );
        // The literal string `x.*y` in the body is the only thing that passes.
        let router = axum::Router::new().route(
            "/",
            axum::routing::get(|| async { "contains x.*y literally" }),
        );
        let url: Url = format!("http://{}/", serve(router).await)
            .parse()
            .expect("url");
        let hit = client
            .probe_expect(&url, ProbeMethod::Get, &expect, Duration::from_secs(5))
            .await;
        assert_eq!(hit.outcome, CheckOutcome::Pass, "the literal bytes pass");
    }

    /// D-025: the assertion reads only the observed `<= max_body` prefix — a
    /// marker split across the read boundary, or entirely past it, is a miss.
    #[tokio::test]
    async fn a_marker_past_the_body_bound_never_passes() {
        // max_body 16: `MARK` lands at bytes 13..17 — split across the bound.
        let router =
            axum::Router::new().route("/", axum::routing::get(|| async { "AAAAAAAAAAAAMARKER" }));
        let url: Url = format!("http://{}/", serve(router).await)
            .parse()
            .expect("url");
        let client = lab_client(16);
        let expect = Expect {
            body_contains: Some(b"MARKER".to_vec()),
            ..Expect::standard()
        };
        let result = client
            .probe_expect(&url, ProbeMethod::Get, &expect, Duration::from_secs(5))
            .await;
        assert_eq!(result.outcome, CheckOutcome::HttpFail { status: 200 });
        assert_eq!(
            result.detail,
            Some("assert_body"),
            "a marker straddling the bound misses: {result:?}"
        );
    }

    /// D-025: the client is built without decompression features — the marker is
    /// searched on the wire bytes, so a compressed bomb only ever costs the
    /// bounded read. A gzipped marker is a miss, never inflation.
    #[tokio::test]
    async fn compressed_bytes_are_searched_as_received() {
        let gzipped: &[u8] = &[
            31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 11, 118, 245, 11, 241, 244, 115, 245, 209, 205, 72,
            45, 74, 5, 0, 125, 143, 180, 174, 13, 0, 0, 0,
        ];
        let router = axum::Router::new().route(
            "/",
            axum::routing::get(move || async move {
                (
                    [(axum::http::header::CONTENT_ENCODING, "gzip")],
                    axum::body::Body::from(gzipped.to_vec()),
                )
            }),
        );
        let url: Url = format!("http://{}/", serve(router).await)
            .parse()
            .expect("url");
        let expect = Expect {
            body_contains: Some(b"SENTINEL-here".to_vec()),
            ..Expect::standard()
        };
        let result = lab_client(64 * 1024)
            .probe_expect(&url, ProbeMethod::Get, &expect, Duration::from_secs(5))
            .await;
        assert_eq!(result.outcome, CheckOutcome::HttpFail { status: 200 });
        assert_eq!(
            result.detail,
            Some("assert_body"),
            "wire bytes are searched, never inflated: {result:?}"
        );
    }

    /// A raw fixture that answers 200 and then dribbles one byte a beat forever —
    /// a slowloris body that no buffer bound would ever finish.
    async fn drip_fixture() -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fixture listener");
        let addr = listener.local_addr().expect("fixture addr");
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let head = b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ntransfer-encoding: chunked\r\n\r\n";
                    if sock.write_all(head).await.is_err() {
                        return;
                    }
                    loop {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        if sock.write_all(b"1\r\nM\r\n").await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        addr
    }

    /// D-025: a slowloris body meets the probe's deadline — the whole attempt,
    /// body read included, sits inside `tokio::time::timeout`.
    #[tokio::test]
    async fn a_body_that_dribbles_meets_the_deadline() {
        let url: Url = format!("http://{}/", drip_fixture().await)
            .parse()
            .expect("url");
        let expect = Expect {
            body_contains: Some(b"SENTINEL".to_vec()),
            ..Expect::standard()
        };
        let result = lab_client(64 * 1024)
            .probe_expect(&url, ProbeMethod::Get, &expect, Duration::from_secs(1))
            .await;
        assert_eq!(
            result.outcome,
            CheckOutcome::Timeout,
            "a dribbling body dies on the deadline: {result:?}"
        );
    }

    /// D-025: an assertion string is evidence to search, never a request target —
    /// a marker that looks like a URL is matched against the already-validated
    /// response and dials nothing.
    #[tokio::test]
    async fn a_marker_is_searched_never_dialed() {
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = hits.clone();
        let router = axum::Router::new().route(
            "/",
            axum::routing::get(move || {
                let count = count.clone();
                async move {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    "plain body"
                }
            }),
        );
        let url: Url = format!("http://{}/", serve(router).await)
            .parse()
            .expect("url");
        let expect = Expect {
            // A marker shaped exactly like a URL — it is data, not a request.
            body_contains: Some(b"http://127.0.0.1:9/internal".to_vec()),
            ..Expect::standard()
        };
        let result = lab_client(64 * 1024)
            .probe_expect(&url, ProbeMethod::Get, &expect, Duration::from_secs(5))
            .await;
        assert_eq!(result.outcome, CheckOutcome::HttpFail { status: 200 });
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the only dial is the validated target: {result:?}"
        );
    }
}
