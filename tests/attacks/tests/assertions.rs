//! M8 attacks on response assertions + the `HEAD` opt-in (D-025): a marker that
//! must never silently pass, a bounded body read in bytes AND time, wire bytes
//! never inflated, `HEAD` preserved across redirects and never retried as `GET`,
//! and an assertion string that is data to search — never a request target.
//! Write-boundary tests run strict; probe-behavior tests run the lab spawn so
//! the instance can probe loopback fixtures the test owns.
mod common;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use common::{authed, client, spawn, spawn_lab};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A signup -> login -> cookie for a member workspace (same recipe as ssrf.rs).
#[expect(
    clippy::expect_used,
    reason = "attack fixture: a signup that set no cookie is a failed test"
)]
async fn account(instance: &common::Instance, email: &str) -> anyhow::Result<String> {
    let http = client()?;
    let invite = http
        .post(format!("{}/api/invites", instance.base))
        .bearer_auth(instance.token)
        .json(&json!({}))
        .send()
        .await?;
    let body: serde_json::Value = invite.json().await?;
    let response = http
        .post(format!("{}/api/signup", instance.base))
        .json(&json!({
            "email": email,
            "password": "a password that is long enough",
            "invite": body["invite"]["code"],
        }))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::CREATED);
    let login = http
        .post(format!("{}/api/login", instance.base))
        .json(&json!({"email": email, "password": "a password that is long enough"}))
        .send()
        .await?;
    assert_eq!(login.status(), StatusCode::NO_CONTENT, "login for {email}");
    let cookie = login
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("__Host-smon_session="))
        .expect("login sets a session cookie")
        .split(';')
        .next()
        .expect("cookie pair")
        .to_owned();
    // The fixtures' 15 s intervals are below the free floor — lift the new
    // tenant to `operator` so the plan gate stays out of the assertion's way.
    let mine: Value = http
        .get(format!("{}/api/workspaces", instance.base))
        .header("cookie", &cookie)
        .send()
        .await?
        .json()
        .await?;
    let id = mine["workspaces"][0]["id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let lift = http
        .patch(format!("{}/api/workspaces/{id}/plan", instance.base))
        .bearer_auth(&instance.token)
        .json(&json!({"plan": "operator"}))
        .send()
        .await?;
    assert_eq!(lift.status(), StatusCode::OK, "operator lift for {email}");
    Ok(cookie)
}

/// Read one request head (through the blank line) — method + path is all a
/// fixture needs; the body is irrelevant for these probes.
async fn read_head(sock: &mut tokio::net::TcpStream) -> String {
    let mut buf = vec![0u8; 4096];
    let mut head = String::new();
    while !head.contains("\r\n\r\n") {
        let n = match sock.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        head.push_str(&String::from_utf8_lossy(&buf[..n]));
    }
    head
}

/// Serve `body` with a 200 and a content-length forever — one task per
/// connection, so a probe chain gets an answer on every hop it owns.
#[expect(clippy::expect_used)]
fn serve_body(body: &'static [u8]) -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("fixture bind");
    let addr = listener.local_addr().expect("fixture addr");
    listener.set_nonblocking(true).expect("nonblocking");
    let listener = tokio::net::TcpListener::from_std(listener).expect("tokio listener");
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                let _ = read_head(&mut sock).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body).await;
            });
        }
    });
    addr
}

/// The `HEAD` redirect probe: `/go` 303s to `/land`; `/land` answers 200 only to
/// `HEAD` — any other method gets 501. A `HEAD` rewritten to `GET` lands as a
/// failure, which is exactly what the honest answer must NOT be.
#[expect(clippy::expect_used)]
fn serve_redirect(methods: Arc<Mutex<Vec<String>>>) -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("fixture bind");
    let addr = listener.local_addr().expect("fixture addr");
    listener.set_nonblocking(true).expect("nonblocking");
    let listener = tokio::net::TcpListener::from_std(listener).expect("tokio");
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let methods = methods.clone();
            tokio::spawn(async move {
                let head = read_head(&mut sock).await;
                let mut parts = head.split_whitespace();
                let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                let response = if path.starts_with("/go") {
                    "HTTP/1.1 303 See Other\r\nlocation: /land\r\ncontent-length: 0\r\n\r\n"
                        .to_owned()
                } else {
                    methods.lock().expect("methods").push(method.to_owned());
                    if method == "HEAD" {
                        "HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n".to_owned()
                    } else {
                        "HTTP/1.1 501 Not Implemented\r\ncontent-length: 0\r\n\r\n".to_owned()
                    }
                };
                let _ = sock.write_all(response.as_bytes()).await;
            });
        }
    });
    addr
}

