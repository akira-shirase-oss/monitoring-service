//! The throttle's client identity: TCP peer + attacker-written `X-Forwarded-For`.
//! An attacker who could pick a fresh key per request would get unlimited guesses —
//! the decoder must never panic and must never believe a hop it shouldn't.
//!
//! Input: `flags` `peer_len` `peer` `xff-lines...`
//!   flags bit0: the peer IS the trusted proxy (only then may XFF matter)
//!   peer_len bytes: a maybe-socket-addr string; invalid → no peer
//!   rest: split on '\n', each line an `x-forwarded-for` header value
#![no_main]

use libfuzzer_sys::fuzz_target;
use monitor_server::throttle::ClientKey;
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;

fuzz_target!(|data: &[u8]| {
    let Some((&flags, rest)) = data.split_first() else {
        return;
    };
    let Some((&peer_len, rest)) = rest.split_first() else {
        return;
    };
    let peer_len = peer_len as usize;
    let (peer_bytes, xff) = rest.split_at(peer_len.min(rest.len()));

    let peer = std::str::from_utf8(peer_bytes)
        .ok()
        .and_then(|s| SocketAddr::from_str(s).ok());
    // flags bit0 set: the peer IS the trusted proxy (only then may XFF matter);
    // clear: an unrelated proxy trusts nothing about this peer's claims.
    let trusted_proxy = if flags & 1 == 1 {
        peer.map(|p| p.ip())
    } else {
        IpAddr::from_str("203.0.113.1").ok()
    };

    let mut headers = http::HeaderMap::new();
    for line in xff.split(|b| *b == b'\n') {
        if let Ok(value) = http::HeaderValue::from_bytes(line) {
            headers.append("x-forwarded-for", value);
        }
    }

    // Never panics; the key is one of three variants whatever the bytes.
    let _ = ClientKey::from_request(peer, &headers, trusted_proxy);
});
