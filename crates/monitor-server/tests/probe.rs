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
            extra_roots: Vec::new(),
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
            extra_roots: Vec::new(),
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

/// TLS classification (M6.1): a broken certificate gets its own committed detail —
/// "couldn't connect" is the least useful thing a monitor can say. Each case uses a real
/// rcgen cert over a real rustls listener; the probe's verifier does the rejecting.
use rustls_pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;

fn cert_for(
    names: &[&str],
    not_before: (i32, u8, u8),
    not_after: (i32, u8, u8),
) -> anyhow::Result<(CertificateDer<'static>, PrivatePkcs8KeyDer<'static>)> {
    let key = rcgen::KeyPair::generate()?;
    let mut params =
        rcgen::CertificateParams::new(names.iter().map(ToString::to_string).collect::<Vec<_>>())?;
    params.not_before = rcgen::date_time_ymd(not_before.0, not_before.1, not_before.2);
    params.not_after = rcgen::date_time_ymd(not_after.0, not_after.1, not_after.2);
    let cert = params.self_signed(&key)?;
    Ok((
        cert.der().clone(),
        PrivatePkcs8KeyDer::from(key.serialize_der()),
    ))
}

/// Serve TLS with the given cert; the handshake is all the probe needs — the client
/// rejects during verification, so the server never has to answer a request.
async fn tls_fixture(
    cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
) -> anyhow::Result<Url> {
    tls_fixture_chain(vec![cert], key).await
}

#[tokio::test]
async fn an_expired_certificate_is_cert_expired() -> anyhow::Result<()> {
    let (cert, key) = cert_for(&["localhost"], (2020, 1, 1), (2020, 1, 2))?;
    let url = tls_fixture(cert, key).await?;
    let result = lab_client(64 * 1024)?
        .probe(&url, Duration::from_secs(3))
        .await;
    assert_eq!(result.outcome, CheckOutcome::Transport);
    assert_eq!(result.detail, Some("cert_expired"));
    Ok(())
}

#[tokio::test]
async fn a_not_yet_valid_certificate_is_cert_not_yet_valid() -> anyhow::Result<()> {
    let (cert, key) = cert_for(&["localhost"], (2040, 1, 1), (2041, 1, 1))?;
    let url = tls_fixture(cert, key).await?;
    let result = lab_client(64 * 1024)?
        .probe(&url, Duration::from_secs(3))
        .await;
    assert_eq!(result.detail, Some("cert_not_yet_valid"));
    Ok(())
}

/// A CA + a leaf it signs: the pure name-mismatch case needs a chain the client
/// *trusts* — verification stops at untrusted before the name is ever checked.
fn ca_and_leaf(
    leaf_names: &[&str],
) -> anyhow::Result<(
    CertificateDer<'static>,
    Vec<CertificateDer<'static>>,
    PrivatePkcs8KeyDer<'static>,
)> {
    let ca_key = rcgen::KeyPair::generate()?;
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new())?;
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.not_before = rcgen::date_time_ymd(2020, 1, 1);
    ca_params.not_after = rcgen::date_time_ymd(2041, 1, 1);
    let ca_cert = ca_params.self_signed(&ca_key)?;
    let issuer = rcgen::Issuer::new(ca_params, ca_key);

    let leaf_key = rcgen::KeyPair::generate()?;
    let mut leaf_params = rcgen::CertificateParams::new(
        leaf_names
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
    )?;
    leaf_params.not_before = rcgen::date_time_ymd(2020, 1, 1);
    leaf_params.not_after = rcgen::date_time_ymd(2041, 1, 1);
    let leaf = leaf_params.signed_by(&leaf_key, &issuer)?;
    Ok((
        ca_cert.der().clone(),
        vec![leaf.der().clone(), ca_cert.der().clone()],
        PrivatePkcs8KeyDer::from(leaf_key.serialize_der()),
    ))
}

async fn tls_fixture_chain(
    chain: Vec<CertificateDer<'static>>,
    key: PrivatePkcs8KeyDer<'static>,
) -> anyhow::Result<Url> {
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key.into())?;
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                if let Ok(mut tls) = acceptor.accept(stream).await {
                    use tokio::io::AsyncWriteExt;
                    let _ = tls
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                        )
                        .await;
                }
            });
        }
    });
    Ok(Url::parse(&format!("https://localhost:{port}/"))?)
}

#[tokio::test]
async fn a_trusted_certificate_for_another_name_is_cert_name_mismatch() -> anyhow::Result<()> {
    let (ca, chain, key) = ca_and_leaf(&["wrong-name.example"])?;
    let url = tls_fixture_chain(chain, key).await?;
    let client = ProbeClient::new(
        ProbePolicy {
            allow_local_targets: true,
            extra_roots: vec![ca],
        },
        64 * 1024,
    )?;
    let result = client.probe(&url, Duration::from_secs(3)).await;
    assert_eq!(result.detail, Some("cert_name_mismatch"));
    Ok(())
}

#[tokio::test]
async fn a_valid_chain_over_https_is_a_pass() -> anyhow::Result<()> {
    let (ca, chain, key) = ca_and_leaf(&["localhost"])?;
    let url = tls_fixture_chain(chain, key).await?;
    let client = ProbeClient::new(
        ProbePolicy {
            allow_local_targets: true,
            extra_roots: vec![ca],
        },
        64 * 1024,
    )?;
    let result = client.probe(&url, Duration::from_secs(3)).await;
    assert_eq!(result.outcome, CheckOutcome::Pass);
    Ok(())
}

#[tokio::test]
async fn a_self_signed_certificate_is_cert_untrusted() -> anyhow::Result<()> {
    let (cert, key) = cert_for(&["localhost"], (2020, 1, 1), (2041, 1, 1))?;
    let url = tls_fixture(cert, key).await?;
    let result = lab_client(64 * 1024)?
        .probe(&url, Duration::from_secs(3))
        .await;
    assert_eq!(result.detail, Some("cert_untrusted"));
    Ok(())
}
