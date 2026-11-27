-- S2 review: a released status-page slug stays held — another workspace can never
-- take a name readers may still associate with the previous owner's page.
-- A workspace may re-take a slug it itself released (the row records who released it);
-- everyone else sees it as taken forever. Rows are only ever inserted: the set of held
-- slugs is bounded by the number of slugs ever used.
CREATE TABLE IF NOT EXISTS status_held_slugs (
    slug         TEXT PRIMARY KEY,
    workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    released_at  TEXT NOT NULL
);
