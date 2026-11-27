//! Rate-limit evasion against a running instance: parallel connections, header
//! tricks, and guess-the-right-token-under-fire. The credential budget is five
//! guesses then 429 — before the credential is even compared — and an exhausted
//! client cannot learn whether a guess was right.
mod common;

use common::{client, spawn};
use reqwest::StatusCode;
use serde_json::json;

/// Fifty wrong guesses at once — however many connections the client opens, the
/// burst is five and the rest answer 429 with Retry-After. Then the *correct*
/// operator token from the same exhausted address is still refused: a throttled
/// address learns nothing about its guesses.
#[tokio::test]
async fn a_connection_flood_does_not_buy_more_guesses() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    let mut guesses = tokio::task::JoinSet::new();
    for n in 0..50_u32 {
        let http = http.clone();
        let base = instance.base.clone();
        guesses.spawn(async move {
            http.post(format!("{base}/api/login"))
                .json(&json!({"token": format!("guess-{n}")}))
                .send()
                .await
                .map(|r| (r.status(), r.headers().clone()))
        });
    }
    let (mut evaluated, mut refused) = (0u32, 0u32);
    while let Some(outcome) = guesses.join_next().await {
        let (status, headers) = outcome??;
        match status {
            StatusCode::UNAUTHORIZED => evaluated += 1,
            StatusCode::TOO_MANY_REQUESTS => {
                refused += 1;
                assert!(
                    headers.get("retry-after").is_some(),
                    "a 429 must say when to retry"
                );
            }
            other => anyhow::bail!("guess answered {other}"),
        }
    }
    assert!(
        (1..=10).contains(&evaluated),
        "fifty parallel guesses: {evaluated} reached the token compare (burst is 5)"
    );
    assert_eq!(evaluated + refused, 50);

    // The same address, the RIGHT token: still 429 — the bucket never tells a
    // flooded client which guess would have landed.
    let honest = http
        .post(format!("{}/api/login", instance.base))
        .json(&json!({"token": instance.token}))
        .send()
        .await?;
    assert_eq!(
        honest.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "an exhausted address must not learn its next guess is right"
    );
    Ok(())
}

/// Slow-and-steady guessing gets at most burst + refill — never a second wind:
/// sequential wrong guesses on a real clock can only evaluate what the tokens
/// paid for (elapsed-aware bound), and a concurrent tail clusters its admits
/// inside one refill period so a refusal is guaranteed on any machine. (The
/// exact 5-then-429 split is the paused-clock unit test's job; wall time here
/// would be the flake this suite exists to avoid.)
#[tokio::test]
async fn steady_guessing_earns_only_burst_plus_elapsed() -> anyhow::Result<()> {
    let instance = spawn().await?;
    let http = client()?;
    let started = std::time::Instant::now();
    let mut evaluated = 0u32;
    for n in 0..5 {
        let status = http
            .post(format!("{}/api/login", instance.base))
            .json(&json!({"token": format!("wrong-{n}")}))
            .send()
            .await?
            .status();
        match status {
            StatusCode::UNAUTHORIZED => evaluated += 1,
            StatusCode::TOO_MANY_REQUESTS => {}
            other => anyhow::bail!("guess {n} answered {other}"),
        }
    }
    // Six more at once: however much wall time the first five took, these admits
    // land inside one refill period — the bucket holds at most ~1 token.
    let mut tail = tokio::task::JoinSet::new();
    for n in 5..11_u32 {
        let http = http.clone();
        let base = instance.base.clone();
        tail.spawn(async move {
            http.post(format!("{base}/api/login"))
                .json(&json!({"token": format!("wrong-{n}")}))
                .send()
                .await
                .map(|r| r.status())
        });
    }
    let mut refused = 0u32;
    while let Some(outcome) = tail.join_next().await {
        match outcome?? {
            StatusCode::UNAUTHORIZED => evaluated += 1,
            StatusCode::TOO_MANY_REQUESTS => refused += 1,
            other => anyhow::bail!("tail guess answered {other}"),
        }
    }
    let refilled = started.elapsed().as_secs() / 2 + 1;
    assert!(
        refused > 0,
        "six clustered guesses all evaluated — the limiter must be gone"
    );
    assert!(
        u64::from(evaluated) <= 5 + refilled,
        "{evaluated} guesses evaluated — more than burst 5 + {refilled} refilled"
    );
    Ok(())
}
