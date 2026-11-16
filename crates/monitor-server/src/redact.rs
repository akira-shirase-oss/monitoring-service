//! URL redaction — the one place that decides how much of a URL may leave the process.
//!
//! Chat webhooks (Slack, Discord, Teams, …) carry their credential in the *path*, others in
//! a query token, SMTP and basic-auth targets in the userinfo. Stripping only the userinfo
//! is not redaction. Two shapes, by destination:
//!
//! * [`origin`] — for logs, error messages and anything an operator's log pipeline keeps:
//!   `scheme://host[:port]`, nothing else. Enough to see *where* alerts go.
//! * [`for_alert`] — for the monitored URL inside an outgoing alert: the receiver needs to
//!   know which endpoint failed, so scheme, host, port and path stay; userinfo, query and
//!   fragment go.

use url::Url;

/// `scheme://host[:port]` — the only part of a URL that may be logged.
pub fn origin(url: &Url) -> String {
    match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{}://{host}:{port}", url.scheme()),
        (Some(host), None) => format!("{}://{host}", url.scheme()),
        (None, _) => format!("{}:", url.scheme()),
    }
}

/// The monitored URL as alerts may carry it: no credentials, no query, no fragment.
pub fn for_alert(url: &Url) -> String {
    let mut safe = url.clone();
    // Both setters only fail on URLs that cannot have userinfo (no host) — nothing to strip.
    let _ = safe.set_username("");
    let _ = safe.set_password(None);
    safe.set_query(None);
    safe.set_fragment(None);
    safe.to_string()
}

/// [`for_alert`] for a URL that arrives as stored text. An unparseable value is not echoed
/// — it may be exactly the credential-bearing string this function exists to hide.
pub fn for_alert_str(raw: &str) -> String {
    Url::parse(raw).map_or_else(|_| "(unparseable url)".to_owned(), |url| for_alert(&url))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(raw: &str) -> Url {
        Url::parse(raw).expect("test url")
    }

    #[test]
    fn origin_keeps_scheme_host_and_explicit_port_only() {
        assert_eq!(
            origin(&url("https://hooks.slack.com/services/T0/B0/XYZ")),
            "https://hooks.slack.com"
        );
        assert_eq!(
            origin(&url("http://u:p@[::1]:9000/hook?t=1#f")),
            "http://[::1]:9000"
        );
        assert_eq!(
            origin(&url("https://example.com:443/x")),
            "https://example.com"
        );
    }

    #[test]
    fn for_alert_drops_userinfo_query_and_fragment() {
        assert_eq!(
            for_alert(&url(
                "https://user:pw@api.example.com:8443/health?key=K#top"
            )),
            "https://api.example.com:8443/health"
        );
        assert_eq!(
            for_alert(&url("https://example.com/")),
            "https://example.com/"
        );
        assert_eq!(for_alert_str("not a url ?token=x"), "(unparseable url)");
    }
}