/// A body that never finishes: headers then one chunked byte a beat, forever.
#[expect(clippy::expect_used)]
fn serve_dribble() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("fixture bind");
    let addr = listener.local_addr().expect("fixture addr");
    listener.set_nonblocking(true).expect("nonblocking");
    let listener = tokio::net::TcpListener::from_std(listener).expect("tokio");
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            tokio::spawn(async move {
                let _ = read_head(&mut sock).await;
                let head = b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ntransfer-encoding: chunked\r\n\r\n";
                if sock.write_all(head).await.is_err() {
                    return;
                }
                loop {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    if sock.write_all(b"1\r\nM\r\n").await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    addr
}

/// Register a monitor, then poll its committed checks until the first row
/// lands — the slot grid can put the first slot up to ~20s out.
async fn wait_check(cookie: &str, base: &str, monitor_id: &str) -> anyhow::Result<Value> {
    let http = client()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let rows: Value = http
            .get(format!("{base}/api/monitors/{monitor_id}/checks"))
            .header("cookie", cookie)
            .send()
            .await?
            .json()
            .await?;
        if let Some(first) = rows["checks"].as_array().and_then(|a| a.first().cloned()) {
            return Ok(first);
        }
        if std::time::Instant::now() > deadline {
            anyhow::bail!("no committed check within 60s for {monitor_id}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
}

#[expect(
    clippy::expect_used,
    reason = "attack fixture: a create reply without an id is a failed test"
)]
async fn create(instance: &common::Instance, cookie: &str, body: Value) -> anyhow::Result<String> {
    let response = client()?
        .post(format!("{}/api/monitors", instance.base))
        .header("cookie", cookie)
        .header("origin", &instance.base)
        .json(&body)
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::CREATED, "create {body}");
    Ok(response.json::<Value>().await?["id"]
        .as_str()
        .expect("id")
        .to_owned())
}

/// The marker is searched in the observed body — a miss on an acceptable status
/// is `http_fail`/`assert_body`, never a silent pass.
#[tokio::test]
async fn a_body_miss_is_failure_evidence() -> anyhow::Result<()> {
    let instance = spawn_lab().await?;
    let cookie = account(&instance, "miss@tenant.example").await?;
    let fixture = serve_body(b"plain body without the marker");
    let id = create(
        &instance,
        &cookie,
        json!({
            "name": "marked", "url": format!("http://{fixture}/"),
            "interval_s": 15, "timeout_s": 3, "expect_body": "MARKER"
        }),
    )
    .await?;
    let check = wait_check(&cookie, &instance.base, &id).await?;
    assert_eq!(
        check["outcome"], "http_fail",
        "the miss is evidence: {check}"
    );
    assert_eq!(check["http_status"], 200, "the answer itself was fine");
    assert_eq!(check["detail"], "assert_body", "the miss word: {check}");
    Ok(())
}

/// Every bound in the expectations config is enforced at write — oversized
/// marker, too many ranges, an empty set, an inverted range, out-of-band
/// statuses, overlapping ranges.
#[tokio::test]
async fn the_expect_config_is_bounded_at_write() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    let cookie = account(&instance, "bounded@tenant.example").await?;
    for body in [
        json!({"expect_body": "x".repeat(257)}),
        json!({"expect_statuses": ["1-9","10-19","20-29","30-39","40-49","50-59","60-69","70-79","80-89"]}),
        json!({"expect_statuses": []}),
        json!({"expect_statuses": ["999-100"]}),
        json!({"expect_statuses": ["600"]}),
        json!({"expect_statuses": ["200-300", "250-350"]}),
    ] {
        let response = http
            .post(format!("{}/api/monitors", instance.base))
            .header("cookie", &cookie)
            .header("origin", &instance.base)
            .json(&{
                let mut req =
                    json!({"name": "bad", "url": "http://93.184.216.34/", "interval_s": 60});
                if let Some(extra) = body.as_object() {
                    req.as_object_mut().expect("object").extend(extra.clone());
                }
                req
            })
            .send()
            .await?;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body} must 422, got {}",
            response.status()
        );
    }
    Ok(())
}

/// `HEAD` carries no body — a body marker under it is a write-time refusal.
#[tokio::test]
async fn head_and_a_body_expectation_is_refused_at_write() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    let cookie = account(&instance, "headbody@tenant.example").await?;
    let response = http
        .post(format!("{}/api/monitors", instance.base))
        .header("cookie", &cookie)
        .header("origin", &instance.base)
        .json(&json!({
            "name": "bad", "url": "http://93.184.216.34/", "interval_s": 60,
            "method": "HEAD", "expect_body": "MARKER"
        }))
        .send()
        .await?;
    assert_eq!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "HEAD + expect_body must 422, got {}",
        response.status()
    );
    Ok(())
}

