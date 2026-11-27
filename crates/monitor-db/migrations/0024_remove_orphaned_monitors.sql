-- `monitors.workspace_id` has no foreign key (it was added to an existing table), so a
-- monitor could be left behind when its workspace was deleted: created in the moment
-- between the request's checks and its insert. Such a monitor belongs to nobody, no
-- credential can reach it, and it was still probed. New monitors are now inserted only
-- into a workspace that exists; this removes any that were left, children first.
DELETE FROM notify_outbox WHERE monitor_id IN
    (SELECT id FROM monitors WHERE workspace_id NOT IN (SELECT id FROM workspaces));
DELETE FROM incidents WHERE monitor_id IN
    (SELECT id FROM monitors WHERE workspace_id NOT IN (SELECT id FROM workspaces));
DELETE FROM checks WHERE monitor_id IN
    (SELECT id FROM monitors WHERE workspace_id NOT IN (SELECT id FROM workspaces));
DELETE FROM check_stats_daily WHERE monitor_id IN
    (SELECT id FROM monitors WHERE workspace_id NOT IN (SELECT id FROM workspaces));
DELETE FROM monitors WHERE workspace_id NOT IN (SELECT id FROM workspaces);
