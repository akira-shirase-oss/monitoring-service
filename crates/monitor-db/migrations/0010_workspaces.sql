-- S1a (D-020): tenancy lives in the same store. Every tenant-owned row carries a
-- `workspace_id`; pre-tenancy rows backfill into the seeded default workspace. The
-- NOT NULL + DEFAULT is what ADD COLUMN allows backfill with — inserts always name
-- the column explicitly, so the default only ever describes the migrated rows.
CREATE TABLE workspaces (
    id          TEXT PRIMARY KEY,           -- uuid; WorkspaceId::DEFAULT for the seed row
    slug        TEXT NOT NULL UNIQUE,       -- public status pages are /s/<slug> (D-021)
    name        TEXT NOT NULL,
    created_at  TEXT NOT NULL
);

INSERT INTO workspaces (id, slug, name, created_at)
VALUES ('00000000-0000-0000-0000-000000000001', 'default', 'Default',
        strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));

-- No REFERENCES clause on the added columns: SQLite refuses a non-NULL default on an
-- added REFERENCES column, and the workspace FK adds nothing the monitors FK doesn't
-- already cover — every child row is anchored through monitor_id.
ALTER TABLE monitors ADD COLUMN workspace_id TEXT NOT NULL
    DEFAULT '00000000-0000-0000-0000-000000000001';
ALTER TABLE checks ADD COLUMN workspace_id TEXT NOT NULL
    DEFAULT '00000000-0000-0000-0000-000000000001';
ALTER TABLE incidents ADD COLUMN workspace_id TEXT NOT NULL
    DEFAULT '00000000-0000-0000-0000-000000000001';
ALTER TABLE maintenance_windows ADD COLUMN workspace_id TEXT NOT NULL
    DEFAULT '00000000-0000-0000-0000-000000000001';
ALTER TABLE notify_outbox ADD COLUMN workspace_id TEXT NOT NULL
    DEFAULT '00000000-0000-0000-0000-000000000001';
ALTER TABLE check_stats_daily ADD COLUMN workspace_id TEXT NOT NULL
    DEFAULT '00000000-0000-0000-0000-000000000001';

CREATE INDEX monitors_workspace ON monitors (workspace_id);
CREATE INDEX incidents_workspace ON incidents (workspace_id, opened_at DESC);