/// The marker must be inside the observed <= 64 KiB prefix — a marker placed
/// past the bound is a miss, never a pass.
#[tokio::test]
async fn the_body_bound_still_holds() -> anyhow::Result<()> {
    let instance = spawn_lab().await?;
    let cookie = account(&instance, "bound@tenant.example").await?;
    // The marker straddles the 64 KiB read bound — its first bytes land inside
    // the observed prefix, the rest arrive past it — so no contiguous match can
    // form inside the window. A probe that reads past the bound passes wrongly.
    let body: &'static [u8] = Box::leak(
        {
            let mut v = vec![b'A'; 64 * 1024 - 3];
            v.extend_from_slice(b"MARKER");
            v.extend_from_slice(&vec![b'A'; 1024]);
            v
        }
        .into_boxed_slice(),
    );
    let fixture = serve_body(body);
    let id = create(
        &instance,
        &cookie,
        json!({
            "name": "deep", "url": format!("http://{fixture}/"),
            "interval_s": 15, "timeout_s": 5, "expect_body": "MARKER"
        }),
    )
    .await?;
    let check = wait_check(&cookie, &instance.base, &id).await?;
    assert_eq!(
        check["outcome"], "http_fail",
        "a marker past the bound misses: {check}"
    );
    assert_eq!(check["detail"], "assert_body");
    Ok(())
}

/// `.*` in a marker is just bytes — there is no regex engine to exploit.
#[tokio::test]
async fn a_literal_regex_is_not_an_engine() -> anyhow::Result<()> {
    let instance = spawn_lab().await?;
    let cookie = account(&instance, "regex@tenant.example").await?;
    // `x.*y` as a pattern would match; as literal bytes it is absent.
    let fixture = serve_body(b"xZZZZy");
    let id = create(
        &instance,
        &cookie,
        json!({
            "name": "literal", "url": format!("http://{fixture}/"),
            "interval_s": 15, "timeout_s": 3, "expect_body": "x.*y"
        }),
    )
    .await?;
    let check = wait_check(&cookie, &instance.base, &id).await?;
    assert_eq!(
        check["detail"], "assert_body",
        "a loose match is a miss: {check}"
    );
    Ok(())
}

/// Expectations are workspace-owned monitor data — never the public snapshot.
/// A published monitor lands on its workspace's own `/s/{slug}` page; its
/// `expect_*` and `method` fields must not.
#[tokio::test]
async fn expectations_stay_out_of_the_public_snapshot() -> anyhow::Result<()> {
    let instance = spawn_lab().await?;
    let http = client()?;
    let cookie = account(&instance, "page@tenant.example").await?;
    let response = http
        .post(format!("{}/api/monitors", instance.base))
        .header("cookie", &cookie)
        .header("origin", &instance.base)
        .json(&json!({
            "name": "secret-check", "url": "http://93.184.216.34/",
            "interval_s": 60, "method": "GET",
            "expect_statuses": ["200-299", "418"], "expect_body": "LEAKME"
        }))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::CREATED);
    let id = response.json::<Value>().await?["id"]
        .as_str()
        .expect("id")
        .to_owned();
    // Publish the monitor on this workspace's own status page — a member's
    // monitor never lands on the operator's global `/status.json` (D-021).
    let page = http
        .put(format!("{}/api/workspace/status-page", instance.base))
        .header("cookie", &cookie)
        .header("origin", &instance.base)
        .json(&json!({"slug": "assert-check", "title": "assertions", "monitor_ids": [id]}))
        .send()
        .await?;
    assert_eq!(page.status(), StatusCode::OK, "status-page publish");
    // Poll the page snapshot until the monitor's service entry lands — then the
    // serialized body must hold none of the expectation fields or the marker.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let body = http
            .get(format!("{}/s/assert-check/status.json", instance.base))
            .send()
            .await?
            .text()
            .await?;
        let populated = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v["services"].as_array().map(|a| !a.is_empty()))
            .unwrap_or(false);
        if !populated {
            if std::time::Instant::now() > deadline {
                anyhow::bail!("status page never populated: {body}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            continue;
        }
        assert!(
            !body.contains("\"expect"),
            "expectation fields leaked into status.json: {body}"
        );
        assert!(
            !body.contains("LEAKME"),
            "the marker value leaked into status.json: {body}"
        );
        assert!(
            !body.contains("\"method\""),
            "the probe method leaked into status.json: {body}"
        );
        break;
    }
    Ok(())
}

