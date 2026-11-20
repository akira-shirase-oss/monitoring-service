//! Header spoofing against a running instance: the client writes headers hoping
//! the server trusts them — `X-Forwarded-For` to move its throttle identity,
//! `X-Smon-Workspace` to pick a tenant it isn't in, `Origin` to smuggle a
//! cross-site cookie write. The instance runs with NO trusted proxy configured,
//! the deployment's safe default: forwarding headers mean nothing.
mod common;

use common::{client, spawn};
use reqwest::StatusCode;
use serde_json::json;

/// Spun `X-Forwarded-For` values do not move the throttle key: with no trusted
/// proxy, the peer is the key — a fresh header per guess still empties the
/// bucket, and the flood is refused while a real credential still works.
#[tokio::test]
async fn forged_x_forwarded_for_does_not_free_guesses() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    // Concurrent guesses, each claiming a different client — the admits cluster
    // inside one refill period on any machine (token spent before argon2).
    let mut guesses = tokio::task::JoinSet::new();
    for n in 0..8_u32 {
        let http = http.clone();
        let base = instance.base.clone();
        guesses.spawn(async move {
            http.post(format!("{base}/api/login"))
                .header("x-forwarded-for", format!("203.0.113.{n}"))
                .json(&json!({"email": "nobody@x.example", "password": "wrong"}))
                .send()
                .await
                .map(|r| r.status())
        });
    }
    let mut refused = false;
    while let Some(status) = guesses.join_next().await {
        refused |= status?? == StatusCode::TOO_MANY_REQUESTS;
    }
    assert!(
        refused,
        "eight XFF-spun guesses from one peer never hit the throttle"
    );
    Ok(())
}

/// The same spin on the anonymous read budget: rotating XFF does not refill the
/// public-read allowance either.
#[tokio::test]
async fn forged_x_forwarded_for_does_not_free_public_reads() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    let mut reads = tokio::task::JoinSet::new();
    for n in 0..40_u32 {
        let http = http.clone();
        let base = instance.base.clone();
        reads.spawn(async move {
            http.get(format!("{base}/s/nope/status.json"))
                .header("x-forwarded-for", format!("198.51.100.{}", n % 250 + 1))
                .send()
                .await
                .map(|r| r.status())
        });
    }
    let mut refused = false;
    while let Some(status) = reads.join_next().await {
        refused |= status?? == StatusCode::TOO_MANY_REQUESTS;
    }
    assert!(refused, "forty XFF-spun reads never hit the read budget");
    Ok(())
}

/// `X-Smon-Workspace` is a claim, not a key: a random workspace id under a real
/// session is the uniform "not a member" deny, not a cross-tenant window.
#[tokio::test]
async fn a_claimed_workspace_header_is_not_a_membership() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    // A real operator bearer — strongest credential there is — with a workspace
    // it cannot name: malformed id and well-formed-but-nonexistent both deny.
    for id in ["not-a-uuid", "00000000-0000-0000-0000-000000000000"] {
        let response = http
            .get(format!("{}/api/monitors", instance.base))
            .bearer_auth(instance.token)
            .header("x-smon-workspace", id)
            .send()
            .await?;
        assert!(
            matches!(
                response.status(),
                StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::UNAUTHORIZED
            ),
            "operator bearer + workspace '{id}' answered {}",
            response.status()
        );
    }
    Ok(())
}

/// Origin on a cookie-authed write must match Host — a cross-site Origin
/// carrying a real cookie is refused before the write can run.
#[tokio::test]
async fn a_cross_site_origin_cannot_spend_a_cookie() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    // Earn a real session (signup does not need Origin — the credential path is
    // the exception browsers can't abuse, since it sets rather than spends).
    let invite = http
        .post(format!("{}/api/invites", instance.base))
        .bearer_auth(instance.token)
        .json(&json!({}))
        .send()
        .await?;
    let code: serde_json::Value = invite.json().await?;
    let signup = http
        .post(format!("{}/api/signup", instance.base))
        .json(&json!({
            "email": "victim@tenant.example",
            "password": "a password that is long enough",
            "invite": code["invite"]["code"],
        }))
        .send()
        .await?;
    assert_eq!(signup.status(), StatusCode::CREATED);
    // Signup makes no session — the cookie is earned at /api/login.
    let login = http
        .post(format!("{}/api/login", instance.base))
        .json(&json!({"email": "victim@tenant.example", "password": "a password that is long enough"}))
        .send()
        .await?;
    assert_eq!(login.status(), StatusCode::NO_CONTENT, "{}", login.status());
    let cookie = login
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("smon_session="))
        .and_then(|line| line.split(';').next())
        .ok_or_else(|| anyhow::anyhow!("login set no session cookie"))?
        .to_owned();

    // Now spend it with a cross-site Origin — the write must not happen.
    let response = http
        .patch(format!("{}/api/workspace", instance.base))
        .header("cookie", &cookie)
        .header("origin", "https://evil.example")
        .json(&json!({"name": "renamed-by-an-attacker"}))
        .send()
        .await?;
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "a cross-site Origin on a cookie write must be refused"
    );
    // Stored-state re-read: the refused write must not have landed — the
    // workspace name stays what the tenant's own signup chose, not the attacker's.
    let workspaces = http
        .get(format!("{}/api/workspaces", instance.base))
        .header("cookie", &cookie)
        .header("origin", &instance.base)
        .send()
        .await?;
    let body: serde_json::Value = workspaces.json().await?;
    assert_ne!(
        body["workspaces"][0]["name"].as_str(),
        Some("renamed-by-an-attacker"),
        "the cross-site write reached the store: {body}"
    );
    Ok(())
}
