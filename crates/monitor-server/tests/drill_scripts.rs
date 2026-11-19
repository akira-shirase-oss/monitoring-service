//! Scripts that start their own monitor-server must never touch someone else's: the backup
//! drill used to bind :58400 (the default an installed monitor listens on) and `kill -9`
//! whatever held it — on a machine with a live instance, that instance.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const DRILL: &str = include_str!("../../../scripts/backup-drill.sh");
const FREEZE: &str = include_str!("../../../scripts/freeze-drill.sh");
const DEV: &str = include_str!("../../../scripts/dev.sh");

fn script(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts")
        .join(name)
}

/// Lines that run (comments stripped).
fn code(script: &str) -> Vec<&str> {
    script
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect()
}

#[test]
fn the_drill_and_dev_scripts_never_target_or_kill_the_default_port() {
    for (name, text) in [
        ("backup-drill.sh", DRILL),
        ("freeze-drill.sh", FREEZE),
        ("dev.sh", DEV),
    ] {
        for line in code(text) {
            assert!(!line.contains("58400"), "{name} targets :58400: {line}");
            let kills_by_port =
                (line.contains("lsof") && line.contains("kill")) || line.contains("xargs kill");
            assert!(
                !kills_by_port,
                "{name} kills by port, not by its own PID: {line}"
            );
        }
    }
    // Each picks its own port, overridable, and guards it before starting anything.
    assert!(DRILL.contains("SMON_DRILL_PORT") && DRILL.contains("require_free_port"));
    assert!(FREEZE.contains("SMON_FREEZE_PORT") && FREEZE.contains("require_free_port"));
    assert!(DEV.contains("SMON_DEV_PORT") && DEV.contains("require_free_port"));
    assert!(
        DEV.contains("SMON_LISTEN"),
        "dev.sh must listen on its own port"
    );
}

/// Hold a port, point a script at it: the script must refuse at once and leave the holder
/// alone (the listener still accepts afterwards).
#[test]
fn a_script_refuses_a_port_that_is_already_in_use_and_kills_nothing() {
    let held = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = held.local_addr().expect("addr").port().to_string();
    let spare = std::net::TcpListener::bind("127.0.0.1:0").expect("bind spare");
    let spare_port = spare.local_addr().expect("addr").port().to_string();
    drop(spare);

    for (name, envs) in [
        (
            "backup-drill.sh",
            vec![
                ("SMON_DRILL_PORT", port.as_str()),
                ("SMON_DRILL_FIXTURE_PORT", spare_port.as_str()),
            ],
        ),
        (
            "freeze-drill.sh",
            vec![
                ("SMON_FREEZE_PORT", port.as_str()),
                ("SMON_FREEZE_FIXTURE_PORT", spare_port.as_str()),
            ],
        ),
        ("dev.sh", vec![("SMON_DEV_PORT", port.as_str())]),
    ] {
        let started = Instant::now();
        let mut command = Command::new("bash");
        command.arg(script(name));
        for (key, value) in &envs {
            command.env(key, value);
        }
        let output = command.output().expect("run script");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "{name} must refuse an occupied port"
        );
        assert!(stderr.contains("in use"), "{name}: {stderr}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{name} must refuse before starting anything"
        );
        assert!(
            std::net::TcpStream::connect(held.local_addr().expect("addr")).is_ok(),
            "{name} must not touch the process holding the port"
        );
    }
}

fn perf_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../perf")
}

/// Every `perf/*.mjs` that starts its own monitor-server — found by listing the directory,
/// so a new harness can't slip past the guard by not being named here.
fn perf_harnesses() -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = std::fs::read_dir(perf_dir())
        .unwrap_or_else(|e| panic!("read perf/: {e}"))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "mjs"))
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            let name = path.file_name()?.to_string_lossy().into_owned();
            js_code(&text)
                .iter()
                .any(|line| line.contains("target/debug/monitor-server"))
                .then_some((name, text))
        })
        .collect();
    found.sort();
    found
}

/// JavaScript lines that run (`//` comments stripped).
fn js_code(script: &str) -> Vec<&str> {
    script
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("//"))
        .collect()
}

