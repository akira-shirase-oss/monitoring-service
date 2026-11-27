//! Account lifecycle under attack, against a running instance: a session in hand
//! but not the password, a second session that should have died, an account
//! deletion that reaches for a neighbour's rows, and an export asked for by
//! someone who is not the owner. Every refusal is asserted by its status and code,
//! and then by re-reading the state it was supposed to protect.
mod common;

use common::{client, spawn};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};

const PASSWORD: &str = "a password that is long enough";
const NEW_PASSWORD: &str = "another password, also long enough";

/// `POST /api/login`: the status, and the session cookie pair when one was set.
async fn login(
    instance: &common::Instance,
    email: &str,
    password: &str,
) -> anyhow::Result<(StatusCode, Option<String>)> {
    let response = client()?
        .post(format!("{}/api/login", instance.base))
        .json(&json!({"email": email, "password": password}))
        .send()
        .await?;
    let cookie = response
        .headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("__Host-smon_session="))
        .and_then(|v| v.split(';').next())
        .map(str::to_owned);
    Ok((response.status(), cookie))
}

/// Mint an operator invite, sign the account up, sign it in: the real flow, over
/// HTTP. Returns the session cookie pair.
async fn sign_up(instance: &common::Instance, email: &str) -> anyhow::Result<String> {
    let http = client()?;
    let invite: Value = http
        .post(format!("{}/api/invites", instance.base))
        .bearer_auth(instance.token)
        .json(&json!({}))
        .send()
        .await?
        .json()
        .await?;
    let code = invite["invite"]["code"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("invite mint carried no code: {invite}"))?;
    let response = http
        .post(format!("{}/api/signup", instance.base))
        .json(&json!({"email": email, "password": PASSWORD, "invite": code}))
        .send()
        .await?;
    anyhow::ensure!(
        response.status() == StatusCode::CREATED,
        "signup for {email}"
    );
    let (status, cookie) = login(instance, email, PASSWORD).await?;
    anyhow::ensure!(status == StatusCode::NO_CONTENT, "login for {email}");
    cookie.ok_or_else(|| anyhow::anyhow!("login set no session cookie"))
}

/// One cookie-authed request the way a browser on this origin sends it. The body
/// comes back as JSON, or `null` when there is none.
async fn ask(
    instance: &common::Instance,
    cookie: &str,
    method: Method,
    path: &str,
    workspace: Option<&str>,
    body: Option<Value>,
) -> anyhow::Result<(StatusCode, Value)> {
    let mut request = client()?
        .request(method, format!("{}{}", instance.base, path))
        .header("cookie", cookie)
        .header("origin", &instance.base);
    if let Some(workspace) = workspace {
        request = request.header("x-smon-workspace", workspace);
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await?;
    let status = response.status();
    let text = response.text().await?;
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

/// The account's own workspace id, from its memberships.
async fn workspace_of(instance: &common::Instance, cookie: &str) -> anyhow::Result<String> {
    let (status, body) = ask(instance, cookie, Method::GET, "/api/workspaces", None, None).await?;
    anyhow::ensure!(status == StatusCode::OK, "workspaces: {body}");
    body["current"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("no current workspace: {body}"))
}

/// A session is not the password. Whoever holds a signed-in browser cannot set a
/// new password without the current one, and the stored credential is unchanged.
#[tokio::test]
async fn a_wrong_password_does_not_change_the_password() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let cookie = sign_up(&instance, "alice@a.example").await?;
    let (status, body) = ask(
        &instance,
        &cookie,
        Method::POST,
        "/api/account/password",
        None,
        Some(json!({"current_password": "not the password at all", "new_password": NEW_PASSWORD})),
    )
    .await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["code"], "unauthenticated", "{body}");
    // Stored-state re-read: the new password opens nothing, the old one still does.
    let (status, _) = login(&instance, "alice@a.example", NEW_PASSWORD).await?;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the password changed without the current one"
    );
    let (status, _) = login(&instance, "alice@a.example", PASSWORD).await?;
    assert_eq!(status, StatusCode::NO_CONTENT);
    Ok(())
}

/// The same for deletion: a session without the password deletes nothing.
#[tokio::test]
async fn a_wrong_password_does_not_delete_the_account() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let cookie = sign_up(&instance, "alice@a.example").await?;
    let (status, body) = ask(
        &instance,
        &cookie,
        Method::DELETE,
        "/api/account",
        None,
        Some(json!({"password": "not the password at all"})),
    )
    .await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(body["error"]["code"], "unauthenticated", "{body}");
    // Stored-state re-read: the session still answers and the account still signs in.
    let (status, body) = ask(
        &instance,
        &cookie,
        Method::GET,
        "/api/workspaces",
        None,
        None,
    )
    .await?;
    assert_eq!(
        status,
        StatusCode::OK,
        "the account was deleted without its password: {body}"
    );
    let (status, _) = login(&instance, "alice@a.example", PASSWORD).await?;
    assert_eq!(status, StatusCode::NO_CONTENT);
    Ok(())
}

