#!/usr/bin/env bash
# The shipped systemd unit is part of the security boundary — a process that parses
# network responses and holds alert credentials gets no privileges it doesn't need.
# This checks what deploy_unit.rs pins statically, from the shell: every hardening
# directive the runbook promises, plus — on a host that has systemd — `systemd-analyze
# verify` and `systemd-analyze security` against the installed unit.
#
#   deploy/droplet/check-unit-hardening.sh [path-to-unit] [installed-unit-name]
#
# Runs anywhere: the directive checks are portable bash; the systemd-analyze half runs
# when the tool exists and is skipped loudly otherwise. Run by tests/deploy_scripts.rs
# (static half on the dev machine) and by the deploy-check workflow (full half on a VM).
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
unit_file="${1:-$here/../monitor-server.service}"
unit_name="${2:-monitor-server.service}"
failures=0; pass=0
ok()  { printf '  ok    %s\n' "$1"; pass=$((pass + 1)); }
bad() { printf '  FAIL  %s\n' "$1"; failures=$((failures + 1)); }

[[ -f "$unit_file" ]] || { echo "error: no unit file at $unit_file" >&2; exit 2; }

# The setting one section carries (last wins, like systemd).
setting() { # setting <section> <key>
  awk -v sec="$1" -v key="$2" '
    /^\[/ { insec = ($0 == "[" sec "]"); next }
    insec && $0 ~ ("^" key "[[:space:]]*=") { sub("^[^=]*=[[:space:]]*", ""); v = $0 }
    END { if (v != "") print v }' "$unit_file"
}
want() { # want <section> <key> <expected>
  got=$(setting "$1" "$2")
  if [[ "$got" == "$3" ]]; then ok "$2 = ${3:-<empty>}"; else bad "$2: want '${3:-<empty>}', got '$got'"; fi
}

echo "identity — never root, no privileges to gain"
want Service User smon
want Service Group smon
want Service NoNewPrivileges true
want Service CapabilityBoundingSet ""
want Service AmbientCapabilities ""

echo "filesystem — a read-only system but the two directories the monitor writes"
want Service ProtectSystem strict
want Service ProtectHome true
want Service PrivateTmp true
want Service PrivateDevices true
want Service ProtectKernelTunables true
want Service ProtectKernelModules true
want Service ProtectControlGroups true
want Service RestrictSUIDSGID true
want Service LockPersonality true
want Service UMask 0077
paths=$(setting Service ReadWritePaths)
[[ "$paths" == "/opt/smon/data /opt/smon/status" ]] \
  && ok "ReadWritePaths is exactly data + status" \
  || bad "ReadWritePaths: want '/opt/smon/data /opt/smon/status', got '$paths'"
families=$(setting Service RestrictAddressFamilies)
[[ "$families" == "AF_INET AF_INET6 AF_UNIX" ]] \
  && ok "RestrictAddressFamilies is INET+INET6+UNIX only" \
  || bad "RestrictAddressFamilies: got '$families'"

echo "bounds and lifecycle — the kernel adds its own caps; the stop outlasts the drain"
[[ -n "$(setting Service MemoryMax)" ]] && ok "MemoryMax is set ($(setting Service MemoryMax))" || bad "no MemoryMax"
[[ -n "$(setting Service LimitNOFILE)" ]] && ok "LimitNOFILE is set ($(setting Service LimitNOFILE))" || bad "no LimitNOFILE"
stop=$(setting Service TimeoutStopSec); stop=${stop%s}
[[ "$stop" =~ ^[0-9]+$ && "$stop" -gt 27 ]] \
  && ok "TimeoutStopSec=${stop}s outlives the 12+15s drain" \
  || bad "TimeoutStopSec=$stop must exceed the drain"
want Service Restart on-failure
[[ -n "$(setting Unit StartLimitBurst)" && -n "$(setting Unit StartLimitIntervalSec)" ]] \
  && ok "a crash loop hits the start limit" || bad "no StartLimit* — a crash loop restarts forever"
mounts=$(setting Unit RequiresMountsFor)
for m in /opt/smon/data /opt/smon/status; do
  [[ " $mounts " == *" $m "* ]] && ok "RequiresMountsFor has $m" || bad "RequiresMountsFor lacks $m"
done
want Service EnvironmentFile /opt/smon/env

echo "systemd-analyze — the real checker, when this host has systemd"
if command -v systemd-analyze >/dev/null 2>&1; then
  if out=$(systemd-analyze verify "$unit_file" 2>&1); then
    [[ -z "$out" ]] && ok "systemd-analyze verify: clean" || { echo "$out" | sed 's/^/  info  /'; ok "systemd-analyze verify: no errors (notes above)"; }
  else
    bad "systemd-analyze verify: $out"
  fi
  if systemctl cat "$unit_name" >/dev/null 2>&1; then
    level=$(systemd-analyze security "$unit_name" 2>/dev/null | sed -nE 's/.*exposure level[^:]*:[[:space:]]*([0-9.]+).*/\1/p' | tail -1)
    systemd-analyze security "$unit_name" 2>/dev/null | sed 's/^/  info  /'
    if [[ "$level" =~ ^[0-9.]+$ ]] && awk -v l="$level" 'BEGIN{exit !(l <= 2.0)}'; then
      ok "systemd-analyze security: exposure $level (≤ 2.0)"
    else
      bad "systemd-analyze security: exposure '$level' — want ≤ 2.0"
    fi
  else
    echo "  info  $unit_name isn't installed here — security score skipped (it runs on the VM install test)"
  fi
else
  echo "  info  no systemd-analyze on this host — verify/security skipped (the VM install test runs them)"
fi

echo
echo "check-unit-hardening: $pass ok, $failures failed"
[[ $failures -eq 0 ]]
