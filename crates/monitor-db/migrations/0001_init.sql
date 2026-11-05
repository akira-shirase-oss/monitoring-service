-- 0001_init.sql — the monitor's local store (SQLite is the authority; no server database).
-- Times are RFC 3339 text. `generation` on checks/incidents lets a late probe result be
-- recognized as stale after an edit or pause (the contract's generation-invalidation case).

CREATE TABLE monitors (
    id          TEXT PRIMARY KEY,             -- uuid v7
    name        TEXT NOT NULL,
    url         TEXT NOT NULL,
    interval_s  INTEGER NOT NULL DEFAULT 60 CHECK (interval_s >= 15),
    timeout_s   INTEGER NOT NULL DEFAULT 5  CHECK (timeout_s > 0 AND timeout_s <= interval_s),
    generation  INTEGER NOT NULL DEFAULT 1,
    paused      INTEGER NOT NULL DEFAULT 0 CHECK (paused IN (0, 1)),
    created_at  TEXT NOT NULL
);

-- One row per slot — including `missed` slots. A miss is recorded evidence: the streak
-- rebuild after restart replays this table, and a gap that reset streaks must replay as a
-- gap (absent rows would count pre-miss and post-miss failures as consecutive, fabricating
-- an incident that never opened). Coverage = non-missed rows / total rows.
CREATE TABLE checks (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    monitor_id   TEXT NOT NULL REFERENCES monitors (id),
    generation   INTEGER NOT NULL,            -- monitor generation when the slot was claimed
    slot_at      TEXT NOT NULL,               -- the scheduled slot, not when the row landed
    outcome      TEXT NOT NULL CHECK (outcome IN ('pass', 'http_fail', 'transport', 'timeout', 'missed')),
    http_status  INTEGER,
    latency_ms   INTEGER,
    detail       TEXT,                        -- classified, sanitized — never tokens or bodies
    recorded_at  TEXT NOT NULL
);
CREATE INDEX checks_monitor_slot ON checks (monitor_id, slot_at DESC);

-- At most one open incident per monitor (partial unique index below).
CREATE TABLE incidents (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    monitor_id   TEXT NOT NULL REFERENCES monitors (id),
    opened_at    TEXT NOT NULL,
    resolved_at  TEXT,                        -- set when two fresh passes confirm recovery
    state        TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'resolved'))
);
CREATE UNIQUE INDEX incidents_one_open ON incidents (monitor_id) WHERE state = 'open';

-- Alert delivery is committed with the incident transition; a separate pool delivers with
-- finite retries under stable event ids.
CREATE TABLE notify_outbox (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id     TEXT NOT NULL UNIQUE,        -- stable id for at-least-once delivery
    monitor_id   TEXT NOT NULL REFERENCES monitors (id),
    incident_id  INTEGER NOT NULL REFERENCES incidents (id),
    kind         TEXT NOT NULL CHECK (kind IN ('incident_opened', 'incident_recovered')),
    attempts     INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TEXT,
    delivered_at TEXT,
    created_at   TEXT NOT NULL
);