/// Changing a password is how a person throws out whoever else is signed in. Every
/// other session of the account must stop answering; the one that made the change
/// keeps working.
#[tokio::test]
async fn a_password_change_ends_every_other_session() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let mine = sign_up(&instance, "alice@a.example").await?;
    let (status, theirs) = login(&instance, "alice@a.example", PASSWORD).await?;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let theirs = theirs.ok_or_else(|| anyhow::anyhow!("the second login set no cookie"))?;
    let (status, _) = ask(
        &instance,
        &theirs,
        Method::GET,
        "/api/workspaces",
        None,
        None,
    )
    .await?;
    assert_eq!(
        status,
        StatusCode::OK,
        "the second session works before the change"
    );

    let (status, body) = ask(
        &instance,
        &mine,
        Method::POST,
        "/api/account/password",
        None,
        Some(json!({"current_password": PASSWORD, "new_password": NEW_PASSWORD})),
    )
    .await?;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    let (status, body) = ask(
        &instance,
        &theirs,
        Method::GET,
        "/api/workspaces",
        None,
        None,
    )
    .await?;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "another session outlived the password change: {body}"
    );
    let (status, _) = ask(&instance, &mine, Method::GET, "/api/workspaces", None, None).await?;
    assert_eq!(
        status,
        StatusCode::OK,
        "the session that made the change stays"
    );
    let (status, _) = login(&instance, "alice@a.example", PASSWORD).await?;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the old password still signs in"
    );
    let (status, _) = login(&instance, "alice@a.example", NEW_PASSWORD).await?;
    assert_eq!(status, StatusCode::NO_CONTENT);
    Ok(())
}

/// An account deletion removes that account's own workspace and nothing of anyone
/// else's: a neighbour's monitor is still there, and still theirs, afterwards.
#[tokio::test]
async fn deleting_an_account_takes_only_its_own_workspace() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let alice = sign_up(&instance, "alice@a.example").await?;
    let bob = sign_up(&instance, "bob@b.example").await?;
    for (cookie, name) in [(&alice, "alice-mon"), (&bob, "bob-mon")] {
        let (status, body) = ask(
            &instance,
            cookie,
            Method::POST,
            "/api/monitors",
            None,
            Some(json!({"name": name, "url": "http://93.184.216.34/", "interval_s": 60})),
        )
        .await?;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    let (status, body) = ask(
        &instance,
        &alice,
        Method::DELETE,
        "/api/account",
        None,
        Some(json!({"password": PASSWORD})),
    )
    .await?;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");

    // The deleted account is gone for good: its session and its password both.
    let (status, _) = ask(
        &instance,
        &alice,
        Method::GET,
        "/api/workspaces",
        None,
        None,
    )
    .await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = login(&instance, "alice@a.example", PASSWORD).await?;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // The neighbour's workspace is whole.
    let (status, body) = ask(&instance, &bob, Method::GET, "/api/monitors", None, None).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let names: Vec<&str> = body["monitors"]
        .as_array()
        .map(|all| all.iter().filter_map(|m| m["name"].as_str()).collect())
        .unwrap_or_default();
    assert_eq!(
        names,
        ["bob-mon"],
        "another account's deletion changed this workspace: {body}"
    );
    Ok(())
}

/// A workspace's export is its owner's. A member of the same workspace, who can
/// read every monitor in it, is still refused the file.
#[tokio::test]
async fn an_export_is_the_owners_only() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let owner = sign_up(&instance, "owner@a.example").await?;
    let member = sign_up(&instance, "member@b.example").await?;
    let workspace = workspace_of(&instance, &owner).await?;

    // The member is seated the real way: invited by the owner, accepted by themselves.
    let (status, body) = ask(
        &instance,
        &owner,
        Method::POST,
        "/api/members",
        None,
        Some(json!({"email": "member@b.example", "role": "member"})),
    )
    .await?;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let (_, pending) = ask(
        &instance,
        &member,
        Method::GET,
        "/api/invitations",
        None,
        None,
    )
    .await?;
    let invitation = pending["invitations"][0]["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no invitation is pending: {pending}"))?;
    let accept = format!("/api/invitations/{invitation}/accept");
    let (status, body) = ask(
        &instance,
        &member,
        Method::POST,
        &accept,
        None,
        Some(json!({})),
    )
    .await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Seated: the member reads the workspace's monitors.
    let seat = Some(workspace.as_str());
    let (status, _) = ask(&instance, &member, Method::GET, "/api/monitors", seat, None).await?;
    assert_eq!(status, StatusCode::OK, "the member is seated");

    let export = "/api/workspace/export";
    let (status, body) = ask(&instance, &member, Method::GET, export, seat, None).await?;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a member was handed the workspace's export: {body}"
    );
    assert_eq!(body["error"]["code"], "forbidden", "{body}");

    let (status, body) = ask(&instance, &owner, Method::GET, export, None, None).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["workspace"]["id"], workspace.as_str(), "{body}");
    Ok(())
}
