#!/usr/bin/env bash
# Fuzz smoke (CI job `fuzz-smoke` in .github/workflows/ci.yml; run locally the same way):
# every decoder of outside input in monitor-server gets a bounded libFuzzer run, seeded
# by its checked-in corpus. A crash is a red-first bug — the target fails, the seed is
# the reproducer. Needs nightly + cargo-fuzz (libFuzzer links sanitizer coverage that
# stable does not ship); absent tools fail loudly — a skipped fuzz gate is no gate.
set -euo pipefail
cd "$(dirname "$0")/.."

seconds="${FUZZ_SMOKE_SECONDS:-20}"

# The toolchain comes from fuzz/rust-toolchain.toml (a pinned dated nightly +
# rust-src) — running inside fuzz/ is what scopes it; the workspace's own pin is
# 1.99.1 and must not be what cargo-fuzz links against.
(cd fuzz && cargo fuzz --version) >/dev/null 2>&1 || {
    echo "cargo-fuzz on the pinned fuzz nightly is required:" >&2
    echo "  (cd fuzz && rustup toolchain install) && cargo install cargo-fuzz" >&2
    exit 1
}

# Every fuzz target MUST have a non-empty seed corpus — an unseeded target only ever
# fuzzes blind space.
targets=()
for bin in fuzz/fuzz_targets/*.rs; do
    name="$(basename "$bin" .rs)"
    dir="fuzz/corpus/$name"
    if [ ! -d "$dir" ] || [ -z "$(ls -A "$dir" 2>/dev/null)" ]; then
        echo "fuzz target '$name' has no seed corpus at $dir" >&2
        exit 1
    fi
    targets+=("$name")
done
[ "${#targets[@]}" -gt 0 ] || { echo "no fuzz targets under fuzz/fuzz_targets" >&2; exit 1; }

for target in "${targets[@]}"; do
    echo "== fuzz smoke: $target (${seconds}s, seeded)"
    # The run writes new finds into whatever corpus dir it is handed — work a
    # copy so the committed seeds stay the seeds.
    run_corpus="$(mktemp -d)/corpus"
    cp -R "fuzz/corpus/$target" "$run_corpus"
    (cd fuzz && cargo fuzz run "$target" "$run_corpus" -- -max_total_time="$seconds") 2>&1 |
        tail -15
done
echo "fuzz smoke: ${#targets[@]} targets, ${seconds}s each — no crashes"
