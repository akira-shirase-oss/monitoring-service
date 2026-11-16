//! `SMON_*` configuration — parsed once at boot, validated, every problem reported at once.
//!
//! A value that is set but invalid fails the boot naming the variable; it never falls back
//! to the default (a typo'd `SMON_MAX_DB_BYTES=2G` silently meaning 2 GiB *or* "no cap" is
//! exactly the 3 a.m. surprise this exists to prevent). Unset means the documented
//! default; set-but-blank is an error, because a blank value almost always means a failed
//! secret mount or a half-edited env file. Messages name the variable and, for non-secret
//! values, the offending text — never a token, webhook URL or SMTP DSN.
//!
//! The lookup is a parameter, so tests pass a map instead of mutating the process
//! environment.

use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;

use monitor_db::RetentionPolicy;
use url::Url;

/// Every variable the server (or the CLI sharing its env file) reads. Anything else with
/// the `SMON_` prefix is reported at boot as unknown — usually a typo.
pub const KNOWN: &[&str] = &[
    "SMON_ADMIN_TOKEN",
    "SMON_AGGREGATE_RETENTION_S",
    "SMON_DASHBOARD_DIR",
    "SMON_DATABASE_URL",
    "SMON_INCIDENT_RETENTION_S",
    "SMON_LISTEN",
    "SMON_LOG_FILE",
    "SMON_LOG_MAX_BYTES",
    "SMON_MAX_DB_BYTES",
    "SMON_MAX_MONITORS",
    "SMON_OUTBOX_RETENTION_S",
    "SMON_PROFILE",
    "SMON_RETENTION_INTERVAL_S",
    "SMON_RETENTION_S",
    "SMON_SERVER",
    "SMON_SESSION_TTL_S",
    "SMON_SMTP_DSN",
    "SMON_SMTP_FROM",
    "SMON_SMTP_TO",
    "SMON_STATUS_DIR",
    "SMON_STATUS_INTERVAL_S",
    "SMON_STATUS_TTL_S",
    "SMON_WEBHOOK_SECRET",
    "SMON_WEBHOOK_URL",
];

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
/// Upper bound on any retention or session horizon: ten years, in seconds.
const MAX_HORIZON_S: u64 = 10 * 366 * 86_400;

/// The SMTP channel's raw settings — all three or none; lettre parses them at boot.
#[derive(Clone)]
pub struct SmtpSettings {
    pub dsn: String,
    pub from: String,
    pub to: String,
}

impl fmt::Debug for SmtpSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SmtpSettings([REDACTED])")
    }
}

#[derive(Clone)]
pub struct Config {
    pub database_url: String,
    pub listen: SocketAddr,
    pub dashboard_dir: String,
    /// `SMON_PROFILE=lab`: loopback/private targets allowed (fixtures only).
    pub lab: bool,
    pub admin_token: Option<String>,
    pub max_db_bytes: u64,
    pub retention: RetentionPolicy,
    pub retention_interval: Duration,
    pub status_dir: String,
    pub status_interval: Duration,
    pub status_ttl: Duration,
    pub log_file: Option<String>,
    pub log_max_bytes: u64,
    pub webhook_url: Option<Url>,
    pub webhook_secret: Option<String>,
    pub smtp: Option<SmtpSettings>,
    pub session_ttl: Duration,
    pub max_monitors: u64,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Allowlist: secrets and credential-bearing URLs are shown only as present/absent.
        f.debug_struct("Config")
            .field("listen", &self.listen)
            .field("lab", &self.lab)
            .field(
                "admin_token",
                &self.admin_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("max_db_bytes", &self.max_db_bytes)
            .field("retention", &self.retention)
            .field(
                "webhook_url",
                &self.webhook_url.as_ref().map(crate::redact::origin),
            )
            .field("smtp", &self.smtp.as_ref().map(|_| "[REDACTED]"))
            .field("max_monitors", &self.max_monitors)
            .finish_non_exhaustive()
    }
}

/// Every problem found, so one boot attempt shows them all.
#[derive(Debug)]
pub struct ConfigError {
    pub problems: Vec<String>,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid configuration ({} problem(s)): {}",
            self.problems.len(),
            self.problems.join("; ")
        )
    }
}

impl std::error::Error for ConfigError {}

