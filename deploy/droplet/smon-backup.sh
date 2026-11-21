#!/usr/bin/env bash
# The off-box backup (deploy/droplet/DEPLOY.md, step 9): `monitor backup` asks the running
# server for a VACUUM INTO snapshot inside SMON_BACKUP_DIR (D-019 — the server names it, no
# client may aim the write), the file is encrypted with age to the droplet's recipient (the
# identity lives on the droplet, with a copy the maintainer keeps offline), uploaded to the
# R2 `smon-backups` bucket under snapshots/, and pruned: the newest of each of the 14 newest
# days and of each of the 8 newest ISO weeks stay, the newest always stays, nothing else does.
# The bucket's 7-day object lock refuses deletes inside the window — everything pruned is
# always older than the lock, so a refusal just gets logged and the run still passes.
#
#   smon-backup run            # snapshot, encrypt, upload, prune (the timer's command)
#   smon-backup prune [--dry]  # the remote retention alone; --dry lists without deleting
#   smon-backup latest         # the newest upload's name and age in hours (smoke.sh reads it)
#
# Reads: SMON_ADMIN_TOKEN from /opt/smon/env; $CREDENTIALS_DIRECTORY/s3_secret_key (or
# /opt/smon/credentials/…); SMON_R2_ENDPOINT / SMON_R2_ACCESS_KEY from
# /opt/smon/credentials/r2.env; the recipient from /opt/smon/backup-age-recipient.
set -euo pipefail
KEEP_DAILY="${KEEP_DAILY:-14}"
KEEP_WEEKLY="${KEEP_WEEKLY:-8}"
KEEP_LOCAL="${KEEP_LOCAL:-3}"
bucket="${BACKUP_BUCKET:-smon-backups}"
creds="${CREDENTIALS_DIRECTORY:-/opt/smon/credentials}"
env_file="${SMON_ENV_FILE:-/opt/smon/env}"
recipient_file="${BACKUP_AGE_RECIPIENT_FILE:-/opt/smon/backup-age-recipient}"
local_url="${SMON_LOCAL_URL:-http://127.0.0.1:58400}"

setting() { sed -n "s/^$1=//p" "$2" | tail -n 1; }
backup_dir=$(setting SMON_BACKUP_DIR "$env_file")
[ -n "$backup_dir" ] || { echo "error: SMON_BACKUP_DIR unset in $env_file" >&2; exit 1; }

r2() {
  # A backup that silently can't reach its bucket is the failure this script exists
  # to prevent: missing R2 credentials is a named error, never a sed "can't read"
  # or a redirect complaint leaked out of a command substitution.
  if [[ ! -f "$creds/r2.env" || ! -s "$creds/s3_secret_key" ]]; then
    echo "error: R2 credentials not configured — want $creds/r2.env and a non-empty $creds/s3_secret_key (install.sh's --r2-secret/--r2-env, DEPLOY.md step 9)" >&2
    exit 1
  fi
  RCLONE_CONFIG_R2_TYPE=s3 RCLONE_CONFIG_R2_PROVIDER=Cloudflare \
    RCLONE_CONFIG_R2_ENDPOINT="$(setting SMON_R2_ENDPOINT "$creds/r2.env")" \
    RCLONE_CONFIG_R2_ACCESS_KEY_ID="$(setting SMON_R2_ACCESS_KEY "$creds/r2.env")" \
    RCLONE_CONFIG_R2_SECRET_ACCESS_KEY="$(tr -d '\n' <"$creds/s3_secret_key")" \
    RCLONE_CONFIG_R2_NO_CHECK_BUCKET=true \
    rclone "$@"
}

uploads() { r2 lsf --files-only "r2:$bucket/snapshots/" | grep -E '\.db\.age$' | sort || true; }
iso_week() { jq -rn --arg d "$1" '$d | strptime("%Y-%m-%d") | strftime("%G-W%V")'; }

