# Sourced by scripts that start their own monitor-server or fixture (backup-drill.sh,
# dev.sh). Each uses its own configurable port — never 58400, the default an installed
# monitor listens on — refuses to start when that port already answers, and stops only
# processes it started itself (by PID, never by port), and only while they are still its
# live children: a PID whose process exited and was reaped can be reused by anything.

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

# is_own_child PID — true while PID is a process whose parent is this shell. A child that
# exited and was reaped no longer passes, even if its PID has been handed to a stranger.
is_own_child() {
    local pid="$1" parent
    [ -n "$pid" ] || return 1
    parent="$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d '[:space:]')" || return 1
    [ "$parent" = "$$" ]
}

# stop_own PID — stop a process this script started (TERM, then KILL after 5 s), checking
# before every signal that PID is still this shell's child.
stop_own() {
    local pid="$1"
    is_own_child "$pid" || { wait "$pid" 2>/dev/null || true; return 0; }
    kill "$pid" 2>/dev/null || true
    for _ in $(seq 1 50); do
        is_own_child "$pid" && kill -0 "$pid" 2>/dev/null || { wait "$pid" 2>/dev/null || true; return 0; }
        sleep 0.1
    done
    is_own_child "$pid" && kill -9 "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
}

# wait_own_http URL PID — wait up to 15 s for URL to answer 2xx while PID (the process this
# script started to serve it) is alive. If that child has exited, whatever answers on the
# port is somebody else's: fail rather than drive it.
wait_own_http() {
    local url="$1" pid="$2"
    for _ in $(seq 1 75); do
        if ! is_own_child "$pid" || ! kill -0 "$pid" 2>/dev/null; then
            echo "the process started to serve $url (pid $pid) exited — refusing to use" \
                "whatever answers there" >&2
            return 1
        fi
        if curl -sf -o /dev/null "$url"; then
            # Still ours after the answer: not a stranger that took the port in a race.
            is_own_child "$pid" && return 0
        fi
        sleep 0.2
    done
    echo "timed out waiting for $url" >&2
    return 1
}
