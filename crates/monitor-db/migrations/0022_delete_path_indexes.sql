-- Deleting an incident makes SQLite look for its outbox rows, and deleting a monitor
-- makes it look for that monitor's incidents, outbox rows and status-page listing.
-- With foreign keys on and no index on the child column, each of those lookups reads
-- the whole child table, for every parent row deleted, while holding the write lock.
-- One workspace with many incidents could stall every workspace's commits by deleting
-- a monitor with its evidence (D-026), and the retention pass paid the same scans.
-- These are the child-side indexes those lookups need.
CREATE INDEX IF NOT EXISTS notify_outbox_incident ON notify_outbox (incident_id);
CREATE INDEX IF NOT EXISTS notify_outbox_monitor ON notify_outbox (monitor_id);
-- `incidents_one_open` is partial (open incidents only); resolved ones need their own.
CREATE INDEX IF NOT EXISTS incidents_monitor ON incidents (monitor_id, id);
-- The table's key leads with the workspace, so a monitor's row was found by a scan.
CREATE INDEX IF NOT EXISTS status_monitors_monitor ON status_monitors (monitor_id);
