#!/usr/bin/env bash
# After install, and after every upgrade: is the SFO3 droplet serving, safely, through
# Cloudflare? Run on the droplet as root. Exits non-zero on any failure, after printing
# every check. That the droplet's own address refuses 443 (only Cloudflare may reach it —
# and AOP refuses a non-Cloudflare TLS client anyway) is checked from another machine:
# DEPLOY.md step 10.
# shellcheck disable=SC2015  # ok and bad only print, so `test && ok || bad` is if-then-else here.
set -uo pipefail
domain="${SMOKE_DOMAIN:-monitor.example.com}"
local_url="http://127.0.0.1:58400"
public="https://$domain"
failures=0
ok() { printf '  ok    %s\n' "$1"; }
bad() { printf '  FAIL  %s\n' "$1"; failures=$((failures + 1)); }
check() { local what=$1; shift; if "$@" >/dev/null 2>&1; then ok "$what"; else bad "$what"; fi; }

echo "services"
for unit in monitor-server caddy smon-backup.timer; do
  systemctl cat "$unit" >/dev/null 2>&1 || { bad "$unit isn't installed"; continue; }
  check "$unit is active" systemctl is-active --quiet "$unit"
done

echo "the server, on loopback"
check "/livez answers 200" curl -fsS --max-time 5 "$local_url/livez"
ready=$(curl -sS --max-time 5 "$local_url/readyz" || true)
if jq -e '.status == "ready" or .status == "degraded"' <<<"$ready" >/dev/null 2>&1; then
  status=$(jq -r '.status' <<<"$ready"); faults=$(jq -rc '.faults // []' <<<"$ready")
  if [ "$status" = ready ]; then ok "/readyz is ready"; else bad "/readyz is degraded: $faults"; fi
else bad "/readyz: $ready"; fi
version=$(curl -fsS --max-time 5 "$local_url/version" || true)
jq -e '.version' <<<"$version" >/dev/null 2>&1 && ok "/version: $(jq -r '.version' <<<"$version")" || bad "/version: $version"
# The API needs the operator's token, like anything else that calls it. It is read
# from the env file and handed to curl in a header file, never on the command line
# where another user could read it from the process list. Without a token the answer
# is 401, which this check used to report as a failed install.
env_file="${SMON_ENV_FILE:-/opt/smon/env}"
hdr=$(mktemp); trap "rm -f '$hdr'" EXIT
token=$(sed -n 's/^SMON_ADMIN_TOKEN=//p' "$env_file" 2>/dev/null | tail -n 1)
if [ -n "$token" ]; then printf 'Authorization: Bearer %s\n' "$token" >"$hdr"; fi
unset token
monitors=$(curl -sS --max-time 5 -H @"$hdr" "$local_url/api/monitors" || true)
jq -e '.monitors | type == "array"' <<<"$monitors" >/dev/null 2>&1 \
  && ok "/api/monitors answers ($(jq '.monitors | length' <<<"$monitors") registered)" \
  || bad "/api/monitors: $monitors"

echo "the public snapshot"
beat=$(ls -t /opt/smon/status/status.json 2>/dev/null | head -1 || true)
if [ -n "$beat" ]; then
  if jq -e '.expires_at' "$beat" >/dev/null 2>&1; then ok "status.json present with expires_at"; else bad "status.json has no expires_at"; fi
else bad "no status.json under /opt/smon/status — the snapshot task isn't writing"; fi

if [[ -n "${SMOKE_LOCAL:-}" ]]; then
  echo "through Cloudflare — skipped (SMOKE_LOCAL; the upgrade path only proves the local side)"
else
echo "through Cloudflare"
code=$(curl -sS -o /dev/null -w '%{http_code} %{ssl_verify_result}' --max-time 10 "$public/livez" || echo "000 x")
[[ "$code" == "200 0" ]] && ok "$public/livez answers 200 over valid TLS" || bad "$public/livez: HTTP/verify $code"
headers=$(curl -sSI --max-time 10 "$public/" || true)
grep -qi '^server: cloudflare' <<<"$headers" && ok "served through Cloudflare" || bad "not through Cloudflare (no server: cloudflare header)"
grep -qi '^content-security-policy:' <<<"$headers" && ok "the dashboard has a content security policy" || bad "no content-security-policy"
fi

echo "backups"
if command -v smon-backup >/dev/null; then
  latest=$(smon-backup latest 2>/dev/null || echo none)
  hours=${latest##* }
  if [[ "$latest" != none && "$hours" =~ ^[0-9]+$ ]] && (( hours < 26 )); then ok "the newest off-box backup is ${hours} h old (${latest%% *})"
  else bad "no off-box backup in the last 26 h ($latest)"; fi
else
  bad "smon-backup isn't installed (DEPLOY.md step 9)"
fi

echo "memory (exit gate: recorded idle)"
current=$(systemctl show monitor-server -p MemoryCurrent --value 2>/dev/null || echo "")
if [[ "$current" =~ ^[0-9]+$ ]]; then ok "monitor-server uses $((current / 1024 / 1024)) MiB"; else bad "no MemoryCurrent ($current)"; fi
free -m | awk '/^Mem:/ {printf "  info  host: %d MiB used of %d, %d available\n", $3, $2, $7}'

echo
if (( failures > 0 )); then echo "smoke: $failures check(s) FAILED"; exit 1; fi
echo "smoke: every check passed"
