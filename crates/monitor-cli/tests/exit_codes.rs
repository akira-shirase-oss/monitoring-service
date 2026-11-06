//! Pinned exit-code contract for the `monitor` binary (docs/modules/05-api-cli.md):
//! 0 success · 1 API/application error · 2 usage error · 3 server unreachable ·
//! 4 unauthorized. Scripts must be able to branch on these.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output};

fn monitor() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_monitor"));
    // The contract is pinned against a clean environment — SMON_SERVER / SMON_ADMIN_TOKEN
    // set on the host must not change what these tests exercise.
    command.env_remove("SMON_SERVER");
    command.env_remove("SMON_ADMIN_TOKEN");
    command
}

/// A minimal canned HTTP server: one response per connection, `Connection: close`.
fn serve(respond: impl Fn(&str, &str) -> (u16, String) + Send + 'static) -> String {
    let listener =
        TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("bind stub server: {e}"));
    let port = listener
        .local_addr()
        .unwrap_or_else(|e| panic!("stub server addr: {e}"))
        .port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut request = Vec::new();
            let mut buf = [0u8; 8192];
            // Read headers, then the Content-Length body if any.
            let header_end = loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break None,
                    Ok(n) => {
                        request.extend_from_slice(&buf[..n]);
                        if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                            break Some(pos + 4);
                        }
                    }
                }
            };
            let Some(header_end) = header_end else {
                continue;
            };
            let head = String::from_utf8_lossy(&request[..header_end]).to_string();
            let content_length = head
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(str::trim)
                        .and_then(|v| v.parse::<usize>().ok())
                })
                .unwrap_or(0);
            while request.len() < header_end + content_length {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            let mut parts = head.split_whitespace();
            let method = parts.next().unwrap_or("").to_string();
            let path = parts.next().unwrap_or("").to_string();
            let (status, body) = respond(&method, &path);
            let reason = match status {
                200 => "OK",
                201 => "Created",
                204 => "No Content",
                401 => "Unauthorized",
                404 => "Not Found",
                409 => "Conflict",
                _ => "Error",
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        }
    });
    format!("http://127.0.0.1:{port}")
}

fn run(args: &[&str]) -> Output {
    monitor()
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn monitor binary: {e}"))
}

#[test]
fn bad_subcommand_is_a_usage_error() {
    let output = run(&["frobnicate"]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn an_empty_edit_is_a_usage_error() {
    let output = run(&["monitors", "edit", "deadbeef"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("nothing to change"));
}

#[test]
fn an_unreachable_server_exits_three() {
    // Bind then drop a listener to get a guaranteed-closed port.
    let port = TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map_or_else(|e| panic!("reserve a closed port: {e}"), |addr| addr.port());
    let output = run(&["--server", &format!("http://127.0.0.1:{port}"), "ping"]);
    assert_eq!(output.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&output.stderr).contains("server unreachable"));
}

#[test]
fn an_unauthorized_answer_exits_four() {
    let base = serve(|_, _| {
        (
            401,
            r#"{"error":{"code":"unauthorized","message":"missing credential"}}"#.to_string(),
        )
    });
    let output = run(&["--server", &base, "monitors", "list"]);
    assert_eq!(output.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unauthorized"));
}

#[test]
fn an_api_error_exits_one() {
    let base = serve(|_, _| {
        (
            409,
            r#"{"error":{"code":"conflict","message":"monitor has recorded history"}}"#.to_string(),
        )
    });
    let output = run(&["--server", &base, "monitors", "pause", "some-id"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("conflict"));
}

#[test]
fn ping_succeeds_against_a_live_server() {
    let base = serve(|_, path| {
        if path == "/livez" || path == "/readyz" {
            (200, r#"{"status":"ok"}"#.to_string())
        } else {
            (
                404,
                r#"{"error":{"code":"not_found","message":"nope"}}"#.to_string(),
            )
        }
    });
    let output = run(&["--server", &base, "ping"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("live and ready"));
}

#[test]
fn list_succeeds_against_a_live_server() {
    let base = serve(|_, path| {
        if path == "/api/monitors" {
            (200, r#"{"monitors":[]}"#.to_string())
        } else {
            (404, "{}".to_string())
        }
    });
    let output = run(&["--server", &base, "monitors", "list"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("no monitors yet"));
}