/// `perf/lab-profile.mjs` defaulted to `--server-port 58400`: next to an installed monitor
/// its own server could not bind, and the harness then drove the installed one — 401s with
/// a token, 100 lab monitors registered into a tokenless loopback install's store. Every
/// harness now has its own default port, refuses a busy one before starting anything, and
/// checks that what answers `/livez` is the server it started.
#[test]
fn the_perf_harnesses_never_target_the_default_port_and_guard_their_own() {
    let harnesses = perf_harnesses();
    let names: Vec<&str> = harnesses.iter().map(|(name, _)| name.as_str()).collect();
    for expected in [
        "lab-profile.mjs",
        "real-app-load.mjs",
        "session-churn.mjs",
        "storm.mjs",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} not found among {names:?}"
        );
    }
    for (name, text) in &harnesses {
        for line in js_code(text) {
            assert!(!line.contains("58400"), "{name} targets :58400: {line}");
        }
        assert!(
            text.contains("from './lib-ports.mjs'"),
            "{name} must use perf/lib-ports.mjs"
        );
        assert!(
            text.contains("await requireFreePorts("),
            "{name} must refuse busy ports before starting anything"
        );
        assert!(
            text.contains("await waitOwnServer("),
            "{name} must check that /livez is answered by the server it started"
        );
        assert!(
            text.contains("arg('server-port'"),
            "{name} must take its server port from --server-port"
        );
    }
}

/// Run `command` in its own process group; past `deadline`, kill the whole group (a
/// harness that failed to refuse has started fixtures and a server of its own).
fn output_within(command: &mut Command, deadline: Duration) -> Option<std::process::Output> {
    use std::os::unix::process::CommandExt;
    let child = command
        .process_group(0)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {command:?} (the perf harnesses need node): {e}"));
    let group = child.id().to_string();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(child.wait_with_output());
    });
    if let Ok(output) = done_rx.recv_timeout(deadline) {
        Some(output.unwrap_or_else(|e| panic!("wait for the harness: {e}")))
    } else {
        let _ = Command::new("kill")
            .args(["-KILL", &format!("-{group}")])
            .status();
        None
    }
}

/// Hold a port and hand it to each harness as its server port: it must refuse at once
/// (before any fixture, `--init` or server starts) and leave the holder alone.
#[test]
fn a_perf_harness_refuses_a_port_that_is_already_in_use_and_kills_nothing() {
    let held = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = held.local_addr().expect("addr").port().to_string();
    let out = tempfile::tempdir().expect("tempdir");
    for (name, _) in perf_harnesses() {
        let started = Instant::now();
        let output = output_within(
            Command::new("node")
                .arg(perf_dir().join(&name))
                .args(["--server-port", &port, "--duration-s", "1"])
                .arg("--out")
                .arg(out.path()),
            Duration::from_secs(15),
        )
        .unwrap_or_else(|| panic!("{name} did not refuse within 15 s (its group was killed)"));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "{name} must refuse an occupied port"
        );
        assert!(stderr.contains("in use"), "{name}: {stderr}");
        assert!(
            stderr.contains("--server-port"),
            "{name} names the override: {stderr}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{name} must refuse before starting anything"
        );
        assert!(
            std::net::TcpStream::connect(held.local_addr().expect("addr")).is_ok(),
            "{name} must not touch the process holding the port"
        );
    }
    assert_eq!(
        std::fs::read_dir(out.path()).expect("read out").count(),
        0,
        "a refused run writes no results"
    );
}

/// RUSTSEC-2026-0459 needs a store opened with a `-wal` someone else wrote. The restore
/// drill — the procedure the RUNBOOK points to — removes `-wal`/`-shm` before it starts the
/// server on the restored file.
#[test]
fn the_restore_drill_removes_the_wal_and_shm_before_restarting() {
    let lines = code(DRILL);
    let restored = lines
        .iter()
        .position(|line| line.starts_with("cp \"$BACKUP\" \"$DB\""))
        .expect("the drill restores the backup over the store");
    let restart = restored
        + lines[restored..]
            .iter()
            .position(|line| line.starts_with("./target/debug/monitor-server"))
            .expect("the drill restarts on the restored store");
    assert!(
        lines[..restart]
            .iter()
            .skip(restored.saturating_sub(3))
            .any(|line| *line == "rm -f \"$DB-wal\" \"$DB-shm\""),
        "-wal/-shm must be removed before the restored store is opened"
    );
}

/// Run `body` in bash with `scripts/lib-ports.sh` sourced.
fn with_lib(body: &str) -> std::process::Output {
    let lib = script("lib-ports.sh");
    Command::new("bash")
        .arg("-c")
        .arg(format!(". '{}'\n{body}", lib.display()))
        .output()
        .unwrap_or_else(|e| panic!("run bash: {e}"))
}

