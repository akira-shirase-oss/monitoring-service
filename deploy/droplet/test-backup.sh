#!/usr/bin/env bash
# Stub-based test for smon-backup.sh — no server, no R2, no age key. Stubs on PATH:
#   curl    — asserts the backup request posts EXACTLY '{}' (no client path), then "the
#             server" drops the named snapshot into SMON_BACKUP_DIR and answers its name.
#   age     — encrypts by copying (proves the file that gets uploaded is the .age artifact).
#   rclone  — serves a scripted object list from $R2_OBJECTS, logs every call to $R2_LOG,
#             refuses deletions named in $R2_LOCK with a lock error (expected — the run
#             passes), and fails any other error named in $R2_FAILS (the run must fail).
# Run by tests/deploy_scripts.rs; by hand: bash deploy/droplet/test-backup.sh
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
t=$(mktemp -d); trap 'rm -rf "$t"' EXIT
mkdir -p "$t/bin" "$t/backups" "$t/creds"
NEW_NAME=monitor-20261118T041700Z-aaaabbbb.db
export R2_LOG="$t/rclone.log" R2_OBJECTS="$t/objects" CURL_LOG="$t/curl.log"
export SMON_BACKUP_DIR="$t/backups" NEW_NAME

cat >"$t/bin/curl" <<'EOF'
#!/usr/bin/env bash
echo "$*" >>"$CURL_LOG"
# the only allowed request body is the empty object — the server draws the name (D-019)
[[ "$*" == *"-d {}"* ]] || { echo "stub: client path or field leaked into the request" >&2; exit 22; }
printf 'x' >"$SMON_BACKUP_DIR/$NEW_NAME"   # "the server" writes the snapshot
printf '{"name":"%s","bytes":1}\n' "$NEW_NAME"
EOF
cat >"$t/bin/age" <<'EOF'
#!/usr/bin/env bash
out=""; in=""
while [ $# -gt 0 ]; do case "$1" in -o) out=$2; shift 2;; --recipients-file) shift 2;; --encrypt) shift;; -i) shift 2;; --decrypt) shift;; *) in=$1; shift;; esac; done
[[ -n "$out" && -n "$in" && -s "$in" ]] || exit 1
{ echo "AGE-ENC"; cat "$in"; } >"$out"
EOF
cat >"$t/bin/rclone" <<'EOF'
#!/usr/bin/env bash
echo "rclone $*" >>"$R2_LOG"
cmd=$1; shift
last=${*: -1}
case "$cmd" in
  lsf) cat "$R2_OBJECTS" 2>/dev/null || true ;;
  copyto) basename "$last" >>"$R2_OBJECTS"; sort -u "$R2_OBJECTS" -o "$R2_OBJECTS" ;;
  deletefile)
    name=$(basename "$last")
    if grep -qxF "$name" "${R2_LOCK:-/dev/null}" 2>/dev/null; then
      echo "ERROR : object is under a retention lock (ObjectLock)" >&2; exit 1
    fi
    if grep -qxF "$name" "${R2_FAILS:-/dev/null}" 2>/dev/null; then
      echo "ERROR : io error: unreachable" >&2; exit 1
    fi
    grep -vxF "$name" "$R2_OBJECTS" >"$R2_OBJECTS.tmp" || true; mv "$R2_OBJECTS.tmp" "$R2_OBJECTS" ;;
  *) echo "stub rclone: unhandled $cmd" >&2; exit 1 ;;
