#!/usr/bin/env bash
# Mutation proof for tests/attacks (the suite rule: an attack test that still passes
# with its protection removed is a decoration, not a gate — two other products'
# suites passed with their protections deleted). Every file in
# tests/attacks/mutants/ is named <test-binary>__<test-name>.patch and removes
# exactly the protection that test asserts. For each mutant this harness:
#
#   1. runs the named test against a CLEAN server build — it must pass
#      (a test already broken proves nothing about the protection),
#   2. applies the patch, rebuilds the real binary, runs the same test — it must
#      FAIL (the test is protection-dependent),
#   3. restores the tree.
#
# A mutant that leaves the test green is a gate failure: the protection is not
# what the test measures. Needs a clean worktree — it applies and reverts diffs.
set -uo pipefail
cd "$(dirname "$0")/.."

mutants=(tests/attacks/mutants/*.patch)
[[ -f "${mutants[0]}" ]] || { echo "no mutant patches under tests/attacks/mutants" >&2; exit 1; }

# Untracked paths (the patches themselves before they're committed) are fine —
# apply/checkout never touches them; tracked sources must be unmodified.
if [[ -n "$(git status --porcelain -- crates tests | grep -v '^??')" ]]; then
    echo "check-attack-mutants needs clean tracked sources under crates/ + tests/" >&2
    exit 2
fi

echo "== baseline build (the clean binary every green check runs against)"
cargo build -p monitor-server --locked 2>&1 | tail -3 || exit 1
# Map each test-file stem to the executable THIS build produced. deps/ keeps
# stale executables from older builds; a find-by-name would silently run an
# old test binary and call a real mutant "survived".
bins_list=$(cargo test -p monitor-attacks --no-run --locked 2>&1)
echo "$bins_list" | grep -q 'Executable tests/' || {
    echo "$bins_list" | tail -20; echo "cargo test --no-run produced no attack binaries" >&2; exit 1;
}
bin_for() {  # bin_for <test-file-stem> → the executable this build made for it
    echo "$bins_list" | sed -n "s/.*Executable tests\/$1\.rs (\(.*\))\$/\1/p" | head -1
}
# Keep a copy of the unmutated server binary: the green check per mutant then
# costs nothing — only the mutated build is paid.
clean_bin="target/debug/monitor-server.clean"
cp target/debug/monitor-server "$clean_bin"

fails=0; killed=0
restore() {
    git checkout -- crates/ 2>/dev/null || true
    rm -f "$clean_bin"
}
trap restore EXIT

for patch in "${mutants[@]}"; do
    name=$(basename "$patch" .patch)
    stem=${name%%__*}; test=${name##*__}
    bin=$(bin_for "$stem")
    if [[ -z "$bin" ]]; then
        echo "NO BINARY: no compiled test executable for '$stem'" >&2
        fails=$((fails + 1)); continue
    fi

    # 1. Green on the clean binary — the test must be alive to be killed.
    if ! SMON_ATTACK_BIN="$clean_bin" "$bin" --exact "$test" >/dev/null 2>&1; then
        echo "UNMUTATED RED: $test fails without any mutation — it proves nothing" >&2
        fails=$((fails + 1)); continue
    fi

    # 2. Apply, rebuild, demand red.
    if ! git apply "$patch" 2>/dev/null; then
        echo "STALE MUTANT: $name no longer applies — refresh it against the tree" >&2
        fails=$((fails + 1)); continue
    fi
    if ! cargo build -p monitor-server --locked >/dev/null 2>&1; then
        echo "DEAD MUTANT: $name does not compile — a mutant must be a real alternative" >&2
        fails=$((fails + 1))
    elif SMON_ATTACK_BIN=target/debug/monitor-server "$bin" --exact "$test" >/dev/null 2>&1; then
        echo "MUTANT SURVIVED: $test still passes with its protection removed" >&2
        fails=$((fails + 1))
    else
        echo "killed: $name"
        killed=$((killed + 1))
    fi
    git checkout -- crates/
done

echo "check-attack-mutants: $killed killed, $fails failed (of ${#mutants[@]})"
[[ $fails -eq 0 ]]
