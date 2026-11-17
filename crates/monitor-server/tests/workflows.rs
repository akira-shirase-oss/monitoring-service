//! Hosted CI is part of the supply chain: a workflow can leak a token, run untrusted code
//! with write access, or pull a moved tag. These rules hold for every file in
//! `.github/workflows/`, found by listing the directory — a new workflow can't skip them:
//!
//! - the token is read-only by default (`permissions: contents: read` at the top);
//! - every action is pinned to a full commit SHA, with the version in a comment;
//! - no `pull_request_target` / `workflow_run` triggers, no self-hosted runners;
//! - checkout never leaves credentials behind (`persist-credentials: false`);
//! - no `${{ … }}` expression inside a `run:` script (values reach scripts via `env:`).
//!
//! Plus Dependabot keeps cargo, npm and the action pins current.

use std::path::{Path, PathBuf};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Every rule `text` breaks, one line each.
fn lint(text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let lines: Vec<&str> = text.lines().collect();

    // Top-level permissions: present, and exactly `contents: read` beneath it.
    match lines
        .iter()
        .position(|line| line.starts_with("permissions:"))
    {
        None => problems.push("no top-level `permissions:`".to_owned()),
        Some(at) => {
            let block: Vec<&str> = lines[at + 1..]
                .iter()
                .take_while(|line| line.is_empty() || line.starts_with(' '))
                .map(|line| line.trim())
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .collect();
            if block != ["contents: read"] {
                problems.push(format!(
                    "top-level permissions must be `contents: read`, got {block:?}"
                ));
            }
        }
    }

    let mut run_block: Option<usize> = None; // indent of an open `run: |` key
    let mut checkout: Option<(usize, usize)> = None; // (line, indent of its list item)
    let checkout_ends = |at: usize, end: usize, problems: &mut Vec<String>| {
        if !lines[at..end]
            .iter()
            .any(|l| l.trim() == "persist-credentials: false")
        {
            problems.push(format!(
                "line {}: checkout without `persist-credentials: false`",
                at + 1
            ));
        }
    };
    for (n, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        // Inside a block script every line counts — Actions expands `${{ }}` even in a
        // shell comment.
        if let Some(key_indent) = run_block {
            if indent(raw) > key_indent {
                if line.contains("${{") {
                    problems.push(format!("line {}: `${{{{` inside a run script", n + 1));
                }
                continue;
            }
            run_block = None;
        }
        if line.starts_with('#') {
            continue;
        }
        if let Some((at, item_indent)) = checkout
            && indent(raw) <= item_indent
        {
            checkout_ends(at, n, &mut problems);
            checkout = None;
        }
        let code = line.split(" #").next().unwrap_or(line);
        for trigger in ["pull_request_target", "workflow_run"] {
            if code
                .split([':', ' ', ',', '[', ']'])
                .any(|word| word == trigger)
            {
                problems.push(format!("line {}: dangerous trigger `{trigger}`", n + 1));
            }
        }
        if code.contains("write-all") {
            problems.push(format!("line {}: `write-all` permissions", n + 1));
        }
        if code.starts_with("runs-on:") && code.contains("self-hosted") {
            problems.push(format!("line {}: self-hosted runner", n + 1));
        }
        let item = code.starts_with("- ");
        let key = code.trim_start_matches("- ");
        // The indent of the key itself (a `- ` item marker adds two columns).
        let key_indent = indent(raw) + if item { 2 } else { 0 };
        if let Some(target) = key.strip_prefix("uses:") {
            let target = target.trim().trim_matches(['\'', '"']);
            check_pin(target, line, n, &mut problems);
            if target.starts_with("actions/checkout@") {
                checkout = Some((n, key_indent.saturating_sub(2)));
            }
        }
        if let Some(script) = key.strip_prefix("run:") {
            let script = script.trim();
            if script.starts_with('|') || script.starts_with('>') {
                run_block = Some(key_indent);
            } else if script.contains("${{") {
                problems.push(format!("line {}: `${{{{` inside a run script", n + 1));
            }
        }
    }
    if let Some((at, _)) = checkout {
        checkout_ends(at, lines.len(), &mut problems);
    }
    problems
}

/// `uses:` must name a local action or a full 40-hex commit, with the version in a comment.
fn check_pin(target: &str, line: &str, n: usize, problems: &mut Vec<String>) {
    let local = target.starts_with("./");
    let pinned = local
        || target
            .split_once('@')
            .is_some_and(|(_, rev)| rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit()));
    if !pinned {
        problems.push(format!(
            "line {}: `{target}` is not pinned to a full commit SHA",
            n + 1
        ));
    } else if !local && !line.contains("# v") {
        problems.push(format!(
            "line {}: pinned `{target}` needs a `# vX.Y.Z` comment",
            n + 1
        ));
    }
}

