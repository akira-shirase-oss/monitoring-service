-- Re-review follow-ups:
-- 1) The invitation email joins used to wrap both sides in lower(), which no
--    plain index can serve. Both columns are normalized at write time
--    (accounts.email is "stored normalized (lowercase, trimmed)" per 0011 and
--    invite_member lowercases before insert), so the joins now compare the
--    columns directly and the existing email indexes apply. Any rows written
--    before that contract are folded here so the plain comparisons stay total;
--    case-duplicates in one workspace collapse to the earliest row first, so
--    folding can never collide on the (workspace_id, email) uniqueness.
DELETE FROM workspace_invitations
 WHERE EXISTS (
        SELECT 1 FROM workspace_invitations o
         WHERE o.workspace_id = workspace_invitations.workspace_id
           AND lower(o.email) = lower(workspace_invitations.email)
           AND o.rowid < workspace_invitations.rowid);
UPDATE workspace_invitations SET email = lower(email) WHERE email != lower(email);

-- 2) Backfill: workspaces created before public slugs were randomized still
--    carry an email-derived slug. Pages that are not published get a fresh
--    random name — a published page keeps its slug because it is the URL the
--    world already knows, and releasing it would put the name on the held list
--    under a live page. Every name being replaced is held first (S3 review):
--    released unheld, a fresh install's seeded `default` and a turned-off
--    page's chosen slug alike would be claimable by another tenant while
--    readers still associate the name with its previous owner (0016's rule).
INSERT OR IGNORE INTO status_held_slugs (slug, workspace_id, released_at)
SELECT slug, id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
  FROM workspaces
 WHERE slug NOT LIKE 'ws-%'
   AND status_title IS NULL;
UPDATE workspaces
SET slug = 'ws-' || lower(hex(randomblob(6)))
WHERE slug NOT LIKE 'ws-%'
  AND status_title IS NULL;
