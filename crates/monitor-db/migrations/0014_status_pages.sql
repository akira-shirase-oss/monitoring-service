-- S2 (D-021): a per-workspace public status page at /s/<workspace-slug>. `status_title`
-- NULL is the page's off switch — a workspace publishes nothing until a member names
-- the page. `status_monitors` is the allowlist of monitors the owner chose to publish,
-- in display order; rows stay when the page is off, so re-enabling restores the set.
ALTER TABLE workspaces ADD COLUMN status_title TEXT;

CREATE TABLE status_monitors (
    workspace_id TEXT NOT NULL REFERENCES workspaces (id) ON DELETE CASCADE,
    monitor_id   TEXT NOT NULL REFERENCES monitors (id) ON DELETE CASCADE,
    position     INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (workspace_id, monitor_id)
);
