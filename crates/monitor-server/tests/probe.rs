//! Probe classification tests against real ephemeral HTTP fixtures: a probe must classify
//! what actually happened — pass, HTTP failure, transport failure or deadline — never infer.

use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use axum::routing::get;
use monitor_domain::CheckOutcome;
use monitor_server::probe::{ProbeClient, ProbePolicy};
use url::Url;

/// A lab-profile client: fixtures bind loopback, which production policy forbids.
fn lab_client(max_body: usize) -> anyhow::Result<ProbeClient> {
    Ok(ProbeClient::new(
        ProbePolicy {
            allow_local_targets: true,
        },
        max_body,
    )?)
}

/// Ephemeral fixture server on a random loopback port.
async fn fixture(router: Router) -> anyhow::Result<Url> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(axum::serve(listener, router).into_future());
    Ok(Url::parse(&format!("http://127.0.0.1:{port}/"))?)
}

#[tokio::test]
async fn a_2xx_is_a_pass() -> anyhow::Result<()> {
    let url = fixture(Router::new().route("/ok", get(|| async { "ok" }))).await?;
    let result = lab_client(64 * 1024)?
        .probe(&url.join("ok")?, Duration::from_secs(2))
        .await;
    assert_eq!(result.outcome, CheckOutcome::Pass);
    assert_eq!(result.http_status, Some(200));
    Ok(())
}

#[tokio::test]
async fn a_5xx_is_http_fail_not_transport() -> anyhow::Result<()> {
    let url =
        fixture(Router::new().route("/fail", get(|| async { StatusCode::INTERNAL_SERVER_ERROR })))
            .await?;
    let result = lab_client(64 * 1024)?
        .probe(&url.join("fail")?, Duration::from_secs(2))
        .await;
    assert_eq!(result.outcome, CheckOutcome::HttpFail { status: 500 });
    Ok(())
}

#[tokio::test]
async fn a_hung_target_is_a_timeout() -> anyhow::Result<()> {
    let url = fixture(Router::new().route(
        "/hang",
        get(|| async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            "late"
        }),
    ))
    .await?;
    let result = lab_client(64 * 1024)?
        .probe(&url.join("hang")?, Duration::from_millis(150))
        .await;
    assert_eq!(result.outcome, CheckOutcome::Timeout);
    assert!(result.latency < Duration::from_secs(5));
    Ok(())
}

#[tokio::test]
async fn a_refused_port_is_transport() -> anyhow::Result<()> {
    // Bind then drop a listener: the port is almost surely closed now.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let url = Url::parse(&format!("http://127.0.0.1:{port}/"))?;
    let result = lab_client(64 * 1024)?
        .probe(&url, Duration::from_secs(2))
        .await;
    assert_eq!(result.outcome, CheckOutcome::Transport);
    Ok(())
}

#[tokio::test]
async fn an_oversized_body_is_read_bounded_not_failed() -> anyhow::Result<()> {
    let big = vec![b'x'; 1024 * 1024];
    let url = fixture(Router::new().route("/big", get(move || async move { big.clone() }))).await?;
    let result = lab_client(4 * 1024)?
        .probe(&url.join("big")?, Duration::from_secs(2))
        .await;
    assert_eq!(result.outcome, CheckOutcome::Pass);
    Ok(())
}

#[tokio::test]
async fn policy_rejects_local_targets_without_the_lab_profile() -> anyhow::Result<()> {
    let client = ProbeClient::new(
        ProbePolicy {
            allow_local_targets: false,
        },
        64 * 1024,
    )?;
    let url = Url::parse("http://127.0.0.1:1/")?;
    let result = client.probe(&url, Duration::from_secs(2)).await;
    assert_eq!(result.outcome, CheckOutcome::Transport);
    assert_eq!(result.detail, Some("rejected_local"));

    let private = Url::parse("http://10.0.0.1/")?;
    let result = client.probe(&private, Duration::from_secs(2)).await;
    assert_eq!(result.detail, Some("rejected_local"));
    Ok(())
}

#[tokio::test]
async fn policy_rejects_non_http_schemes() -> anyhow::Result<()> {
    let client = lab_client(64 * 1024)?;
    let url = Url::parse("ftp://example.com/")?;
    let result = client.probe(&url, Duration::from_secs(2)).await;
    assert_eq!(result.outcome, CheckOutcome::Transport);
    assert_eq!(result.detail, Some("rejected_scheme"));
    Ok(())
}

/// Redirects (M5.2): a public hop chain is followed to its final answer — the redirect
/// itself is not the evidence, what it leads to is.
#[tokio::test]
async fn a_redirect_chain_is_followed_to_the_final_answer() -> anyhow::Result<()> {
    let url = fixture(
        Router::new()
            .route(
                "/go",
                get(|| async { (StatusCode::FOUND, [("location", "/mid")]) }),
            )
            .route(
                "/mid",
                get(|| async { (StatusCode::MOVED_PERMANENTLY, [("location", "/ok")]) }),
            )
            .route("/ok", get(|| async { "landed" })),
    )
    .await?;
    let result = lab_client(64 * 1024)?
        .probe(&url.join("go")?, Duration::from_secs(2))
        .await;
    assert_eq!(result.outcome, CheckOutcome::Pass);
    assert_eq!(result.http_status, Some(200));
    Ok(())
}

/// Past the hop limit, the last 3xx stands as evidence — a redirect loop is `http_fail`,
/// never a hang or a pass.
#[tokio::test]
async fn a_redirect_loop_is_http_fail_after_the_hop_limit() -> anyhow::Result<()> {
    let url = fixture(Router::new().route(
        "/loop",
        get(|| async { (StatusCode::FOUND, [("location", "/loop")]) }),
    ))
    .await?;
    let result = lab_client(64 * 1024)?
        .probe(&url.join("loop")?, Duration::from_secs(2))
        .await;
    assert_eq!(result.outcome, CheckOutcome::HttpFail { status: 302 });
    assert_eq!(result.http_status, Some(302));
    Ok(())
}
