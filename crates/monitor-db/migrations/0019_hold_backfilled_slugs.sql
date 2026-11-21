-- Hold the name 0018's backfill replaced that can still be named (N3 ruling:
-- 0018 is restored to its applied bytes; the hold lives in its own migration).
-- A database that applied the earlier 0018 already rewrote unpublished legacy
-- slugs to `ws-*` before any hold existed — those old names are unrecoverable.
-- `default` is the exception: the seeded workspace's name is known, and every
-- reader who ever knew it must never see it serve another tenant (0016's rule).
INSERT OR IGNORE INTO status_held_slugs (slug, workspace_id, released_at)
SELECT 'default', id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
  FROM workspaces
 WHERE id = '00000000-0000-0000-0000-000000000001';
