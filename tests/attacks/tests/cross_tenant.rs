//! Cross-tenant attempts against a running instance: account B, credential in
//! hand, names account A's workspace explicitly and tries to read or write A's
//! resources. Every answer must be the uniform "not a member" deny — never a
//! 404-vs-403 oracle, never a leaked row.
mod common;

use common::{client, spawn};
use reqwest::StatusCode;
use serde_json::json;

/// Mint an operator invite, sign the account up, and return its session cookie —
/// the real flow, over HTTP.
#[expect(
    clippy::expect_used,
    reason = "test helper: a signup or sign-in that fails is a failed test"
)]
async fn sign_up(instance: &common::Instance, email: &str) -> anyhow::Result<String> {
    let http = client()?;
    let invite = http
        .post(format!("{}/api/invites", instance.base))
        .bearer_auth(instance.token)
        .json(&json!({}))
        .send()
        .await?;
    assert_eq!(invite.status(), StatusCode::CREATED);
    let body: serde_json::Value = invite.json().await?;
    let code = body["invite"]["code"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("invite mint carried no code: {body}"))?;

    let response = http
        .post(format!("{}/api/signup", instance.base))
        .json(
            &json!({"email": email, "password": "a password that is long enough", "invite": code}),
        )
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::CREATED, "signup for {email}");
    // Signup makes no session — the cookie is earned at /api/login.
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
        .find(|v| v.starts_with("smon_session="))
        .ok_or_else(|| anyhow::anyhow!("login set no session cookie"))?;
    Ok(cookie
        .split(';')
        .next()
        .expect("a cookie line always has a first pair")
        .to_owned())
}

/// The account's default workspace id — from its own memberships list.
async fn workspace_of(instance: &common::Instance, cookie: &str) -> anyhow::Result<String> {
    let response = client()?
        .get(format!("{}/api/workspaces", instance.base))
        .header("cookie", cookie)
        .header("origin", &instance.base)
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value = response.json().await?;
    body["workspaces"]
        .as_array()
        .and_then(|ws| ws.first())
        .and_then(|w| w["id"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("no workspace in memberships: {body}"))
}

/// A monitor under `cookie`'s workspace — returns its id. The target is a public
/// IP literal (the strict profile judges literals without a lookup; the suite
/// runs with no DNS at all).
async fn make_monitor(
    instance: &common::Instance,
    cookie: &str,
    workspace: &str,
    name: &str,
) -> anyhow::Result<String> {
    let response = client()?
        .post(format!("{}/api/monitors", instance.base))
        .header("cookie", cookie)
        .header("origin", &instance.base)
        .header("x-smon-workspace", workspace)
        .json(&json!({"name": name, "url": "http://93.184.216.34/", "interval_s": 60}))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::CREATED);
    let body: serde_json::Value = response.json().await?;
    body["id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("created monitor carried no id: {body}"))
}

/// B names A's workspace on every surface — the deny is the uniform member deny
/// and no row of A's leaks.
#[tokio::test]
async fn a_foreign_workspace_header_unlocks_nothing() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    let a_cookie = sign_up(&instance, "alice@a.example").await?;
    let b_cookie = sign_up(&instance, "bob@b.example").await?;
    let a_ws = workspace_of(&instance, &a_cookie).await?;
    let a_monitor = make_monitor(&instance, &a_cookie, &a_ws, "alice-mon").await?;

    for (method, path) in [
        (reqwest::Method::GET, "/api/monitors".to_owned()),
        (reqwest::Method::GET, format!("/api/monitors/{a_monitor}")),
        (reqwest::Method::PATCH, format!("/api/monitors/{a_monitor}")),
        (
            reqwest::Method::DELETE,
            format!("/api/monitors/{a_monitor}"),
        ),
        (
            reqwest::Method::GET,
            format!("/api/monitors/{a_monitor}/checks"),
        ),
        (reqwest::Method::GET, "/api/members".to_owned()),
        (reqwest::Method::GET, "/api/tokens".to_owned()),
        (reqwest::Method::POST, "/api/tokens".to_owned()),
        (
            reqwest::Method::GET,
            "/api/workspace/status-page".to_owned(),
        ),
        (
            reqwest::Method::PUT,
            "/api/workspace/status-page".to_owned(),
        ),
        (reqwest::Method::PUT, "/api/workspace/webhook".to_owned()),
        (reqwest::Method::GET, "/api/incidents".to_owned()),
    ] {
        let response = http
            .request(method.clone(), format!("{}{}", instance.base, path))
            .header("cookie", &b_cookie)
            .header("origin", &instance.base)
            .header("x-smon-workspace", &a_ws)
            .json(&json!({}))
            .send()
            .await?;
        let status = response.status();
        assert!(
            status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND,
            "{method} {path} under a foreign workspace answered {status}"
        );
    }

    // Without the header B scopes to its own workspace — A's monitor is a plain
    // 404, the same answer a never-created id gets.
    let response = http
        .get(format!("{}/api/monitors/{a_monitor}", instance.base))
        .header("cookie", &b_cookie)
        .header("origin", &instance.base)
        .send()
        .await?;
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "a foreign id in B's own scope must be 404, not a hint it exists"
    );
    Ok(())
}

