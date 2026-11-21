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

# Tracked sources must be unmodified — apply/checkout restores only what is in
# the index, so a dirty tree would leak one mutant's changes into the next run.
if [[ -n "$(git status --porcelain -- crates tests | grep -v '^??')" ]]; then
    echo "check-attack-mutants needs clean tracked sources under crates/ + tests/" >&2
    exit 2
fi

# F re-review: the mutants directory is a fixed contract — every file must be
# git-tracked and named <stem>__<test>.patch or .expect. An untracked or
# stray file is a gate failure, never a quiet skip.
inventory_fails=0
for f in tests/attacks/mutants/*; do
    base=$(basename "$f")
    case "$base" in
        *__*.patch|*__*.expect) ;;
        *) echo "STRAY FILE: tests/attacks/mutants/$base is not a .patch/.expect pair member" >&2
           inventory_fails=$((inventory_fails + 1)) ;;
    esac
    if ! git ls-files --error-unmatch "tests/attacks/mutants/$base" >/dev/null 2>&1; then
        echo "UNTRACKED: tests/attacks/mutants/$base is not committed — the gate must run on the contract" >&2
        inventory_fails=$((inventory_fails + 1))
    fi
done
# And the pairs are complete both ways: a patch with no .expect cannot pin a
# kill; an .expect with no patch is an orphan.
for f in tests/attacks/mutants/*.expect; do
    [[ -f "${f%.expect}.patch" ]] || {
        echo "ORPHAN EXPECT: $f has no matching patch" >&2
        inventory_fails=$((inventory_fails + 1)); }
done
[[ $inventory_fails -eq 0 ]] || exit 2

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
# Restore every path the LAST applied patch touched — not just crates/: a
# mutant that edits anything else must still leave the tree it found (F).
last_patch_paths=""
restore() {
    if [[ -n "$last_patch_paths" ]]; then
        echo "$last_patch_paths" | while IFS= read -r p; do
            [[ -n "$p" ]] && git checkout -- "$p" 2>/dev/null || true
        done
    fi
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
        # produces (file:line, one per line). A panic at a setup step or a
        # helper is not in it, so a server crash can never pass as a kill.
        expect_file="tests/attacks/mutants/${stem}__${tname}.expect"
        if [[ ! -s "$expect_file" ]]; then
            echo "NO EXPECT: $stem::$tname has no $expect_file" >&2
            fails=$((fails + 1))
        else
            # Validate EVERY pin: right file, in-range line, and the pinned
            # line is an assertion — assert*/panic!, never a setup .expect.
            while IFS= read -r pin; do
                if [[ ! "$pin" =~ ^${stem}\.rs:[0-9]+:$ ]]; then
                    echo "BAD PIN: $expect_file line '$pin' is not '<stem>.rs:<line>:'" >&2
                    fails=$((fails + 1)); continue
                fi
                lineno=${pin#*:}; lineno=${lineno%:}
                file_lines=$(wc -l < "$file")
                if (( lineno < 1 || lineno > file_lines )); then
                    echo "BAD PIN: $pin is outside $file (has $file_lines lines)" >&2
                    fails=$((fails + 1)); continue
                fi
                pinned=$(sed -n "${lineno}p" "$file")
                case "$pinned" in
                    *assert*|*panic*) ;;
                    *) echo "SETUP PIN: $pin does not land on an assertion — pins name the protection's own assert, never a setup line: $pinned" >&2
                       fails=$((fails + 1)) ;;
                esac
            done < "$expect_file"
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

    # 2. Apply, rebuild, demand the expected red. The restore list is exactly
    # the paths THIS patch touches — a mutant may live outside crates/.
    last_patch_paths=$(git apply --numstat "$patch" 2>/dev/null | awk '{print $3}')
    if ! git apply "$patch" 2>/dev/null; then
        echo "STALE MUTANT: $name no longer applies — refresh it against the tree" >&2
        last_patch_paths=""
        fails=$((fails + 1)); rm -f "$log"; continue
    fi
    # -D warnings on the crates a mutant can touch — a mutant that compiles
    # with warnings is not a real alternative (F re-review). cargo rustc keeps
    # the flag off the (unchanged, already built) dependency graph.
    if ! cargo rustc -p monitor-db --lib --locked -- -D warnings >/dev/null 2>&1 \
        || ! cargo rustc -p monitor-server --bin monitor-server --locked -- -D warnings >/dev/null 2>&1; then
        echo "DEAD MUTANT: $name does not compile under -D warnings — a mutant must be a real alternative" >&2
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
    echo "$last_patch_paths" | while IFS= read -r p; do
        [[ -n "$p" ]] && git checkout -- "$p" 2>/dev/null || true
    done
    last_patch_paths=""
done

echo "check-attack-mutants: $killed killed, $fails failed (of ${#mutants[@]})"
[[ $fails -eq 0 ]]