prune() {
  local mode=${1:-apply} name day week days="" weeks="" ndays=0 nweeks=0 first=1
  local -a keep=() drop=()
  while IFS= read -r name; do
    [[ -z "$name" ]] && continue
    day_raw=${name:8:8}; day="${day_raw:0:4}-${day_raw:4:2}-${day_raw:6:2}"
    week=$(iso_week "$day")   # monitor-YYYYMMDDTHHMMSSZ-rand.db.age
    local why=""
    if (( first )); then why="newest"; first=0; fi
    if [[ " $days " != *" $day "* ]] && (( ndays < KEEP_DAILY )); then days+=" $day"; ndays=$((ndays + 1)); why+=" daily"; fi
    if [[ " $weeks " != *" $week "* ]] && (( nweeks < KEEP_WEEKLY )); then weeks+=" $week"; nweeks=$((nweeks + 1)); why+=" weekly"; fi
    if [[ -n "$why" ]]; then keep+=("$name"); else drop+=("$name"); fi
  done < <(uploads | sort -r)
  for name in ${keep[@]+"${keep[@]}"}; do echo "keep   $name"; done
  for name in ${drop[@]+"${drop[@]}"}; do
    if [[ "$mode" == dry ]]; then echo "prune  $name (dry run)"; continue; fi
    if out=$(r2 deletefile "r2:$bucket/snapshots/$name" 2>&1); then
      echo "pruned $name"
    elif grep -qiE 'object.?lock|retention|immutab' <<<"$out"; then
      echo "kept   $name (inside the bucket's lock window)"
    else
      echo "error: deleting $name: $out" >&2
      exit 1
    fi
  done
}

latest() {
  local name stamp epoch
  name=$(uploads | tail -n 1)
  [[ -n "$name" ]] || { echo "none"; return 1; }
  stamp=$(sed -E 's/^monitor-(.*)-[^-]+\.db\.age$/\1/' <<<"$name")
  epoch=$(jq -rn --arg s "$stamp" '$s | strptime("%Y%m%dT%H%M%SZ") | mktime')
  echo "$name $(( ($(date -u +%s) - epoch) / 3600 ))"
}

run() {
  [[ -s "$recipient_file" ]] || { echo "error: no age recipient in $recipient_file (DEPLOY.md step 9)" >&2; exit 1; }
  local token name hdr
  token=$(setting SMON_ADMIN_TOKEN "$env_file")
  [[ -n "$token" ]] || { echo "error: no SMON_ADMIN_TOKEN in $env_file" >&2; exit 1; }
  # The token travels in a root-only header file — argv is visible to every local
  # user via `ps` (S2 review). The path is baked into an EXIT trap: a RETURN trap
  # does not run when `set -e` aborts inside the function, which would strand the
  # token file on a curl failure.
  hdr=$(mktemp); trap "rm -f '$hdr'" EXIT
  printf 'authorization: Bearer %s\n' "$token" >"$hdr"
  name=$(curl -fsS --max-time 30 -X POST "$local_url/api/backup" \
    -H "@$hdr" -H 'content-type: application/json' -d '{}' \
    | jq -r '.name // empty')
  rm -f "$hdr"; trap - EXIT
  [[ -n "$name" && -s "$backup_dir/$name" ]] \
    || { echo "error: the server did not produce a snapshot in $backup_dir" >&2; exit 1; }

  work=$(mktemp -d); trap "rm -rf '$work'" EXIT
  age --encrypt --recipients-file "$recipient_file" -o "$work/$name.age" "$backup_dir/$name"
  [[ -s "$work/$name.age" ]] || { echo "error: the encrypted snapshot is empty" >&2; exit 1; }
  r2 copyto "$work/$name.age" "r2:$bucket/snapshots/$name.age"
  echo "smon-backup: uploaded snapshots/$name.age ($(wc -c <"$work/$name.age" | tr -d ' ') bytes)"

  # Local retention: the newest few stay on box for a fast rollback; the off-box copy is the durable one.
  ls -1t "$backup_dir"/monitor-*.db 2>/dev/null | tail -n +$((KEEP_LOCAL + 1)) | while read -r old; do
    rm -f "$old" && echo "pruned local $old"
  done
  prune apply
}

case "${1:-}" in
  run) run ;;
  prune) if [[ "${2:-}" == --dry ]]; then prune dry; else prune apply; fi ;;
  latest) latest ;;
  *) echo "usage: smon-backup run | prune [--dry] | latest" >&2; exit 2 ;;
esac
