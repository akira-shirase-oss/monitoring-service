#!/usr/bin/env bash
# Cloudflare's published address ranges, applied on the monitor droplet (deploy/droplet/DEPLOY.md):
#   - Caddy trusts them, so it forwards the visitor's address (/etc/caddy/cloudflare-trusted.caddy);
#   - ufw lets only them reach 443 (SSH stays as you set it).
# smon's SMON_TRUSTED_PROXY is deliberately NOT rewritten: Caddy is the only proxy the server
# sees (127.0.0.1), and Caddy already attributes the visitor from X-Forwarded-For over these
# ranges — the server just reads the right-most entry.
# Run as root after install and again whenever Cloudflare changes its list (rarely; a monthly
# timer is fine). Refuses to write anything from a list that doesn't look like address ranges.
#
#   sudo deploy/droplet/cloudflare-ips.sh [--print]
set -euo pipefail
v4=$(curl -fsS --max-time 10 https://www.cloudflare.com/ips-v4)
v6=$(curl -fsS --max-time 10 https://www.cloudflare.com/ips-v6)
ranges=$(printf '%s\n%s\n' "$v4" "$v6" | tr -d '\r' | sed '/^$/d')
count=$(wc -l <<<"$ranges" | tr -d ' ')
while IFS= read -r range; do
  [[ "$range" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}/[0-9]{1,2}$ || "$range" =~ ^[0-9a-f:]+/[0-9]{1,3}$ ]] \
    || { echo "error: '$range' isn't an address range; nothing written" >&2; exit 1; }
done <<<"$ranges"
(( count >= 10 )) || { echo "error: only $count ranges from Cloudflare; nothing written" >&2; exit 1; }

if [[ "${1:-}" == --print ]]; then
  echo "$ranges"
  exit 0
fi
[[ $EUID -eq 0 ]] || { echo "error: run as root (or pass --print)" >&2; exit 2; }

printf 'trusted_proxies static %s\n' "$(paste -sd' ' <<<"$ranges")" >/etc/caddy/cloudflare-trusted.caddy

# ufw: 443 from Cloudflare only. Existing rules for 443 are replaced; SSH is left alone.
while ufw status numbered | grep -qE '\b443(/tcp)?\b'; do
  n=$(ufw status numbered | grep -E '\b443(/tcp)?\b' | head -1 | sed -E 's/^\[ *([0-9]+)\].*/\1/')
  yes | ufw delete "$n" >/dev/null
done
while IFS= read -r range; do
  ufw allow proto tcp from "$range" to any port 443 comment 'cloudflare' >/dev/null
done <<<"$ranges"

echo "cloudflare-ips: $count ranges trusted by Caddy and allowed to 443"
echo "next: sudo systemctl reload caddy"
