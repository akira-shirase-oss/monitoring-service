//! The status-page slug validator — a path segment an attacker picks, also the
//! file name the writer uses. Arbitrary bytes must never panic the check, and
//! the check must be stable.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let slug = String::from_utf8_lossy(data);
    assert_eq!(
        monitor_server::valid_slug(&slug),
        monitor_server::valid_slug(&slug),
        "slug validation must be deterministic"
    );
});