fn workflows() -> Vec<(String, String)> {
    let dir = repo().join(".github/workflows");
    let mut found: Vec<(String, String)> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.extension()
                        .is_some_and(|ext| ext == "yml" || ext == "yaml")
                })
                .filter_map(|path| {
                    let text = std::fs::read_to_string(&path).ok()?;
                    Some((path.file_name()?.to_string_lossy().into_owned(), text))
                })
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}

#[test]
fn every_workflow_is_read_only_pinned_and_injection_free() {
    let found = workflows();
    assert!(
        found.iter().any(|(name, _)| name == "ci.yml"),
        "hosted CI must exist: .github/workflows/ci.yml"
    );
    for (name, text) in &found {
        let problems = lint(text);
        assert!(problems.is_empty(), "{name}:\n{}", problems.join("\n"));
    }
}

/// CI runs the same gate as a developer, on every push and pull request, and nothing in it
/// depends on the port an installed monitor listens on.
#[test]
fn ci_runs_the_gate_on_push_and_pull_request() {
    let text = std::fs::read_to_string(repo().join(".github/workflows/ci.yml"))
        .expect("read .github/workflows/ci.yml");
    for needle in [
        "push:",
        "pull_request:",
        "schedule:",
        "scripts/check.sh",
        "npm ci",
        "npm audit --omit=dev --audit-level=moderate",
        "cargo-deny",
        "runs-on: ubuntu-latest",
    ] {
        assert!(text.contains(needle), "ci.yml lacks `{needle}`");
    }
    assert!(!text.contains("58400"), "CI must not depend on :58400");
}

#[test]
fn dependabot_watches_cargo_npm_and_the_action_pins() {
    let text = std::fs::read_to_string(repo().join(".github/dependabot.yml"))
        .expect("read .github/dependabot.yml");
    for ecosystem in ["cargo", "npm", "github-actions"] {
        assert!(
            text.contains(&format!("package-ecosystem: {ecosystem}")),
            "dependabot.yml lacks {ecosystem}"
        );
    }
    assert_eq!(
        text.matches("interval: weekly").count(),
        3,
        "each ecosystem updates weekly"
    );
    assert!(text.matches("groups:").count() >= 3, "updates are grouped");
    // A semver-major in a group of patches held the patches hostage (#2: hmac 0.13 didn't
    // compile, so the uuid patch couldn't land). Groups take minor and patch updates
    // only; each major comes as a pull request of its own.
    assert_eq!(
        text.matches("update-types: [minor, patch]").count(),
        3,
        "every group excludes majors"
    );
}

/// Dependabot's branches get the `pull_request` run; a push run on the same SHA only
/// doubled the work (and the flakes).
#[test]
fn ci_skips_push_runs_on_dependabot_branches() {
    let text = std::fs::read_to_string(repo().join(".github/workflows/ci.yml"))
        .expect("read .github/workflows/ci.yml");
    assert!(
        text.contains("branches-ignore: ['dependabot/**']"),
        "push runs on dependabot/** duplicate the pull_request run"
    );
}

/// The lint itself, against a workflow that breaks every rule once.
#[test]
fn the_lint_catches_each_rule() {
    let bad = r#"name: bad
on:
  pull_request_target:
permissions: write-all
jobs:
  x:
    runs-on: self-hosted
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@0123456789abcdef0123456789abcdef01234567
      - run: echo ${{ github.event.pull_request.title }}
      - name: block
        run: |
          echo "${{ github.head_ref }}"
"#;
    let problems = lint(bad).join("\n");
    for expected in [
        "top-level permissions",
        "dangerous trigger `pull_request_target`",
        "write-all",
        "self-hosted",
        "`actions/checkout@v4` is not pinned",
        "needs a `# vX.Y.Z` comment",
        "checkout without `persist-credentials: false`",
        "line 11: `${{` inside a run script",
        "line 14: `${{` inside a run script",
    ] {
        assert!(
            problems.contains(expected),
            "missed {expected}:\n{problems}"
        );
    }

    let good = r#"name: good
on:
  push:
permissions:
  contents: read
jobs:
  x:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@0123456789abcdef0123456789abcdef01234567 # v7.0.1
        with:
          persist-credentials: false
      - run: echo "$TITLE"
        env:
          TITLE: ${{ github.ref_name }}
"#;
    assert_eq!(lint(good), Vec::<String>::new());
}
