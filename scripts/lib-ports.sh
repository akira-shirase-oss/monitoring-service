# Sourced by scripts that start their own monitor-server or fixture (backup-drill.sh,
# dev.sh). Each uses its own configurable port — never 58400, the default an installed
# monitor listens on — refuses to start when that port already answers, and stops only
# processes it started itself (by PID, never by port).

# True when something accepts TCP connections on 127.0.0.1:$1 (bash's /dev/tcp: no lsof).
port_in_use() {
    (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null
}

# require_free_port PORT VARIABLE — exit 1 if PORT is taken, naming the override.
require_free_port() {
    if port_in_use "$1"; then
        echo "port $1 is already in use on 127.0.0.1 — refusing to run; set $2 to a free" \
            "port. This script never stops a process it did not start." >&2
        exit 1
    fi
}

# stop_own PID — stop a process this script started (TERM, then KILL after 5 s).
stop_own() {
    local pid="$1"
    [ -n "$pid" ] || return 0
    kill -0 "$pid" 2>/dev/null || return 0
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 1 50); do
        kill -0 "$pid" 2>/dev/null || { wait "$pid" 2>/dev/null || true; return 0; }
        sleep 0.1
    done
    kill -9 "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
}

# wait_http URL — wait up to 15 s for URL to answer 2xx.
wait_http() {
    for _ in $(seq 1 75); do
        curl -sf -o /dev/null "$1" && return 0
        sleep 0.2
    done
    echo "timed out waiting for $1" >&2
    return 1
}
