//! The Cookie header → session id decoder. Attacker-controlled bytes on every
//! request; the parse must never panic and must only yield a non-empty value.
//!
//! Input: `name_flag` `cookie-header-bytes...`
//!   name_flag 0 → the `__Host-` name, otherwise the dev-escape name — both parse
//!   paths are the same code (`CookieSecure::name()` picks the string).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&flag, cookie)) = data.split_first() else {
        return;
    };
    let name = if flag == 0 {
        "__Host-smon_session"
    } else {
        "smon_session"
    };
    let Ok(value) = http::HeaderValue::from_bytes(cookie) else {
        return;
    };
    if let Some(id) = monitor_server::auth::session_cookie(Some(&value), name) {
        assert!(!id.is_empty(), "an empty session id must never yield");
    }
});
