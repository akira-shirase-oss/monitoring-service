//! Guide drift check: the dashboard's single glossary (`vocabulary.ts`) must document
//! every wire string — statuses, check outcomes, the transport detail taxonomy, and the
//! incident fields/states. Adding a variant or a word without updating the guide fails
//! here; stale guide entries (words the API can never emit) fail too.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use monitor_db::encode_slot;
use monitor_domain::{CheckOutcome, SlotObservation, Status};

/// Parse `word: 'x'` + `kind: 'y'` pairs out of the glossary — deliberately dumb
/// (the file keeps that field order; a format change fails loudly here, not silently).
fn glossary() -> HashMap<String, String> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/dashboard/src/lib/vocabulary.ts");
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let mut words = HashMap::new();
    let mut rest = src.as_str();
    while let Some(wpos) = rest.find("word: '") {
        rest = &rest[wpos + 7..];
        let word = rest
            .split('\'')
            .next()
            .unwrap_or_else(|| panic!("unterminated word literal"));
        let kpos = rest
            .find("kind: '")
            .unwrap_or_else(|| panic!("kind after word {word}"));
        let krest = &rest[kpos + 7..];
        let kind = krest
            .split('\'')
            .next()
            .unwrap_or_else(|| panic!("unterminated kind literal"));
        words.insert(word.to_string(), kind.to_string());
        rest = krest;
    }
    words
}

#[test]
fn every_monitor_status_is_in_the_guide() {
    let words = glossary();
    let statuses: HashSet<&str> = words
        .iter()
        .filter(|(_, k)| k.as_str() == "status")
        .map(|(w, _)| w.as_str())
        .collect();
    // serde-serialized names — add a Status variant and this breaks.
    for status in [Status::Up, Status::Down, Status::Unknown] {
        let wire = serde_json::to_value(status).unwrap_or_else(|e| panic!("serialize status: {e}"));
        let wire = wire
            .as_str()
            .unwrap_or_else(|| panic!("status serializes as a string"));
        assert!(statuses.contains(wire), "guide missing status {wire}");
    }
    // `paused` is a monitor flag, not a Status — the UI still shows it, so it is a word.
    assert!(statuses.contains("paused"), "guide missing paused");
}

#[test]
fn every_slot_outcome_is_in_the_guide() {
    let words = glossary();
    let outcomes: HashSet<&str> = words
        .iter()
        .filter(|(_, k)| k.as_str() == "outcome")
        .map(|(w, _)| w.as_str())
        .collect();
    // Enumerate through the *store's own* encoder — the string the guide must document
    // is whatever commit actually writes.
    for slot in [
        SlotObservation::Missed,
        SlotObservation::Probe(CheckOutcome::Pass),
        SlotObservation::Probe(CheckOutcome::HttpFail { status: 500 }),
        SlotObservation::Probe(CheckOutcome::Transport),
        SlotObservation::Probe(CheckOutcome::Timeout),
    ] {
        let (wire, _) = encode_slot(slot, Some(500));
        assert!(outcomes.contains(wire), "guide missing outcome {wire}");
    }
}

#[test]
fn every_transport_detail_is_in_the_guide() {
    let words = glossary();
    let details: HashSet<&str> = words
        .iter()
        .filter(|(_, k)| k.as_str() == "detail")
        .map(|(w, _)| w.as_str())
        .collect();
    // The committed detail taxonomy (probe.rs classify()) — a pinned set: a new detail
    // string means a new taxonomy word, and this list must grow with it.
    for detail in [
        "connect",
        "tls",
        "body",
        "request",
        "rejected_local",
        "cert_expired",
        "cert_not_yet_valid",
        "cert_name_mismatch",
        "cert_untrusted",
        "cert_revoked",
    ] {
        assert!(details.contains(detail), "guide missing detail {detail}");
    }
}

#[test]
fn every_incident_word_is_in_the_guide() {
    let words = glossary();
    let incidents: HashSet<&str> = words
        .iter()
        .filter(|(_, k)| k.as_str() == "incident")
        .map(|(w, _)| w.as_str())
        .collect();
    for word in [
        "first_fail_at",
        "opened_at",
        "last_fail_at",
        "resolved_at",
        "open",
        "resolved",
    ] {
        assert!(
            incidents.contains(word),
            "guide missing incident word {word}"
        );
    }
}

#[test]
fn the_guide_has_no_stale_words() {
    // Anything the guide documents but the API can never emit is a lie by omission —
    // the word set must not exceed the real wire vocabulary.
    let words = glossary();
    let real: HashSet<&str> = [
        "up",
        "down",
        "unknown",
        "paused",
        "pass",
        "http_fail",
        "transport",
        "timeout",
        "missed",
        "connect",
        "tls",
        "body",
        "request",
        "rejected_local",
        "cert_expired",
        "cert_not_yet_valid",
        "cert_name_mismatch",
        "cert_untrusted",
        "cert_revoked",
        "first_fail_at",
        "opened_at",
        "last_fail_at",
        "resolved_at",
        "open",
        "resolved",
    ]
    .into_iter()
    .collect();
    for word in words.keys() {
        assert!(real.contains(word.as_str()), "stale guide word: {word}");
    }
}
