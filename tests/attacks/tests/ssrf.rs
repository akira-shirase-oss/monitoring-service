//! SSRF against a running instance: the monitor and webhook targets are the two
//! places the server will dial for a tenant. Every local/non-HTTP form — plain
//! literals, the v4-in-v6 map, integer and hex tricks — must be refused at the
//! save gate, and the refusals must not name the scheme that was caught.
mod common;

use common::{client, spawn};
use reqwest::StatusCode;
use serde_json::json;

/// The instance's operator Bearer can create workspaces? No — targets are
/// workspace-scoped: mint an account the real way for a workspace to write in.
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
    // Signup makes no session — the cookie is earned at /api/login.
    let login = http
        .post(format!("{}/api/login", instance.base))
        .json(&json!({"email": email, "password": "a password that is long enough"}))
        .send()
        .await?;
    assert_eq!(login.status(), StatusCode::NO_CONTENT, "login for {email}");
    Ok(login
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("smon_session="))
        .expect("login sets a session cookie")
        .split(';')
        .next()
        .expect("cookie pair")
        .to_owned())
}

/// Every form of "localhost" a URL can take: the SSRF gate must refuse them all
/// at save — a dial must never happen.
#[tokio::test]
async fn local_targets_are_refused_at_save_in_every_spelling() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    let cookie = account(&instance, "ssrf@tenant.example").await?;

    for url in [
        "http://127.0.0.1/",
        "http://127.0.0.1:58400/livez",
        "http://127.1/",
        "http://0.0.0.0/",
        "http://[::1]/",
        "http://[0:0:0:0:0:0:0:1]/",
        "http://[::ffff:127.0.0.1]/",
        "http://10.0.0.4/",
        "http://172.16.0.1/",
        "http://192.168.1.1/",
        "http://169.254.169.254/latest/meta-data",
        "http://100.64.0.1/",
        "http://198.18.0.1/",
        "http://0x7f000001/",
        "http://2130706433/",
        "ftp://example.com/",
        "file:///etc/passwd",
        "gopher://127.0.0.1/",
        "javascript:alert(1)",
    ] {
        let response = http
            .post(format!("{}/api/monitors", instance.base))
            .header("cookie", &cookie)
            .header("origin", &instance.base)
            .json(&json!({"name": "ssrf", "url": url, "interval_s": 60}))
            .send()
            .await?;
        let status = response.status();
        assert!(
            matches!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY | StatusCode::BAD_REQUEST | StatusCode::FORBIDDEN
            ),
            "target {url} answered {status} — it must be refused at save"
        );
    }
    // Stored-state re-read: nineteen refusals must mean nineteen non-creates —
    // the workspace's monitor list is still empty.
    let list = http
        .get(format!("{}/api/monitors", instance.base))
        .header("cookie", &cookie)
        .header("origin", &instance.base)
        .send()
        .await?;
    let body: serde_json::Value = list.json().await?;
    assert_eq!(
        body["monitors"].as_array().map(Vec::len),
        Some(0),
        "a refused target was saved anyway: {body}"
    );
    Ok(())
}

/// The control: a public IP literal on http passes the save gate (no DNS needed —
/// literals are judged directly). The refusals above are local-shape refusals,
/// not a dead create path.
#[tokio::test]
async fn a_public_literal_is_created_fine() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    let cookie = account(&instance, "control@tenant.example").await?;
    let response = http
        .post(format!("{}/api/monitors", instance.base))
        .header("cookie", &cookie)
        .header("origin", &instance.base)
        .json(&json!({"name": "fine", "url": "http://93.184.216.34/", "interval_s": 60}))
        .send()
        .await?;
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "a public literal target must be creatable: {}",
        response.status()
    );
    Ok(())
}

/// The tenant webhook is the other outbound dialer: same local-target refusals
/// at its save gate.
#[tokio::test]
async fn the_webhook_target_gate_refuses_local_addresses() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    let cookie = account(&instance, "hook@tenant.example").await?;
    for url in [
        "http://127.0.0.1/hook",
        "http://[::1]/hook",
        "http://169.254.169.254/",
        "http://10.0.0.9/hook",
        "ftp://x/hook",
    ] {
        let response = http
            .put(format!("{}/api/workspace/webhook", instance.base))
            .header("cookie", &cookie)
            .header("origin", &instance.base)
            .json(&json!({"url": url}))
            .send()
            .await?;
        assert!(
            matches!(
                response.status(),
                StatusCode::UNPROCESSABLE_ENTITY | StatusCode::BAD_REQUEST | StatusCode::FORBIDDEN
            ),
            "webhook target {url} answered {} — refused at save is required",
            response.status()
        );
    }
    // Stored-state re-read: no refused URL may be sitting in the workspace's
    // webhook slot waiting for the next delivery attempt.
    let read = http
        .get(format!("{}/api/workspace/webhook", instance.base))
        .header("cookie", &cookie)
        .header("origin", &instance.base)
        .send()
        .await?;
    let body: serde_json::Value = read.json().await?;
    assert!(
        body["webhook"].is_null(),
        "a refused webhook target was stored: {body}"
    );
    Ok(())
}
