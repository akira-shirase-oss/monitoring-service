//! Resource attacks against a running instance: a body over the 16 KiB cap, a
//! body that never finishes, headers dripped a byte at a time, a single header
//! the size of a request body. Every one is refused inside a deadline — the
//! attacker's slowness must never hold a slot open.
mod common;

use common::{client, spawn};
use reqwest::StatusCode;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A body past the cap is refused before it is parsed — the 413 lands without a
/// session and without reading the megabyte.
#[tokio::test]
async fn an_oversized_body_is_413() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    let body = format!("{{\"token\":\"{}\"}}", "x".repeat(1024 * 1024));
    let response = http
        .post(format!("{}/api/login", instance.base))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await;
    match response {
        Ok(r) => assert_eq!(
            r.status(),
            StatusCode::PAYLOAD_TOO_LARGE,
            "a 1 MiB body must be 413, got {}",
            r.status()
        ),
        // The 413 lands and the connection closes while the megabyte is still
        // uploading — the refusal reached us before the body was ever read.
        Err(e) => assert!(
            e.is_request() || e.is_body(),
            "unexpected client error: {e}"
        ),
    }
    // The refusal must not have cost a slot or killed the listener: a normal
    // request right after still gets the server's own answer.
    let alive = http
        .post(format!("{}/api/login", instance.base))
        .json(&json!({"token": "wrong"}))
        .send()
        .await?;
    assert_eq!(
        alive.status(),
        StatusCode::UNAUTHORIZED,
        "after the oversized body the server stopped answering normally"
    );
    Ok(())
}

/// A request body that never arrives is cut at the deadline — 408, not a parked
/// connection. The client supplies a stream that yields once then stays pending;
/// the server's request timeout decides, inside the client timeout.
#[tokio::test]
async fn a_body_that_never_finishes_is_408() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let stream = tokio_stream::pending::<Result<bytes::Bytes, std::io::Error>>();
    let response = http
        .post(format!("{}/api/login", instance.base))
        .header("content-type", "application/json")
        .body(reqwest::Body::wrap_stream(stream))
        .send()
        .await?;
    assert_eq!(
        response.status(),
        StatusCode::REQUEST_TIMEOUT,
        "a never-ending body must be 408, got {}",
        response.status()
    );
    // The timed-out request must release its slot — the next normal request is
    // answered by the server, not stalled on the abandoned body's budget.
    let after = http
        .post(format!("{}/api/login", instance.base))
        .json(&json!({"token": "wrong"}))
        .send()
        .await?;
    assert_eq!(
        after.status(),
        StatusCode::UNAUTHORIZED,
        "after the stalled body the server stopped answering normally"
    );
    Ok(())
}

/// Headers dripped a byte at a time, forever: the header never completes, so only
/// the server's header-read deadline can end the connection — our read ends
/// (response or close) inside 18 s. A finite drip that finishes inside the
/// deadline would test nothing: any refusal it got back would be an answered
/// request, not a cut.
#[tokio::test]
async fn drip_fed_headers_are_cut() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let addr = instance
        .base
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("base was not http: {}", instance.base))?
        .to_owned();
    let stream = tokio::net::TcpStream::connect(&addr).await?;
    let (mut reader, mut writer) = stream.into_split();
    writer
        .write_all(b"POST /api/login HTTP/1.1\r\nHost: x\r\nContent-Length: 10\r\n")
        .await?;
    // A byte every 700 ms and never the terminating blank line: the header is
    // still in progress at every deadline the server could pick.
    let drip = tokio::spawn(async move {
        loop {
            for byte in b"X-Drip: 1\r\n" {
                if writer.write_all(&[*byte]).await.is_err() {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            }
        }
    });

    let mut buf = Vec::new();
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(18),
        reader.read_to_end(&mut buf),
    )
    .await;
    drip.abort();
    match outcome {
        // The socket closed — with or without a 408 line first — before the drip
        // could finish. Either way the server refused to park on the header.
        Ok(_) => {
            if !buf.is_empty() {
                let text = String::from_utf8_lossy(&buf);
                assert!(
                    text.contains("HTTP/1.1 4") || text.contains("HTTP/1.0 4"),
                    "a response must be a refusal, got: {text:.120}"
                );
            }
        }
        Err(_) => {
            anyhow::bail!(
                "the server held a drip-fed header past 18 s — the read timeout never cut it"
            );
        }
    }
    // The cut connection freed its read slot — the listener still answers.
    let alive = client()?
        .get(format!("{}/livez", instance.base))
        .send()
        .await?;
    assert!(alive.status().is_success(), "the drip starved the listener");
    Ok(())
}

/// One header the size of a body: the HTTP parser's buffer refuses it before the
/// router ever sees the request — a 431 or a closed connection, never a 401.
#[tokio::test]
async fn an_oversized_header_is_refused() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    let response = http
        .get(format!("{}/api/monitors", instance.base))
        .header("x-giant", "x".repeat(1024 * 1024))
        .send()
        .await;
    if let Ok(r) = response {
        assert!(
            matches!(
                r.status(),
                StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE | StatusCode::BAD_REQUEST
            ),
            "a 1 MiB header must be refused at the parser, got {}",
            r.status()
        );
    }
    // A client-side send failure means the parser cut the connection — also a
    // refusal, and never a processed request.
    // The parser is still serving: a normal request gets the server's own 401.
    let alive = http
        .get(format!("{}/api/monitors", instance.base))
        .send()
        .await?;
    assert_eq!(
        alive.status(),
        StatusCode::UNAUTHORIZED,
        "after the giant header the parser stopped serving requests"
    );
    Ok(())
}