/// A monitor id is not an oracle: B asking for A's id and a random id get the
/// same answer, byte for byte.
#[tokio::test]
async fn foreign_and_nonexistent_ids_answer_identically() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    let a_cookie = sign_up(&instance, "alice@a.example").await?;
    let b_cookie = sign_up(&instance, "bob@b.example").await?;
    let a_ws = workspace_of(&instance, &a_cookie).await?;
    let a_monitor = make_monitor(&instance, &a_cookie, &a_ws, "alice-mon").await?;

    let foreign = http
        .get(format!("{}/api/monitors/{a_monitor}", instance.base))
        .header("cookie", &b_cookie)
        .header("origin", &instance.base)
        .send()
        .await?;
    let random = http
        .get(format!(
            "{}/api/monitors/00000000-0000-0000-0000-000000000000",
            instance.base
        ))
        .header("cookie", &b_cookie)
        .header("origin", &instance.base)
        .send()
        .await?;
    assert_eq!(foreign.status(), random.status());
    assert_eq!(
        foreign.text().await?,
        random.text().await?,
        "a foreign id and a never-created id must answer byte-identically"
    );
    Ok(())
}

/// An invitation is a seat only its invitee can take: B cannot accept the
/// invitation addressed to A's other address, and a non-member's invitation
/// reads stay their own.
#[tokio::test]
async fn a_seat_belongs_to_its_invitee_only() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    let a_cookie = sign_up(&instance, "alice@a.example").await?;
    let b_cookie = sign_up(&instance, "bob@b.example").await?;
    let a_ws = workspace_of(&instance, &a_cookie).await?;

    // A invites an address B does not control (S2-4: the invitee accepts the seat;
    // the owner's POST is a uniform response whether the email is known or not).
    let response = http
        .post(format!("{}/api/members", instance.base))
        .header("cookie", &a_cookie)
        .header("origin", &instance.base)
        .header("x-smon-workspace", &a_ws)
        .json(&json!({"email": "carol@a.example", "role": "member"}))
        .send()
        .await?;
    assert!(
        response.status().is_success(),
        "the invitation POST is a uniform answer: {}",
        response.status()
    );

    // B's own invitation list carries nothing of A's workspace — and B cannot
    // accept a seat addressed to carol@a.example (B has no such invitation; the
    // answer is a uniform deny, not a lookup of A's row).
    let list = http
        .get(format!("{}/api/invitations", instance.base))
        .header("cookie", &b_cookie)
        .header("origin", &instance.base)
        .send()
        .await?;
    assert_eq!(list.status(), StatusCode::OK);
    let body: serde_json::Value = list.json().await?;
    assert_eq!(
        body["invitations"].as_array().map(Vec::len),
        Some(0),
        "bob's invitation list must not show carol's seat: {body}"
    );
    // Accepting any id B could guess — carol's invitation id is never exposed to
    // B — answers the same deny as a made-up one.
    for id in [
        "00000000-0000-0000-0000-000000000000",
        "ffffffff-ffff-ffff-ffff-ffffffffffff",
    ] {
        let response = http
            .post(format!("{}/api/invitations/{id}/accept", instance.base))
            .header("cookie", &b_cookie)
            .header("origin", &instance.base)
            .send()
            .await?;
        assert!(
            matches!(
                response.status(),
                StatusCode::NOT_FOUND | StatusCode::FORBIDDEN | StatusCode::GONE
            ),
            "accepting a guessed invitation id answered {}",
            response.status()
        );
    }
    Ok(())
}
