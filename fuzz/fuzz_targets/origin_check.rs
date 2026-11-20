//! The Origin-vs-Host gate behind every cookie-authenticated write: attacker
//! Origin, attacker Host — the compare must never panic and never lie open.
//!
//! Input: `flags` `authority_len` `authority` `origin-header-bytes...`
//!   flags bit0: no authority at all (a request without a Host)
//!   authority_len bytes: the maybe-host string; invalid UTF-8 → lossy
//!   rest: the Origin header value
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&flags, rest)) = data.split_first() else {
        return;
    };
    let Some((&len, rest)) = rest.split_first() else {
        return;
    };
    let (authority_bytes, origin_bytes) = rest.split_at((len as usize).min(rest.len()));
    let authority = String::from_utf8_lossy(authority_bytes).into_owned();
    let authority = (flags & 1 == 0).then_some(authority.as_str());
    let Ok(origin) = http::HeaderValue::from_bytes(origin_bytes) else {
        return;
    };
    // Same input, same answer — and the compare itself never panics.
    let a = monitor_server::auth::origin_matches_host(&origin, authority);
    let b = monitor_server::auth::origin_matches_host(&origin, authority);
    assert_eq!(a, b, "the origin check must be deterministic");
});