/// F4: `stop_own` signalled whatever PID it was handed once `kill -0` said it existed. A
/// child that died on its own and was reaped leaves its PID free for reuse (PIDs wrap
/// under heavy build load), and cleanup then SIGTERMs — then SIGKILLs — a stranger. It now
/// signals only a live child of the calling shell.
#[test]
fn stop_own_never_signals_a_process_that_is_not_its_child() {
    // A process this test started, so its parent is the test, not the script's shell.
    let mut stranger = Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap_or_else(|e| panic!("spawn sleep: {e}"));
    let output = with_lib(&format!("stop_own {}", stranger.id()));
    let alive = stranger.try_wait().ok().flatten().is_none();
    let _ = stranger.kill();
    let _ = stranger.wait();
    assert!(output.status.success(), "{output:?}");
    assert!(alive, "stop_own killed a process it did not start");

    // Its own child it does stop.
    let output = with_lib(
        "sleep 30 & child=$!\nstop_own \"$child\"\nif kill -0 \"$child\" 2>/dev/null; then exit 1; fi",
    );
    assert!(output.status.success(), "own child not stopped: {output:?}");
}

/// F4: a script that waits for its own server must not settle for *a* server: if the child
/// it started has exited (it lost a bind race, say), whatever answers on that port is
/// someone else's — `wait_own_http` fails instead of driving it.
#[test]
fn wait_own_http_fails_when_its_child_died_even_if_the_url_answers() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            use std::io::{Read, Write};
            let mut stream = stream;
            let mut buf = [0_u8; 1024];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok");
        }
    });
    let output = with_lib(&format!(
        "true & child=$!\nwait \"$child\"\nwait_own_http http://127.0.0.1:{port}/livez \"$child\""
    ));
    assert!(
        !output.status.success(),
        "a dead child must fail the wait: {output:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("exited"),
        "{output:?}"
    );
    // A live child and an answering URL pass.
    let output = with_lib(&format!(
        "sleep 30 & child=$!\nwait_own_http http://127.0.0.1:{port}/livez \"$child\"\nstatus=$?\nstop_own \"$child\"\nexit $status"
    ));
    assert!(output.status.success(), "{output:?}");
}

/// The drills use the guarded wait for every process they start.
#[test]
fn the_drill_waits_only_on_its_own_processes() {
    for (name, text) in [("backup-drill.sh", DRILL), ("freeze-drill.sh", FREEZE)] {
        let lines = code(text);
        assert!(
            !lines.iter().any(|line| line.starts_with("wait_http ")),
            "{name} must use wait_own_http, which also checks the child is alive"
        );
        assert!(
            lines
                .iter()
                .filter(|line| line.starts_with("wait_own_http "))
                .count()
                >= 2,
            "{name} must wait on its own processes"
        );
    }
}

/// The freeze drill must register a monitor the API will accept: an interval below
/// `MIN_INTERVAL` is refused with a validation error, and the drill then `KeyError`s on
/// `"id"` — the exact setup failure this pin exists to keep fixed.
#[test]
fn the_freeze_drill_registers_a_monitor_with_a_valid_interval() {
    // The payload lives inside a double-quoted bash string, so the file carries
    // `\"interval_s\":$INTERVAL` — follow the variable to its literal assignment.
    assert!(
        FREEZE.contains("interval_s\\\":$INTERVAL"),
        "the drill must post the shared INTERVAL variable, not an inline literal"
    );
    let pos = FREEZE.find("INTERVAL=").expect("the drill sets INTERVAL");
    let digits: String = FREEZE[pos + "INTERVAL=".len()..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let interval: u64 = digits.parse().expect("INTERVAL must be a literal number");
    assert!(
        interval >= monitor_domain::MIN_INTERVAL.as_secs(),
        "freeze-drill INTERVAL={interval} is below the domain minimum {}",
        monitor_domain::MIN_INTERVAL.as_secs()
    );
}

/// First check slots land inside the interval window (D-018 phase spread), so a fixed
/// wait shorter than the interval is a drill that can never pass — the freeze drill
/// failed exactly that way ("monitor never came up" after 12 s at a 15 s interval).
/// Both drills derive their waits from the interval they register, and a timeout prints
/// the monitor's checks before failing.
#[test]
fn the_drills_wait_budgets_come_from_the_interval_and_report_on_timeout() {
    for (name, text) in [("backup-drill.sh", DRILL), ("freeze-drill.sh", FREEZE)] {
        assert!(text.contains("INTERVAL="), "{name} must name its interval");
        assert!(
            text.contains("* INTERVAL"),
            "{name} must derive its wait budget from the interval, not a fixed count"
        );
        assert!(
            !text.contains("seq 1 12") && !text.contains("seq 1 16"),
            "{name} still carries a fixed wait that pre-dates the phase spread"
        );
        assert!(
            text.contains("/checks?limit="),
            "{name} must print the monitor's checks on timeout"
        );
    }
}
