//! The URL-layer SSRF gate every target passes: monitor URLs at save, tenant
//! webhook URLs at save and at every send, and each redirect hop a probe follows.
//! Attacker-shaped URLs must never panic the parse or the policy check.
//!
//! Input: `allow_local` `url-bytes...`
//!   allow_local != 0 builds the lab policy (local literals permitted)
#![no_main]

use libfuzzer_sys::fuzz_target;
use monitor_server::probe::ProbePolicy;

fuzz_target!(|data: &[u8]| {
    let Some((&allow_local, url_bytes)) = data.split_first() else {
        return;
    };
    let Ok(text) = std::str::from_utf8(url_bytes) else {
        return;
    };
    let policy = ProbePolicy {
        allow_local_targets: allow_local != 0,
        extra_roots: Vec::new(),
    };
    if let Ok(url) = url::Url::parse(text) {
        let first = policy.check(&url);
        let second = policy.check(&url);
        assert_eq!(first, second, "the SSRF gate must answer the same way twice");
    }
});
