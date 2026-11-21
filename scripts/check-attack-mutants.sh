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
#      FAIL THE TEST it names (the expected failure: `test <name> ... FAILED`
#      in the binary's own output),
#   3. restores the tree.
#
# A mutant that leaves the test green is a gate failure: the protection is not
# what the test measures. And the other escapes are failures too, never kills —
# a non-compiling mutant is not a real alternative, a mutant that only breaks
# the server's boot proves the suite never reached the protection, a timeout
# proves nothing. Needs a clean worktree — it applies and reverts diffs.
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

# Where this build puts its binaries: honour CARGO_TARGET_DIR exactly like
# cargo does — a hardcoded target/ either misses what it built or finds stale
# binaries from a default-layout build and calls them a kill.
target_dir="${CARGO_TARGET_DIR:-target}/debug"

echo "== baseline build (the clean binary every green check runs against)"
cargo build -p monitor-server --locked 2>&1 | tail -3 || exit 1
[[ -x "$target_dir/monitor-server" ]] || {
    echo "no monitor-server binary under $target_dir — was CARGO_TARGET_DIR set for the build too?" >&2
    exit 1
}
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
clean_bin="$target_dir/monitor-server.clean"
cp "$target_dir/monitor-server" "$clean_bin"

fails=0; killed=0
restore() {
    git checkout -- crates/ 2>/dev/null || true
    rm -f "$clean_bin"
}
trap restore EXIT

# Coverage is half the proof: every `async fn` test under tests/attacks/tests/
# must name a mutant file — a test without one is a protection nothing measures.
for file in tests/attacks/tests/*.rs; do
    stem=$(basename "$file" .rs)
    while IFS= read -r tname; do
        if [[ ! -f "tests/attacks/mutants/${stem}__${tname}.patch" ]]; then
            echo "NO MUTANT: $stem::$tname has no ${stem}__${tname}.patch" >&2
            fails=$((fails + 1))
        fi
        # F re-review: a kill must land on the test's OWN protection check —
        # the .expect file pins the panic location(s) the decisive assertion
        # produces (file:line, one per line). A panic at a setup .expect or a
        # helper is not in it, so a server crash can never pass as a kill.
        if [[ ! -s "tests/attacks/mutants/${stem}__${tname}.expect" ]]; then
            echo "NO EXPECT: $stem::$tname has no ${stem}__${tname}.expect" >&2
            fails=$((fails + 1))
        fi
    done < <(awk '
        /^[[:space:]]*#\[(tokio::)?test/ {
            # The fn line may follow attributes/blanks and may be indented or
            # non-async — print it, let sed pull the name.
            while ((getline line) > 0 && line ~ /^[[:space:]]*(#|$|\/\/)/) continue
            print line
        }' "$file" | sed -n 's/.*fn[[:space:]]\{1,\}\([a-z0-9_]*\).*/\1/p')
done

# Run one test binary to a bounded wait: a kill is only the EXPECTED failure —
# the named test's own `... FAILED` line. A hung binary proves nothing and the
# server never answering /livez means the mutation broke the boot, not the
# protection the test measures. The limit is a plain integer or it is not used —
# shell arithmetic must never see an unvalidated value.
run_bounded() {  # run_bounded <server-bin> <test-binary> <test-name> <log> → the test's exit, 124 on expiry
    local server=$1 bin=$2 test=$3 log=$4 limit=${MUTANT_TEST_LIMIT:-120}
    [[ $limit =~ ^[0-9]+$ ]] || limit=120
    SMON_ATTACK_BIN="$server" "$bin" --exact "$test" >"$log" 2>&1 &
    local pid=$!
    ( sleep "$limit"; kill -9 "$pid" 2>/dev/null ) &
    local watchdog=$!
    wait "$pid" 2>/dev/null
    local rc=$?
    kill "$watchdog" 2>/dev/null; wait "$watchdog" 2>/dev/null
    if [[ $rc -eq 137 ]]; then return 124; fi
    return "$rc"
}

for patch in "${mutants[@]}"; do
    name=$(basename "$patch" .patch)
    stem=${name%%__*}; test=${name##*__}
    bin=$(bin_for "$stem")
    if [[ -z "$bin" ]]; then
        echo "NO BINARY: no compiled test executable for '$stem'" >&2
        fails=$((fails + 1)); continue
    fi
    log=$(mktemp)

    # 1. Green on the clean binary — the test must be alive to be killed, and
    # it must be the named test that passes (a wrong name runs nothing).
    if ! run_bounded "$clean_bin" "$bin" "$test" "$log"; then
        echo "UNMUTATED RED: $test fails without any mutation — it proves nothing" >&2
        tail -5 "$log" >&2; fails=$((fails + 1)); rm -f "$log"; continue
    fi
    if ! grep -q "^test ${test} \.\.\. ok" "$log"; then
        echo "WRONG NAME: no 'test $test ... ok' line — the patch names no real test" >&2
        fails=$((fails + 1)); rm -f "$log"; continue
    fi

    # 2. Apply, rebuild, demand the expected red.
    if ! git apply "$patch" 2>/dev/null; then
        echo "STALE MUTANT: $name no longer applies — refresh it against the tree" >&2
        fails=$((fails + 1)); rm -f "$log"; continue
    fi
    if ! cargo build -p monitor-server --locked >/dev/null 2>&1; then
        echo "DEAD MUTANT: $name does not compile — a mutant must be a real alternative" >&2
        fails=$((fails + 1))
    elif run_bounded "$target_dir/monitor-server" "$bin" "$test" "$log"; then
        echo "MUTANT SURVIVED: $test still passes with its protection removed" >&2
        fails=$((fails + 1))
    else
        rc=$?
        if [[ $rc -eq 124 ]]; then
            echo "INCONCLUSIVE: $name timed out — a hung mutant is not a kill" >&2
            fails=$((fails + 1))
        elif grep -q "never answered /livez" "$log"; then
            echo "INCONCLUSIVE: $name broke the server's boot, not the assertion — not a kill" >&2
            fails=$((fails + 1))
        elif grep -q "^test ${test} \.\.\. FAILED" "$log"; then
            # The named test failed — but on WHAT? A mutant that breaks the
            # test's own setup still prints its FAILED line: the kill is only
            # real when the panic lands on the file:line the .expect sidecar
            # pins — the protection's own assertion — never a fixture's
            # .expect, a helper, or a crash elsewhere (F re-review).
            if grep -q "panicked at .*tests/attacks/tests/${stem}\.rs" "$log" \
                && grep -F -q -f "tests/attacks/mutants/${name}.expect" "$log"; then
                echo "killed: $name"
                killed=$((killed + 1))
            else
                echo "INCONCLUSIVE: $name failed $test off its pinned assertion — not a kill:" >&2
                grep "panicked at" "$log" | head -3 >&2
                fails=$((fails + 1))
            fi
        else
            echo "INCONCLUSIVE: $name failed some other way — not a kill:" >&2
            tail -5 "$log" >&2
            fails=$((fails + 1))
        fi
    fi
    rm -f "$log"
    git checkout -- crates/
done

echo "check-attack-mutants: $killed killed, $fails failed (of ${#mutants[@]})"
[[ $fails -eq 0 ]]
