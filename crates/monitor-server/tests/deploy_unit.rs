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

const DEPLOY_SH: &str = include_str!("../../../scripts/deploy.sh");

/// The `KEY=value` lines of the env template `deploy.sh` writes, as an operator gets them
/// by deleting the leading `# `.
fn env_template_settings() -> Option<Vec<(String, &'static str)>> {
    let start = DEPLOY_SH.find("<<'ENV'\n")? + "<<'ENV'\n".len();
    let end = start + DEPLOY_SH[start..].find("\nENV\n")?;
    Some(
        DEPLOY_SH[start..end]
            .lines()
            .filter_map(|line| line.strip_prefix("# SMON_"))
            .filter_map(|rest| rest.split_once('='))
            .map(|(key, value)| (format!("SMON_{key}"), value))
            .collect(),
    )
}

/// systemd's `EnvironmentFile=` has no inline comments: `KEY=43200   # note` sets the value
/// to the whole string. With strict `SMON_*` parsing, uncommenting such a template line
/// stops the boot. Every template line must be a bare `KEY=value`, and every example value
/// in it must pass the parser as written.
#[test]
fn the_env_template_has_no_inline_comments_and_its_examples_parse() {
    let settings = env_template_settings().expect("deploy.sh writes an env template");
    assert!(settings.len() >= 15, "template parsed: {settings:?}");
    for (key, value) in &settings {
        assert!(
            !value.contains('#'),
            "{key}: inline comment in the template value {value:?} — systemd keeps it"
        );
    }
    let examples: std::collections::HashMap<String, String> = settings
        .iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(key, value)| (key.clone(), (*value).to_owned()))
        .collect();
    if let Err(error) =
        monitor_server::config::Config::from_lookup(|name| examples.get(name).cloned())
    {
        panic!("the template's example values must boot: {error}");
    }
}