esac
EOF
chmod +x "$t/bin"/*

: >"$R2_OBJECTS"   # five old objects, days 10–14 — three must survive the prune
for d in $(seq -w 10 14); do echo "monitor-202611${d}T010000Z-old${d}.db.age" >>"$R2_OBJECTS"; done
: >"$R2_LOG"
printf 'r2-secret\n' >"$t/creds/s3_secret_key"
printf 'SMON_R2_ENDPOINT=https://acct.example.r2.dev\nSMON_R2_ACCESS_KEY=ak\n' >"$t/creds/r2.env"
printf 'SMON_ADMIN_TOKEN=tok\nSMON_BACKUP_DIR=%s\n' "$SMON_BACKUP_DIR" >"$t/env"
printf 'age1recipient\n' >"$t/recipient"
export PATH="$t/bin:$PATH"
export SMON_ENV_FILE="$t/env" CREDENTIALS_DIRECTORY="$t/creds" BACKUP_AGE_RECIPIENT_FILE="$t/recipient"
export KEEP_DAILY=3 KEEP_WEEKLY=1 KEEP_LOCAL=2

pass=0; fail=0
ok()  { pass=$((pass+1)); echo "  ok    $1"; }
bad() { fail=$((fail+1)); echo "  FAIL  $1"; }

echo "run: snapshot → encrypt → upload → prune"
out=$(bash "$here/smon-backup.sh" run 2>&1) && code=0 || code=$?
[[ $code -eq 0 ]] && ok "run exits 0" || bad "run failed ($code): $out"
grep -q 'copyto.*aaaabbbb.db.age' "$R2_LOG" && ok "the encrypted artifact was uploaded" || bad "no upload in $R2_LOG"
grep -qxF "$NEW_NAME.age" "$R2_OBJECTS" && ok "bucket now holds $NEW_NAME.age" || bad "bucket missing the upload"
grep -q '\-d {}' "$CURL_LOG" && ok "the request carried only '{}'" || bad "request was: $(cat "$CURL_LOG")"
grep -q 'pruned monitor-20261112T010000Z-old12.db.age' <<<"$out" && ok "the over-retention objects were pruned" || bad "expected a prune in: $out"

echo "prune --dry lists without deleting"
before=$(cat "$R2_OBJECTS")
bash "$here/smon-backup.sh" prune --dry >/dev/null
[[ "$(cat "$R2_OBJECTS")" == "$before" ]] && ok "dry run changed nothing" || bad "dry run mutated the bucket"

echo "retention order: newest + KEEP_DAILY days + KEEP_WEEKLY weeks"
: >"$R2_OBJECTS"
for d in $(seq -w 1 20); do echo "monitor-202611${d}T010000Z-r${d}.db.age" >>"$R2_OBJECTS"; done
dry=$(bash "$here/smon-backup.sh" prune --dry)
kept=$(grep -c '^keep' <<<"$dry"); pruned=$(grep -c '^prune' <<<"$dry")
# day 20 is newest AND fills the only weekly slot; 19 and 18 fill daily slots → 3 keeps
[[ $kept -eq 3 && $pruned -eq 17 ]] && ok "keeps newest + 3 daily + 1 weekly (3 objects), prunes 17" || bad "keep=$kept prune=$pruned want 3/17"
grep -q '^keep   monitor-20261120' <<<"$dry" && ok "the newest survives" || bad "newest was dropped"

echo "a lock refusal on a young object is expected, not a failure"
: >"$R2_OBJECTS"
for d in $(seq -w 1 12); do echo "monitor-202611${d}T010000Z-r${d}.db.age" >>"$R2_OBJECTS"; done
echo "monitor-20261109T010000Z-r09.db.age" >"$t/lock"; export R2_LOCK="$t/lock"
out=$(bash "$here/smon-backup.sh" prune 2>&1) && code=0 || code=$?
[[ $code -eq 0 ]] && ok "locked object tolerated (exit 0)" || bad "lock refusal failed the run: $out"
grep -q 'kept.*lock window' <<<"$out" && ok "the refusal is reported as kept/locked" || bad "no kept line in: $out"
grep -qxF 'monitor-20261109T010000Z-r09.db.age' "$R2_OBJECTS" && ok "the locked object stays" || bad "locked object vanished"
unset R2_LOCK

echo "any other error fails the run"
: >"$R2_OBJECTS"
for d in $(seq -w 1 12); do echo "monitor-202611${d}T010000Z-r${d}.db.age" >>"$R2_OBJECTS"; done
echo "monitor-20261108T010000Z-r08.db.age" >"$t/fails"; export R2_FAILS="$t/fails"
if out=$(bash "$here/smon-backup.sh" prune 2>&1); then bad "non-lock delete error should fail"; else ok "non-lock error fails the run"; fi
unset R2_FAILS

echo "no client path reaches the server"
! grep -qE '\-d \{[^ }]' "$CURL_LOG" && ok "no path-shaped payload was ever sent" || bad "payload: $(cat "$CURL_LOG")"

echo; echo "test-backup: $pass ok, $fail failed"
[[ $fail -eq 0 ]]
