-- S3 (D-022): every workspace carries a plan — the tier whose limits the server
-- enforces on it. Signup mints `free`; the seeded default workspace is the
-- operator's own monitoring and gets `operator`, so the migration never silently
-- downgrades the maintainer's running probes (15 s intervals would violate the
-- free floor). The CHECK is the same closed set `monitor_domain::Plan` parses.
ALTER TABLE workspaces ADD COLUMN plan TEXT NOT NULL DEFAULT 'free'
    CHECK (plan IN ('free', 'operator'));

UPDATE workspaces SET plan = 'operator'
    WHERE id = '00000000-0000-0000-0000-000000000001';
