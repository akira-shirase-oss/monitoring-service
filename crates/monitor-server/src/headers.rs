//! Browser-facing response headers, on every response the server sends (dashboard pages,
//! the SPA fallback, the API, the public snapshot).
//!
//! - `Content-Security-Policy`: scripts only from this origin plus the dashboard build's
//!   own inline bootstrap, allowed by hash ([`dashboard_csp`] reads the build at boot) —
//!   never `'unsafe-inline'`; no plugins, no `<base>`, forms only to this origin, and no
//!   framing (`frame-ancestors 'none'`). Styles keep `'unsafe-inline'`: the build sets
//!   inline `style` attributes, and a style can't run code.
//! - `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY` (older browsers),
//!   `Referrer-Policy: no-referrer`.

use std::collections::BTreeSet;
use std::path::Path;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderValue, header};
use axum::response::Response;
use sha2::{Digest, Sha256};

/// How deep and how much of the dashboard build is read for inline scripts — a build is a
/// handful of HTML files; anything past this is not a build.
const MAX_DEPTH: usize = 4;
const MAX_HTML_FILES: usize = 256;
const MAX_HTML_BYTES: u64 = 2 * 1024 * 1024;

/// The policy for a dashboard build at `dir` (a missing or unreadable build gives the
/// policy without inline hashes: nothing inline runs). Blocking file I/O — run it on a
/// blocking thread, once, at boot.
pub fn dashboard_csp(dir: &Path) -> String {
    let mut hashes = BTreeSet::new();
    let mut files = 0;
    collect_inline_hashes(dir, 0, &mut files, &mut hashes);
    let inline = hashes.iter().fold(String::new(), |mut out, hash| {
        out.push_str(" '");
        out.push_str(hash);
        out.push('\'');
        out
    });
    format!(
        "default-src 'self'; script-src 'self'{inline}; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data:; font-src 'self' data:; connect-src 'self'; object-src 'none'; \
         base-uri 'none'; form-action 'self'; frame-ancestors 'none'"
    )
}

fn collect_inline_hashes(
    dir: &Path,
    depth: usize,
    files: &mut usize,
    hashes: &mut BTreeSet<String>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() && depth < MAX_DEPTH {
            collect_inline_hashes(&path, depth + 1, files, hashes);
        } else if kind.is_file()
            && path.extension().is_some_and(|ext| ext == "html")
            && *files < MAX_HTML_FILES
            && entry
                .metadata()
                .is_ok_and(|meta| meta.len() <= MAX_HTML_BYTES)
        {
            *files += 1;
            if let Ok(page) = std::fs::read_to_string(&path) {
                hashes.extend(inline_scripts(&page).map(|script| {
                    format!("sha256-{}", base64(&Sha256::digest(script.as_bytes())))
                }));
            }
        }
    }
}

/// The text of every `<script>` element without a `src` — exactly the bytes a browser
/// hashes for a CSP source expression.
fn inline_scripts(page: &str) -> impl Iterator<Item = &str> {
    let mut rest = page;
    std::iter::from_fn(move || {
        loop {
            let open = rest.find("<script")?;
            let after = &rest[open + "<script".len()..];
            let tag_end = after.find('>')?;
            let attributes = &after[..tag_end];
            let body = &after[tag_end + 1..];
            let close = body.find("</script>")?;
            rest = &body[close + "</script>".len()..];
            let external = attributes
                .split_whitespace()
                .any(|attribute| attribute.starts_with("src="));
            if !external {
                return Some(&body[..close]);
            }
        }
    })
}

/// Standard, padded base64 (RFC 4648) — all a CSP hash needs, without a dependency.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let triple = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(triple[0]) << 16) | (u32::from(triple[1]) << 8) | u32::from(triple[2]);
        for (i, shift) in [18_u32, 12, 6, 0].into_iter().enumerate() {
            out.push(if i <= chunk.len() {
                char::from(ALPHABET[usize::try_from((n >> shift) & 63).unwrap_or(0)])
            } else {
                '='
            });
        }
    }
    out
}

/// Fallback when a computed policy can't be a header value: nothing inline runs.
const STRICT: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; \
     img-src 'self' data:; font-src 'self' data:; connect-src 'self'; object-src 'none'; \
     base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// Wrap the whole app (routes and fallback) so every response carries the headers.
pub fn harden(app: Router, csp: &str) -> Router {
    let csp = HeaderValue::from_str(csp).unwrap_or_else(|_| {
        tracing::warn!("computed CSP is not a valid header; using the strict policy");
        HeaderValue::from_static(STRICT)
    });
    app.layer(axum::middleware::map_response_with_state(csp, add_headers))
}

async fn add_headers(State(csp): State<HeaderValue>, mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_SECURITY_POLICY, csp);
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), expected);
        }
    }

    #[test]
    fn only_inline_scripts_are_hashed() {
        let page = "<script src=\"/a.js\"></script><script type=\"module\">one()</script>\
                    <p></p><script>\ntwo()\n</script><script async src=/b.js></script>";
        assert_eq!(
            inline_scripts(page).collect::<Vec<_>>(),
            ["one()", "\ntwo()\n"]
        );
        assert_eq!(inline_scripts("<script>unclosed").count(), 0);
    }

    #[test]
    fn a_missing_build_allows_no_inline_script() {
        let csp = dashboard_csp(Path::new("/nonexistent/dashboard"));
        assert!(csp.contains("script-src 'self';"), "{csp}");
        assert!(HeaderValue::from_str(&csp).is_ok());
        assert_eq!(csp, STRICT);
    }
}
