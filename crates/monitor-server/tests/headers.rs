//! Browser-facing hardening, against the real binary serving a dashboard build: every
//! response — dashboard pages, the SPA fallback, the API and the public snapshot — carries
//! a Content-Security-Policy that allows only this origin's scripts plus the build's own
//! inline bootstrap (by hash, never `'unsafe-inline'`), refuses framing, turns off MIME
//! sniffing and sends no referrer to other origins. And the public status page builds its rows as text, never
//! as HTML, under a CSP of its own.

use std::io::BufRead;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The fixture's inline bootstrap script, byte for byte, and its CSP hash (computed
/// independently: `base64(sha256(script))`).
const INLINE: &str = "\n\tconsole.log(\"dashboard boot\");\n";
const INLINE_HASH: &str = "'sha256-sq9pEW6nQrW2/6bxSy1DRge0xoCfmDb/jOgRjgwoPZk='";

/// A dashboard build with an inline bootstrap and an external module, like SvelteKit's.
fn dashboard(dir: &std::path::Path) -> std::io::Result<()> {
    let page = format!(
        "<!doctype html><html><head><script type=\"module\" src=\"/_app/start.js\"></script>\
         </head><body><div style=\"display: contents\"><script>{INLINE}</script></div></body></html>"
    );
    std::fs::create_dir_all(dir.join("_app"))?;
    std::fs::write(dir.join("_app/start.js"), "export {};")?;
    std::fs::write(dir.join("200.html"), &page)?;
    std::fs::write(dir.join("index.html"), &page)
}

struct Server(Child);

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Start the real server on an ephemeral port; return it with its bound address.
fn start(dir: &std::path::Path) -> anyhow::Result<(Server, String)> {
    let env = |command: &mut Command| {
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env(
                "SMON_DATABASE_URL",
                format!("sqlite:{}", dir.join("h.db").display()),
            )
            .env("SMON_LISTEN", "127.0.0.1:0")
            .env("SMON_STATUS_DIR", dir.join("status"))
            .env("SMON_STATUS_INTERVAL_S", "1")
            .env("SMON_DASHBOARD_DIR", dir.join("dashboard"));
    };
    let mut init = Command::new(env!("CARGO_BIN_EXE_monitor-server"));
    env(&mut init);
    anyhow::ensure!(init.arg("--init").output()?.status.success(), "--init");

    let mut command = Command::new(env!("CARGO_BIN_EXE_monitor-server"));
    env(&mut command);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("stdout"))?;
    let server = Server(child);
    let deadline = Instant::now() + Duration::from_secs(30);
    for line in std::io::BufReader::new(stdout).lines() {
        let line = line?;
        if line.contains("monitor-server listening") {
            let log: serde_json::Value = serde_json::from_str(&line)?;
            let addr = log["fields"]["addr"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("no addr in {line}"))?
                .to_owned();
            return Ok((server, addr));
        }
        anyhow::ensure!(Instant::now() < deadline, "server never listened");
    }
    anyhow::bail!("server exited before listening")
}

#[tokio::test]
async fn every_response_carries_the_csp_and_hardening_headers() -> anyhow::Result<()> {
    let tmp = tempfile::tempdir()?;
    dashboard(&tmp.path().join("dashboard"))?;
    let (_server, addr) = tokio::task::spawn_blocking({
        let dir = tmp.path().to_path_buf();
        move || start(&dir)
    })
    .await??;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()?;
    // The snapshot appears within a beat or two.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !tmp.path().join("status/status.json").exists() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    for path in [
        "/",
        "/index.html",
        "/some/client/route",
        "/api/monitors",
        "/livez",
        "/status.json",
    ] {
        let response = client.get(format!("http://{addr}{path}")).send().await?;
        assert!(
            response.status().is_success(),
            "{path}: {}",
            response.status()
        );
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
                .unwrap_or_default()
        };
        let csp = header("content-security-policy");
        let directive = |name: &str| {
            csp.split(';')
                .map(str::trim)
                .find(|d| d.split_whitespace().next() == Some(name))
                .unwrap_or_default()
                .to_owned()
        };
        assert_eq!(
            directive("default-src"),
            "default-src 'self'",
            "{path}: {csp}"
        );
        let scripts = directive("script-src");
        assert!(
            scripts.starts_with("script-src 'self'") && scripts.contains(INLINE_HASH),
            "{path}: the build's inline bootstrap is allowed by hash: {csp}"
        );
        assert!(
            !scripts.contains("unsafe-inline") && !scripts.contains("unsafe-eval"),
            "{path}: {csp}"
        );
        for fixed in [
            "object-src 'none'",
            "base-uri 'none'",
            "frame-ancestors 'none'",
            "form-action 'self'",
        ] {
            assert_eq!(
                directive(fixed.split(' ').next().unwrap_or_default()),
                fixed,
                "{path}"
            );
        }
        assert_eq!(header("x-content-type-options"), "nosniff", "{path}");
        // The static routes run under the same stack as the API (L7): ids, trace, timeout.
        assert!(
            !header("x-request-id").is_empty(),
            "{path}: no x-request-id"
        );
        assert_eq!(header("x-frame-options"), "DENY", "{path}");
        assert_eq!(header("referrer-policy"), "same-origin", "{path}");
    }
    Ok(())
}

/// Standard base64 (RFC 4648, padded) — the test's own, so the status page's pinned hash
/// is checked independently of any server code.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[test]
fn base64_matches_rfc_4648_vectors() {
    for (input, expected) in [
        ("", ""),
        ("f", "Zg=="),
        ("fo", "Zm8="),
        ("foo", "Zm9v"),
        ("foobar", "Zm9vYmFy"),
    ] {
        assert_eq!(base64(input.as_bytes()), expected);
    }
}

/// The public status page renders snapshot fields — aliases come from operators — so it
/// builds rows with `textContent`, never `innerHTML`, and pins its one inline script by
/// hash in its own CSP (it is hosted on any static host, without our headers).
#[test]
fn the_status_page_renders_text_and_pins_its_script() {
    use sha2::{Digest, Sha256};
    let page = include_str!("../../../apps/status/index.html");
    assert!(
        !page.contains("innerHTML") && !page.contains("insertAdjacentHTML"),
        "the status page must not build HTML from snapshot text"
    );
    let open = page.find("<script>").expect("one inline script") + "<script>".len();
    let close = open + page[open..].find("</script>").expect("closed");
    let hash = format!(
        "'sha256-{}'",
        base64(&Sha256::digest(&page.as_bytes()[open..close]))
    );
    let meta = page
        .lines()
        .find(|line| line.contains("http-equiv=\"Content-Security-Policy\""))
        .expect("the page declares its own CSP");
    assert!(
        meta.contains(&format!("script-src {hash}")),
        "the CSP pins the current script ({hash}): {meta}"
    );
    for fixed in [
        "default-src 'none'",
        "connect-src 'self'",
        "base-uri 'none'",
    ] {
        assert!(meta.contains(fixed), "{fixed}: {meta}");
    }
}
