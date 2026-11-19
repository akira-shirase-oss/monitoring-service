-- S1d (D-017): a workspace's own webhook channel. `webhook_url` is validated at save
-- and re-resolved + revalidated at every send by the tenant deliverer; `webhook_secret`
-- signs its payloads. `notify_outbox.last_error` records why the last attempt failed —
-- including a send-time SSRF refusal — so a refused delivery is evidence, not silence.
ALTER TABLE workspaces ADD COLUMN webhook_url TEXT;
ALTER TABLE workspaces ADD COLUMN webhook_secret TEXT;

-- 'tenant_webhook' joins the channel set. SQLite can't ALTER a CHECK, so the table is
-- rebuilt with the widened constraint and `last_error` in one pass.
CREATE TABLE notify_outbox_new (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id     TEXT NOT NULL UNIQUE,
    monitor_id   TEXT NOT NULL REFERENCES monitors (id),
    workspace_id TEXT NOT NULL,
    incident_id  INTEGER NOT NULL REFERENCES incidents (id),
    kind         TEXT NOT NULL CHECK (kind IN ('incident_opened', 'incident_recovered')),
    channel      TEXT NOT NULL DEFAULT 'webhook'
                     CHECK (channel IN ('webhook', 'email', 'tenant_webhook')),
    attempts     INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TEXT,
    delivered_at TEXT,
    created_at   TEXT NOT NULL,
    last_error   TEXT
);
INSERT INTO notify_outbox_new
    (id, event_id, monitor_id, workspace_id, incident_id, kind, channel,
     attempts, next_attempt_at, delivered_at, created_at, last_error)
SELECT id, event_id, monitor_id, workspace_id, incident_id, kind, channel,
       attempts, next_attempt_at, delivered_at, created_at, NULL
  FROM notify_outbox;
DROP TABLE notify_outbox;
ALTER TABLE notify_outbox_new RENAME TO notify_outbox;
