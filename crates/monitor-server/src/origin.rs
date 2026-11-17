//! Which origin a monitor's probes count against: the **resolved address and port**, not
//! the URL's spelling — so `localhost`, `127.0.0.1` and any other alias of one server share
//! its per-origin bound and its per-origin capacity (D-018), instead of each spelling
//! getting its own.
//!
//! Resolution is cached process-wide: a hostname's address is the same fact for the
//! coordinator (which refreshes every monitor's name in the background) and for the API's
//! capacity check (which resolves the monitor being written). IP literals need no lookup.
//! A name that hasn't been resolved yet, or doesn't resolve, keys by its spelling. The key
//! is the numerically lowest address a name resolves to, with the port.
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

/// The URL's own spelling, `host:port` (lower-case, no trailing dot).
pub fn spelling(url: &Url) -> String {
    let (host, port) = name_and_port(url);
    format!("{host}:{port}")
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

/// An IP literal's key, which needs no lookup.
fn literal_key(url: &Url) -> Option<String> {
    let port = url.port_or_known_default().unwrap_or_default();
    match url.host()? {
        Host::Ipv4(v4) => Some(address_key(IpAddr::V4(v4), port)),
        Host::Ipv6(v6) => Some(address_key(IpAddr::V6(v6), port)),
        Host::Domain(_) => None,
    }
}

/// The origin key for `url` from what is known now, without waiting on DNS.
pub fn key(url: &Url) -> String {
    if let Some(key) = literal_key(url) {
        return key;
    }
    let name = name_and_port(url);
    cache()
        .get(&name)
        .and_then(|entry| entry.key.clone())
        .unwrap_or_else(|| spelling(url))
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

/// Resolve `url`'s host now (bounded by [`LOOKUP_TIMEOUT`]), remember it, and return its
/// key.
pub async fn resolve(url: &Url) -> String {
    if let Some(key) = literal_key(url) {
        return key;
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
            (host, port),
            Entry {
                key: resolved.clone(),
                at: Instant::now(),
            },
        );
    }
    resolved.unwrap_or_else(|| spelling(url))
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
        assert_eq!(key(&url("http://127.0.0.1:9/x")), "127.0.0.1:9");
        assert_eq!(key(&url("http://[::ffff:127.0.0.1]:9/")), "127.0.0.1:9");
        assert_eq!(key(&url("https://[2001:db8::1]/")), "[2001:db8::1]:443");
        assert_eq!(spelling(&url("http://API.Example.:80/")), "api.example:80");
    }

    #[tokio::test]
    async fn a_resolved_alias_keys_like_its_address() {
        let alias = url("http://localhost:9/a");
        assert_eq!(resolve(&alias).await, "127.0.0.1:9");
        assert_eq!(key(&alias), key(&url("http://127.0.0.1:9/b")));
        // A name that does not resolve keys by its spelling.
        let nowhere = url("http://does-not-exist.invalid:9/");
        assert_eq!(resolve(&nowhere).await, "does-not-exist.invalid:9");
    }
}
