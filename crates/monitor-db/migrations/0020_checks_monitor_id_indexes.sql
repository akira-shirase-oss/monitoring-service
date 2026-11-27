-- G3 re-review: `recent_outcomes` orders by (monitor_id, id DESC) on boot
-- rebuild, but only checks_monitor_slot(monitor_id, slot_at DESC) existed —
-- every monitor's rebuild sorted its whole match set through a temp B-tree,
-- so restart time grew with fleet history and upgrades could out-wait the
-- readiness poll and roll back.
CREATE INDEX IF NOT EXISTS checks_monitor_id ON checks (monitor_id, id);

-- The coverage-gap tail selects only the newest `missed`/`address_budget`
-- rows — a partial index so the tiny gap fraction pays for its own search.
-- The predicate uses IS exactly as the query does: SQLite only serves a
-- partial index whose WHERE the query repeats verbatim.
CREATE INDEX IF NOT EXISTS checks_monitor_budget_gaps ON checks (monitor_id, id)
 WHERE outcome = 'missed' AND detail IS 'address_budget';
