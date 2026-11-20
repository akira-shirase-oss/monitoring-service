//! Which origin a monitor's probes count against: the **resolved address and the
//! requested host** — `EXAMPLE.com` and `example.com.` are spellings of one host and
//! share its per-origin bound (D-018), while `localhost` and `127.0.0.1` are different
//! origins: on shared hosting one address is thousands of sites, so the bound's
//! identity is the site, not the machine (S2 re-review).
//!
//! Resolution is cached process-wide: a hostname's address is the same fact for the
//! coordinator (which refreshes every monitor's name in the background) and for the API's
//! capacity check (which resolves the monitor being written). IP literals need no lookup.
//! A name that hasn't been resolved yet, or doesn't resolve, keys by its spelling. The key
//! is the numerically lowest address a name resolves to **with the requested host**
//! (`addr:port|host`, S2 re-review): shared hosting puts thousands of sites on one
//! address, so the bound's identity is the site, not the machine.
//!
//! These lookups only *name* an origin; probes resolve again through the SSRF policy
//! resolver (probe.rs) before any connection.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use url::{Host, Url};

/// How long a resolved key is trusted before it is looked up again.
const TTL: Duration = Duration::from_secs(300);
/// How long a failed lookup is remembered (the spelling is used meanwhile).
const NEGATIVE_TTL: Duration = Duration::from_secs(30);
/// One lookup's deadline.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);
/// Names remembered at once.
const MAX_ENTRIES: usize = 8192;
/// Lookups in flight at once during a refresh.
const CONCURRENCY: usize = 16;

struct Entry {
    key: Option<String>,
    at: Instant,
}

static CACHE: LazyLock<Mutex<HashMap<(String, u16), Entry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn cache() -> std::sync::MutexGuard<'static, HashMap<(String, u16), Entry>> {
    // Plain values only; a panic mid-insert can't leave an entry half-written.
    CACHE.lock().unwrap_or_else(PoisonError::into_inner)
}

fn name_and_port(url: &Url) -> (String, u16) {
    let host = url
        .host_str()
        .unwrap_or_default()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    (host, url.port_or_known_default().unwrap_or_default())
}

fn address_key(ip: IpAddr, port: u16) -> String {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 @ IpAddr::V4(_) => v4,
    };
    match ip {
        IpAddr::V4(v4) => format!("{v4}:{port}"),
        IpAddr::V6(v6) => format!("[{v6}]:{port}"),
    }
}

/// An IP literal's `(addr, host)` pair, which needs no lookup: both parts are the
/// *normalized* address, so `127.0.0.1` and its v4-mapped v6 spelling are one
/// origin, not two.
fn literal_key(url: &Url) -> Option<(String, String)> {
    let port = url.port_or_known_default().unwrap_or_default();
    match url.host()? {
        Host::Ipv4(v4) => Some((
            address_key(IpAddr::V4(v4), port),
            IpAddr::V4(v4).to_string(),
        )),
        Host::Ipv6(v6) => {
            let ip = v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4);
            Some((address_key(ip, port), ip.to_string()))
        }
        Host::Domain(_) => None,
    }
}

/// The origin key for `url` from what is known now, without waiting on DNS:
/// the resolved (or literal, or spelling) `addr:port`, then `|`, then the host.
pub fn key(url: &Url) -> String {
    if let Some((addr, host)) = literal_key(url) {
        return format!("{addr}|{host}");
    }
    let (host, port) = name_and_port(url);
    let addr = cache()
        .get(&(host.clone(), port))
        .and_then(|entry| entry.key.clone())
        .unwrap_or_else(|| format!("{host}:{port}"));
    format!("{addr}|{host}")
}

/// `stale`, test-visible: proves the in-lock capacity check resolves nothing — a
/// name an in-lock `refresh` would have looked up comes back cached, not stale.
#[doc(hidden)]
pub fn name_is_stale(url: &Url) -> bool {
    stale(url)
}

