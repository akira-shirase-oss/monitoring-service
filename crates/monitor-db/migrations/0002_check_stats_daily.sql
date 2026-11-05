-- M4.1 retention: expired raw checks roll into per-day aggregates before deletion, so
-- coverage over long windows keeps an honest answer after the raw rows are gone.
-- One row per monitor per UTC day, one column per outcome — the union of this table and
-- `checks` is the complete observation history.
CREATE TABLE check_stats_daily (
    monitor_id TEXT NOT NULL REFERENCES monitors (id),
    day TEXT NOT NULL,
    pass INTEGER NOT NULL DEFAULT 0,
    http_fail INTEGER NOT NULL DEFAULT 0,
    transport INTEGER NOT NULL DEFAULT 0,
    timeout INTEGER NOT NULL DEFAULT 0,
    missed INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (monitor_id, day)
);
