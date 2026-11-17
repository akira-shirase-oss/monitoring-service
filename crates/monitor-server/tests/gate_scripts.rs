//! The gate's own rules, exercised: `scripts/check-ignored.sh` and
//! `scripts/check-advisories.sh` must fail exactly when they should.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn script(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts")
        .join(name)
}

fn run(command: &mut Command) -> Output {
    command
        .output()
        .unwrap_or_else(|e| panic!("run {command:?}: {e}"))
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A scratch repository with `files` in its work tree and `ignore` as its .gitignore.
fn repo(ignore: &str, files: &[&str]) -> anyhow::Result<tempfile::TempDir> {
    let dir = tempfile::tempdir()?;
    anyhow::ensure!(
        run(Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path()))
        .status
        .success(),
        "git init"
    );
    std::fs::write(dir.path().join(".gitignore"), ignore)?;
    for file in files {
        let path = dir.path().join(file);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, "x")?;
    }
    Ok(dir)
}

/// The old guard listed a handful of directories, so an ignore rule that swallowed
/// `perf/`, `test/`, a crate manifest or dashboard config passed. Now anything ignored
/// that is not a known artifact fails, wherever it is.
#[test]
fn an_ignored_source_path_anywhere_fails_the_gate() {
    let dir = repo(
        "perf/\nnode_modules/\n*.db\n",
        &["perf/run.mjs", "node_modules/x/index.js", "monitor.db"],
    )
    .expect("scratch repo");
    let output = run(Command::new("bash")
        .arg(script("check-ignored.sh"))
        .current_dir(dir.path()));
    assert!(!output.status.success(), "an ignored perf/ must fail");
    assert!(stderr(&output).contains("perf/"), "{}", stderr(&output));

    // Artifacts alone are fine.
    let dir = repo(
        "node_modules/\n*.db\ntarget/\n",
        &["node_modules/x/index.js", "monitor.db", "target/x"],
    )
    .expect("scratch repo");
    let output = run(Command::new("bash")
        .arg(script("check-ignored.sh"))
        .current_dir(dir.path()));
    assert!(output.status.success(), "{}", stderr(&output));
}

/// deny.toml's exceptions are dated decisions: past the re-check date the gate fails.
#[test]
fn an_advisory_exception_past_its_recheck_date_fails_the_gate() {
    let check = |today: &str| {
        run(Command::new("bash")
            .arg(script("check-advisories.sh"))
            .env("SMON_GATE_TODAY", today)
            .env("CARGO_DENY_CMD", "true")) // the date rule alone
    };
    let output = check("2026-12-17");
    assert!(!output.status.success(), "past the date must fail");
    assert!(
        stderr(&output).contains("RUSTSEC-2026-0459"),
        "{}",
        stderr(&output)
    );
    assert!(
        check("2026-12-16").status.success(),
        "the due date itself still passes"
    );
}

/// Locally a missing cargo-deny is a warning; in CI it is a failure, never a skip.
#[test]
fn a_missing_cargo_deny_fails_in_ci_and_warns_locally() {
    let check = |ci: Option<&str>| {
        let mut command = Command::new("bash");
        command
            .arg(script("check-advisories.sh"))
            .env("SMON_GATE_TODAY", "2026-11-16")
            .env("CARGO_DENY_CMD", "/nonexistent/cargo-deny")
            .env_remove("CI");
        if let Some(value) = ci {
            command.env("CI", value);
        }
        run(&mut command)
    };
    let ci = check(Some("true"));
    assert!(!ci.status.success(), "CI without cargo-deny must fail");
    assert!(stderr(&ci).contains("required in CI"), "{}", stderr(&ci));
    let local = check(None);
    assert!(local.status.success(), "{}", stderr(&local));
    assert!(stderr(&local).contains("skipping"), "{}", stderr(&local));
}

/// Supply chain beyond advisories: the gate also runs cargo-deny's `bans`, `licenses` and
/// `sources` checks — crates only from crates.io (no git or unknown registries), and only
/// licences on an explicit permissive allow-list (a copyleft-only crate fails the gate).
#[test]
fn the_gate_checks_licences_sources_and_bans_too() {
    let gate = std::fs::read_to_string(script("check-advisories.sh")).expect("read gate");
    assert!(
        gate.contains("check advisories bans licenses sources"),
        "check-advisories.sh must run all four cargo-deny checks"
    );
    let deny = std::fs::read_to_string(script("../deny.toml")).expect("read deny.toml");
    let section = |name: &str| -> String {
        let start = deny
            .find(&format!("\n[{name}]"))
            .unwrap_or_else(|| panic!("deny.toml has no [{name}]"));
        let rest = &deny[start + 1..];
        let end = rest[1..].find("\n[").map_or(rest.len(), |at| at + 1);
        rest[..end].to_owned()
    };
    let sources = section("sources");
    assert!(sources.contains("unknown-registry = \"deny\""), "{sources}");
    assert!(sources.contains("unknown-git = \"deny\""), "{sources}");
    let licenses = section("licenses");
    for permissive in ["\"MIT\"", "\"Apache-2.0\""] {
        assert!(licenses.contains(permissive), "{licenses}");
    }
    for copyleft in ["GPL", "AGPL", "SSPL"] {
        assert!(
            !licenses.contains(&format!("\"{copyleft}")),
            "{copyleft} on the allow-list: {licenses}"
        );
    }
}
