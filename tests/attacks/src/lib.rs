//! `monitor-attacks`: the pentest regression suite (workspace rule, 2026-11-20).
//! Everything lives in `tests/` — each file boots the real `monitor-server`
//! binary on a loopback port and fires real attack requests at it, asserting the
//! refusal. This crate exists so the suite builds as a workspace member.
