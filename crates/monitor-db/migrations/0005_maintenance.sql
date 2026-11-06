-- Maintenance windows: schedule-scoped alert suppression. Checks still record; a
-- threshold crossing inside an active window yields Effect::IncidentSuppressed
-- (no incident row, no outbox row). Windows are read at commit time via the gate —
-- never cached — so creating or deleting a window takes effect on the next slot.

CREATE TABLE maintenance_windows (
    id          INTEGER PRIMARY KEY,
    monitor_id  TEXT NOT NULL REFERENCES monitors (id) ON DELETE CASCADE,
    starts_at   TEXT NOT NULL,
    ends_at     TEXT NOT NULL,
    reason      TEXT NOT NULL DEFAULT '',
    created_at  TEXT NOT NULL,
    CHECK (ends_at > starts_at)
);

CREATE INDEX maintenance_windows_monitor ON maintenance_windows (monitor_id, ends_at);
