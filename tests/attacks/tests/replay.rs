//! Replay against a running instance: a copied session cookie after logout, a
//! used invite code a second time, a stale-generation PATCH echoed back, and a
//! revoked workspace token. Every replayed credential or write must refuse —
//! nothing that worked once may keep working after it was retired.
mod common;

use common::{client, spawn};
use reqwest::StatusCode;
use serde_json::json;

/// A real account: mint invite → signup → session cookie.
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
        .find(|v| v.starts_with("__Host-smon_session="))
        .expect("login sets a session cookie")
        .split(';')
        .next()
        .expect("cookie pair")
        .to_owned())
}

/// The stolen-cookie case: a cookie captured before logout must be dead after it.
#[tokio::test]
async fn a_logged_out_session_cookie_replays_to_401() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    let cookie = account(&instance, "victim@tenant.example").await?;

    // Sanity: the live cookie works once (the replay premise).
    let live = http
        .get(format!("{}/api/monitors", instance.base))
        .header("cookie", &cookie)
        .header("origin", &instance.base)
        .send()
        .await?;
    assert_eq!(live.status(), StatusCode::OK);

    // The victim logs out; an attacker replaying the captured cookie gets 401.
    let out = http
        .post(format!("{}/api/logout", instance.base))
        .header("cookie", &cookie)
        .header("origin", &instance.base)
        .send()
        .await?;
    assert!(out.status().is_success(), "logout: {}", out.status());
    for path in ["/api/monitors", "/api/workspaces"] {
        let replayed = http
            .get(format!("{}{}", instance.base, path))
            .header("cookie", &cookie)
            .header("origin", &instance.base)
            .send()
            .await?;
        assert_eq!(
            replayed.status(),
            StatusCode::UNAUTHORIZED,
            "a logged-out cookie replayed against {path}"
        );
    }
    // A dead cookie must be dead for writes too — replay it against a mutation.
    let write = http
        .patch(format!("{}/api/workspace", instance.base))
        .header("cookie", &cookie)
        .header("origin", &instance.base)
        .json(&json!({"name": "replayed"}))
        .send()
        .await?;
    assert_eq!(
        write.status(),
        StatusCode::UNAUTHORIZED,
        "a logged-out cookie replayed against a write"
    );
    Ok(())
}

/// An invite code is single-use: the same code a second time is refused, even
/// for a different email.
#[tokio::test]
async fn a_used_invite_code_does_not_replay() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    let invite = http
        .post(format!("{}/api/invites", instance.base))
        .bearer_auth(instance.token)
        .json(&json!({}))
        .send()
        .await?;
    let body: serde_json::Value = invite.json().await?;
    let code = body["invite"]["code"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no invite code: {body}"))?
        .to_owned();

    let first = http
        .post(format!("{}/api/signup", instance.base))
        .json(&json!({
            "email": "first@tenant.example",
            "password": "a password that is long enough",
            "invite": code,
        }))
        .send()
        .await?;
    // Setup errors propagate — only the protection's own asserts may panic.
    anyhow::ensure!(
        first.status() == StatusCode::CREATED,
        "setup: first use must succeed, got {}",
        first.status()
    );

    let replay = http
        .post(format!("{}/api/signup", instance.base))
        .json(&json!({
            "email": "second@tenant.example",
            "password": "a password that is long enough",
            "invite": code,
        }))
        .send()
        .await?;
    assert!(
        matches!(
            replay.status(),
            StatusCode::FORBIDDEN
                | StatusCode::UNAUTHORIZED
                | StatusCode::CONFLICT
                | StatusCode::GONE
                | StatusCode::UNPROCESSABLE_ENTITY
        ),
        "a used invite replayed answered {}",
        replay.status()
    );
    // Stored-state re-read: the refusal must mean no account — the replayed
    // signup's credentials must not open a session.
    let login = http
        .post(format!("{}/api/login", instance.base))
        .json(&json!({"email": "second@tenant.example", "password": "a password that is long enough"}))
        .send()
        .await?;
    assert_eq!(
        login.status(),
        StatusCode::UNAUTHORIZED,
        "the replayed signup left a working account behind"
    );
    Ok(())
}

