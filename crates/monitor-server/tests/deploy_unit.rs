//! The shipped systemd unit is part of the security boundary: a monitor that parses
//! responses from the network and holds alert credentials must not run as root.

const UNIT: &str = include_str!("../../../deploy/monitor-server.service");

/// `key=value` settings of the `[Service]` section (later lines win, as in systemd).
fn service_setting(key: &str) -> Option<&'static str> {
    let mut in_service = false;
    let mut found = None;
    for line in UNIT.lines().map(str::trim) {
        if line.starts_with('[') {
            in_service = line == "[Service]";
            continue;
        }
        if in_service
            && let Some((name, value)) = line.split_once('=')
            && name.trim() == key
        {
            found = Some(value.trim());
        }
    }
    found
}

#[test]
fn the_unit_runs_as_a_dedicated_unprivileged_user() {
    let user = service_setting("User").expect("the unit must set User=");
    assert!(!matches!(user, "" | "root" | "0"), "User={user} is root");
    let group = service_setting("Group").expect("the unit must set Group=");
    assert!(!matches!(group, "" | "root" | "0"), "Group={group} is root");
    // Nothing the monitor does needs a capability: it binds an unprivileged port.
    assert_eq!(service_setting("CapabilityBoundingSet"), Some(""));
    assert_eq!(service_setting("NoNewPrivileges"), Some("true"));
    assert_eq!(service_setting("ProtectSystem"), Some("strict"));
    assert_eq!(service_setting("ProtectHome"), Some("true"));
}

/// systemd's stop sends SIGTERM, then SIGKILL after `TimeoutStopSec`. The server drains
/// HTTP for up to 12 s and the engine for up to 15 s; the kill must not land inside that.
#[test]
fn the_stop_timeout_outlasts_the_drain() {
    let timeout: u64 = service_setting("TimeoutStopSec")
        .and_then(|value| value.trim_end_matches('s').parse().ok())
        .expect("the unit must set TimeoutStopSec in seconds");
    assert!(
        timeout > 12 + 15,
        "TimeoutStopSec={timeout} cuts the drain short"
    );
}