struct Reader<F> {
    lookup: F,
    problems: Vec<String>,
}

impl<F: Fn(&str) -> Option<String>> Reader<F> {
    /// The raw value: `None` when unset; a blank value is a problem (and treated as unset).
    fn raw(&mut self, name: &str) -> Option<String> {
        let value = (self.lookup)(name)?;
        if value.trim().is_empty() {
            self.problems.push(format!(
                "{name} is set but blank; unset it to use the default"
            ));
            return None;
        }
        Some(value)
    }

    fn string(&mut self, name: &str, default: &str) -> String {
        self.raw(name).unwrap_or_else(|| default.to_owned())
    }

    /// A parsed non-secret value; the offending text is echoed back.
    fn parse<T: FromStr>(&mut self, name: &str, default: T) -> T
    where
        T::Err: fmt::Display,
    {
        match self.raw(name) {
            None => default,
            Some(raw) => match raw.trim().parse() {
                Ok(value) => value,
                Err(error) => {
                    self.problems.push(format!("{name}={raw:?}: {error}"));
                    default
                }
            },
        }
    }

    /// An integer inside `range`.
    fn ranged(&mut self, name: &str, default: u64, range: std::ops::RangeInclusive<u64>) -> u64 {
        let value = self.parse(name, default);
        if !range.contains(&value) {
            self.problems.push(format!(
                "{name}={value} is outside {}..={}",
                range.start(),
                range.end()
            ));
            return default;
        }
        value
    }

    fn seconds(
        &mut self,
        name: &str,
        default: Duration,
        range: std::ops::RangeInclusive<u64>,
    ) -> Duration {
        Duration::from_secs(self.ranged(name, default.as_secs(), range))
    }

    /// D-012's horizons and how often a retention pass runs.
    fn retention(&mut self) -> (RetentionPolicy, Duration) {
        let env = self;
        let defaults = RetentionPolicy::default();
        let retention = RetentionPolicy {
            checks: env.seconds("SMON_RETENTION_S", defaults.checks, 60..=MAX_HORIZON_S),
            aggregates: env.seconds(
                "SMON_AGGREGATE_RETENTION_S",
                defaults.aggregates,
                86_400..=MAX_HORIZON_S,
            ),
            incidents: env.seconds(
                "SMON_INCIDENT_RETENTION_S",
                defaults.incidents,
                86_400..=MAX_HORIZON_S,
            ),
            delivered_outbox: env.seconds(
                "SMON_OUTBOX_RETENTION_S",
                defaults.delivered_outbox,
                60..=MAX_HORIZON_S,
            ),
        };
        let retention_interval = env.seconds(
            "SMON_RETENTION_INTERVAL_S",
            Duration::from_hours(1),
            1..=86_400,
        );
        (retention, retention_interval)
    }

    /// The public heartbeat: where, how often, and how long each beat stays fresh.
    fn status(&mut self) -> (String, Duration, Duration) {
        let env = self;
        let status_dir = env.string("SMON_STATUS_DIR", "status");
        let status_interval =
            env.seconds("SMON_STATUS_INTERVAL_S", Duration::from_secs(30), 1..=3_600);
        let status_ttl = env.seconds("SMON_STATUS_TTL_S", Duration::from_secs(90), 2..=86_400);
        if status_ttl <= status_interval {
            env.problems.push(format!(
                "SMON_STATUS_TTL_S ({}) must exceed SMON_STATUS_INTERVAL_S ({}) — otherwise every \
                 heartbeat expires before the next one is written",
                status_ttl.as_secs(),
                status_interval.as_secs()
            ));
        }
        (status_dir, status_interval, status_ttl)
    }