/// The edit path revalidates the merged bundle — a HEAD swap onto a monitor
/// holding a marker, and a garbage status set, both refuse.
#[tokio::test]
async fn patch_revalidates() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    let cookie = account(&instance, "patch@tenant.example").await?;
    let id = create(
        &instance,
        &cookie,
        json!({
            "name": "patched", "url": "http://93.184.216.34/", "interval_s": 60,
            "expect_body": "MARKER"
        }),
    )
    .await?;
    for patch in [
        json!({"method": "HEAD"}),
        json!({"expect_statuses": ["999-100"]}),
    ] {
        let response = http
            .patch(format!("{}/api/monitors/{id}", instance.base))
            .header("cookie", &cookie)
            .header("origin", &instance.base)
            .json(&patch)
            .send()
            .await?;
        assert_eq!(
            response.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "patch {patch} must 422, got {}",
            response.status()
        );
    }
    Ok(())
}

/// A body that never finishes dies on the monitor's own deadline — the read is
/// bounded in time, not just bytes.
#[tokio::test]
async fn a_slow_body_never_outruns_the_deadline() -> anyhow::Result<()> {
    let instance = spawn_lab().await?;
    let cookie = account(&instance, "slow@tenant.example").await?;
    let fixture = serve_dribble();
    let id = create(
        &instance,
        &cookie,
        json!({
            "name": "slow", "url": format!("http://{fixture}/"),
            "interval_s": 15, "timeout_s": 2, "expect_body": "SENTINEL"
        }),
    )
    .await?;
    // A hung probe commits nothing — the missed check maps to Null, and the
    // assert below is where a deadline-removing mutant goes red.
    let check = wait_check(&cookie, &instance.base, &id)
        .await
        .unwrap_or(Value::Null);
    assert_eq!(
        check["outcome"], "timeout",
        "the dribble dies on the deadline: {check}"
    );
    Ok(())
}

/// The client is built without decompression features — the assertion reads the
/// wire bytes, so a compressed bomb costs only the bounded read. A gzipped
/// marker misses; it is never inflated into a pass.
#[tokio::test]
async fn compressed_bytes_are_never_inflated() -> anyhow::Result<()> {
    let instance = spawn_lab().await?;
    let cookie = account(&instance, "gzip@tenant.example").await?;
    // gzip.compress(b"SENTINEL-here") — the marker exists only inside the stream.
    let gzipped: &[u8] = &[
        31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 11, 118, 245, 11, 241, 244, 115, 245, 209, 205, 72, 45,
        74, 5, 0, 125, 143, 180, 174, 13, 0, 0, 0,
    ];
    let fixture = serve_body(gzipped);
    let id = create(
        &instance,
        &cookie,
        json!({
            "name": "gz", "url": format!("http://{fixture}/"),
            "interval_s": 15, "timeout_s": 3, "expect_body": "SENTINEL-here"
        }),
    )
    .await?;
    let check = wait_check(&cookie, &instance.base, &id).await?;
    assert_eq!(
        check["detail"], "assert_body",
        "wire bytes are searched, never inflated: {check}"
    );
    Ok(())
}

/// A `HEAD` probe stays `HEAD` across a 303 — tower-http rewrites every other
/// method to `GET`, so the fixture's method gate is the evidence.
#[tokio::test]
async fn head_stays_head_across_a_redirect() -> anyhow::Result<()> {
    let instance = spawn_lab().await?;
    let cookie = account(&instance, "redirect@tenant.example").await?;
    let methods: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let fixture = serve_redirect(methods.clone());
    let id = create(
        &instance,
        &cookie,
        json!({
            "name": "head", "url": format!("http://{fixture}/go"),
            "interval_s": 15, "timeout_s": 5, "method": "HEAD"
        }),
    )
    .await?;
    let check = wait_check(&cookie, &instance.base, &id).await?;
    assert_eq!(
        check["outcome"], "pass",
        "HEAD lands as HEAD through the 303: {check}"
    );
    assert_eq!(
        methods.lock().expect("methods").as_slice(),
        ["HEAD"],
        "the landing saw exactly one HEAD"
    );
    Ok(())
}

/// An assertion string is searched against the already-validated response — a
/// marker shaped like a URL dials nothing.
#[tokio::test]
async fn a_marker_is_searched_never_dialed() -> anyhow::Result<()> {
    let instance = spawn_lab().await?;
    let cookie = account(&instance, "ssrf@tenant.example").await?;
    let fixture = serve_body(b"plain body");
    let id = create(
        &instance,
        &cookie,
        json!({
            "name": "marker-url", "url": format!("http://{fixture}/"),
            "interval_s": 15, "timeout_s": 3,
            "expect_body": "http://127.0.0.1:9/internal"
        }),
    )
    .await?;
    let check = wait_check(&cookie, &instance.base, &id).await?;
    // The marker is data: it missed the real body, and the probe never dialed
    // the 127.0.0.1:9 it names (that would be a transport refusal, not a miss).
    assert_eq!(
        check["detail"], "assert_body",
        "the marker was searched, never requested: {check}"
    );
    Ok(())
}
