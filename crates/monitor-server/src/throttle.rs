//! Credential guessing is throttled per client address (M1, L1).
//!
//! Every attempt that presents a credential — `POST /api/login`, or an
//! `Authorization: Bearer` header on `/api` — first takes one token from its address's
//! bucket ([`BURST`] tokens, one back every [`REFILL_EVERY`]); an empty bucket is 429 with
//! `Retry-After`, *before* the credential is compared, so a throttled address can't learn
//! whether a guess was right. A correct credential gives its token back, so the owner
//! never pays for using the right token; a wrong one keeps it spent. One client can
//! therefore guess at most `BURST`, then one per `REFILL_EVERY`, and its guesses touch no
//! other address's budget: an anonymous flood can't lock the owner out.
//!
//! The address is the TCP peer — IPv4 as is, IPv6 by /64 (one host usually holds a whole
//! /64, so rotating within it buys nothing). Behind a reverse proxy every client shares
//! the proxy's address unless `SMON_TRUSTED_PROXY` names it; then the right-most
//! `X-Forwarded-For` entry (the address the proxy itself saw) is the client.
//!
//! Memory is bounded: at most [`MAX_TRACKED`] addresses. A full table first forgets
//! addresses whose bucket has refilled (they are no different from new ones), then the
//! fullest bucket — the least-throttled client loses the least by being forgotten.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::http::HeaderMap;
use tokio::time::Instant;

/// Guesses an address may make back to back.
pub const BURST: f64 = 5.0;
/// One guess comes back this often.
pub const REFILL_EVERY: Duration = Duration::from_secs(2);
/// Anonymous reads an address may make back to back — a page reload storm, not a
/// credential guess, so the budget is far above the guessing one.
pub const READ_BURST: f64 = 30.0;
/// One anonymous read comes back this often (2/s sustained — a viewer refreshing an
/// incident page never notices; a flood is still bounded).
pub const READ_REFILL_EVERY: Duration = Duration::from_millis(500);
/// Addresses tracked at once (about 40 bytes each).
pub const MAX_TRACKED: usize = 4096;

/// Who is guessing: an IPv4 address, or an IPv6 /64.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClientKey {
    V4(Ipv4Addr),
    V6Net([u16; 4]),
    /// No peer address known (in-process tests): one shared bucket.
    Unknown,
}

impl ClientKey {
    pub fn of(ip: IpAddr) -> Self {
        match ip {
            IpAddr::V4(v4) => Self::V4(v4),
            IpAddr::V6(v6) => {
                if let Some(v4) = v6.to_ipv4_mapped() {
                    Self::V4(v4)
                } else {
                    let s = v6.segments();
                    Self::V6Net([s[0], s[1], s[2], s[3]])
                }
            }
        }
    }

    /// The client behind a request: the TCP peer, or — only when the peer is the trusted
    /// proxy — the right-most `X-Forwarded-For` entry. A client can put anything in that
    /// header; only the hop our own proxy appended is believed.
    pub fn from_request(
        peer: Option<SocketAddr>,
        headers: &HeaderMap,
        trusted_proxy: Option<IpAddr>,
    ) -> Self {
        let Some(peer) = peer else {
            return Self::Unknown;
        };
        // Only the right-most hop: the one our proxy appended. If it doesn't parse, the
        // client is the peer — never an entry further left, which the client wrote.
        if trusted_proxy.is_some_and(|proxy| proxy == peer.ip())
            && let Some(last_line) = headers.get_all("x-forwarded-for").iter().next_back()
            && let Some(hop) = last_line
                .to_str()
                .ok()
                .and_then(|line| line.rsplit(',').next())
            && let Ok(client) = hop.trim().parse::<IpAddr>()
        {
            return Self::of(client);
        }
        Self::of(peer.ip())
    }
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn refilled(self, now: Instant, burst: f64, refill_every: Duration) -> f64 {
        let earned =
            now.saturating_duration_since(self.at).as_secs_f64() / refill_every.as_secs_f64();
        (self.tokens + earned).min(burst)
    }
}

/// The per-address buckets. Cheap to clone (one shared table).
#[derive(Debug, Clone)]
pub struct Throttle {
    buckets: Arc<Mutex<HashMap<ClientKey, Bucket>>>,
    capacity: usize,
    burst: f64,
    refill_every: Duration,
}

impl Default for Throttle {
    /// The credential-guessing budget.
    fn default() -> Self {
        Self::with_capacity(MAX_TRACKED)
    }
}

/// A taken token. [`Attempt::succeeded`] gives it back; dropping it keeps it spent.
#[derive(Debug)]
pub struct Attempt {
    throttle: Throttle,
    key: ClientKey,
}

impl Attempt {
    /// The credential was right: the owner doesn't pay for using it.
    pub fn succeeded(self) {
        let now = Instant::now();
        let mut buckets = self.throttle.lock();
        if let Some(bucket) = buckets.get_mut(&self.key) {
            *bucket = Bucket {
                tokens: (bucket.refilled(now, self.throttle.burst, self.throttle.refill_every)
                    + 1.0)
                    .min(self.throttle.burst),
                at: now,
            };
        }
    }
}