    /// Alert channels. URLs and DSNs are credentials: validated, never echoed.
    fn alerts(&mut self) -> (Option<Url>, Option<String>, Option<SmtpSettings>) {
        let env = self;
        let webhook_url = env.raw("SMON_WEBHOOK_URL").and_then(|raw| {
            match Url::parse(raw.trim()) {
                Ok(url) if matches!(url.scheme(), "http" | "https") && url.host().is_some() => {
                    Some(url)
                }
                // The URL is itself the credential for most chat webhooks — not echoed.
                _ => {
                    env.problems
                        .push("SMON_WEBHOOK_URL must be an absolute http(s) URL".to_owned());
                    None
                }
            }
        });
        let webhook_secret = env.raw("SMON_WEBHOOK_SECRET");
        let smtp = match (
            env.raw("SMON_SMTP_DSN"),
            env.raw("SMON_SMTP_FROM"),
            env.raw("SMON_SMTP_TO"),
        ) {
            (None, None, None) => None,
            (Some(dsn), Some(from), Some(to)) => Some(SmtpSettings { dsn, from, to }),
            _ => {
                env.problems.push(
                    "SMON_SMTP_DSN, SMON_SMTP_FROM and SMON_SMTP_TO must be set together"
                        .to_owned(),
                );
                None
            }
        };
        (webhook_url, webhook_secret, smtp)
    }
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| {
            std::env::var_os(name).map(|value| value.to_string_lossy().into_owned())
        })
    }

    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mut env = Reader {
            lookup,
            problems: Vec::new(),
        };

        let database_url = env.string("SMON_DATABASE_URL", "sqlite:monitor.db");
        if !database_url.starts_with("sqlite:") {
            env.problems
                .push("SMON_DATABASE_URL must be a sqlite: URL".to_owned());
        }
        let listen = env.parse("SMON_LISTEN", SocketAddr::from(([127, 0, 0, 1], 58400)));
        let dashboard_dir = env.string("SMON_DASHBOARD_DIR", "apps/dashboard/build");
        let lab = match env.raw("SMON_PROFILE").as_deref().map(str::trim) {
            None | Some("production") => false,
            Some("lab") => true,
            Some(other) => {
                env.problems.push(format!(
                    "SMON_PROFILE={other:?}: expected `lab` or `production`"
                ));
                false
            }
        };
        // Secrets: presence is checked, contents never echoed.
        let admin_token = env.raw("SMON_ADMIN_TOKEN");
        let max_db_bytes = env.ranged("SMON_MAX_DB_BYTES", 2 * 1024 * MIB, MIB..=u64::MAX);
        let (retention, retention_interval) = env.retention();
        let (status_dir, status_interval, status_ttl) = env.status();
        let log_file = env.raw("SMON_LOG_FILE");
        let log_max_bytes = env.ranged("SMON_LOG_MAX_BYTES", 64 * MIB, 4 * KIB..=u64::MAX);
        let (webhook_url, webhook_secret, smtp) = env.alerts();
        let session_ttl = env.seconds(
            "SMON_SESSION_TTL_S",
            Duration::from_hours(12),
            60..=MAX_HORIZON_S,
        );
        let max_monitors = env.ranged("SMON_MAX_MONITORS", 100, 1..=100_000);

        if !env.problems.is_empty() {
            return Err(ConfigError {
                problems: env.problems,
            });
        }
        Ok(Self {
            database_url,
            listen,
            dashboard_dir,
            lab,
            admin_token,
            max_db_bytes,
            retention,
            retention_interval,
            status_dir,
            status_interval,
            status_ttl,
            log_file,
            log_max_bytes,
            webhook_url,
            webhook_secret,
            smtp,
            session_ttl,
            max_monitors,
        })
    }

    /// `SMON_*` names present in the environment that nothing reads — likely typos.
    pub fn unknown_keys(names: impl IntoIterator<Item = String>) -> Vec<String> {
        let mut unknown: Vec<String> = names
            .into_iter()
            .filter(|name| name.starts_with("SMON_") && !KNOWN.contains(&name.as_str()))
            .collect();
        unknown.sort();
        unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn load(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Config::from_lookup(|name| map.get(name).cloned())
    }

    #[test]
    fn an_empty_environment_is_the_documented_defaults() {
        let config = load(&[]).expect("defaults are valid");
        assert_eq!(config.listen.to_string(), "127.0.0.1:58400");
        assert_eq!(config.max_db_bytes, 2 * 1024 * MIB);
        assert_eq!(config.retention, RetentionPolicy::default());
        assert_eq!(config.status_ttl, Duration::from_secs(90));
        assert_eq!(config.session_ttl, Duration::from_hours(12));
        assert_eq!(config.max_monitors, 100);
        assert!(!config.lab);
        assert!(config.smtp.is_none() && config.webhook_url.is_none());
    }

    /// The bug this module fixes: every one of these used to fall back to its default
    /// without a word — `SMON_MAX_DB_BYTES=2G` meant 2 GiB by accident, `SMON_PROFILE=Lab`
    /// meant production, `SMON_STATUS_TTL_S=90s` meant 90.
    #[test]
    fn invalid_values_fail_instead_of_defaulting() {
        for (name, value) in [
            ("SMON_MAX_DB_BYTES", "2G"),
            ("SMON_RETENTION_S", "7d"),
            ("SMON_RETENTION_INTERVAL_S", "-1"),
            ("SMON_STATUS_INTERVAL_S", "0"),
            ("SMON_STATUS_TTL_S", "90s"),
            ("SMON_LOG_MAX_BYTES", "lots"),
            ("SMON_SESSION_TTL_S", "12h"),
            ("SMON_MAX_MONITORS", "0"),
            ("SMON_LISTEN", "localhost"),
            ("SMON_PROFILE", "Lab"),
            ("SMON_DATABASE_URL", "postgres://x"),
            ("SMON_OUTBOX_RETENTION_S", "1.5"),
        ] {
            let error = load(&[(name, value)]).expect_err(name);
            assert!(
                error.problems.iter().any(|p| p.contains(name)),
                "{name}={value}: {error}"
            );
        }
    }

    #[test]
    fn every_problem_is_reported_together() {
        let error = load(&[
            ("SMON_MAX_DB_BYTES", "2G"),
            ("SMON_LISTEN", "nope"),
            ("SMON_SMTP_DSN", "smtp://relay"),
        ])
        .expect_err("three problems");
        assert_eq!(error.problems.len(), 3, "{error}");
    }

    #[test]
    fn blank_values_are_errors_not_defaults() {
        let error = load(&[("SMON_ADMIN_TOKEN", "  ")]).expect_err("blank token");
        assert!(error.problems[0].contains("SMON_ADMIN_TOKEN"));
    }

    #[test]
    fn cross_field_rules_hold() {
        let error = load(&[
            ("SMON_STATUS_INTERVAL_S", "90"),
            ("SMON_STATUS_TTL_S", "60"),
        ])
        .expect_err("ttl below interval");
        assert!(error.to_string().contains("must exceed"));
        let error = load(&[("SMON_SMTP_FROM", "a@example.com")]).expect_err("partial smtp");
        assert!(error.to_string().contains("set together"));
    }

    #[test]
    fn secrets_never_appear_in_errors_or_debug() {
        let error = load(&[
            (
                "SMON_WEBHOOK_URL",
                "ftp://hooks.slack.com/services/T0/B0/S3CRET",
            ),
            ("SMON_SMTP_DSN", "smtps://user:hunter2@relay"),
        ])
        .expect_err("bad webhook + partial smtp");
        let text = error.to_string();
        assert!(
            !text.contains("S3CRET") && !text.contains("hunter2"),
            "{text}"
        );

        let config = load(&[
            ("SMON_ADMIN_TOKEN", "tok-123"),
            (
                "SMON_WEBHOOK_URL",
                "https://hooks.slack.com/services/T0/B0/S3CRET",
            ),
        ])
        .expect("valid");
        let debug = format!("{config:?}");
        assert!(
            !debug.contains("tok-123") && !debug.contains("S3CRET"),
            "{debug}"
        );
    }

    #[test]
    fn valid_overrides_are_applied() {
        let config = load(&[
            ("SMON_PROFILE", "lab"),
            ("SMON_LISTEN", "0.0.0.0:9000"),
            ("SMON_MAX_MONITORS", "250"),
            ("SMON_RETENTION_S", "86400"),
            ("SMON_WEBHOOK_URL", "https://hooks.example.com/in"),
        ])
        .expect("valid");
        assert!(config.lab);
        assert_eq!(config.listen.port(), 9000);
        assert_eq!(config.max_monitors, 250);
        assert_eq!(config.retention.checks, Duration::from_hours(24));
        assert!(config.webhook_url.is_some());
    }

    #[test]
    fn unknown_smon_keys_are_flagged() {
        let unknown = Config::unknown_keys(
            ["SMON_LISTEN", "SMON_RETENTON_S", "PATH", "SMON_SERVER"].map(str::to_owned),
        );
        assert_eq!(unknown, ["SMON_RETENTON_S"]);
    }
}
