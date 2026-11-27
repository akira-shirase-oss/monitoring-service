//! Auth bypass attempts against a running instance: no credential, a forged
//! cookie, a malformed Authorization, a token with the right shape but wrong
//! value — every authed surface must answer the same 401 envelope, and the
//! open routes must stay open.
mod common;

use common::{authed, client, spawn};
use reqwest::StatusCode;
use serde_json::json;

/// The read surfaces an attacker probes first: every one denies without a credential.
#[tokio::test]
async fn every_authed_read_denies_without_a_credential() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    for path in [
        "/api/monitors",
        "/api/incidents",
        "/api/members",
        "/api/members/invitations",
        "/api/tokens",
        "/api/workspaces",
        "/api/workspace",
        "/api/workspace/status-page",
        "/api/workspace/webhook",
        "/api/workspace/export",
        "/api/invitations",
    ] {
        let response = http
            .get(format!("{}{}", instance.base, path))
            .send()
            .await?;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "GET {path} answered without a credential"
        );
    }
    Ok(())
}

/// Writes deny before any body is trusted: an unauthed POST/PATCH/DELETE on the
/// mutating routes never reaches the handler's store.
#[tokio::test]
async fn every_authed_write_denies_without_a_credential() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    for (method, path) in [
        (reqwest::Method::POST, "/api/monitors"),
        (reqwest::Method::POST, "/api/invites"),
        (reqwest::Method::POST, "/api/members"),
        (reqwest::Method::POST, "/api/tokens"),
        (reqwest::Method::POST, "/api/backup"),
        (reqwest::Method::POST, "/api/workspace/webhook/test"),
        (reqwest::Method::PATCH, "/api/workspace"),
        (reqwest::Method::PUT, "/api/workspace/status-page"),
        (reqwest::Method::PUT, "/api/workspace/webhook"),
        (reqwest::Method::POST, "/api/account/password"),
        (reqwest::Method::DELETE, "/api/account"),
        (
            reqwest::Method::DELETE,
            "/api/tokens/00000000-0000-0000-0000-000000000000",
        ),
    ] {
        let response = http
            .request(method.clone(), format!("{}{}", instance.base, path))
            .json(&json!({}))
            .send()
            .await?;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path} answered without a credential"
        );
    }
    // Stored-state re-read: a status alone could hide a write that landed before
    // the denial — the store must still be empty under a real credential.
    for (path, key) in [("/api/monitors", "monitors"), ("/api/tokens", "tokens")] {
        let list = authed(&instance, reqwest::Method::GET, path).send().await?;
        let body: serde_json::Value = list.json().await?;
        assert_eq!(
            body[key].as_array().map(Vec::len),
            Some(0),
            "an unauthed write left rows in {path}: {body}"
        );
    }
    Ok(())
}

/// Forged credentials of every shape: a made-up cookie id, a random bearer, a
/// truncated bearer, a Basic blob, an empty bearer — all uniform 401.
#[tokio::test]
async fn forged_credentials_all_answer_the_same_401() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    for (name, header, value) in [
        (
            "forged cookie",
            "cookie",
            "__Host-smon_session=forged-id-123",
        ),
        ("wrong bearer", "authorization", "Bearer not-the-token"),
        ("empty bearer", "authorization", "Bearer "),
        ("basic blob", "authorization", "Basic dXNlcjpwYXNz"),
        (
            "bearer typo",
            "authorization",
            "bearer attack-operator-token",
        ),
        (
            "bearer suffix",
            "authorization",
            "Bearer attack-operator-token extra",
        ),
    ] {
        let response = http
            .get(format!("{}/api/monitors", instance.base))
            .header(header, value)
            .send()
            .await?;
        assert_eq!(
            response.status(),
            StatusCode::UNAUTHORIZED,
            "{name}: {value:?} was not 401"
        );
    }
    Ok(())
}

/// The open surface is small and stays open: liveness, readiness, version, the
/// public status route (which answers 404 here, not auth-challenged).
#[tokio::test]
async fn only_health_and_public_surfaces_answer_unauthed() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    for path in ["/livez", "/readyz", "/version"] {
        let status = http
            .get(format!("{}{}", instance.base, path))
            .send()
            .await?
            .status();
        assert!(status.is_success(), "{path} should be open, got {status}");
    }
    let response = http
        .get(format!(
            "{}/s/definitely-not-a-page/status.json",
            instance.base
        ))
        .send()
        .await?;
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "an unknown slug is 404, never a credential prompt"
    );
    // The other half of the name: the authed surfaces refuse without a
    // credential. The layer that gates them is the protection this test
    // measures, so the refusal is the specific 401 — not any failure.
    for path in ["/api/monitors", "/api/incidents", "/api/workspaces"] {
        let status = http
            .get(format!("{}{}", instance.base, path))
            .send()
            .await?
            .status();
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{path} must refuse unauthed, got {status}"
        );
    }
    Ok(())
}

/// A real credential must work — the suite's refusals are credential checks,
/// not a dead server. And the 401 body is the uniform envelope.
#[tokio::test]
async fn the_operator_bearer_opens_the_api_and_denials_are_enveloped() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let ok = authed(&instance, reqwest::Method::GET, "/api/monitors")
        .send()
        .await?;
    assert_eq!(ok.status(), StatusCode::OK);

    let denied = client()?
        .get(format!("{}/api/monitors", instance.base))
        .send()
        .await?;
    assert_eq!(
        denied.status(),
        StatusCode::UNAUTHORIZED,
        "an unauthenticated request must be denied"
    );
    let body: serde_json::Value = denied
        .json()
        .await
        .expect("the denial must be a JSON envelope");
    assert!(
        body["error"].is_object(),
        "a denial is the error envelope, got {body}"
    );
    Ok(())
}