/// Optimistic-concurrency replay: a PATCH carrying a stale generation is a
/// conflict, not a silent overwrite of the newer write.
#[tokio::test]
async fn a_stale_generation_patch_is_a_conflict() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    let cookie = account(&instance, "editor@tenant.example").await?;
    let created = http
        .post(format!("{}/api/monitors", instance.base))
        .header("cookie", &cookie)
        .header("origin", &instance.base)
        .json(&json!({"name": "gen", "url": "http://93.184.216.34/", "interval_s": 60}))
        .send()
        .await?;
    assert_eq!(created.status(), StatusCode::CREATED);
    let body: serde_json::Value = created.json().await?;
    let id = body["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("create carried no id: {body}"))?
        .to_owned();
    // The create answer is just the id — the generation is read back off the view.
    let view: serde_json::Value = http
        .get(format!("{}/api/monitors/{id}", instance.base))
        .header("cookie", &cookie)
        .header("origin", &instance.base)
        .send()
        .await?
        .json()
        .await?;
    let generation = view["generation"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("view carried no generation: {view}"))?;

    // Write 1 at generation g succeeds; replaying the SAME generation after it
    // must conflict — the stale write can't shadow the newer one. The replay
    // carries a different name so a silent overwrite would be visible.
    let patch = |g: serde_json::Value, name: &str| {
        http.patch(format!("{}/api/monitors/{id}", instance.base))
            .header("cookie", &cookie)
            .header("origin", &instance.base)
            .json(&json!({"name": name, "generation": g}))
    };
    let first = patch(serde_json::json!(generation), "first-write")
        .send()
        .await?;
    // Setup errors propagate — only the protection's own asserts may panic.
    anyhow::ensure!(
        first.status().is_success(),
        "setup: the live generation patches fine, got {}",
        first.status()
    );
    let replayed = patch(serde_json::json!(generation), "shadow-write")
        .send()
        .await?;
    assert_eq!(
        replayed.status(),
        StatusCode::CONFLICT,
        "the same generation twice must conflict, got {}",
        replayed.status()
    );
    // Stored-state re-read: the conflict must be a refusal, not an applied write —
    // the store still shows the first write's name.
    let view: serde_json::Value = http
        .get(format!("{}/api/monitors/{id}", instance.base))
        .header("cookie", &cookie)
        .header("origin", &instance.base)
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(
        view["name"].as_str(),
        Some("first-write"),
        "the stale write reached the store: {view}"
    );
    Ok(())
}

/// A revoked workspace API token is dead immediately: use → revoke → use again.
#[tokio::test]
async fn a_revoked_workspace_token_stops_working() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    let cookie = account(&instance, "tokener@tenant.example").await?;

    // Mint a workspace API token (owner work under the session — a fresh account
    // owns the workspace signup made).
    let minted = http
        .post(format!("{}/api/tokens", instance.base))
        .header("cookie", &cookie)
        .header("origin", &instance.base)
        .json(&json!({"name": "ci", "role": "member"}))
        .send()
        .await?;
    assert_eq!(minted.status(), StatusCode::CREATED, "{}", minted.status());
    let body: serde_json::Value = minted.json().await?;
    let id = body["token"]["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("mint carried no token id: {body}"))?
        .to_owned();
    let token = body["token"]["secret"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("mint carried no once-shown secret: {body}"))?
        .to_owned();

    // Live once, then revoke.
    let live = http
        .get(format!("{}/api/monitors", instance.base))
        .bearer_auth(&token)
        .send()
        .await?;
    assert_eq!(live.status(), StatusCode::OK);
    let revoked = http
        .delete(format!("{}/api/tokens/{id}", instance.base))
        .header("cookie", &cookie)
        .header("origin", &instance.base)
        .send()
        .await?;
    assert!(
        revoked.status().is_success(),
        "revoke: {}",
        revoked.status()
    );

    // The replayed Bearer is dead.
    let replayed = http
        .get(format!("{}/api/monitors", instance.base))
        .bearer_auth(&token)
        .send()
        .await?;
    assert_eq!(
        replayed.status(),
        StatusCode::UNAUTHORIZED,
        "a revoked token must not replay"
    );
    // Stored-state re-read: revocation is a stamped row kept as history, not a
    // delete — the token still lists, now carrying its revoked_at.
    let list = http
        .get(format!("{}/api/tokens", instance.base))
        .header("cookie", &cookie)
        .header("origin", &instance.base)
        .send()
        .await?;
    let body: serde_json::Value = list.json().await?;
    let stamped = body["tokens"].as_array().and_then(|ts| {
        ts.iter()
            .find(|t| t["id"].as_str() == Some(id.as_str()))
            .map(|t| !t["revoked_at"].is_null())
    });
    assert_eq!(
        stamped,
        Some(true),
        "the revoked stamp must be on the row that survives as history: {body}"
    );
    Ok(())
}