/// Whether `url`'s name needs a (fresh) lookup.
fn stale(url: &Url) -> bool {
    if literal_key(url).is_some() {
        return false;
    }
    cache().get(&name_and_port(url)).is_none_or(|entry| {
        let ttl = if entry.key.is_some() {
            TTL
        } else {
            NEGATIVE_TTL
        };
        entry.at.elapsed() >= ttl
    })
}

/// Resolve `url`'s host now (bounded by [`LOOKUP_TIMEOUT`]), remember the address, and
/// return its `addr:port|host` key.
pub async fn resolve(url: &Url) -> String {
    if let Some((addr, host)) = literal_key(url) {
        return format!("{addr}|{host}");
    }
    let (host, port) = name_and_port(url);
    let lookup = tokio::time::timeout(
        LOOKUP_TIMEOUT,
        tokio::net::lookup_host((host.clone(), port)),
    );
    let resolved = match lookup.await {
        Ok(Ok(addrs)) => addrs
            .map(|addr| addr.ip())
            .min()
            .map(|ip| address_key(ip, port)),
        Ok(Err(_)) | Err(_) => None,
    };
    {
        let mut cache = cache();
        if cache.len() >= MAX_ENTRIES {
            cache.retain(|_, entry| entry.at.elapsed() < TTL);
            if cache.len() >= MAX_ENTRIES {
                cache.clear();
            }
        }
        cache.insert(
            (host.clone(), port),
            Entry {
                key: resolved.clone(),
                at: Instant::now(),
            },
        );
    }
    let addr = resolved.unwrap_or_else(|| format!("{host}:{port}"));
    format!("{addr}|{host}")
}

/// Resolve every stale name among `urls`, [`CONCURRENCY`] at a time, within `deadline`
/// overall; names not done by then keep their spelling until a later refresh.
pub async fn refresh(urls: impl IntoIterator<Item = Url>, deadline: Duration) {
    let mut pending: Vec<Url> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for url in urls {
        if stale(&url) && seen.insert(name_and_port(&url)) {
            pending.push(url);
        }
    }
    let work = async {
        let mut lookups = tokio::task::JoinSet::new();
        for url in pending {
            if lookups.len() >= CONCURRENCY {
                let _ = lookups.join_next().await;
            }
            lookups.spawn(async move { resolve(&url).await });
        }
        while lookups.join_next().await.is_some() {}
    };
    let _ = tokio::time::timeout(deadline, work).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).expect("url")
    }

    #[test]
    fn literals_key_by_address_and_spellings_are_normalised() {
        assert_eq!(key(&url("http://127.0.0.1:9/x")), "127.0.0.1:9|127.0.0.1");
        assert_eq!(
            key(&url("http://[::ffff:127.0.0.1]:9/")),
            "127.0.0.1:9|127.0.0.1"
        );
        assert_eq!(
            key(&url("https://[2001:db8::1]/")),
            "[2001:db8::1]:443|2001:db8::1"
        );
        // The URL's own spelling normalises (case, trailing dot).
        assert_eq!(
            key(&url("http://API.Example.:80/")),
            "api.example:80|api.example"
        );
    }

    /// S2 re-review: the key is an (address, host) pair. A name and the address it
    /// resolves to are different origins — shared hosting puts many hosts on one
    /// address, and the per-origin bound is a per-workspace share anyway.
    #[tokio::test]
    async fn a_resolved_name_keeps_its_host_in_the_key() {
        let alias = url("http://localhost:9/a");
        assert_eq!(resolve(&alias).await, "127.0.0.1:9|localhost");
        assert_eq!(key(&alias), "127.0.0.1:9|localhost");
        assert_ne!(key(&alias), key(&url("http://127.0.0.1:9/b")));
        // Spellings of one host still collapse to the resolved key (case, dot).
        assert_eq!(key(&url("http://LOCALHOST.:9/")), "127.0.0.1:9|localhost");
        // A name that does not resolve keys by its spelling.
        let nowhere = url("http://does-not-exist.invalid:9/");
        assert_eq!(
            resolve(&nowhere).await,
            "does-not-exist.invalid:9|does-not-exist.invalid"
        );
    }
}