impl Throttle {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buckets: Arc::new(Mutex::new(HashMap::new())),
            capacity: capacity.max(1),
            burst: BURST,
            refill_every: REFILL_EVERY,
        }
    }

    /// The anonymous-read budget: separate buckets from credential guessing, so a loop
    /// against the public status pages can't lock out a sign-in, and a sign-in attack
    /// can't starve readers mid-incident (S2 review).
    pub fn reads() -> Self {
        Self {
            buckets: Arc::new(Mutex::new(HashMap::new())),
            capacity: MAX_TRACKED,
            burst: READ_BURST,
            refill_every: READ_REFILL_EVERY,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ClientKey, Bucket>> {
        // The map holds plain numbers; a panic mid-update can't leave it inconsistent.
        self.buckets.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Take one token for `key`, or say how long until one is back (`Err`).
    pub fn admit(&self, key: ClientKey) -> Result<Attempt, Duration> {
        let now = Instant::now();
        let mut buckets = self.lock();
        if !buckets.contains_key(&key) && buckets.len() >= self.capacity {
            make_room(&mut buckets, now, self.burst, self.refill_every);
        }
        let bucket = buckets.entry(key).or_insert(Bucket {
            tokens: self.burst,
            at: now,
        });
        let tokens = bucket.refilled(now, self.burst, self.refill_every);
        if tokens < 1.0 {
            *bucket = Bucket { tokens, at: now };
            let wait = (1.0 - tokens) * self.refill_every.as_secs_f64();
            return Err(Duration::from_secs_f64(wait));
        }
        *bucket = Bucket {
            tokens: tokens - 1.0,
            at: now,
        };
        drop(buckets);
        Ok(Attempt {
            throttle: self.clone(),
            key,
        })
    }

    /// Addresses currently tracked.
    pub fn tracked(&self) -> usize {
        self.lock().len()
    }
}

/// Forget refilled buckets; if none, forget the fullest one.
fn make_room(buckets: &mut HashMap<ClientKey, Bucket>, now: Instant, burst: f64, refill: Duration) {
    buckets.retain(|_, bucket| bucket.refilled(now, burst, refill) < burst);
    if buckets.is_empty() {
        return;
    }
    let fullest = buckets
        .iter()
        .max_by(|a, b| {
            a.1.refilled(now, burst, refill)
                .total_cmp(&b.1.refilled(now, burst, refill))
        })
        .map(|(key, _)| *key);
    if let Some(key) = fullest {
        buckets.remove(&key);
    }
}

/// `Retry-After` in whole seconds, rounded up, at least 1.
pub fn retry_after_secs(wait: Duration) -> u64 {
    (wait.as_secs() + u64::from(wait.subsec_nanos() > 0)).max(1)
}

/// Wrong logins sleeping their penalty at once, across all addresses. The penalty is a
/// courtesy delay — the per-address buckets are what bound guessing — so past this
/// ceiling a wrong login is answered at once rather than holding another task. A right
/// token never waits for it.
pub const PENALTY_SLOTS: usize = 16;

/// What `routes()` hands every request (an extension): the credential-guessing
/// buckets, the separate anonymous-read buckets, the penalty ceiling, and which
/// peer may speak for clients through `X-Forwarded-For`.
#[derive(Debug, Clone)]
pub struct Guard {
    pub throttle: Throttle,
    /// Anonymous public reads (`/s/{slug}/status.json`): their own buckets, so a read
    /// loop can't spend the credential budget and a guessing flood can't starve
    /// readers (S2 review). Never refunded — there is no credential to earn it back.
    pub reads: Throttle,
    pub penalties: Arc<tokio::sync::Semaphore>,
    pub trusted_proxy: Option<IpAddr>,
}

impl Guard {
    pub fn new(trusted_proxy: Option<IpAddr>) -> Self {
        Self {
            throttle: Throttle::default(),
            reads: Throttle::reads(),
            penalties: Arc::new(tokio::sync::Semaphore::new(PENALTY_SLOTS)),
            trusted_proxy,
        }
    }

    /// The client key for a request with these extensions and headers.
    pub fn client(&self, extensions: &axum::http::Extensions, headers: &HeaderMap) -> ClientKey {
        let peer = extensions
            .get::<axum::extract::ConnectInfo<SocketAddr>>()
            .map(|info| info.0);
        ClientKey::from_request(peer, headers, self.trusted_proxy)
    }
}

/// The 429 a throttled address gets.
pub fn too_many(wait: Duration) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        axum::http::StatusCode::TOO_MANY_REQUESTS,
        [(
            axum::http::header::RETRY_AFTER,
            retry_after_secs(wait).to_string(),
        )],
        axum::Json(serde_json::json!({
            "error": { "code": "too_many_requests",
                       "message": "too many failed credentials from this address; retry later" }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(ip: &str) -> ClientKey {
        ClientKey::of(ip.parse().expect("ip"))
    }

    #[test]
    fn ipv6_clients_are_grouped_by_64_and_mapped_v4_is_v4() {
        assert_eq!(key("2001:db8::1"), key("2001:db8::ffff:1234"));
        assert_ne!(key("2001:db8::1"), key("2001:db8:0:1::1"));
        assert_eq!(key("::ffff:198.51.100.1"), key("198.51.100.1"));
        assert_ne!(key("198.51.100.1"), key("198.51.100.2"));
    }

    #[test]
    fn forwarded_for_is_believed_only_from_the_trusted_proxy() {
        let mut headers = HeaderMap::new();
        headers.append(
            "x-forwarded-for",
            "6.6.6.6, 198.51.100.9".parse().expect("hv"),
        );
        let proxy: SocketAddr = "127.0.0.1:5000".parse().expect("addr");
        let stranger: SocketAddr = "203.0.113.5:5000".parse().expect("addr");
        let trusted = Some(proxy.ip());
        // Through our proxy: the hop it appended (right-most), not what the client wrote.
        assert_eq!(
            ClientKey::from_request(Some(proxy), &headers, trusted),
            key("198.51.100.9")
        );
        // From anyone else, or with no proxy configured: the header is ignored.
        assert_eq!(
            ClientKey::from_request(Some(stranger), &headers, trusted),
            key("203.0.113.5")
        );
        assert_eq!(
            ClientKey::from_request(Some(proxy), &headers, None),
            key("127.0.0.1")
        );
        assert_eq!(
            ClientKey::from_request(Some(proxy), &HeaderMap::new(), trusted),
            key("127.0.0.1")
        );
        assert_eq!(
            ClientKey::from_request(None, &headers, trusted),
            ClientKey::Unknown
        );
    }

    /// Only the right-most hop — the one our proxy appended — is believed. If it doesn't
    /// parse (a proxy that writes `ip:port`, a mangled header), the client is the peer:
    /// never an entry to its left, which the client wrote itself.
    #[test]
    fn an_unparseable_proxy_hop_falls_back_to_the_peer_not_to_client_entries() {
        let proxy: SocketAddr = "127.0.0.1:5000".parse().expect("addr");
        let trusted = Some(proxy.ip());
        for forwarded in [
            vec!["6.6.6.6, 198.51.100.9:443"],
            vec!["6.6.6.6, not-an-ip"],
            vec!["6.6.6.6,"],
            vec!["6.6.6.6", "garbage"], // the last header line holds the proxy's hop
        ] {
            let mut headers = HeaderMap::new();
            for value in &forwarded {
                headers.append("x-forwarded-for", value.parse().expect("hv"));
            }
            assert_eq!(
                ClientKey::from_request(Some(proxy), &headers, trusted),
                key("127.0.0.1"),
                "{forwarded:?}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_success_refunds_and_a_failure_stays_spent() {
        let throttle = Throttle::default();
        let k = key("198.51.100.1");
        for _ in 0..20 {
            throttle
                .admit(k)
                .expect("correct credentials never run out")
                .succeeded();
        }
        for _ in 0..5 {
            drop(throttle.admit(k).expect("burst"));
        }
        let wait = throttle.admit(k).expect_err("spent");
        assert!(wait <= REFILL_EVERY && wait > Duration::ZERO, "{wait:?}");
        assert_eq!(retry_after_secs(wait), 2);
        tokio::time::advance(REFILL_EVERY).await;
        drop(throttle.admit(k).expect("one back"));
        assert!(throttle.admit(k).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn the_table_never_grows_past_its_capacity() {
        let throttle = Throttle::with_capacity(64);
        for n in 0..10_000_u32 {
            let ip = Ipv4Addr::from(0xC633_6400 + n); // 198.51.100.0 onwards
            let _ = throttle.admit(ClientKey::V4(ip));
            assert!(throttle.tracked() <= 64);
        }
        // A heavily throttled address survives the churn of fresh ones.
        let hot = key("203.0.113.1");
        for _ in 0..5 {
            drop(throttle.admit(hot).expect("burst"));
        }
        for n in 0..1000_u32 {
            drop(throttle.admit(ClientKey::V4(Ipv4Addr::from(0x0A00_0000 + n))));
        }
        assert!(
            throttle.admit(hot).is_err(),
            "evicting it would forgive its guesses"
        );
    }

    #[test]
    fn retry_after_is_whole_seconds_at_least_one() {
        assert_eq!(retry_after_secs(Duration::from_millis(1)), 1);
        assert_eq!(retry_after_secs(Duration::from_millis(1500)), 2);
        assert_eq!(retry_after_secs(Duration::from_secs(2)), 2);
        assert_eq!(retry_after_secs(Duration::ZERO), 1);
    }
}
